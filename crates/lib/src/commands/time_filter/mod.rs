//! Filter history PBF to a snapshot at a cutoff timestamp.
//!
//! Keeps, for each object ID, the latest version with `timestamp <= cutoff`.
//! If that latest version is deleted (`visible=false`), the object is omitted.
//!
//! Two paths, picked from the header's `HistoricalInformation` required
//! feature flag:
//! - **History input**: decode workers select and encode each blob's
//!   versions, deciding every object the blob closes; an ordered stitcher
//!   joins the objects whose versions straddle blob boundaries, without
//!   touching an element. Output blocks are per input blob.
//! - **Snapshot input**: parallel per-block filter. Each (kind, id) appears
//!   in exactly one block, so blocks are independent.
//!
//! Neither path reads blob indexdata: every data blob is decoded and its
//! elements are handled by their actual kind and ID, and the output's
//! indexdata is built fresh. So time-filter has no `--force`, accepts a
//! wholly unindexed input as it accepts an indexed one, and believes no
//! sidecar claim. A partially indexed input is refused on both paths, as by
//! every command (the reader and the schedule walk refuse it once both
//! kinds of data blob have been seen).
//! It does believe two header declarations: `Sort.Type_then_ID` (required;
//! the history path's grouping needs each object's versions contiguous) and
//! `HistoricalInformation` (which picks the path). It checks the first as it
//! reads, one key compare per element: an element sorting below its
//! predecessor is `Unsorted`, on both paths. The history path also needs
//! each object's versions strictly ascending, and the snapshot path each
//! object once; either broken is `InvalidData`. `sort --distrust-metadata`
//! orders descending versions; a repeated version or object survives `sort`,
//! which keeps every record.

use super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_sorted, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::commands::split::Limits;
use crate::owned::{dense_node_metadata, element_metadata};
use crate::read::input::{InputSource, PinnedInput};
use crate::scan::classify::{build_classify_schedule, parallel_classify_phase_ordered};
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter};
use crate::{DenseNode, Element, ElementKind, ElementReader, Node, PrimitiveBlock, Relation, Way};

/// Statistics from a `time-filter` snapshot operation.
#[derive(Default)]
pub struct TimeFilterStats {
    pub versions_seen: u64,
    pub versions_before_cutoff: u64,
    pub elements_written: u64,
    pub dropped_deleted: u64,
    pub dropped_no_snapshot_version: u64,
}

impl TimeFilterStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "time-filter: {} versions scanned, {} <= cutoff, {} elements written ({} deleted at cutoff, {} without version <= cutoff)",
            self.versions_seen,
            self.versions_before_cutoff,
            self.elements_written,
            self.dropped_deleted,
            self.dropped_no_snapshot_version,
        )?;

        Ok(())
    }
}

/// Filter history PBF to a snapshot at `cutoff_timestamp` (UNIX seconds).
///
/// Requires sorted input (`Sort.Type_then_ID`) so each object's versions are
/// contiguous and the snapshot can be computed in one streaming pass.
///
/// A snapshot input is read in two passes, and which path runs is only known
/// once the header is read, so the input must be a regular file or a block
/// device on either path (`InvalidArgument` otherwise), and is pinned for the
/// run (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin
/// is used as it is. Where a later open cannot reach the pinned file, an
/// input replaced mid-run fails with `InputReplaced` and nothing is
/// published.
pub fn time_filter<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    cutoff_timestamp: i64,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TimeFilterStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = time_filter_to(
        input.into(),
        &output,
        cutoff_timestamp,
        write,
        overrides,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`time_filter`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn time_filter_to(
    input: InputSource<'_>,
    output: &crate::output::BoundOutput,
    cutoff_timestamp: i64,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<TimeFilterStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    // The snapshot path opens the input twice, each through its own open
    // file description: the header reader below, then the schedule walk
    // whose descriptor the classify pool preads. The history path reads
    // everything through the header reader. Both opens are derived from this
    // one pin, so a file renamed over the input mid-run cannot have its
    // header (which picks the path) read from one file and its blobs
    // filtered from the other: where `/proc/self/fd` reopens the pin every
    // open reaches the pinned file, otherwise on Unix a replaced input fails
    // the run (`PinnedInput` says what it does off Unix). Taken before any
    // other open of the input, so on the history path too: which path runs
    // is learned from the header, so whether the input is reopened is not
    // known until it has been read once, and a FIFO is refused up front.
    let pin = input.pin(host)?;
    let reader = ElementReader::open_in(&pin, direct_io, host)?;
    require_sorted(
        reader.header().is_sorted(),
        pin.path(),
        "Input history PBF",
        "Time filtering requires sorted input to preserve type-then-ID ordering while selecting the snapshot.",
    )?;

    let is_history = reader.header().has_historical_information();
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        // Both paths write at most one version of each object, the one at
        // the cutoff, so a history input's declaration is not carried: the
        // output is a snapshot, which is what apply-changes takes as a base.
        //
        // Ways are written from their refs alone, and a `LocationsOnWays`
        // declaration is dropped and warned, on both paths, on purpose:
        // inline coordinates are as of when the input was written, not as of
        // the cutoff. A kept way whose node moved after the cutoff would carry
        // the later position, against the earlier node version a history
        // input writes, or against a node a snapshot input drops because its
        // only version is newer than the cutoff. sort and `cat --dedupe`
        // re-time nothing, so they keep the arrays; this command cannot.
        &super::output_header::OutputFacts {
            replication: super::output_header::Replication::AsOf(cutoff_timestamp),
            history: super::output_header::History::Snapshot,
            ..super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded)
        },
        overrides,
        backend,
        host,
    )?;

    let stats = if is_history {
        time_filter_history(
            reader,
            &mut writer,
            cutoff_timestamp,
            pin.path(),
            &host.invocation,
        )?
    } else {
        // Drop the reader before the snapshot path - the migrated
        // snapshot path opens the input via HeaderWalker through
        // build_classify_schedule, not ElementReader, so holding the
        // reader open here would just keep an extra fd around.
        drop(reader);
        time_filter_snapshot(&pin, &mut writer, cutoff_timestamp, compression, host)?
    };

    writer.flush()?;
    host.invocation
        .counter("timefilter_versions_seen", stats.versions_seen);
    host.invocation.counter(
        "timefilter_versions_before_cutoff",
        stats.versions_before_cutoff,
    );
    host.invocation
        .counter("timefilter_elements_written", stats.elements_written);
    host.invocation
        .counter("timefilter_dropped_deleted", stats.dropped_deleted);
    host.invocation.counter(
        "timefilter_dropped_no_snapshot_version",
        stats.dropped_no_snapshot_version,
    );
    host.invocation
        .counter("timefilter_is_history_path", is_history);
    Ok(stats)
}

