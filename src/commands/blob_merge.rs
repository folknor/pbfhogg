//! Blob-level permutation merge: the one engine behind `sort` (one input,
//! every element kept) and `cat --dedupe` (any number of inputs, exact
//! duplicates dropped).
//!
//! Pass 1 ([`index_inputs`]) walks every input's blob headers with
//! [`HeaderWalker`] (`O_DIRECT` under `--direct-io`) and records one [`Frame`] per OSMData blob it keeps, with
//! one [`BlobEntry`] per element kind the frame holds: one for an ordinary
//! blob, one per kind present for a block no single index describes. [`plan`]
//! orders the entries by (kind, first key), flags those whose ID ranges
//! overlap a same-kind neighbour plus those that must be rewritten whatever
//! their range (internally out of order, or a block no single index
//! describes), and groups the flagged ones into kind-bounded overlap runs.
//! Pass 2 ([`write_plan`]) writes the entries in order: a frame outside every
//! run is copied through (coalesced into one `copy_file_range` per contiguous
//! stretch of one input where the output can splice, or reframed with a
//! scanned index where its own is missing or false), and each overlap run is
//! decoded and merged through a binary heap and re-encoded.
//!
//! Overlap runs are swept on worker threads and stream their blocks to the
//! write loop in run order, through a bounded channel per run and an
//! admission window over the runs (see [`write_plan`] for the memory bound
//! and the shutdown protocol). A run is one serial sweep however large, so a
//! single giant overlap component streams rather than being held whole.
//!
//! The commands differ only in their [`IndexPolicy`] and in whether the sweep
//! drops exact duplicates; their markers, counters and diagnostic keys stay
//! in the command modules.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::os::unix::fs::FileExt as _;

use crate::Element;
use crate::ElementKind;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{
    BboxClaim, BlobIndex, TagIndex, try_scan_block_ids_and_tags, try_scan_block_ids_checked,
    try_scan_block_tags,
};
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, read_dense_node, read_node, read_relation, read_way,
    write_single_node_local, write_single_relation_local, write_single_way_local,
};
use crate::read::header_walker::{BlobHeaderMeta, HeaderWalker, Positional};
use crate::read::input::{InputFile, PinnedInput};
use crate::writer::{PbfWriter, reframe_raw_with_index};

// ---------------------------------------------------------------------------
// Blob index
// ---------------------------------------------------------------------------

/// One OSMData blob of one input that pass 1 kept: where its frame is, and
/// what a passthrough of it writes.
pub(crate) struct Frame {
    /// Which input (an index into the slice handed to [`index_inputs`], and
    /// into the sources pass 2 opens from it).
    pub(crate) source: usize,
    /// Byte offset of the complete frame in its input.
    pub(crate) file_offset: u64,
    /// Length of the complete frame (4 + header_len + data_size).
    pub(crate) frame_len: u64,
    /// The blob header a passthrough gives this frame instead of its own:
    /// set for a frame without indexdata, and for one whose indexdata pass 1
    /// scanned and found false. `None` copies the frame whole. [`plan`]
    /// clears it on a rewritten frame, so after planning it is exactly "this
    /// passthrough is reframed".
    pub(crate) reindex: Option<Reindex>,
}

/// The index and tag key set a reframed passthrough carries, both scanned
/// from its payload, so it is indexed as plain `cat` indexes it.
pub(crate) struct Reindex {
    index: BlobIndex,
    tagdata: Option<Box<[u8]>>,
    /// The frame carried indexdata its payload contradicts (only `sort`,
    /// under a header that does not claim `Sort.Type_then_ID`, scans an
    /// indexed blob and can find this).
    replaces_false_claim: bool,
}

/// One element kind's share of a [`Frame`]: what the plan orders and the
/// sweep merges. An ordinary blob has one; a block no single index describes
/// has one per kind it holds, all of the same frame and all rewritten.
pub(crate) struct BlobEntry {
    /// The frame (an index into [`Plan::frames`]).
    pub(crate) frame: usize,
    /// Element kind + ID range of this kind's elements in the frame.
    pub(crate) index: BlobIndex,
    /// True when this blob's elements are internally out of canonical OSM ID
    /// order. Set by the payload scan, which runs for every blob except an
    /// indexed one under a header claiming `Sort.Type_then_ID`: intra-blob
    /// order there is a precondition of the claim itself (see
    /// CORRECTNESS.md). A flagged blob is routed into the rewrite path so the
    /// sweep reorders it, even when its range overlaps no neighbour. Not
    /// computed for a restructured entry, which is rewritten anyway.
    pub(crate) intra_unsorted: bool,
    /// The frame's block has no single index (kinds change across groups,
    /// or a group carries more than one element field), so it can never be
    /// copied: every one of its entries is rewritten through the sweep,
    /// whose extraction takes only the entry's kind from the frame.
    pub(crate) restructured: bool,
    /// True when `index` was derived by decoding the payload, false when it
    /// was read out of the BlobHeader and believed. Only the element counters
    /// need this: a passthrough blob's `index.count` on the header-only path
    /// is the file's own unverified claim, reported apart from the observed
    /// counts (`passthrough_claimed`).
    pub(crate) index_scanned: bool,
}

/// What pass 1 found: the frames it kept, their entries in input then file
/// order, and the blobs it dropped.
pub(crate) struct Indexed {
    pub(crate) frames: Vec<Frame>,
    pub(crate) entries: Vec<BlobEntry>,
    /// Blobs whose block decoded to no element at all. They are left out of
    /// the output, as plain `cat` leaves them out: no index can describe
    /// them, and they hold no OSM content (only whatever unknown fields they
    /// carried, which go with them).
    pub(crate) omitted: u64,
}

/// What pass 1 accepts and refuses: the part of a command's input contract
/// the shared walk enforces.
pub(crate) struct IndexPolicy {
    /// Refuse an input whose header does not claim `Sort.Type_then_ID`, with
    /// this reason (`cat --dedupe`, which promises a merge of sorted inputs).
    pub(crate) require_sorted: Option<&'static str>,
    /// Exempt the walk from the command-input refusal of a partially indexed
    /// input (`HeaderWalker::accepting_partial_index`). `sort` refuses one,
    /// as every command does; `cat --dedupe` accepts it, because `cat` is
    /// the remedy, scanning each unindexed blob (reference/indexdata-trust.md,
    /// "Missing and partial indexdata").
    pub(crate) accept_partial_index: bool,
}