// ---------------------------------------------------------------------------
// History path: per-blob selection on the decode workers, ordered stitch.
// ---------------------------------------------------------------------------
//
// One object's versions are contiguous under the sorted claim, so within a
// blob every group but the last is closed by the next one, and every group
// but the first opened in this blob. A decode worker therefore decides and
// encodes every interior group itself, and the first group too when it has
// a version at or before the cutoff in this blob: the group ends here, so
// that version supersedes anything an earlier blob held. What it cannot
// decide is the last group, which a later blob may continue, and a first
// group with no eligible version here, whose winner (if any) an earlier blob
// holds. The ordered stitcher on the calling thread joins those across blob
// seams, a group spanning any number of blobs included, and touches no
// element: it compares keys and versions, counts, and picks which of two
// pre-encoded endings of the held blob to write.
//
// Selection reads identity, version, timestamp and visibility only; a
// version's payload is encoded only when it is the object's winner (or, for
// the last group, its blob's candidate). A superseded version's user and
// role string indices are therefore not resolved, so a bad index there is no
// longer refused: those fields never reach the output, and the dense
// metadata deltas are still accumulated by the iterator, so no later
// record's value depends on the skipped lookup. Speed on normal input
// outranks refusing a malformed record the answer does not contain.

/// Where a blob sits in the input, for locating a refusal the stitcher
/// raises about it.
type BlobAt = (Option<u64>, Option<crate::ByteOffset>);

/// A blob's first or last record: its object's place in the sorted order and
/// that record's version.
#[derive(Clone, Copy)]
struct Edge {
    key: OrderKey,
    version: Option<i32>,
}

/// The edges of a blob holding at least one element.
struct Edges {
    first: Edge,
    last: Edge,
    /// The blob holds one object only: its first group is its last.
    single: bool,
    /// The first group (when it is not the last) had a version at or before
    /// the cutoff in this blob, so the worker decided and counted it.
    head_decided: bool,
}

/// The last group's candidate in its blob: its last version at or before
/// the cutoff.
enum Tail {
    None,
    Deleted,
    /// Visible: the blob's ending with the candidate appended, carried as a
    /// result because a later blob may supersede it, and then its encoding
    /// is not this run's concern.
    Visible(crate::Result<Vec<OwnedBlock>>),
}

/// One input blob's share of the history snapshot.
struct HistorySlice {
    at: BlobAt,
    /// Records seen and at or before the cutoff, plus the outcomes of the
    /// groups the worker decided.
    stats: TimeFilterStats,
    /// `None` for a blob holding no element.
    edges: Option<Edges>,
    /// Blocks closed before the blob's last open block: unconditional.
    done: Vec<OwnedBlock>,
    /// The last open block without the tail candidate.
    open: Vec<OwnedBlock>,
    tail: Tail,
}

/// The decode-worker side of the history path.
struct HistoryBlobs<'l> {
    cutoff: i64,
    limits: &'l Limits,
}

impl crate::read::pipeline::DecodeTransform for HistoryBlobs<'_> {
    type Out = HistorySlice;
    const PARSE_INDEXDATA: bool = false;

    fn apply(
        &self,
        blob: &crate::blob::Blob,
        decode: crate::read::pipeline::Decoder<'_>,
    ) -> Option<crate::Result<HistorySlice>> {
        if blob.get_type() != crate::blob::BlobType::OsmData {
            return None;
        }
        let at = (blob.blob_index(), blob.offset());
        Some(
            decode
                .block()
                .and_then(|block| select_blob(&block, self.cutoff, self.limits, at)),
        )
    }
}

/// Blocks fitted to the blob limits as the builder closes them
/// (`commands/split.rs`): a block over the payload limit is cut on its own
/// bytes, and one the cut finds no plan for goes on whole for the writer to
/// refuse.
struct Fitted<'l> {
    blocks: Vec<OwnedBlock>,
    limits: &'l Limits,
}