/// Pass 1: walk every input, in order, and index its OSMData blobs.
///
/// Returns one header per input, in input order, and what the walk indexed.
/// Each input must start with its one OSMHeader: a first
/// frame of any other type is the missing-header error at once, and a later
/// OSMHeader is refused by the walk's shared framing policy
/// (`read::frame::check_header_position`) rather than ignored, because what
/// it declares would otherwise be silently left out of the output header.
///
/// The walk preads headers only (`fadvise(RANDOM)`), never pulling skipped
/// payloads into the page cache: `sort`'s planet pass 1 measured 32 GB read
/// through a buffered reader against 674 MB walked (reference/
/// performance-history.md, "Sort"). An indexed blob under a header claiming
/// `Sort.Type_then_ID` is indexed from its indexdata alone. Every other blob
/// is preaded, decompressed and scanned, which also checks its intra-blob
/// order: indexdata alone does not prove it (`cat` attaches indexdata to
/// payloads it never reorders), and the trusted ordering signal is the header
/// claim, the contract `ElementReader` keys its guarantees on.
///
/// A scanned payload ends in one of four ways. Malformed bytes fail the run
/// with the scan's own error. A block one index describes becomes one entry;
/// without indexdata it is also scanned for its tag keys, so a passthrough of
/// it is reframed with both, as plain `cat` indexes it. A block no single
/// index describes is decoded: with no element it is omitted, otherwise it
/// gets one restructured entry per kind it holds. An indexed blob scanned for
/// order (`sort` under a header without the sorted claim) whose indexdata
/// its payload contradicts is reframed with the scanned index rather than
/// copied with the false one. Its claim holds exactly when `check --index`
/// would report nothing against it: kind, count and ID range equal, and a
/// bbox it carries containing the scanned one (an absent bbox is no claim;
/// a node coordinate outside the decimicrodegree range, where the scan has
/// no box, makes any carried box false). [`BlobIndex::bbox_claim`] is that
/// rule's bbox half, shared with the check.
///
/// `direct_io` (the `--direct-io` flag) walks every input `O_DIRECT`
/// ([`HeaderWalker::open_direct`]): the same probes and payload reads, as
/// aligned reads that bypass the page cache (where the filesystem honours
/// `O_DIRECT`; `crate::direct_io` names the ones that may not). Without the
/// `linux-direct-io` feature it is refused before any input is walked.
#[hotpath::measure]
pub(crate) fn index_inputs(
    inputs: &[PinnedInput],
    policy: &IndexPolicy,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<(Vec<crate::HeaderBlock>, Indexed)> {
    #[cfg(not(feature = "linux-direct-io"))]
    if direct_io {
        return Err(crate::file_reader::direct_reads_unsupported());
    }
    let mut walk = Walk {
        policy,
        host,
        indexed: Indexed {
            frames: Vec::new(),
            entries: Vec::new(),
            omitted: 0,
        },
        data_buf: Vec::new(),
        decompress_buf: Vec::new(),
        noted_unclaimed: false,
    };
    let mut headers = Vec::with_capacity(inputs.len());
    for (source, input) in inputs.iter().enumerate() {
        #[cfg(feature = "linux-direct-io")]
        if direct_io {
            let walker = HeaderWalker::open_direct(input, host)?;
            headers.push(walk.input(source, input, walker)?);
            continue;
        }
        let walker = HeaderWalker::open(input, host)?;
        headers.push(walk.input(source, input, walker)?);
    }
    Ok((headers, walk.indexed))
}

/// Pass-1 state shared across every input of a run.
struct Walk<'a> {
    policy: &'a IndexPolicy,
    host: &'a crate::host::Host,
    indexed: Indexed,
    data_buf: Vec<u8>,
    decompress_buf: Vec<u8>,
    noted_unclaimed: bool,
}

/// What the payload scan made of one block.
enum Scanned {
    /// One index describes it. `tagdata` is its serialized tag key set, when
    /// the scan was asked for it and the key set encodes.
    Block {
        index: BlobIndex,
        sorted: bool,
        tagdata: Option<Box<[u8]>>,
    },
    /// No single index does: the index of each kind it holds, in kind order,
    /// empty when it holds no element.
    Kinds(Vec<BlobIndex>),
}

impl Walk<'_> {
    /// Index one input over `walker`, freshly opened on it.
    fn input<P: Positional>(
        &mut self,
        source: usize,
        input: &PinnedInput,
        mut walker: HeaderWalker<P>,
    ) -> Result<crate::HeaderBlock> {
        if self.policy.accept_partial_index {
            walker = walker.accepting_partial_index();
        }
        let mut header: Option<crate::HeaderBlock> = None;
        while let Some(meta) = walker.next_header()? {
            let claims_sorted = header.as_ref().map(crate::HeaderBlock::is_sorted);
            match meta.blob_type {
                // The walk refuses an OSMHeader anywhere but first.
                BlobKind::OsmHeader => header = Some(self.header(&walker, &meta)?),
                _ if claims_sorted.is_none() => {
                    return Err(crate::error::new_missing_header(input.path()));
                }
                BlobKind::OsmData => {
                    self.data_blob(source, &walker, &meta, claims_sorted == Some(true))?;
                }
                // Unknown extension blob: the walk has already stepped past it.
                _ => {}
            }
        }
        header.ok_or_else(|| crate::error::new_missing_header(input.path()))
    }

    fn header<P: Positional>(
        &mut self,
        walker: &HeaderWalker<P>,
        meta: &BlobHeaderMeta,
    ) -> Result<crate::HeaderBlock> {
        let located = |e| walker.locate_meta(meta, e);
        walker
            .pread_data(meta.data_offset, meta.data_size, &mut self.data_buf)
            .map_err(located)?;
        let hdr = decode_blob_to_headerblock(&self.data_buf).map_err(located)?;
        if let Some(reason) = self.policy.require_sorted {
            let path = walker.path();
            crate::commands::require_sorted(
                hdr.is_sorted(),
                path,
                &format!("Input file {}", path.display()),
                reason,
            )
            .map_err(located)?;
        }
        Ok(hdr)
    }

    /// Index one OSMData blob: push its frame and entries, or count it
    /// omitted. See [`index_inputs`] for the outcomes.
    fn data_blob<P: Positional>(
        &mut self,
        source: usize,
        walker: &HeaderWalker<P>,
        meta: &BlobHeaderMeta,
        claims_sorted: bool,
    ) -> Result<()> {
        let frame = self.indexed.frames.len();
        let mut new_frame = Frame {
            source,
            file_offset: meta.frame_start,
            frame_len: meta.frame_size as u64,
            reindex: None,
        };
        let claimed = match meta.index {
            // Believed: the frame's own header (its tagdata included) is
            // what a passthrough copies.
            Some(index) if claims_sorted => {
                self.indexed.frames.push(new_frame);
                self.indexed.entries.push(BlobEntry {
                    frame,
                    index,
                    intra_unsorted: false,
                    restructured: false,
                    index_scanned: false,
                });
                return Ok(());
            }
            claimed => claimed,
        };
        if claimed.is_some() && !self.noted_unclaimed {
            self.noted_unclaimed = true;
            // Only `sort` reaches this: `cat --dedupe` refuses an input
            // whose header does not claim sorted.
            crate::host::diag!(
                self.host.diagnostics,
                Note,
                "sort.verify_unclaimed_order",
                "input does not declare Sort.Type_then_ID; verifying \
                 intra-blob order (pass 1 decodes every blob payload)"
            );
        }
        let located = |e| walker.locate_meta(meta, e);
        // Tag keys are wanted only where a passthrough is reframed: a blob
        // without indexdata. An indexed one is copied whole unless its claim
        // turns out false, which rescans the buffer below.
        let scanned = self
            .scan_payload(walker, meta, claimed.is_none())
            .map_err(located)?;
        match scanned {
            Scanned::Block {
                index,
                sorted,
                tagdata,
            } => {
                new_frame.reindex = match claimed {
                    None => Some(Reindex {
                        index,
                        tagdata,
                        replaces_false_claim: false,
                    }),
                    Some(claim) if claim_holds(&claim, &index) => None,
                    Some(_) => Some(Reindex {
                        index,
                        tagdata: scanned_tagdata(&self.decompress_buf).map_err(located)?,
                        replaces_false_claim: true,
                    }),
                };
                self.indexed.frames.push(new_frame);
                self.indexed.entries.push(BlobEntry {
                    frame,
                    index,
                    intra_unsorted: !sorted,
                    restructured: false,
                    index_scanned: true,
                });
            }
            Scanned::Kinds(kinds) if kinds.is_empty() => self.indexed.omitted += 1,
            Scanned::Kinds(kinds) => {
                self.indexed.frames.push(new_frame);
                self.indexed
                    .entries
                    .extend(kinds.into_iter().map(|index| BlobEntry {
                        frame,
                        index,
                        intra_unsorted: false,
                        restructured: true,
                        index_scanned: true,
                    }));
            }
        }
        Ok(())
    }

    /// Pread, decompress and scan one payload's IDs, checking intra-blob
    /// monotonicity on the way (one compare per element, on a scan that
    /// visits every ID anyway), and its tag keys when `with_tags`. A block
    /// no single index describes is decoded for the kinds it holds.
    fn scan_payload<P: Positional>(
        &mut self,
        walker: &HeaderWalker<P>,
        meta: &BlobHeaderMeta,
        with_tags: bool,
    ) -> Result<Scanned> {
        walker.pread_data(meta.data_offset, meta.data_size, &mut self.data_buf)?;
        decompress_blob_data_into(&self.data_buf, &mut self.decompress_buf)?;
        let (index, tags) = if with_tags {
            try_scan_block_ids_and_tags(&self.decompress_buf)?
        } else {
            (try_scan_block_ids_checked(&self.decompress_buf)?, None)
        };
        match index {
            Some((index, sorted)) => Ok(Scanned::Block {
                index,
                sorted,
                tagdata: tags.as_ref().and_then(serialize_tags),
            }),
            None => kind_indexes(&self.decompress_buf, &self.host.invocation).map(Scanned::Kinds),
        }
    }
}