impl crate::commands::BlockSink for Fitted<'_> {
    fn flush(&mut self, bb: &mut BlockBuilder) -> Result<()> {
        if let Some(block) = bb.take_owned()? {
            let blocks = &mut self.blocks;
            crate::commands::split::fit_scanned(block, self.limits, |piece, _| {
                blocks.push(piece);
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// Encode the winners of `block` (positions, ascending) in one walk: the
/// blocks they close go to `done`, the block left open to `open`. With a
/// visible `candidate` (a position after every winner), the alternative
/// ending is the open block with the candidate appended, under the usual kind
/// and count checks; it is built on a clone of the builder taken just before
/// the candidate, so the open block's elements are encoded once and the clone
/// holds exactly the state a re-encode of them from an empty builder would
/// reach. It comes back as a result, not raised: a later blob may supersede
/// the candidate, and then its encoding is not this run's concern. The
/// unconditional blocks' errors come first.
fn encode_endings(
    block: &PrimitiveBlock,
    winners: &[usize],
    candidate: Option<usize>,
    done: &mut Fitted<'_>,
    open: &mut Fitted<'_>,
) -> Result<Option<crate::Result<Vec<OwnedBlock>>>> {
    let mut bb = BlockBuilder::new();
    let mut wanted = winners.iter().copied().peekable();
    let mut bufs = EncodeBufs::default();
    let mut elements = block.elements().enumerate();
    let mut at_candidate = None;
    for (pos, element) in elements.by_ref() {
        if wanted.peek() == Some(&pos) {
            wanted.next();
            encode_kept(&element, &mut bb, done, &mut bufs)?;
        } else if Some(pos) == candidate {
            at_candidate = Some(element);
            break;
        } else if wanted.peek().is_none() && candidate.is_none() {
            break;
        }
    }
    let fork = at_candidate.as_ref().map(|_| bb.clone());
    flush_local(&mut bb, open)?;
    let (Some(element), Some(mut fork)) = (at_candidate, fork) else {
        return Ok(None);
    };
    let mut alt = Fitted {
        blocks: Vec::new(),
        limits: open.limits,
    };
    let ending = encode_kept(&element, &mut fork, &mut alt, &mut bufs)
        .and_then(|()| flush_local(&mut fork, &mut alt))
        .map(|()| alt.blocks);
    Ok(Some(ending))
}

fn element_visible(element: &Element<'_>) -> bool {
    match element {
        Element::DenseNode(dn) => dense_visible(dn),
        Element::Node(n) => node_visible(n),
        Element::Way(w) => way_visible(w),
        Element::Relation(r) => relation_visible(r),
    }
}

/// One group's state during a blob's selection pass.
struct Group {
    key: OrderKey,
    last_version: Option<i32>,
    /// Position and visibility of its last version at or before the cutoff.
    winner: Option<(usize, bool)>,
}

/// One blob's selection pass: what it decided, and its edges.
struct Selection {
    stats: TimeFilterStats,
    /// Positions of the decided visible winners, ascending.
    winners: Vec<usize>,
    first: Option<Edge>,
    /// The blob's last group, still open.
    last: Option<Group>,
    head_decided: bool,
    /// Whether any group closed in the blob (it holds more than one object).
    closed_any: bool,
}

/// Select one blob's versions and encode what it decides; see the section
/// comment above.
#[hotpath::measure]
fn select_blob(
    block: &PrimitiveBlock,
    cutoff: i64,
    limits: &Limits,
    at: BlobAt,
) -> crate::Result<HistorySlice> {
    let Selection {
        stats,
        winners,
        first,
        last,
        head_decided,
        closed_any,
    } = select(block, cutoff)?;
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(HistorySlice {
            at,
            stats,
            edges: None,
            done: Vec::new(),
            open: Vec::new(),
            tail: Tail::None,
        });
    };

    let mut done = Fitted {
        blocks: Vec::new(),
        limits,
    };
    let mut open = Fitted {
        blocks: Vec::new(),
        limits,
    };
    let candidate = match last.winner {
        Some((pos, true)) => Some(pos),
        _ => None,
    };
    let ending = encode_endings(block, &winners, candidate, &mut done, &mut open)?;
    let tail = match (last.winner, ending) {
        (None, _) => Tail::None,
        (Some((_, false)), _) => Tail::Deleted,
        (Some((_, true)), ending) => Tail::Visible(ending.unwrap_or_else(|| {
            Err(crate::Error::internal(
                "history time-filter: a visible candidate produced no ending",
            ))
        })),
    };

    Ok(HistorySlice {
        at,
        stats,
        edges: Some(Edges {
            first,
            last: Edge {
                key: last.key,
                version: last.last_version,
            },
            single: !closed_any,
            head_decided,
        }),
        done: done.blocks,
        open: open.blocks,
        tail,
    })
}

/// The selection pass over one blob: identity, version, timestamp and
/// visibility only.
fn select(block: &PrimitiveBlock, cutoff: i64) -> crate::Result<Selection> {
    let mut stats = TimeFilterStats::default();
    let mut winners: Vec<usize> = Vec::new();
    let mut first: Option<Edge> = None;
    let mut head_decided = false;
    let mut closed_any = false;
    let mut group: Option<Group> = None;

    // Decide a closed group: written, deleted or without an eligible
    // version. The first group with no eligible version is the stitcher's.
    let mut close = |g: &Group, is_first: bool, stats: &mut TimeFilterStats| match g.winner {
        Some((pos, true)) => {
            winners.push(pos);
            stats.elements_written += 1;
            true
        }
        Some((_, false)) => {
            stats.dropped_deleted += 1;
            true
        }
        None => {
            if !is_first {
                stats.dropped_no_snapshot_version += 1;
            }
            false
        }
    };

    for (pos, element) in block.elements().enumerate() {
        let (kind, id, timestamp, version) = element_identity_and_timestamp(&element)?;
        stats.versions_seen += 1;
        let key = order_key(kind, id);
        // The grouping believes the sorted header: an object whose versions
        // are not contiguous, or come back after its group closed, would be
        // written twice, and within a group the versions must ascend for the
        // last one at or before the cutoff to be the right one.
        let continues = match group.as_mut() {
            None => false,
            Some(g) => match key.cmp(&g.key) {
                std::cmp::Ordering::Less => return Err(order_violation(kind, id, g.key)),
                std::cmp::Ordering::Equal => {
                    match (g.last_version, version) {
                        (Some(previous), Some(next)) if next > previous => {}
                        (previous, next) => {
                            return Err(version_violation(kind, id, previous, next));
                        }
                    }
                    g.last_version = version;
                    true
                }
                std::cmp::Ordering::Greater => false,
            },
        };
        if !continues {
            if let Some(g) = group.take() {
                let decided = close(&g, !closed_any, &mut stats);
                if !closed_any {
                    head_decided = decided;
                }
                closed_any = true;
            }
            if first.is_none() {
                first = Some(Edge { key, version });
            }
            group = Some(Group {
                key,
                last_version: version,
                winner: None,
            });
        }
        if timestamp <= cutoff {
            stats.versions_before_cutoff += 1;
            if let Some(g) = group.as_mut() {
                g.winner = Some((pos, element_visible(&element)));
            }
        }
    }

    Ok(Selection {
        stats,
        winners,
        first,
        last: group,
        head_decided,
        closed_any,
    })
}

/// Locate a refusal the stitcher raises about the blob at `at`.
#[cold]
fn locate_at(
    e: crate::Error,
    source: &std::sync::Arc<std::path::Path>,
    at: BlobAt,
) -> crate::Error {
    e.locate_blob(crate::error::Location::new(Some(source), at.0, at.1))
}

/// The open group at a blob seam.
struct Pending {
    key: OrderKey,
    /// Its last record's version, which the next record must exceed.
    last_version: Option<i32>,
    /// Its last version at or before the cutoff so far. A visible one is
    /// the alternative ending of the held blob.
    candidate: Candidate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Candidate {
    None,
    Deleted,
    Visible,
}

/// The ending of the latest blob with output, not yet written because the
/// open group decides which of its two forms is right.
struct Held {
    at: BlobAt,
    open: Vec<OwnedBlock>,
    alt: Option<crate::Result<Vec<OwnedBlock>>>,
}

/// The ordered consumer of the history path.
struct Stitch<'w> {
    writer: &'w mut PbfWriter<crate::file_writer::FileWriter>,
    source: std::sync::Arc<std::path::Path>,
    stats: TimeFilterStats,
    pending: Option<Pending>,
    held: Option<Held>,
}

impl Stitch<'_> {
    fn write(&mut self, blocks: Vec<OwnedBlock>) -> Result<()> {
        for block in blocks {
            self.writer.write_primitive_block_owned(block)?;
        }
        Ok(())
    }

    /// Write the held ending, with the candidate when `with_candidate`.
    fn release(&mut self, with_candidate: bool) -> Result<()> {
        let held = self.held.take();
        let blocks = match (held, with_candidate) {
            (None, false) => return Ok(()),
            (Some(held), false) => held.open,
            (
                Some(Held {
                    at,
                    alt: Some(ending),
                    ..
                }),
                true,
            ) => ending.map_err(|e| locate_at(e, &self.source, at))?,
            (_, true) => {
                return Err(crate::Error::internal(
                    "time-filter: a visible candidate without its blob's ending",
                ));
            }
        };
        self.write(blocks)
    }

    /// The open group has ended: count it once and write the held ending.
    fn resolve(&mut self) -> Result<()> {
        let Some(pending) = self.pending.take() else {
            return self.release(false);
        };
        match pending.candidate {
            Candidate::Visible => self.stats.elements_written += 1,
            Candidate::Deleted => self.stats.dropped_deleted += 1,
            Candidate::None => self.stats.dropped_no_snapshot_version += 1,
        }
        self.release(pending.candidate == Candidate::Visible)
    }

    fn absorb(&mut self, slice: HistorySlice) -> Result<()> {
        let HistorySlice {
            at,
            stats,
            edges,
            done,
            open,
            tail,
        } = slice;
        self.stats.versions_seen += stats.versions_seen;
        self.stats.versions_before_cutoff += stats.versions_before_cutoff;
        self.stats.elements_written += stats.elements_written;
        self.stats.dropped_deleted += stats.dropped_deleted;
        self.stats.dropped_no_snapshot_version += stats.dropped_no_snapshot_version;
        let Some(edges) = edges else {
            return Ok(());
        };
        let candidate = match &tail {
            Tail::None => Candidate::None,
            Tail::Deleted => Candidate::Deleted,
            Tail::Visible(_) => Candidate::Visible,
        };
        let alt = match tail {
            Tail::Visible(ending) => Some(ending),
            Tail::None | Tail::Deleted => None,
        };

        let joins = match self.pending.as_mut() {
            None => false,
            Some(p) => {
                let (kind, id) = key_identity(edges.first.key);
                match edges.first.key.cmp(&p.key) {
                    std::cmp::Ordering::Less => {
                        return Err(locate_at(
                            order_violation(kind, id, p.key),
                            &self.source,
                            at,
                        ));
                    }
                    std::cmp::Ordering::Equal => {
                        match (p.last_version, edges.first.version) {
                            (Some(previous), Some(next)) if next > previous => {}
                            (previous, next) => {
                                return Err(locate_at(
                                    version_violation(kind, id, previous, next),
                                    &self.source,
                                    at,
                                ));
                            }
                        }
                        true
                    }
                    std::cmp::Ordering::Greater => false,
                }
            }
        };

        if joins && edges.single {
            // The whole blob continues the open group, which stays open: it
            // holds nothing decided, and a candidate here supersedes the
            // held one.
            if let Some(p) = self.pending.as_mut() {
                p.last_version = edges.last.version;
                if candidate != Candidate::None {
                    p.candidate = candidate;
                }
            }
            if candidate != Candidate::None {
                self.release(false)?;
                if let Some(ending) = alt {
                    self.held = Some(Held {
                        at,
                        open,
                        alt: Some(ending),
                    });
                }
            }
            return Ok(());
        }

        if joins && edges.head_decided {
            // The worker decided and counted the joined group from its own
            // later version: the held candidate is superseded.
            self.pending = None;
            self.release(false)?;
        } else {
            // The group open at the seam ends before this blob's output (a
            // joined first group with no eligible version here ends with
            // what the earlier blobs held).
            self.resolve()?;
            if !joins && !edges.single && !edges.head_decided {
                self.stats.dropped_no_snapshot_version += 1;
            }
        }

        self.write(done)?;
        self.pending = Some(Pending {
            key: edges.last.key,
            last_version: edges.last.version,
            candidate,
        });
        self.held = Some(Held { at, open, alt });
        Ok(())
    }
}

#[hotpath::measure]
fn time_filter_history(
    reader: ElementReader<crate::file_reader::FileReader>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
    input: &std::path::Path,
    invocation: &crate::host::Recorder,
) -> Result<TimeFilterStats> {
    let limits = Limits::new(writer.compression());
    let mut stitch = Stitch {
        writer,
        source: std::sync::Arc::from(input),
        stats: TimeFilterStats::default(),
        pending: None,
        held: None,
    };

    invocation.marker("TIMEFILTER_HISTORY_START");
    // Element order across blobs is the reader's, which the stitcher's
    // seam checks depend on. Don't add per-element `Instant::now()` timing
    // to the selection: Japan scale is 344 M elements and the time-source
    // overhead alone doubled wall in an earlier iteration of this
    // instrumentation; run with `--hotpath` instead.
    reader.for_each_fused_blob(
        HistoryBlobs {
            cutoff: cutoff_timestamp,
            limits: &limits,
        },
        |slice| stitch.absorb(slice),
    )?;
    stitch.resolve()?;
    invocation.marker("TIMEFILTER_HISTORY_END");
    Ok(stitch.stats)
}

// ---------------------------------------------------------------------------
// Snapshot path: parallel per-block filter.
// ---------------------------------------------------------------------------
//
// For non-history input each (kind, id) appears exactly once in the input,
// so blocks are independent: no cross-block group straddling, no version
// selection, no clone-into-OwnedElement on the consumer thread. Each worker
// iterates its block, drops elements whose timestamp > cutoff or whose
// visible=false (rare in snapshot inputs but the field can be set
// explicitly), and writes surviving elements straight into a local
// BlockBuilder via reference. Consumer drains batch results in order.

/// Snapshot path migrated 2026-04-28 from the
/// `for_each_primitive_block_batch_budgeted` + `par_iter().map(thread_local
/// BB)` + drain shape to `parallel_classify_phase` + `ReorderBuffer` (since
/// replaced by `parallel_classify_phase_ordered`, whose window bounds what the
/// hand-kept reorder buffer did not). The
/// pre-migration shape hit a structural ~28 GB anon ceiling at planet (five
/// SIGKILL'd attempts, last three with full instrumentation) because the
/// `par_iter().collect()` step materialises the full batch's
/// `Vec<OwnedBlockTriple>` results before draining, and the upstream pipeline
/// can hold its bounded decode window of additional PrimitiveBlocks in flight
/// while the batch drains.
/// Allocator knobs (`malloc_trim` per batch, `M_MMAP_THRESHOLD=64K`,
/// `decode_ahead=8`) all hit the same ceiling: mallinfo2 showed bytes
/// shifting between `arena` and `hblkhd` but the total live set was
/// genuine working set, not retention. The migration mirrors the
/// `cat --clean` (`b347c0a`, 28.9 GB -> 750 MB) and `check --ids`
/// (`516129e`, 29.2 GB -> 504 MB) precedents.
#[hotpath::measure]
fn time_filter_snapshot(
    input: &PinnedInput,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
    compression: Compression,
    host: &crate::host::Entered<'_>,
) -> Result<TimeFilterStats> {
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };

    host.invocation.marker("TIMEFILTER_SNAPSHOT_START");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; these samples are inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "timefilter_snapshot_start_mallinfo");

    // No kind or blob filter: every OsmData blob is scheduled whatever its
    // indexdata says or lacks, and `filter_block_snapshot` works on the
    // decoded elements by their actual kind. A partially indexed file is
    // refused by the walk all the same, as by every command-input read
    // (reference/indexdata-trust.md, "Missing and partial indexdata").
    let (schedule, shared_file) = build_classify_schedule(
        input,
        None,
        crate::scan::classify::KindClaims::Unchecked,
        host,
    )?;

    if schedule.is_empty() {
        host.invocation.marker("TIMEFILTER_SNAPSHOT_END");
        return Ok(stats);
    }

    // A block's first and last input keys, `None` for a block holding no
    // element, so the drain can hold consecutive blocks to the sorted claim
    // whatever the gate kept of them.
    type Bounds = Option<(OrderKey, OrderKey)>;
    type PhaseResult = crate::Result<(Vec<Vec<u8>>, TimeFilterStats, Bounds)>;
    let mut previous_last: Option<OrderKey> = None;

    // Each worker keeps one BlockBuilder across blobs and frames every block
    // as it fills, straight from the builder's buffer; the same worker shape
    // as cat --clean's. The ordered pool's window bounds the framed results
    // alive between decode and write.
    parallel_classify_phase_ordered(
        &shared_file,
        &schedule,
        None,
        None,
        || crate::commands::WorkerBuilder::new(BlockBuilder::new()),
        |block, worker| -> PhaseResult {
            let bb = worker.task()?;
            let mut output =
                crate::commands::FramedBlocks::new(compression, &host.invocation.metrics().writer);
            let mut bounds: Bounds = None;
            let block_stats =
                filter_block_snapshot(block, cutoff_timestamp, bb, &mut output, &mut bounds)?;
            flush_local(bb, &mut output)?;
            let framed = output.frames.into_iter().map(|(_, frame)| frame).collect();
            Ok((framed, block_stats, bounds))
        },
        |position, (framed, block_stats, bounds)| {
            // Called in schedule order; a write error ends the pass at once.
            // A failed blob ends it too, located with its input.
            if let Some((first, last)) = bounds {
                if let Some(previous) = previous_last {
                    let (kind, id) = key_identity(first);
                    let refusal = match first.cmp(&previous) {
                        std::cmp::Ordering::Less => Some(order_violation(kind, id, previous)),
                        std::cmp::Ordering::Equal => Some(snapshot_duplicate(kind, id)),
                        std::cmp::Ordering::Greater => None,
                    };
                    if let Some(e) = refusal {
                        return Err(shared_file.locate_blob(&schedule[position], e));
                    }
                }
                previous_last = Some(last);
            }
            stats.versions_seen += block_stats.versions_seen;
            stats.versions_before_cutoff += block_stats.versions_before_cutoff;
            stats.elements_written += block_stats.elements_written;
            stats.dropped_deleted += block_stats.dropped_deleted;
            stats.dropped_no_snapshot_version += block_stats.dropped_no_snapshot_version;
            for blob in framed {
                writer.write_raw_owned(blob)?;
            }
            Ok(())
        },
        host,
    )?;

    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "timefilter_snapshot_end_mallinfo");
    host.invocation.marker("TIMEFILTER_SNAPSHOT_END");
    Ok(stats)
}