/// Whether a blob's indexdata claim is true of the index scanned from its
/// payload, by `check --index`'s rule: kind, count and ID range exact, and
/// the bbox judged by [`BlobIndex::bbox_claim`] (an absent one no claim, a
/// present one containing the scanned box; a node coordinate outside the
/// decimicrodegree range makes any box false).
fn claim_holds(claim: &BlobIndex, scanned: &BlobIndex) -> bool {
    claim.kind == scanned.kind
        && claim.count == scanned.count
        && claim.min_id == scanned.min_id
        && claim.max_id == scanned.max_id
        && matches!(
            claim.bbox_claim(scanned),
            BboxClaim::Absent | BboxClaim::Holds
        )
}

/// The serialized tag key set of a block one index describes.
fn scanned_tagdata(block: &[u8]) -> Result<Option<Box<[u8]>>> {
    Ok(try_scan_block_tags(block)?
        .as_ref()
        .and_then(serialize_tags))
}

fn serialize_tags(tags: &TagIndex) -> Option<Box<[u8]>> {
    tags.serialize().map(Vec::into_boxed_slice)
}

/// The index of each element kind a block holds, in kind order, for a block
/// the scan found no single index for. Decoding is the cold path's cost: such
/// blocks come only from third-party producers, and a decode failure here is
/// the block's real error (the scan validates less than the decoder does).
/// The bbox is left out, since these entries are rewritten, never copied.
/// One cancellation check before the decode: the walk checks between blobs,
/// and one block is bounded by the blob size limit.
fn kind_indexes(block: &[u8], cancel: &crate::host::Invocation) -> Result<Vec<BlobIndex>> {
    cancel.check()?;
    let block = crate::PrimitiveBlock::from_vec(block.to_vec())?;
    let mut ranges: [Option<(i64, i64, u64)>; 3] = [None; 3];
    for element in block.elements() {
        let (kind, id) = match &element {
            Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
            Element::Node(n) => (ElementKind::Node, n.id()),
            Element::Way(w) => (ElementKind::Way, w.id()),
            Element::Relation(r) => (ElementKind::Relation, r.id()),
        };
        let range = &mut ranges[usize::from(type_order(kind))];
        *range = Some(match *range {
            None => (id, id, 1),
            Some((min, max, count)) => (min.min(id), max.max(id), count + 1),
        });
    }
    Ok([ElementKind::Node, ElementKind::Way, ElementKind::Relation]
        .into_iter()
        .zip(ranges)
        .filter_map(|(kind, range)| {
            range.map(|(min_id, max_id, count)| BlobIndex {
                kind,
                min_id,
                max_id,
                count,
                bbox: None,
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Plan: order, overlap detection, runs
// ---------------------------------------------------------------------------

/// One rewrite run, `entries[start..end]`, all of `kind`: an overlap
/// component, or one entry that overlaps nothing but must be rewritten.
pub(crate) struct Run {
    start: usize,
    end: usize,
    kind: ElementKind,
}

/// The entries in output order and what pass 2 does with each.
///
/// The counts are of input blobs (frames), never of entries, so a blob that
/// holds several kinds counts once in each total: `blobs_total` is
/// `omitted + passthrough + rewritten`, the passthroughs being exactly the
/// frames no entry of which is rewritten. The attributions (`overlapping`,
/// `intra_unsorted`, `restructured`) are not additive: a restructured blob
/// whose way entry joins an overlap component counts in both.
pub(crate) struct Plan {
    pub(crate) frames: Vec<Frame>,
    pub(crate) entries: Vec<BlobEntry>,
    /// Per entry: rewritten in an overlap run, rather than copied through.
    pub(crate) rewrite: Vec<bool>,
    /// Blobs with an entry in a genuine range-overlap run.
    pub(crate) overlapping: usize,
    /// Blobs routed to rewrite only for intra-blob disorder (disjoint from
    /// `overlapping` and `restructured`).
    pub(crate) intra_unsorted: usize,
    /// Blobs whose block no single index describes, rewritten whatever their
    /// ranges.
    pub(crate) restructured: usize,
    /// Blobs left out because they hold no element ([`Indexed::omitted`]).
    pub(crate) omitted: u64,
    /// Blobs with at least one rewritten entry.
    pub(crate) blobs_rewritten: u64,
    /// Passthrough blobs reframed because their indexdata was false.
    pub(crate) false_claims_replaced: u64,
    pub(crate) runs: Vec<Run>,
}

impl Plan {
    /// Every input data blob pass 1 saw, omitted ones included.
    pub(crate) fn blobs_total(&self) -> u64 {
        self.frames.len() as u64 + self.omitted
    }
}

/// Order the entries by (kind, first key), find the overlap runs and fold
/// in the entries that must be rewritten whatever their range.
///
/// The sort is stable, so entries with equal keys keep input then file
/// order, which the sweep's heap pushes (and so which of two exact
/// duplicates survives) depend on.
pub(crate) fn plan(indexed: Indexed) -> Plan {
    let Indexed {
        mut frames,
        mut entries,
        omitted,
    } = indexed;
    entries.sort_by_key(|e| {
        (
            type_order(e.index.kind),
            crate::osm_id::blob_osm_first_key(e.index.min_id, e.index.max_id),
        )
    });
    let mut rewrite = vec![false; entries.len()];
    let mut runs = Vec::new();
    // Per frame: an entry in an overlap component, rewritten for disorder
    // alone, restructured.
    let mut in_overlap = vec![false; frames.len()];
    let mut for_disorder = vec![false; frames.len()];
    let mut components = overlap_components(&entries).into_iter().peekable();
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        if let Some(&(start, end)) = components.peek()
            && start == i
        {
            components.next();
            rewrite[start..end].fill(true);
            for entry in &entries[start..end] {
                in_overlap[entry.frame] = true;
            }
            runs.push(Run { start, end, kind });
            i = end;
            continue;
        }
        // An entry that overlaps nothing but cannot be copied: rewritten
        // alone, so the sweep reorders it (disorder) or re-encodes its kind's
        // share of the block (restructured). One both unsorted and
        // overlapping is in its component already and counted there,
        // keeping the attributions disjoint.
        let entry = &entries[i];
        if entry.restructured || entry.intra_unsorted {
            rewrite[i] = true;
            if !entry.restructured {
                for_disorder[entry.frame] = true;
            }
            runs.push(Run {
                start: i,
                end: i + 1,
                kind,
            });
        }
        i += 1;
    }

    let mut rewritten = vec![false; frames.len()];
    let mut restructured = vec![false; frames.len()];
    for (entry, &r) in entries.iter().zip(&rewrite) {
        rewritten[entry.frame] |= r;
        restructured[entry.frame] |= entry.restructured;
    }
    debug_assert!(
        entries
            .iter()
            .zip(&rewrite)
            .all(|(e, &r)| r || !e.restructured),
        "a restructured entry is never copied",
    );
    // A rewritten frame is never reframed: what its passthrough would have
    // carried goes, so `reindex` now means "this passthrough is reframed".
    for (frame, &r) in frames.iter_mut().zip(&rewritten) {
        if r {
            frame.reindex = None;
        }
    }
    let count = |flags: &[bool]| flags.iter().filter(|&&f| f).count();
    let false_claims_replaced = frames
        .iter()
        .filter(|f| f.reindex.as_ref().is_some_and(|r| r.replaces_false_claim))
        .count() as u64;
    Plan {
        overlapping: count(&in_overlap),
        intra_unsorted: count(&for_disorder),
        restructured: count(&restructured),
        omitted,
        blobs_rewritten: count(&rewritten) as u64,
        false_claims_replaced,
        frames,
        entries,
        rewrite,
        runs,
    }
}

impl Plan {
    /// What the output's way blobs are, for the output header: a way blob in
    /// a rewrite run is re-encoded without enrichments, a reframed
    /// passthrough way blob keeps its payload under a new header (field 5
    /// dropped), every other way blob is copied whole.
    pub(crate) fn ways(&self) -> crate::commands::output_header::Ways {
        let way_blobs = || {
            self.entries
                .iter()
                .zip(&self.rewrite)
                .filter(|(e, _)| e.index.kind == ElementKind::Way)
        };
        crate::commands::output_header::Ways::from_copy(
            way_blobs().any(|(_, &r)| r),
            way_blobs().any(|(e, &r)| !r && self.frames[e.frame].reindex.is_some()),
        )
    }
}

fn type_order(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Node => 0,
        ElementKind::Way => 1,
        ElementKind::Relation => 2,
    }
}

/// The range-overlap components, each `(start, end)` with at least two
/// entries.
///
/// Entries arrive sorted by first key within a kind. The test is a
/// running-maximum sweep, not a pairwise-adjacent comparison, because overlap
/// is **not transitive**: with A=[1..1000], B=[2..3], C=[500..600], A/B overlap
/// but B/C do not (`B.max=3 < C.min=500`), so a pairwise scan leaves C
/// unflagged. The sweep-merge then emits A's ids 500..1000 and C follows as a
/// raw passthrough blob carrying ids 500..600 - output that is not sorted while
/// the header still stamps `Sort.Type_then_ID`, and (in `cat --dedupe`) A/C
/// duplicates that the "exact duplicates removed" contract promised to drop.
///
/// Extending the run while `next.first_key <= run_max` closes that: once a blob
/// joins a run, every later blob whose first key falls inside the run's widest
/// reach so far joins too. Components stay kind-bounded - the downstream
/// sweep is kind-gated and silently drops off-kind elements handed to it
/// (the `cat --dedupe` regression where `merge_pbf([A, A])` lost its ways
/// and relations).
///
/// Each component is its own run, swept on its own: the next component of a
/// kind starts above this one's widest reach, so no element and no equal
/// (id, version) pair crosses the boundary, and sweeping them apart emits the
/// same element sequence as one sweep over both (that sweep empties its heap
/// at the boundary anyway). What the boundary does change is blocks: each run
/// flushes its builder, so adjacent components no longer share an output
/// block - and no longer share `BlockBuilder`'s backfill of default metadata
/// when one carries node metadata and the other does not. That is what lets
/// a file merged with itself, all two-blob components, sweep in parallel.
fn overlap_components(entries: &[BlobEntry]) -> Vec<(usize, usize)> {
    let mut components = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        let mut run_max =
            crate::osm_id::blob_osm_last_key(entries[i].index.min_id, entries[i].index.max_id);
        let mut j = i + 1;
        while j < entries.len()
            && entries[j].index.kind == kind
            && crate::osm_id::blob_osm_first_key(entries[j].index.min_id, entries[j].index.max_id)
                <= run_max
        {
            run_max = run_max.max(crate::osm_id::blob_osm_last_key(
                entries[j].index.min_id,
                entries[j].index.max_id,
            ));
            j += 1;
        }
        if j > i + 1 {
            components.push((i, j));
            i = j;
        } else {
            i += 1;
        }
    }
    components
}

// ---------------------------------------------------------------------------
// Pass 2
// ---------------------------------------------------------------------------

/// Open pass 2's inputs: one positional-read source per argument, from its
/// pin, consuming the pins and dropping each as soon as its source is open
/// (a repeated argument's shared pin goes with its last argument). An open,
/// verified source reads its file whatever its path names later, so the run
/// holds about one input descriptor per argument from here on, which is what
/// `cat --dedupe` asks the host for. The walk's descriptions are closed by
/// now; these are fresh ones.
///
/// They are buffered whatever `--direct-io` says: pass 2 has never honoured
/// the flag. The splicing backends (`--io-uring` among them, which
/// `--direct-io` does not turn off) copy from these descriptors at unaligned
/// frame offsets, so there a direct pass 2 needs a second descriptor per
/// input beside the one the copy uses, which changes what the run asks the
/// host for; only on a backend that cannot splice could one direct
/// descriptor serve.
pub(crate) fn open_sources(inputs: Vec<PinnedInput>) -> Result<Vec<InputFile>> {
    let mut sources = Vec::with_capacity(inputs.len());
    for pin in inputs {
        sources.push(InputFile::open(&pin)?);
    }
    Ok(sources)
}

/// What pass 2 did.
#[derive(Default)]
pub(crate) struct WriteCounts {
    /// Observed counts: elements a sweep emitted, plus passthrough blobs
    /// whose index pass 1 scanned from the payload.
    pub(crate) nodes: u64,
    pub(crate) ways: u64,
    pub(crate) relations: u64,
    /// Elements in passthrough blobs counted from their own unverified
    /// indexdata.
    pub(crate) passthrough_claimed: u64,
    /// Input blobs copied through, raw or reframed.
    pub(crate) blobs_passthrough: u64,
    /// Input blobs with at least one entry rewritten ([`Plan::blobs_rewritten`]).
    pub(crate) blobs_rewritten: u64,
    pub(crate) duplicates_removed: u64,
    pub(crate) copy_range_calls: u64,
    pub(crate) copy_range_coalesced: u64,
}

/// What one overlap run emitted.
#[derive(Debug, Default)]
struct RunCounts {
    elements: u64,
    duplicates: u64,
}

/// Pass 2: write the plan in order to `writer`.
///
/// `dedupe` drops, inside overlap runs, an element whose (id, version)
/// equals the one just emitted. `use_copy_range` is whether the output can
/// splice (`OutputBackend::supports_copy_range`).
///
/// With no overlap runs (an already-sorted input: production `sort`) this is
/// one serial loop and no thread starts. Otherwise each run is one task of a
/// streamed ordered pool ([`crate::ordered_pool::run_ordered_streams`]):
/// runs are swept on worker threads (sized as the decode pools are), each
/// sending its blocks as the builder fills them through a channel of
/// [`OVERLAP_RUN_CHANNEL_DEPTH`] messages, while this thread writes the
/// entries in order, a run's blocks as they arrive. At most `window` runs
/// ([`OVERLAP_RUN_WINDOW_PER_WORKER`] per worker) are issued ahead of the
/// one being written. Memory: at most `window x depth` blocks buffered plus
/// one blocked send per worker, beside each active sweep's heap (bounded by
/// its run's overlap depth; up to `min(window, workers)` heaps at once).
/// Progress, shutdown and the failure order are the pool's; the first error
/// met in run order is returned, unless a worker panicked, which the join
/// re-raises. Its stall counters (`ordered_pool_*_wait_ns`) are this pass's:
/// the write loop waiting on a sweep is `ordered_pool_result_recv_wait_ns`,
/// a sweep blocked behind the write loop `ordered_pool_result_send_wait_ns`.
///
/// [`OVERLAP_RUN_CHANNEL_DEPTH`]: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH
/// [`OVERLAP_RUN_WINDOW_PER_WORKER`]: crate::tunables::OVERLAP_RUN_WINDOW_PER_WORKER
#[hotpath::measure]
pub(crate) fn write_plan(
    plan: &Plan,
    sources: &[InputFile],
    writer: &mut PbfWriter<FileWriter>,
    dedupe: bool,
    use_copy_range: bool,
    host: &crate::host::Host,
) -> Result<WriteCounts> {
    let mut out = PlanWriter {
        plan,
        sources,
        writer,
        use_copy_range,
        // Counted per frame by the plan: a blob holding several kinds is
        // rewritten in several runs but is one blob.
        counts: WriteCounts {
            blobs_rewritten: plan.blobs_rewritten,
            ..WriteCounts::default()
        },
        copy: CopyRun::default(),
        frame_buf: Vec::new(),
        cursor: 0,
    };
    if !plan.runs.is_empty() {
        let workers = crate::host::sized_workers(
            &*host.resources,
            crate::tunables::DECODE_WORKERS_RESERVED,
            crate::tunables::DECODE_WORKERS_MIN,
            crate::tunables::DECODE_WORKERS_FALLBACK,
        )
        .clamp(1, plan.runs.len());
        let window = std::num::NonZeroUsize::new(
            workers.saturating_mul(crate::tunables::OVERLAP_RUN_WINDOW_PER_WORKER),
        )
        .unwrap_or(std::num::NonZeroUsize::MIN);
        crate::ordered_pool::run_ordered_streams(
            plan.runs.len(),
            workers,
            crate::ordered_pool::StreamConfig {
                window,
                depth: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH,
                cancel: &host.invocation,
            },
            |_| Vec::new(),
            |frame_buf: &mut Vec<u8>, k, emitter| {
                let run = &plan.runs[k];
                Sweep {
                    entries: &plan.entries[run.start..run.end],
                    frames: &plan.frames,
                    sources,
                    dedupe,
                    cancel: &host.invocation,
                    emitter,
                }
                .run_kind(run.kind, frame_buf)
            },
            &mut out,
        )?;
    }
    out.passthrough_until(plan.entries.len())?;
    let PlanWriter {
        mut copy,
        writer,
        mut counts,
        ..
    } = out;
    copy.flush(writer, sources, &mut counts)?;
    Ok(counts)
}

/// The ordered write loop: passthrough entries written (or coalesced into a
/// copy) in plan order, each overlap run's blocks at its position.
struct PlanWriter<'a> {
    plan: &'a Plan,
    sources: &'a [InputFile],
    writer: &'a mut PbfWriter<FileWriter>,
    use_copy_range: bool,
    counts: WriteCounts,
    /// Passthrough coalescer: adjacent blobs contiguous in one input collapse
    /// into one `copy_file_range`. On already-sorted input this is the entire
    /// file, cutting the syscall count from O(blobs) to O(1). Mirrors the
    /// contiguous-range coalescer in apply_changes.
    copy: CopyRun,
    frame_buf: Vec<u8>,
    /// The next plan entry to write.
    cursor: usize,
}

impl PlanWriter<'_> {
    /// Write the passthrough entries from the cursor up to `end`. Every one
    /// must be a copy: a rewrite entry the runs do not start at is a plan
    /// defect.
    fn passthrough_until(&mut self, end: usize) -> Result<()> {
        while self.cursor < end {
            let i = self.cursor;
            if self.plan.rewrite[i] {
                return Err(Error::internal("rewrite entry outside its planned run"));
            }
            // A copied entry is its frame's only one: a frame with several
            // is restructured, and every restructured entry is rewritten.
            let entry = &self.plan.entries[i];
            let frame = &self.plan.frames[entry.frame];
            match &frame.reindex {
                None if self.use_copy_range => {
                    self.copy
                        .offer(frame, self.writer, self.sources, &mut self.counts)?;
                }
                reindex => {
                    self.copy
                        .flush(self.writer, self.sources, &mut self.counts)?;
                    write_passthrough(
                        frame,
                        reindex.as_ref(),
                        &self.sources[frame.source],
                        self.writer,
                        &mut self.frame_buf,
                    )?;
                }
            }
            count_passthrough(entry, &mut self.counts);
            self.counts.blobs_passthrough += 1;
            self.cursor += 1;
        }
        Ok(())
    }
}

impl crate::ordered_pool::StreamConsumer<OwnedBlock, RunCounts> for PlanWriter<'_> {
    fn before_task(&mut self, k: usize) -> Result<()> {
        let start = self.plan.runs[k].start;
        self.passthrough_until(start)?;
        if self.cursor != start {
            return Err(Error::internal("rewrite entry outside its planned run"));
        }
        self.copy.flush(self.writer, self.sources, &mut self.counts)
    }

    fn item(&mut self, _k: usize, block: OwnedBlock) -> Result<()> {
        self.writer.write_primitive_block_owned(block)
    }

    fn finish_task(&mut self, k: usize, emitted: RunCounts) -> Result<()> {
        let run = &self.plan.runs[k];
        match run.kind {
            ElementKind::Node => self.counts.nodes += emitted.elements,
            ElementKind::Way => self.counts.ways += emitted.elements,
            ElementKind::Relation => self.counts.relations += emitted.elements,
        }
        self.counts.duplicates_removed += emitted.duplicates;
        self.cursor = run.end;
        Ok(())
    }
}

/// The in-flight `copy_file_range` stretch: (source, start, end).
#[derive(Default)]
struct CopyRun(Option<(usize, u64, u64)>);

impl CopyRun {
    /// Extend the stretch with `entry` when it continues it in the same
    /// input, otherwise flush the stretch and start a new one.
    fn offer(
        &mut self,
        frame: &Frame,
        writer: &mut PbfWriter<FileWriter>,
        sources: &[InputFile],
        counts: &mut WriteCounts,
    ) -> Result<()> {
        match &mut self.0 {
            Some((source, _, end)) if *source == frame.source && *end == frame.file_offset => {
                *end += frame.frame_len;
                counts.copy_range_coalesced += 1;
            }
            _ => {
                self.flush(writer, sources, counts)?;
                self.0 = Some((
                    frame.source,
                    frame.file_offset,
                    frame.file_offset + frame.frame_len,
                ));
            }
        }
        Ok(())
    }

    /// Hand the stretch to the writer as one copy. No-op when none is open.
    fn flush(
        &mut self,
        writer: &mut PbfWriter<FileWriter>,
        sources: &[InputFile],
        counts: &mut WriteCounts,
    ) -> Result<()> {
        if let Some((source, start, end)) = self.0.take() {
            writer.write_raw_copy(&sources[source], start, end - start)?;
            counts.copy_range_calls += 1;
        }
        Ok(())
    }
}