/// Snapshot-gate: returns `Some(())` if the element survives both the cutoff
/// timestamp check and the visibility check. Updates `stats` for the drop
/// counters along the way. Returning `None` means the caller should `continue`.
fn snapshot_gate(
    ts: Option<crate::Result<i64>>,
    ident: (&str, i64),
    visible: bool,
    cutoff_timestamp: i64,
    stats: &mut TimeFilterStats,
) -> crate::Result<Option<()>> {
    // An element with no timestamp is not an element from before the
    // cutoff. See the timestamp section at the bottom of this file.
    let ts = require_timestamp(ts, ident.0, ident.1)?;
    if ts > cutoff_timestamp {
        stats.dropped_no_snapshot_version += 1;
        return Ok(None);
    }
    stats.versions_before_cutoff += 1;
    if !visible {
        stats.dropped_deleted += 1;
        return Ok(None);
    }
    Ok(Some(()))
}

#[hotpath::measure]
fn filter_block_snapshot(
    block: &PrimitiveBlock,
    cutoff_timestamp: i64,
    bb: &mut BlockBuilder,
    output: &mut impl crate::commands::BlockSink,
    bounds: &mut Option<(OrderKey, OrderKey)>,
) -> crate::Result<TimeFilterStats> {
    let mut stats = TimeFilterStats::default();
    let mut bufs = EncodeBufs::default();

    for element in block.elements() {
        stats.versions_seen += 1;
        // Every input element, before the gate drops any: the output keeps
        // input order under the input's sorted claim, and keeps each
        // occurrence of an object.
        let (kind, id) = match &element {
            Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
            Element::Node(n) => (ElementKind::Node, n.id()),
            Element::Way(w) => (ElementKind::Way, w.id()),
            Element::Relation(r) => (ElementKind::Relation, r.id()),
        };
        let key = order_key(kind, id);
        match bounds.as_mut() {
            None => *bounds = Some((key, key)),
            Some((_, last)) => {
                match key.cmp(last) {
                    std::cmp::Ordering::Less => return Err(order_violation(kind, id, *last)),
                    std::cmp::Ordering::Equal => return Err(snapshot_duplicate(kind, id)),
                    std::cmp::Ordering::Greater => {}
                }
                *last = key;
            }
        }
        let (ts, visible) = match &element {
            Element::DenseNode(dn) => (dense_timestamp(dn), dense_visible(dn)),
            Element::Node(n) => (node_timestamp(n), node_visible(n)),
            Element::Way(w) => (way_timestamp(w), way_visible(w)),
            Element::Relation(r) => (relation_timestamp(r), relation_visible(r)),
        };
        let Some(()) = snapshot_gate(
            ts,
            (kind.as_str(), id),
            visible,
            cutoff_timestamp,
            &mut stats,
        )?
        else {
            continue;
        };
        encode_kept(&element, bb, output, &mut bufs)?;
        stats.elements_written += 1;
    }
    Ok(stats)
}