/// Passthrough without the copy path: the frame raw when it keeps its own
/// header (an output that cannot splice, such as `--direct-io`), else
/// reframed with the index and tag keys pass 1 scanned.
fn write_passthrough(
    frame: &Frame,
    reindex: Option<&Reindex>,
    source: &InputFile,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
) -> Result<()> {
    read_frame(source, frame, frame_buf)?;
    match reindex {
        None => writer.write_raw(frame_buf)?,
        Some(reindex) => {
            let reframed = blob_bytes(frame_buf)
                .and_then(|blob| {
                    reframe_raw_with_index(
                        blob,
                        &reindex.index.serialize(),
                        reindex.tagdata.as_deref(),
                    )
                })
                .map_err(|e| source.locate_at(frame.file_offset, e))?;
            writer.write_raw(&reframed)?;
        }
    }
    Ok(())
}

/// Count a passthrough blob: observed when pass 1 scanned its payload, a
/// claim when its count is the blob's own indexdata.
fn count_passthrough(entry: &BlobEntry, counts: &mut WriteCounts) {
    if !entry.index_scanned {
        counts.passthrough_claimed += entry.index.count;
        return;
    }
    match entry.index.kind {
        ElementKind::Node => counts.nodes += entry.index.count,
        ElementKind::Way => counts.ways += entry.index.count,
        ElementKind::Relation => counts.relations += entry.index.count,
    }
}

/// Re-read a complete frame from its input. A frame that no longer reads
/// (the input changed between the passes) fails located at the input and
/// the frame's offset.
pub(crate) fn read_frame(source: &InputFile, frame: &Frame, buf: &mut Vec<u8>) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let len = frame.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    source
        .file()
        .read_exact_at(buf, frame.file_offset)
        .map_err(|e| source.locate_at(frame.file_offset, Error::from(e)))
}

/// Re-read and decode an overlap-run entry's frame, a read or decode
/// failure located at its input and frame offset.
pub(crate) fn read_block(
    source: &InputFile,
    frame: &Frame,
    buf: &mut Vec<u8>,
) -> Result<crate::PrimitiveBlock> {
    read_frame(source, frame, buf)?;
    blob_bytes(buf)
        .and_then(decode_blob_to_primitiveblock)
        .map_err(|e| source.locate_at(frame.file_offset, e))
}

/// The Blob bytes of a complete frame, `[4-byte header_len][BlobHeader][Blob]`.
fn blob_bytes(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 4 {
        return Err(Error::invalid_data("frame too short"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(Error::invalid_data("invalid header length"));
    }
    Ok(&frame[blob_start..])
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Identity of an owned element in the sweep: OSM ID order, and for
/// `cat --dedupe` an exact duplicate is equal (type, id, version).
///
/// Absent metadata is version 0, the same mapping the `Owned*` `Ord` impls
/// use and libosmium's default version. That is element identity only; it is
/// unrelated to the OSC writers' rule that an absent version is not printed
/// as 0. An "exact duplicate" is equal in (type, id, version) and nothing
/// else: coordinates, tags, members and the rest of the metadata are not
/// compared, and the heap gives no input-order guarantee for which of two
/// such elements survives.
trait SweepElement: Ord {
    fn id(&self) -> i64;
    fn version(&self) -> i32;
}

impl SweepElement for OwnedNode {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl SweepElement for OwnedWay {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl SweepElement for OwnedRelation {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

/// One overlap run's sweep, on its worker: its entries, the inputs, and the
/// run's way out to the write loop.
struct Sweep<'a, 'e> {
    entries: &'a [BlobEntry],
    frames: &'a [Frame],
    sources: &'a [InputFile],
    dedupe: bool,
    cancel: &'a crate::host::Invocation,
    emitter: &'a crate::ordered_pool::Emitter<'e, OwnedBlock, RunCounts>,
}

impl Sweep<'_, '_> {
    /// Sweep the run, as `kind`, handing each block to the write loop as it
    /// fills.
    ///
    /// The extraction closures take only `kind`'s elements from each frame, and
    /// that is what lets a restructured frame (several kinds in one block) give
    /// each of its entries just its own kind's share.
    fn run_kind(&self, kind: ElementKind, frame_buf: &mut Vec<u8>) -> Result<RunCounts> {
        match kind {
            ElementKind::Node => self.run(
                |e, heap| {
                    match e {
                        Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn)?)),
                        Element::Node(n) => heap.push(Reverse(read_node(n)?)),
                        _ => {}
                    }
                    Ok(())
                },
                write_single_node_local,
                frame_buf,
            ),
            ElementKind::Way => self.run(
                |e, heap| {
                    if let Element::Way(w) = e {
                        heap.push(Reverse(read_way(w)?));
                    }
                    Ok(())
                },
                write_single_way_local,
                frame_buf,
            ),
            ElementKind::Relation => self.run(
                |e, heap| {
                    if let Element::Relation(r) = e {
                        heap.push(Reverse(read_relation(r)?));
                    }
                    Ok(())
                },
                write_single_relation_local,
                frame_buf,
            ),
        }
    }

    /// The sweep's cooperative stop: the run cancelled, or abandoned (the
    /// write loop has returned, or a thread unwound), so nothing it still
    /// produces can be written. An abandoned sweep's error is never
    /// delivered as the run's: the run has already ended.
    fn checkpoint(&self) -> Result<()> {
        self.cancel.check()?;
        if self.emitter.abandoned() {
            return Err(Error::internal(
                "overlap run abandoned: its write loop is gone",
            ));
        }
        Ok(())
    }

    /// Before each blob, emit every element below the blob's first ID; push
    /// the blob's elements; at the end, drain the heap. One `BlockBuilder`
    /// for the run, flushed at its end.
    ///
    /// The heap holds every element of the run not yet below the next blob's
    /// first ID, so it is bounded by the run's overlap width, not by the
    /// input: one blob whose range spans a kind's whole ID space makes it the
    /// whole kind. That is inherent to an in-memory sweep merge - it must
    /// hold whatever overlaps the current position - and bounding it would
    /// take an external merge. Streaming bounds only what the sweep has
    /// already re-encoded.
    ///
    /// [`checkpoint`](Self::checkpoint) runs before each blob is read and
    /// after every [`SWEEP_CHECKPOINT_REMOVALS`] heap removals (duplicate
    /// discards included), so neither a growing heap nor its final drain
    /// runs on unchecked: at most that many removals, or one blob's read and
    /// decode, pass between checks. That bounds work, not wall time.
    ///
    /// [`SWEEP_CHECKPOINT_REMOVALS`]: crate::tunables::SWEEP_CHECKPOINT_REMOVALS
    fn run<T: SweepElement>(
        &self,
        mut extract: impl FnMut(&Element<'_>, &mut BinaryHeap<Reverse<T>>) -> Result<()>,
        mut write_elem: impl FnMut(&T, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> Result<()>,
        frame_buf: &mut Vec<u8>,
    ) -> Result<RunCounts> {
        let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
        let mut bb = BlockBuilder::new();
        let mut filled: Vec<OwnedBlock> = Vec::new();
        let mut counts = RunCounts::default();
        let mut removals = 0_usize;
        let mut write = |elem: &T, counts: &mut RunCounts| -> Result<()> {
            write_elem(elem, &mut bb, &mut filled)?;
            counts.elements += 1;
            filled
                .drain(..)
                .try_for_each(|block| self.emitter.send(block))
        };

        for entry in self.entries {
            // Once per blob, before it is read: a pathological input can put
            // most of a file into one run, swept on one worker, and a run
            // whose heap only grows writes nothing until it drains.
            self.checkpoint()?;
            let below = crate::osm_id::blob_osm_first_id(entry.index.min_id, entry.index.max_id);
            while heap
                .peek()
                .is_some_and(|Reverse(e)| crate::osm_id::osm_id_cmp(e.id(), below).is_lt())
            {
                self.pop_one(&mut heap, &mut counts, &mut removals, &mut write)?;
            }
            let frame = &self.frames[entry.frame];
            let source = &self.sources[frame.source];
            let block = read_block(source, frame, frame_buf)?;
            for element in block.elements() {
                extract(&element, &mut heap).map_err(|e| source.locate_at(frame.file_offset, e))?;
            }
        }
        while !heap.is_empty() {
            self.pop_one(&mut heap, &mut counts, &mut removals, &mut write)?;
        }
        drop(write);
        crate::commands::flush_local(&mut bb, &mut filled)?;
        filled
            .into_iter()
            .try_for_each(|block| self.emitter.send(block))?;
        Ok(counts)
    }

    /// Pop and write the smallest element; under dedupe, then drop every
    /// element equal to it in (id, version). Every removal counts toward
    /// the next [`checkpoint`](Self::checkpoint).
    fn pop_one<T: SweepElement>(
        &self,
        heap: &mut BinaryHeap<Reverse<T>>,
        counts: &mut RunCounts,
        removals: &mut usize,
        write: &mut impl FnMut(&T, &mut RunCounts) -> Result<()>,
    ) -> Result<()> {
        let Some(Reverse(elem)) = heap.pop() else {
            return Ok(());
        };
        self.count_removal(removals)?;
        write(&elem, counts)?;
        if self.dedupe {
            let (id, version) = (elem.id(), elem.version());
            while heap
                .peek()
                .is_some_and(|Reverse(e)| e.id() == id && e.version() == version)
            {
                heap.pop();
                counts.duplicates += 1;
                self.count_removal(removals)?;
            }
        }
        Ok(())
    }

    /// One heap removal; a checkpoint on every
    /// [`SWEEP_CHECKPOINT_REMOVALS`]th.
    ///
    /// [`SWEEP_CHECKPOINT_REMOVALS`]: crate::tunables::SWEEP_CHECKPOINT_REMOVALS
    fn count_removal(&self, removals: &mut usize) -> Result<()> {
        *removals += 1;
        if *removals >= crate::tunables::SWEEP_CHECKPOINT_REMOVALS {
            *removals = 0;
            self.checkpoint()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ElementKind, min_id: i64, max_id: i64) -> BlobEntry {
        BlobEntry {
            frame: 0,
            index: BlobIndex {
                kind,
                min_id,
                max_id,
                count: 1,
                bbox: None,
            },
            intra_unsorted: false,
            restructured: false,
            index_scanned: true,
        }
    }

    fn frame() -> Frame {
        Frame {
            source: 0,
            file_offset: 0,
            frame_len: 0,
            reindex: None,
        }
    }

    /// Each entry on a frame of its own, numbered in order.
    fn indexed(mut entries: Vec<BlobEntry>) -> Indexed {
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.frame = i;
        }
        Indexed {
            frames: entries.iter().map(|_| frame()).collect(),
            entries,
            omitted: 0,
        }
    }

    /// Per entry: inside an overlap component.
    fn detect_overlaps(entries: &[BlobEntry]) -> Vec<bool> {
        let mut flags = vec![false; entries.len()];
        for (start, end) in overlap_components(entries) {
            flags[start..end].fill(true);
        }
        flags
    }

    /// The transitivity failure. Entries are in first-key order:
    /// A=[1..1000], B=[2..3], C=[500..600]. A/B overlap; B/C do NOT, so a
    /// pairwise-adjacent scan leaves C unflagged although its range sits
    /// inside A's: the run [A,B] then flushes A's ids 500..1000 and C is
    /// copied after it with ids 500..600 - unsorted output under the sorted
    /// stamp, and in `cat --dedupe` A/C duplicates that survive. Realistic
    /// for merging extracts, whose per-blob ID ranges are wide and sparse.
    #[test]
    fn overlap_runs_are_transitive_not_pairwise() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Node, 500, 600),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, true],
            "C=[500..600] lies inside A=[1..1000] and must join the run even \
             though it does not overlap its immediate predecessor B=[2..3]"
        );
    }

    /// Passing counterpart: disjoint blobs stay on the passthrough path, so
    /// a "flag everything" regression cannot satisfy the test above.
    #[test]
    fn disjoint_blobs_are_not_flagged_as_overlapping() {
        let entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 11, 20),
            entry(ElementKind::Node, 21, 30),
        ];
        assert_eq!(detect_overlaps(&entries), vec![false, false, false]);
    }

    /// Runs stay kind-bounded: the sweep is kind-gated and silently drops
    /// off-kind elements handed to it.
    #[test]
    fn a_run_never_crosses_a_kind_boundary() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Way, 5, 6),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the way blob shares an id range with the node run but is a \
             different kind, so it must not be pulled into it"
        );
    }

    /// A run that ends must not tint the blob that follows it.
    #[test]
    fn a_new_run_starts_after_the_previous_one_ends() {
        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
            entry(ElementKind::Node, 250, 260),
        ];
        assert_eq!(detect_overlaps(&entries), vec![true, true, true, true]);

        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the blob after a run must be judged on its own, not inherit the \
             run's flag"
        );
    }

    /// Each overlap component is its own run, adjacent ones included, so
    /// they sweep in parallel; an internally unsorted blob that overlaps
    /// nothing is a run alone; one inside a component is counted there.
    #[test]
    fn runs_are_overlap_components_and_lone_unsorted_blobs() {
        let mut entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 5, 6),
            entry(ElementKind::Node, 20, 30),
            entry(ElementKind::Node, 25, 26),
            entry(ElementKind::Node, 40, 50),
            entry(ElementKind::Way, 1, 10),
            entry(ElementKind::Way, 5, 6),
            entry(ElementKind::Way, 40, 50),
        ];
        entries[1].intra_unsorted = true;
        entries[4].intra_unsorted = true;
        let plan = plan(indexed(entries));
        let runs: Vec<_> = plan.runs.iter().map(|r| (r.start, r.end, r.kind)).collect();
        assert_eq!(
            runs,
            vec![
                (0, 2, ElementKind::Node),
                (2, 4, ElementKind::Node),
                (4, 5, ElementKind::Node),
                (5, 7, ElementKind::Way),
            ]
        );
        assert_eq!((plan.overlapping, plan.intra_unsorted), (6, 1));
        assert_eq!(
            plan.rewrite,
            vec![true, true, true, true, true, true, true, false]
        );
    }

    /// A restructured frame's entries are each rewritten, wherever their
    /// kinds sort, and the frame counts once: as restructured, as rewritten,
    /// and as overlapping when one of its entries joins a component.
    #[test]
    fn a_restructured_frame_is_rewritten_per_kind_and_counted_once() {
        let mut entries = vec![
            entry(ElementKind::Node, 1, 5),
            entry(ElementKind::Way, 1, 5),
            entry(ElementKind::Way, 3, 9),
            entry(ElementKind::Relation, 50, 60),
        ];
        for e in &mut entries[..2] {
            e.restructured = true;
        }
        let mut indexed = indexed(entries);
        // Frame 0 holds the node and the first way entry.
        indexed.entries[1].frame = 0;
        indexed.frames.pop();
        indexed.entries[2].frame = 1;
        indexed.entries[3].frame = 2;
        indexed.frames[1].reindex = Some(Reindex {
            index: indexed.entries[2].index,
            tagdata: None,
            replaces_false_claim: true,
        });
        indexed.omitted = 4;
        let plan = plan(indexed);

        let runs: Vec<_> = plan.runs.iter().map(|r| (r.start, r.end, r.kind)).collect();
        assert_eq!(
            runs,
            vec![(0, 1, ElementKind::Node), (1, 3, ElementKind::Way)],
            "the node share is rewritten alone, the way share in its component"
        );
        assert_eq!(plan.rewrite, vec![true, true, true, false]);
        assert_eq!(
            (plan.overlapping, plan.intra_unsorted, plan.restructured),
            (2, 0, 1)
        );
        assert_eq!(plan.blobs_rewritten, 2, "frame 0 once, frame 1 once");
        assert_eq!(plan.blobs_total(), 7, "three frames plus four omitted");
        assert!(
            plan.frames[1].reindex.is_none(),
            "a rewritten frame is never reframed"
        );
        assert_eq!(plan.false_claims_replaced, 0);
    }

    /// The claim rule is `check --index`'s: kind, count and range exact, a
    /// claimed bbox containing the scanned one, an absent one no claim, and
    /// any claimed one false over a coordinate out of range.
    #[test]
    fn a_claim_holds_on_exact_ranges_and_a_containing_bbox() {
        let scanned = BlobIndex {
            bbox: Some(crate::blob_meta::BlobBbox::new(10, 20, 10, 20)),
            ..entry(ElementKind::Node, 1, 10).index
        };
        let claim = |f: fn(&mut BlobIndex)| {
            let mut c = scanned;
            f(&mut c);
            claim_holds(&c, &scanned)
        };
        assert!(claim(|_| {}));
        assert!(claim(|c| c.bbox = None), "no bbox is no spatial claim");
        assert!(claim(|c| {
            c.bbox = Some(crate::blob_meta::BlobBbox::new(0, 30, 0, 30));
        }));
        assert!(!claim(|c| {
            c.bbox = Some(crate::blob_meta::BlobBbox::new(15, 20, 10, 20));
        }));
        assert!(!claim(|c| c.count = 2));
        assert!(!claim(|c| c.max_id = 11));
        assert!(!claim(|c| c.min_id = 0));
        assert!(!claim(|c| c.kind = ElementKind::Way));
        let boxless = BlobIndex {
            bbox: None,
            ..scanned
        };
        assert!(
            !claim_holds(&scanned, &boxless),
            "a node blob the scan gives no box has a coordinate out of range, \
             which no claimed box contains"
        );
    }

    /// A passthrough blob's count is a claim when it came from indexdata and
    /// an observation when pass 1 scanned the payload. Only the latter may
    /// reach the observed per-kind totals.
    #[test]
    fn passthrough_counts_split_claimed_from_scanned() {
        let mut counts = WriteCounts::default();
        let mut claimed = entry(ElementKind::Node, 1, 10);
        claimed.index_scanned = false;
        claimed.index.count = 1_000_000;
        count_passthrough(&claimed, &mut counts);
        assert_eq!(counts.nodes, 0, "an indexdata claim is not an observation");
        assert_eq!(counts.passthrough_claimed, 1_000_000);

        let mut scanned = entry(ElementKind::Way, 1, 10);
        scanned.index.count = 7;
        count_passthrough(&scanned, &mut counts);
        assert_eq!(counts.ways, 7, "a pass-1 scan count is observed");
        assert_eq!(counts.passthrough_claimed, 1_000_000);
    }
}