/// Reused scratch for [`encode_kept`], borrowing from one block.
#[derive(Default)]
struct EncodeBufs<'a> {
    tags: Vec<(&'a str, &'a str)>,
    refs: Vec<i64>,
    members: Vec<MemberData<'a>>,
}

/// Encode a kept element from its borrowed form into `bb`, closing the
/// builder's block into `output` first when it cannot take the element.
/// Shared by both paths: a snapshot survivor and a history winner are
/// written alike.
#[inline]
fn encode_kept<'a>(
    element: &Element<'a>,
    bb: &mut BlockBuilder,
    output: &mut (impl crate::commands::BlockSink + ?Sized),
    bufs: &mut EncodeBufs<'a>,
) -> crate::Result<()> {
    match element {
        Element::DenseNode(dn) => {
            ensure_node_capacity_local(bb, output)?;
            bufs.tags.clear();
            bufs.tags.extend(dn.tags());
            let meta = dense_node_metadata(dn)?;
            bb.add_node_dense(
                dn.id(),
                dn.decimicro_lat()?,
                dn.decimicro_lon()?,
                bufs.tags.iter().copied(),
                meta.as_ref(),
            );
        }
        Element::Node(n) => {
            ensure_node_capacity_local(bb, output)?;
            bufs.tags.clear();
            bufs.tags.extend(n.tags());
            let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
            bb.add_node_partial(
                n.id(),
                n.decimicro_lat()?,
                n.decimicro_lon()?,
                bufs.tags.iter().copied(),
                meta.as_ref(),
            );
        }
        Element::Way(w) => {
            ensure_way_capacity_local(bb, output)?;
            bufs.tags.clear();
            bufs.tags.extend(w.tags());
            bufs.refs.clear();
            bufs.refs.extend(w.refs());
            let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
            // Refs only, never the inline coordinates: they are not as of
            // the cutoff (see `time_filter_to`, where the header drops the
            // declaration).
            bb.add_way_partial(w.id(), bufs.tags.iter().copied(), &bufs.refs, meta.as_ref());
        }
        Element::Relation(r) => {
            ensure_relation_capacity_local(bb, output)?;
            bufs.tags.clear();
            bufs.tags.extend(r.tags());
            r.members_into(&mut bufs.members)?;
            let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
            bb.add_relation_partial(
                r.id(),
                bufs.tags.iter().copied(),
                &bufs.members,
                meta.as_ref(),
            );
        }
    }
    Ok(())
}

fn dense_visible(dn: &DenseNode<'_>) -> bool {
    #[allow(clippy::redundant_closure_for_method_calls)]
    dn.info().is_none_or(|i| i.visible())
}

fn node_visible(n: &Node<'_>) -> bool {
    n.info().visible()
}

fn way_visible(w: &Way<'_>) -> bool {
    w.info().visible()
}

fn relation_visible(r: &Relation<'_>) -> bool {
    r.info().visible()
}

/// One element's identity for the history path: kind, ID, timestamp (which
/// must be present) and version (`None` when it claims none: absent, or a
/// sentinel below 1 such as dense `-1`).
fn element_identity_and_timestamp(
    element: &Element<'_>,
) -> crate::Result<(ElementKind, i64, i64, Option<i32>)> {
    let (kind, id, ts, version) = match element {
        Element::DenseNode(dn) => (
            ElementKind::Node,
            dn.id(),
            dense_timestamp(dn),
            dn.info().and_then(crate::DenseNodeInfo::claimed_version),
        ),
        Element::Node(n) => (
            ElementKind::Node,
            n.id(),
            node_timestamp(n),
            n.info().claimed_version(),
        ),
        Element::Way(w) => (
            ElementKind::Way,
            w.id(),
            way_timestamp(w),
            w.info().claimed_version(),
        ),
        Element::Relation(r) => (
            ElementKind::Relation,
            r.id(),
            relation_timestamp(r),
            r.info().claimed_version(),
        ),
    };
    Ok((kind, id, require_timestamp(ts, kind.as_str(), id)?, version))
}

/// An element's place in `Sort.Type_then_ID` order: its kind, then its ID in
/// canonical OSM order.
type OrderKey = (u8, crate::osm_id::OsmIdKey);

fn order_key(kind: ElementKind, id: i64) -> OrderKey {
    (kind.ordinal(), crate::osm_id::osm_id_key(id))
}

/// The element a key was made from, for naming it in a refusal.
fn key_identity(key: OrderKey) -> (ElementKind, i64) {
    (
        ElementKind::from_ordinal(key.0).unwrap_or(ElementKind::Node),
        crate::osm_id::osm_id_from_key(key.1),
    )
}

/// `kind id` sorts below `previous`, which the input's `Sort.Type_then_ID`
/// header promised it would not: the history path's grouping would emit
/// the object twice, the snapshot path's output would carry the false sort
/// claim.
#[cold]
fn order_violation(kind: ElementKind, id: i64, previous: OrderKey) -> crate::Error {
    let previous_kind =
        ElementKind::from_ordinal(previous.0).map_or("unknown", ElementKind::as_str);
    crate::error::new_unsorted(format!(
        "time-filter: {kind} {id} follows {previous_kind} {}, out of the Sort.Type_then_ID \
         order the input's header declares; time-filter groups each object's versions by \
         that order",
        crate::osm_id::osm_id_from_key(previous.1)
    ))
}

/// The same object appears twice in a snapshot (non-history) input, which
/// the snapshot path keeps once per occurrence: both would be written.
#[cold]
fn snapshot_duplicate(kind: ElementKind, id: i64) -> crate::Error {
    crate::Error::invalid_data(format!(
        "time-filter: {kind} {id} appears more than once in an input whose header does not \
         declare HistoricalInformation; the snapshot path writes every occurrence, so it \
         cannot pick the version at the cutoff. A history file must declare the feature."
    ))
}

/// One object's versions in a history input out of order: the history path
/// keeps the last version at or before the cutoff in input order, so it
/// needs them strictly ascending.
#[cold]
fn version_violation(
    kind: ElementKind,
    id: i64,
    previous: Option<i32>,
    next: Option<i32>,
) -> crate::Error {
    let show =
        |v: Option<i32>| v.map_or_else(|| "no version".to_owned(), |v| format!("version {v}"));
    crate::Error::invalid_data(format!(
        "time-filter: {kind} {id} has {} after {} in a history input; its versions must be \
         strictly ascending for the version at the cutoff to be the last one at or before it",
        show(next),
        show(previous)
    ))
}

// ---------------------------------------------------------------------------
// Timestamps
//
// Absent is not the epoch. Every one of these used to end in
// `unwrap_or(0)`, so an element with no `Info` - or an `Info` with no
// timestamp field, which is what `osmium --drop-metadata` and several
// pbfhogg paths produce - reported timestamp 0. Zero is at or below every
// plausible cutoff, so `snapshot_gate` kept the element and counted it in
// `versions_before_cutoff`, and `time-filter` printed "N versions scanned,
// N <= cutoff, N elements written" having filtered nothing at all. A
// command that reports success having done nothing is the worst shape of
// bug in this domain, so a metadata-stripped input is now a hard error
// naming the element.
// ---------------------------------------------------------------------------

// Every helper below returns the claim in whole seconds, rounded down
// (`div_euclid`), so a version is eligible when its timestamp falls within
// or before the cutoff second: -1 ms is second -1, not 0. The claim's
// scaling by `date_granularity` is exact, and one that overflows `i64` is an
// `Err` the caller refuses: a wrapped value would be kept or dropped by an
// arbitrary comparison, or read as padding on a versionless dense slot.

/// The claimed timestamp in whole seconds, rounded down.
fn claim_seconds(claim: Option<crate::Result<i64>>) -> Option<crate::Result<i64>> {
    claim.map(|ms| ms.map(|ms| ms.div_euclid(1000)))
}

/// `None` means the element carries no timestamp at all.
fn dense_timestamp(dn: &DenseNode<'_>) -> Option<crate::Result<i64>> {
    // A timestamp column, when the group carries one, has a value for every
    // node, so the column alone does not say this node has a timestamp.
    // Absence is `info()` returning None (a metadata-stripped dense block),
    // a DenseInfo without the timestamp column (libosmium writes any subset
    // of columns), or a slot that claims no version and holds timestamp 0:
    // the padding BlockBuilder and libosmium store for a node added without
    // metadata among nodes that carry it. Each reads 0 and must not pass as
    // the epoch; `claimed_milli_timestamp` is the slot rule every metadata
    // output shares.
    claim_seconds(dn.info()?.claimed_milli_timestamp())
}

fn node_timestamp(n: &Node<'_>) -> Option<crate::Result<i64>> {
    claim_seconds(n.info().claimed_milli_timestamp())
}

fn way_timestamp(w: &Way<'_>) -> Option<crate::Result<i64>> {
    claim_seconds(w.info().claimed_milli_timestamp())
}

fn relation_timestamp(r: &Relation<'_>) -> Option<crate::Result<i64>> {
    claim_seconds(r.info().claimed_milli_timestamp())
}

/// Turn an absent timestamp into the error that names it, and an
/// overflowing one into its scaling error, named the same way.
fn require_timestamp(ts: Option<crate::Result<i64>>, kind: &str, id: i64) -> crate::Result<i64> {
    let ts = ts
        .transpose()
        .map_err(|err| err.context(format!("time-filter: {kind} {id}")))?;
    ts.ok_or_else(|| {
        crate::Error::invalid_data(format!(
            "time-filter: {kind} {id} carries no timestamp. Filtering by time needs one, and \
             treating an absent timestamp as the Unix epoch would place every such element at \
             or below any cutoff - so the command would keep the entire input, write it out \
             unchanged, and report success having filtered nothing. Either the input has had \
             its metadata stripped (osmium --drop-metadata and several pbfhogg write paths do \
             this), or the element is a dense node with no version and timestamp 0, which the \
             format cannot tell apart from a node written without metadata; re-run \
             time-filter against an input whose elements carry their timestamps."
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::blob_meta::BlobIndex;
    use crate::elements::MemberId;

    const CUTOFF: i64 = 250;

    /// What a fixture blob's header says about it.
    #[derive(Clone, Copy, Debug)]
    enum Claim {
        /// Indexdata scanned from the payload.
        Honest,
        /// No indexdata.
        Absent,
        /// Parseable indexdata that is false on every field: another kind,
        /// a range holding none of its IDs and a wrong count.
        False,
    }

    /// One fixture element: kind, ID, version, timestamp, visible.
    type Version = (char, i64, i32, i64, bool);

    /// The history fixture's blobs. Node 2 and way 11 straddle a blob
    /// boundary with versions on both sides of the cutoff; node 3 is deleted
    /// at the cutoff, node 4 has no version at or before it, and way 10 and
    /// relation 100 have a later version past it.
    const HISTORY: &[&[Version]] = &[
        &[
            ('n', 1, 1, 100, true),
            ('n', 2, 1, 100, true),
            ('n', 2, 2, 200, true),
        ],
        &[
            ('n', 2, 3, 300, true),
            ('n', 3, 1, 100, true),
            ('n', 3, 2, 150, false),
            ('n', 4, 1, 400, true),
        ],
        &[
            ('w', 10, 1, 100, true),
            ('w', 10, 2, 300, true),
            ('w', 11, 1, 200, true),
        ],
        &[('w', 11, 2, 240, true)],
        &[('r', 100, 1, 100, true), ('r', 100, 2, 260, true)],
    ];

    /// The snapshot fixture's blobs: each object once. Node 2 and way 11 are
    /// past the cutoff and node 3 is deleted.
    const SNAPSHOT: &[&[Version]] = &[
        &[('n', 1, 1, 100, true), ('n', 2, 1, 300, true)],
        &[('n', 3, 2, 100, false), ('n', 4, 1, 200, true)],
        &[('w', 10, 1, 100, true)],
        &[('w', 11, 1, 300, true)],
        &[('r', 100, 1, 100, true)],
    ];

    fn write_fixture(path: &Path, history: bool, claims: &[Claim]) {
        write_blobs(
            path,
            history,
            if history { HISTORY } else { SNAPSHOT },
            claims,
        );
    }

    /// `blobs` as one block each, the header declaring the input sorted (and
    /// a history file when `history`).
    fn write_blobs(path: &Path, history: bool, blobs: &[&[Version]], claims: &[Claim]) {
        assert_eq!(blobs.len(), claims.len());
        let file = std::fs::File::create(path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        let mut header = crate::block_builder::HeaderBuilder::new().sorted();
        if history {
            header = header.historical();
        }
        writer.write_header(&header.build().unwrap()).unwrap();
        let mut bb = BlockBuilder::new();
        for (blob, claim) in blobs.iter().zip(claims) {
            for &(kind, id, version, timestamp, visible) in *blob {
                let meta = crate::block_builder::Metadata {
                    version,
                    timestamp,
                    changeset: 7,
                    uid: 3,
                    user: "u",
                    visible,
                };
                let tags = [("name", "x")];
                match kind {
                    'n' => bb.add_node(id, 10, 10, tags, Some(&meta)),
                    'w' => bb.add_way(id, tags, &[1, 2], Some(&meta)),
                    _ => bb.add_relation(
                        id,
                        tags,
                        &[MemberData {
                            id: MemberId::Way(10),
                            role: "outer",
                        }],
                        Some(&meta),
                    ),
                }
            }
            let bytes = bb.take().unwrap().unwrap();
            match claim {
                Claim::Honest => writer.write_primitive_block(bytes).unwrap(),
                Claim::Absent => writer.write_primitive_block_no_indexdata(bytes).unwrap(),
                Claim::False => {
                    let kind = match blob[0].0 {
                        'n' => ElementKind::Relation,
                        'w' => ElementKind::Node,
                        _ => ElementKind::Way,
                    };
                    let index = BlobIndex {
                        kind,
                        min_id: 1000,
                        max_id: 2000,
                        count: 99,
                        bbox: None,
                    };
                    writer
                        .write_primitive_block_owned(crate::block_builder::OwnedBlock {
                            bytes: bytes.to_vec(),
                            index,
                            tagdata: None,
                            way_members: None,
                        })
                        .unwrap();
                }
            }
        }
        writer.flush().unwrap();
    }

    /// An output element: kind, ID, version.
    type Written = (char, i64, i32);

    /// One run: output bytes, the written elements in file order, and stats.
    #[expect(
        clippy::unwrap_in_result,
        reason = "test helper: the command's result is what is under test, the fixture I/O \
                  around it is not"
    )]
    fn run(input: &Path) -> crate::Result<(Vec<u8>, Vec<Written>, [u64; 5])> {
        let dir = crate::path_guard::tests::scratch();
        let output = dir.path().join("out.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let s = time_filter(
            input,
            staged,
            CUTOFF,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &host,
        )?;
        let mut elements = Vec::new();
        ElementReader::open_in(&output, false, &host)
            .unwrap()
            .for_each(|element| {
                elements.push(match element {
                    Element::DenseNode(n) => ('n', n.id(), n.info().unwrap().version()),
                    Element::Node(n) => ('n', n.id(), n.info().version().unwrap()),
                    Element::Way(w) => ('w', w.id(), w.info().version().unwrap()),
                    Element::Relation(r) => ('r', r.id(), r.info().version().unwrap()),
                });
            })
            .unwrap();
        let stats = [
            s.versions_seen,
            s.versions_before_cutoff,
            s.elements_written,
            s.dropped_deleted,
            s.dropped_no_snapshot_version,
        ];
        Ok((std::fs::read(&output).unwrap(), elements, stats))
    }

    /// Claims are judged in whole seconds rounded down: a negative
    /// sub-second timestamp belongs to the second before the epoch, not to
    /// second 0, and a scaling error passes through to the refusal.
    #[test]
    fn claim_seconds_rounds_down() {
        for (ms, want) in [
            (-1, -1),
            (-1000, -1),
            (-1001, -2),
            (0, 0),
            (999, 0),
            (1500, 1),
        ] {
            assert_eq!(
                super::claim_seconds(Some(Ok(ms))).unwrap().unwrap(),
                want,
                "{ms} ms"
            );
        }
        assert!(super::claim_seconds(None).is_none());
        let err = crate::read::claims::scale_milli_timestamp(i64::MAX, 1000).unwrap_err();
        let cause = err.to_string();
        let got = super::require_timestamp(super::claim_seconds(Some(Err(err))), "node", 7)
            .expect_err("an unscalable timestamp is refused");
        let got = got.to_string();
        assert!(
            got.contains("time-filter: node 7") && got.contains(&cause),
            "expected the scaling refusal under the element's context, got: {got}"
        );
    }

    /// Neither path reads indexdata, so an input whose indexdata is false on
    /// every field, or absent on every blob, gives the wholly indexed file's
    /// output byte for byte, with the same stats. (A partially indexed input
    /// is refused on both paths mid-write, which this crate's test builds
    /// assert on as a writer dropped unflushed; the CLI input-contract table
    /// pins that refusal, in both orders.)
    #[test]
    fn indexdata_never_changes_the_output() {
        use Claim::{Absent as A, False as F, Honest as H};
        let dir = crate::path_guard::tests::scratch();
        let history_expected: &[Written] = &[
            ('n', 1, 1),
            ('n', 2, 2),
            ('w', 10, 1),
            ('w', 11, 2),
            ('r', 100, 1),
        ];
        let snapshot_expected: &[Written] =
            &[('n', 1, 1), ('n', 4, 1), ('w', 10, 1), ('r', 100, 1)];
        for (history, expected, expected_stats) in [
            (true, history_expected, [13, 9, 5, 1, 1]),
            (false, snapshot_expected, [7, 5, 4, 1, 2]),
        ] {
            let whole = dir.path().join(format!("whole-{history}.pbf"));
            write_fixture(&whole, history, &[H; 5]);
            let reference = run(&whole).unwrap();
            assert_eq!(reference.1, expected, "reference, history {history}");
            assert_eq!(
                reference.2, expected_stats,
                "reference stats, history {history}"
            );
            for (name, claims) in [("all false", [F; 5]), ("all absent", [A; 5])] {
                let what = format!("{name}, history {history}");
                let input = dir.path().join(format!("{name}-{history}.pbf"));
                write_fixture(&input, history, &claims);
                let got = run(&input).unwrap();
                assert_eq!(got.1, expected, "{what}");
                assert_eq!(got.2, reference.2, "stats: {what}");
                assert!(got.0 == reference.0, "output bytes differ: {what}");
            }
        }
    }

    /// An object whose versions span many blobs is decided once across the
    /// seams, whichever blob holds its last version at or before the
    /// cutoff: one opening it after a decided object, single-object blobs
    /// that replace, delete or leave its candidate, and a closing blob
    /// whose first record is past the cutoff, followed by another object.
    /// It is written in its own place in the order, counted once.
    #[test]
    fn an_object_spanning_many_blobs_is_decided_once() {
        use Claim::Honest as H;
        let dir = crate::path_guard::tests::scratch();
        let opening: &[Version] = &[('w', 5, 1, 100, true), ('w', 7, 1, 100, true)];
        let replace: &[Version] = &[('w', 7, 2, 200, true)];
        let delete: &[Version] = &[('w', 7, 3, 220, false)];
        let restore: &[Version] = &[('w', 7, 4, 240, true)];
        let later: &[Version] = &[('w', 7, 5, 245, true)];
        let past: &[Version] = &[('w', 7, 6, 300, true)];
        let closing: &[Version] = &[('w', 7, 7, 400, true), ('w', 8, 1, 100, true)];
        /// Name, input blobs, expected output, expected stats.
        type Case<'a> = (&'a str, &'a [&'a [Version]], &'a [Written], [u64; 5]);
        let cases: &[Case<'_>] = &[
            (
                "replaced, deleted, restored",
                &[opening, replace, delete, restore, past, closing],
                &[('w', 5, 1), ('w', 7, 4), ('w', 8, 1)],
                [8, 6, 3, 0, 0],
            ),
            (
                "deleted last",
                &[opening, replace, delete, past, closing],
                &[('w', 5, 1), ('w', 8, 1)],
                [7, 5, 2, 1, 0],
            ),
            (
                "opening version kept",
                &[opening, past, closing],
                &[('w', 5, 1), ('w', 7, 1), ('w', 8, 1)],
                [5, 3, 3, 0, 0],
            ),
            (
                "superseded in the next blob",
                &[opening, replace, later, closing],
                &[('w', 5, 1), ('w', 7, 5), ('w', 8, 1)],
                [6, 5, 3, 0, 0],
            ),
            (
                "decided by the closing blob",
                &[opening, &[('w', 7, 2, 200, true), ('w', 8, 1, 100, true)]],
                &[('w', 5, 1), ('w', 7, 2), ('w', 8, 1)],
                [4, 4, 3, 0, 0],
            ),
            (
                "deleted by the closing blob",
                &[opening, &[('w', 7, 2, 200, false), ('w', 8, 1, 100, true)]],
                &[('w', 5, 1), ('w', 8, 1)],
                [4, 4, 2, 1, 0],
            ),
            (
                "no version at the cutoff",
                &[
                    &[('w', 5, 1, 100, true), ('w', 7, 1, 300, true)],
                    past,
                    closing,
                ],
                &[('w', 5, 1), ('w', 8, 1)],
                [5, 2, 2, 0, 1],
            ),
        ];
        for (i, &(name, blobs, expected, stats)) in cases.iter().enumerate() {
            let input = dir.path().join(format!("seams-{i}.pbf"));
            write_blobs(&input, true, blobs, &vec![H; blobs.len()]);
            let got = run(&input).unwrap_or_else(|e| panic!("{name}: {}", e.report()));
            assert_eq!(got.1, expected, "{name}");
            assert_eq!(got.2, stats, "stats: {name}");
        }
    }

    /// History survivors near the payload limit, one per input blob, each
    /// the candidate of its blob's last object: every one is written, in
    /// order, each output block fitted to the limit and indexed from its own
    /// contents. (Output blocks are per input blob, so these never share a
    /// block; the cut itself is the shared one `commands/split.rs` tests.)
    #[test]
    fn large_history_survivors_are_written_fitted_and_indexed() {
        const LARGE: usize = 13 << 20;
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("large.pbf");
        let file = std::fs::File::create(&input).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::None,
        );
        let header = crate::block_builder::HeaderBuilder::new()
            .sorted()
            .historical();
        writer.write_header(&header.build().unwrap()).unwrap();
        let mut bb = BlockBuilder::new();
        let ids = [100i64, 101, 102, 103];
        for (k, &id) in ids.iter().enumerate() {
            let fill = char::from(b'a' + u8::try_from(k).unwrap());
            let value: String = std::iter::repeat_n(fill, LARGE).collect();
            let meta = crate::block_builder::Metadata {
                version: 1,
                timestamp: 100,
                changeset: 7,
                uid: 3,
                user: "u",
                visible: true,
            };
            bb.add_relation(
                id,
                [("note", value.as_str())],
                &[MemberData {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                Some(&meta),
            );
            // One relation per input blob: each blob frames.
            writer
                .write_primitive_block(bb.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
        drop(writer);

        let output = dir.path().join("out.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let options = WriteOptions {
            compression: Compression::None,
            ..WriteOptions::default()
        };
        let stats = time_filter(
            &input,
            staged,
            CUTOFF,
            &options,
            &HeaderOverrides::default(),
            &host,
        )
        .unwrap_or_else(|e| panic!("time-filter: {}", e.report()));
        assert_eq!(stats.elements_written, 4);

        let mut blocks = Vec::new();
        for blob in crate::BlobReader::open_in(output.as_path(), false, &host).unwrap() {
            let blob = blob.unwrap();
            let index = blob.index();
            if let crate::BlobDecode::OsmData(block) = blob.decode().unwrap() {
                let got: Vec<(i64, usize)> = block
                    .elements()
                    .map(|e| match e {
                        Element::Relation(r) => (r.id(), r.tags().next().unwrap().1.len()),
                        _ => panic!("only relations were written"),
                    })
                    .collect();
                blocks.push((index, got));
            }
        }
        assert!(blocks.len() >= 2, "{} blocks", blocks.len());
        let all: Vec<(i64, usize)> = blocks.iter().flat_map(|(_, g)| g.clone()).collect();
        assert_eq!(all, ids.iter().map(|&id| (id, LARGE)).collect::<Vec<_>>());
        for (index, got) in &blocks {
            let index = index.unwrap();
            assert_eq!(index.count, got.len() as u64);
            assert_eq!(index.min_id, got[0].0);
            assert_eq!(index.max_id, got[got.len() - 1].0);
        }
    }
}
