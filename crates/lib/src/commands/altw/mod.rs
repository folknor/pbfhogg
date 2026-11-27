//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

mod eager;
pub mod external;
mod passthrough;
mod reframe;
mod sparse;

use std::path::Path;
use std::str::FromStr;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::tunables::{AUTO_ANON_HEADROOM_BYTES, AUTO_STORE_BUDGET_PCT};
use crate::write::WriteOptions;
use crate::{Element, ElementKind, ElementReader, MemberId, PrimitiveBlock};

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, writer_from_header,
};
use crate::commands::input_front::InputFront;
use crate::idset::IdSet;
use crate::read::input::{InputSource, PinnedInput};

use super::Result;
use crate::error::Error;

use self::passthrough::write_output_passthrough;
use self::sparse::{SparseArrayIndex, build_node_index_sparse};

/// Every block produced for one input blob, with each the writer would
/// refuse cut between elements by the shared cut (`commands/split.rs`).
/// Splicing coordinates grows a way block, so a way blob that framed can
/// come out too large for one blob, and a block carrying a field-5 bitmap
/// can hold more ways than its BlobHeader has room for. A block that fits
/// costs one length comparison. Returns the field-5 bytes written, summed
/// over the final blocks, for the injected-prepass counter.
pub(super) fn fit_output(
    blocks: Vec<OwnedBlock>,
    limits: &crate::commands::split::Limits,
) -> Result<(Vec<OwnedBlock>, usize)> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut field5_bytes = 0usize;
    for block in blocks {
        crate::commands::split::fit_scanned(block, limits, |piece, _| {
            field5_bytes += piece.way_members.as_ref().map_or(0, Vec::len);
            out.push(piece);
            Ok(())
        })?;
    }
    Ok((out, field5_bytes))
}

/// Bytes available to an unprivileged writer on the filesystem holding `dir`
/// (`statvfs` `f_bavail * f_frsize`). `purpose` names the caller in the
/// error text. The one free-space probe for both backends: sparse checks it
/// before preallocating, external before stage 2.
#[cfg(target_os = "linux")]
fn filesystem_available_bytes(dir: &Path, purpose: &str) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::invalid_argument("scratch directory path contains a NUL byte"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // Safety: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(
            Error::from(std::io::Error::last_os_error()).context(format!(
                "failed to query free space for {purpose} in {}",
                dir.display()
            )),
        );
    }
    // Safety: statvfs initialized `stats` on success.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// Shared refusal text for a referenced node rejected by the coordinate owner.
fn node_coordinate_domain_error(node_id: i64, lat: i32, lon: i32) -> Error {
    Error::invalid_data(format!(
        "node {node_id} has coordinates outside the geographic domain \
         (lat {lat}, lon {lon} in decimicrodegrees; latitude must be \
         within +/-90 degrees and longitude within +/-180). Refusing \
         to build a node index from it."
    ))
}

/// Two records for one referenced node ID at different coordinates. Neither
/// is evidence the other is wrong, and stream order is not evidence either,
/// so no winner is picked: every index strategy refuses the same inputs,
/// whatever order its workers see the records in. Identical duplicates are
/// accepted, since they cannot change an answer.
fn conflicting_node_coordinates_error(node_id: i64, a: (i32, i32), b: (i32, i32)) -> Error {
    Error::invalid_data(format!(
        "node {node_id} occurs more than once with different coordinates \
         (lat {}, lon {} and lat {}, lon {} in decimicrodegrees). Refusing \
         to choose one as the location of the ways that reference it.",
        a.0, a.1, b.0, b.1
    ))
}

/// What ALTW states about its output: input order, and every way written with
/// inline coordinates (plus the injected prepass fields when asked).
///
/// The sorted claim is carried unchecked on the sparse path, deliberately:
/// sparse cannot see most disorder (way IDs are never compared, relation
/// blobs may stay raw, the indexed header is written before pass 2), so
/// dropping the claim on the node disorder pass 1 could see would only change
/// which false claims survive, and a false claim puts the file outside the
/// contract (`reference/indexdata-trust.md`, "Raw copies carry claims
/// forward"; osmium carries it the same way). External checks order because
/// its join depends on it.
fn altw_output_facts(inject_prepass: bool) -> crate::commands::output_header::OutputFacts {
    use crate::commands::output_header::{OutputFacts, Ways};
    OutputFacts::carry(Ways::Located {
        prepass: inject_prepass,
    })
}

/// Relations need no edits; nodes can bypass filtering only when all are kept.
/// Unknown kinds must decode, and ways always need location splicing.
fn blob_is_passthrough(kind: Option<crate::ElementKind>, keep_untagged_nodes: bool) -> bool {
    matches!(kind, Some(ElementKind::Relation))
        || matches!(kind, Some(ElementKind::Node) if keep_untagged_nodes)
}

/// Select relation dependencies without owning their accumulation strategy.
/// Sparse streams per-blob vectors into one IdSet; external inserts directly
/// while preading its existing metadata table. Keep those measured shapes.
///
/// Every relation's ID goes to `walk`, memberless ones included, so the
/// caller can hold the blob's claim to what it holds. The block holds only
/// relations: both callers refuse a foreign kind first. Members of either
/// sign are emitted as they are: the sets they go into are signed, so a
/// negative member node is retained and a negative member way marked like
/// any other, where osmium's `--keep-member-nodes` folds a member onto its
/// absolute value.
///
/// A built block on purpose, not a projection on the block validator: it
/// reads the `type` tag as well as the members, and the scan is not worth
/// moving. External runs it on its own thread beside stage 1's pass A
/// (planet: about 10 s inside a 32 s pass A), off the critical path; sparse
/// runs it on the parallel pool over the relation blobs only, which
/// extract's relation classify reads in about 1 s at planet (an estimate:
/// no planet sparse cell is recorded), under that command's noise floor.
fn relation_member_ids(
    block: &PrimitiveBlock,
    want_nodes: bool,
    want_ways: bool,
    walk: &mut crate::blob_meta::ClaimWalk<'_>,
    mut emit: impl FnMut(bool, i64),
) -> Result<()> {
    for element in block.elements_skip_metadata() {
        if let Element::Relation(r) = element {
            walk.observe(r.id())?;
            let complete = r
                .tags()
                .any(|(key, value)| key == "type" && matches!(value, "multipolygon" | "boundary"));
            for member in r.members() {
                match member.id {
                    MemberId::Node(id) if want_nodes => emit(false, id),
                    MemberId::Way(id) if want_ways && complete => emit(true, id),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index type selection
// ---------------------------------------------------------------------------

/// Strategy for storing node coordinates during add-locations-to-ways.
///
/// `Auto` is the default. It runs sparse, the default before it, unless it
/// has a known page-cache budget that the estimated sparse store exceeds:
/// when the budget is unknown (every non-Linux target among others), or the
/// available memory does not exceed the sparse run's fixed anon allowance,
/// it keeps sparse rather than imposing external's scratch-disk demand on a
/// caller who never asked for it. The routing threshold is a chosen point
/// between two measured inputs, not a fitted crossover; see
/// `reference/pipeline.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexType {
    /// Rank-indexed flat sparse array, sized at `referenced.total_count() * 8`
    /// bytes. Workers store coordinates with relaxed `AtomicU64` mmap stores
    /// at byte offset `RankedIdSet::numeric_rank_if_set(node_id) << 3` (not pwrites, so no
    /// syscall per tuple). Works on any PBF. Runtime and page-cache behavior
    /// depend on the input and host; see `reference/performance/add-locations-to-ways.md` for
    /// measurements.
    Sparse,
    /// External join via double radix permutation. Uses bounded working
    /// memory and sequential I/O, and requires sorted input with indexdata.
    /// Its temporary-disk demand depends on the input; see
    /// `reference/performance/add-locations-to-ways.md` for measurements.
    External,
    /// Selects a backend at runtime (the default). Unsorted or unindexed
    /// input uses sparse. For sorted, indexed input, external is selected
    /// when the estimated sparse store exceeds a fixed share of the
    /// page-cache budget; when the budget is unknown, or the available
    /// memory does not exceed the sparse run's anon allowance, sparse is
    /// kept and the store estimate is skipped. An input whose indexdata
    /// the external join would refuse runs sparse; a sparse choice against
    /// a known budget is judged again on the exact store after the first
    /// pass, and may switch to external there. See `reference/pipeline.md`
    /// for the routing rule and `reference/performance/add-locations-to-ways.md` for measurements.
    #[default]
    Auto,
}

/// Configuration for [`add_locations_to_ways`].
#[derive(Clone, Copy, Debug)]
pub struct AltwOptions {
    pub keep_untagged_nodes: bool,
    /// Output compression, backend and the shared input flags. The sparse
    /// path's output pass prefers the parallel writer, so a `Buffered`
    /// backend builds `PbfWriter::to_path_parallel` there; the external
    /// join's final stage uses the backend as given.
    pub write: WriteOptions,
    pub index_type: IndexType,
    /// Emit the private WayMembers-v1 and SharedNodePins-v1 prepass metadata.
    pub inject_prepass: bool,
    /// Accept ways whose node references cannot be resolved, filling the
    /// unresolved vertices with `(0, 0)`.
    ///
    /// Off by default. A `(0, 0)` filler is Null Island, a real coordinate
    /// off the coast of Ghana, so an unresolved reference written that way is
    /// indistinguishable from a genuine one and silently relocates geometry.
    /// Absence is not zero: with this off, an input that cannot be fully
    /// resolved is refused rather than quietly degraded. Matches osmium's
    /// `--ignore-missing-nodes` in name and in default.
    pub ignore_missing_nodes: bool,
}

impl AltwOptions {
    /// Refuse option combinations in which `force` can never take effect,
    /// before any input is opened. [`add_locations_to_ways`] calls it first.
    ///
    /// - A requested external index needs indexdata (its ID bucket ranges come
    ///   from per-blob indexdata), so on an unindexed input it refuses and on
    ///   an indexed one `force` does nothing. `IndexType::Auto` routes an
    ///   unindexed input to sparse, so `force` stays meaningful there.
    /// - `inject_prepass` attaches per-blob WayMembers metadata, which the
    ///   decode-all fallback cannot do, so it needs indexdata too.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` naming the refused combination.
    pub fn validate(&self) -> Result<()> {
        if !self.write.force {
            return Ok(());
        }
        if self.index_type == IndexType::External {
            return Err(Error::invalid_argument(
                "add-locations-to-ways: force has no effect with the external index, \
                 which requires indexed input because its ID bucket ranges come from \
                 per-blob indexdata",
            ));
        }
        if self.inject_prepass {
            return Err(Error::invalid_argument(
                "add-locations-to-ways: force has no effect with inject_prepass, which \
                 requires indexed input because the decode-all fallback cannot attach \
                 per-blob WayMembers metadata",
            ));
        }
        Ok(())
    }
}

/// Parse error for [`IndexType`].
#[derive(Debug, Clone)]
pub struct ParseIndexTypeError(String);

impl std::fmt::Display for ParseIndexTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseIndexTypeError {}

impl FromStr for IndexType {
    type Err = ParseIndexTypeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "sparse" => Ok(Self::Sparse),
            "external" => Ok(Self::External),
            "auto" => Ok(Self::Auto),
            // Keep this rejected spelling in the public parser. Removing it
            // changes the observable error for downstream callers that still
            // submit the old value. The diagnostic preserves the measured
            // pointer to sparse, and the CLI delegates here so its advice stays
            // aligned with the library parser.
            "dense" => Err(ParseIndexTypeError(
                "index type 'dense' was removed in favor of 'sparse'. Sparse \
                 (rank-indexed flat) is faster than dense at every measured \
                 scale and works in regimes dense doesn't. Use 'sparse' \
                 instead."
                    .to_string(),
            )),
            _ => Err(ParseIndexTypeError(format!(
                "unknown index type '{s}': expected 'sparse', 'external', or 'auto'"
            ))),
        }
    }
}

impl std::fmt::Display for IndexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sparse => f.write_str("sparse"),
            Self::External => f.write_str("external"),
            Self::Auto => f.write_str("auto"),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved backend
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedBackend {
    Sparse,
    External,
}

impl std::fmt::Display for ResolvedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sparse => f.write_str("sparse"),
            Self::External => f.write_str("external"),
        }
    }
}

enum NodeIndex<'a> {
    Sparse(&'a SparseArrayIndex),
    Eager(eager::Lookup<'a>),
    // Keep the reframe wire-format tests independent of a node store.
    #[cfg(test)]
    InMemory(std::collections::HashMap<i64, (i32, i32)>),
}

impl NodeIndex<'_> {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Sparse(index) => index.get(node_id),
            Self::Eager(index) => index.get(node_id),
            #[cfg(test)]
            Self::InMemory(map) => map.get(&node_id).copied(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
///
/// Every input node is counted once in `nodes_read` and once in either
/// `nodes_written` or `nodes_dropped`. A node blob copied raw or dropped
/// whole without a decode is credited by its indexdata `count` claim
/// (reference/indexdata-trust.md); decoded blobs are counted per element.
#[derive(Default)]
pub struct Stats {
    pub nodes_read: u64,
    pub nodes_written: u64,
    pub nodes_dropped: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    pub missing_locations: u64,
    pub blobs_passthrough: u64,
    pub blobs_decoded: u64,
}

impl Stats {
    /// Accumulate stats from another `Stats` instance into this one.
    ///
    /// Claimed indexdata counts feed these sums unbounded, so a sum that
    /// does not fit is refused as invalid data rather than wrapped or
    /// saturated into a wrong summary. On error `self` is unchanged.
    pub fn merge(&mut self, src: &Stats) -> crate::Result<()> {
        fn add(a: u64, b: u64, field: &str) -> crate::Result<u64> {
            a.checked_add(b).ok_or_else(|| {
                crate::Error::invalid_data(format!(
                    "add-locations-to-ways statistics overflow in {field} \
                     (indexdata count claims too large)"
                ))
            })
        }
        let merged = Stats {
            nodes_read: add(self.nodes_read, src.nodes_read, "nodes_read")?,
            nodes_written: add(self.nodes_written, src.nodes_written, "nodes_written")?,
            nodes_dropped: add(self.nodes_dropped, src.nodes_dropped, "nodes_dropped")?,
            ways_written: add(self.ways_written, src.ways_written, "ways_written")?,
            relations_written: add(
                self.relations_written,
                src.relations_written,
                "relations_written",
            )?,
            missing_locations: add(
                self.missing_locations,
                src.missing_locations,
                "missing_locations",
            )?,
            blobs_passthrough: add(
                self.blobs_passthrough,
                src.blobs_passthrough,
                "blobs_passthrough",
            )?,
            blobs_decoded: add(self.blobs_decoded, src.blobs_decoded, "blobs_decoded")?,
        };
        *self = merged;
        Ok(())
    }

    /// Print a summary of the operation to stderr.
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "add-locations-to-ways: {} nodes read, {} written, {} dropped, \
             {} ways, {} relations, {} missing locations",
            self.nodes_read,
            self.nodes_written,
            self.nodes_dropped,
            self.ways_written,
            self.relations_written,
            self.missing_locations,
        )?;
        if self.blobs_passthrough > 0 {
            writeln!(
                w,
                "  Blobs: {} passthrough, {} decoded",
                self.blobs_passthrough, self.blobs_decoded,
            )?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Embed node coordinates into ways.
///
/// Build a node coordinate index, then write the ways with their coordinates.
/// Sparse ALTW first collects way references when the all-node store does
/// not fit the page-cache budget, or when prepass metadata is requested.
///
/// If `keep_untagged_nodes` is false, nodes with zero tags are omitted from
/// the output (their coordinates are still used for ways).
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`InputSource::pin`]): a path is pinned here, a caller's
/// [`PinnedInput`] is used as it is. Where a later open cannot reach the
/// pinned file, an input replaced mid-run fails with `InputReplaced`.
pub fn add_locations_to_ways<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<Stats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = add_locations_to_ways_to(input, &output, options, overrides, host, &scratch)?;
    drop(scratch);
    output.publish()?;
    Ok(result)
}

/// [`add_locations_to_ways`] writing to a borrowed output it does not publish.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(crate) fn add_locations_to_ways_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
) -> Result<Stats> {
    options.validate()?;
    // Every pass below opens the input again, each through its own open
    // file description (the front read, the header walks, the worker
    // pools, external's stage readers), and each is derived from this one
    // pin, so a file renamed over the input mid-run cannot be read partly
    // from each: where `/proc/self/fd` reopens the pin every open reaches
    // the pinned file, otherwise on Unix a replaced input fails the run
    // (`PinnedInput` says what it does off Unix). Taken before any other
    // open of it.
    let pin = input.into().pin(host)?;
    let input = &pin;
    // Every precondition that depends on the input (sorted, indexed, the
    // header) is answered by this one front read, whichever backend runs.
    // The header walks that follow are separate and deliberate (see
    // `passthrough::build_schedule`). The reference-selected sparse store
    // and the external join check scratch space once their first pass has
    // the counts needed to size it.
    let mut front = InputFront::open(input, options.write.direct_reads, host)?;
    // Auto-select: sparse unless the input qualifies for external AND a
    // known page-cache budget is exceeded by the estimated store (see
    // IndexType::Auto).
    // `recheck` is the page-cache budget auto routed sparse on, when it
    // could have chosen external: the exact store is judged against it
    // again once pass 0 has counted it.
    let allow_eager = !options.inject_prepass;
    let (backend, route) = match options.index_type {
        IndexType::Sparse => (
            ResolvedBackend::Sparse,
            SparseRoute::for_eager(front.indexed()? && allow_eager, host),
        ),
        IndexType::External => (ResolvedBackend::External, SparseRoute::default()),
        IndexType::Auto => auto_select_backend(input, &mut front, allow_eager, host)?,
    };

    // External join has its own pipeline - dispatch early.
    if backend == ResolvedBackend::External {
        return run_external(input, front, output, options, overrides, host, scratch);
    }

    let indexdata_present = front.require_indexdata(
        options.write.force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed and re-encoded (significantly slower).",
        host,
    )?;

    // Suggest external index for sorted indexed PBFs on *explicit* sparse
    // selection only. When auto resolved to sparse it has already explained
    // why on stderr (the store fits, or no budget could justify external);
    // hinting external there would contradict that note.
    let hint_external =
        options.index_type == IndexType::Sparse && indexdata_present && front.is_sorted()?;
    drop(front);
    if hint_external {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.external_hint",
            "this sorted indexed PBF is eligible for the external index, \
             which uses bounded working memory and sequential I/O. Sparse is faster \
             while its node store fits in the page cache; external avoids the \
             page-cache thrashing sparse hits when it does not. The automatic \
             choice picks between them from the estimated store size."
        );
    }

    let all_nodes = indexdata_present && allow_eager && route.fits_all_nodes(input, host)?;
    let (nodes, shared) = if all_nodes {
        (SparseNodes::AllNodes, None)
    } else {
        // Pass 0 shrinks the store to the way-referenced working set and
        // supplies the shared-reference set when prepass injection is on.
        host.invocation.marker("ALTW_PASS0_START");
        let (referenced, shared) = collect_way_referenced_node_ids(
            input,
            options.write.direct_reads,
            options.inject_prepass,
            host,
        )?;
        host.invocation.marker("ALTW_PASS0_END");
        let referenced_count = referenced.len();
        host.invocation
            .counter("altw_referenced_node_ids", referenced_count);
        if let Some(budget) = route.budget.filter(|_| route.recheck)
            && exact_store_exceeds_budget(
                referenced_count,
                &referenced,
                shared.as_ref(),
                budget,
                host,
            )
        {
            drop((referenced, shared));
            let front = InputFront::open(input, options.write.direct_reads, host)?;
            return run_external(input, front, output, options, overrides, host, scratch);
        }
        (SparseNodes::Referenced(referenced), shared)
    };
    host.invocation.counter("altw_index_kind", 1);
    host.invocation
        .counter("altw_sparse_all_nodes", u64::from(all_nodes));

    // Relation members are collected before any node is read: the node pass
    // writes retained nodes as it stores coordinates, and a relation member
    // is retained whatever its tags. Nothing in the scan depends on the
    // store.
    let relation_scan = if options.keep_untagged_nodes && !options.inject_prepass {
        (None, None)
    } else {
        host.invocation.marker("ALTW_REL_MEMBER_SCAN_START");
        let ids = collect_relation_member_ids(
            input,
            options.write.direct_reads,
            !options.keep_untagged_nodes,
            options.inject_prepass,
            host,
        )?;
        host.invocation.marker("ALTW_REL_MEMBER_SCAN_END");
        host.invocation.counter(
            "altw_relation_member_node_ids",
            ids.0.as_ref().map_or(0, IdSet::len),
        );
        ids
    };
    let inject_metrics = inject_metrics::InjectMetrics::default();
    let stats = write_output_checked(
        input,
        output,
        scratch,
        nodes,
        options.keep_untagged_nodes,
        relation_scan.0.as_ref(),
        relation_scan.1.as_ref(),
        shared.as_ref(),
        options.inject_prepass,
        &inject_metrics,
        &options.write,
        indexdata_present,
        overrides,
        host,
    )?;
    emit_stats_counters(&stats, &host.invocation);
    if options.inject_prepass {
        let member_ways = relation_scan.1.as_ref().map_or(0, IdSet::len);
        inject_metrics.emit(member_ways, &host.invocation);
    }
    enforce_missing_locations_policy(stats.missing_locations, options.ignore_missing_nodes)?;
    Ok(stats)
}

/// The external join, chosen up front or by the post-pass-0 recheck. It
/// applies the missing-location gate itself, as soon as stage 2 has counted.
fn run_external(
    input: &PinnedInput,
    front: InputFront<'_>,
    output: &crate::output::BoundOutput,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
) -> Result<Stats> {
    external::external_join_in(input, front, output, options, overrides, host, scratch)
}

/// The post-pass-0 recheck: whether the exact sparse store, now that pass 0
/// has counted it, exceeds the share of the page-cache budget auto routed
/// sparse on. Emits its note only when it re-routes.
///
/// The header estimate counts indexed nodes, so it misses referenced IDs
/// the input does not hold (each takes a slot all the same) and charges
/// standalone ones that take none; `referenced_count * 8` is the store
/// exactly. The negative families of `referenced` (with the rank index
/// pass 1 builds) and of `shared` are anon memory outside the measured
/// allowance the budget already subtracts, so they come off the page cache
/// the store is judged against: external when
/// `100 * store + PCT * anon > PCT * budget`. Both are zero without a
/// negative ID. The budget is the one auto routed on, not a fresh probe,
/// which would charge pass 0's own sets against the allowance a second
/// time.
fn exact_store_exceeds_budget(
    referenced_count: u64,
    referenced: &IdSet,
    shared: Option<&IdSet>,
    budget: u64,
    host: &crate::host::Entered<'_>,
) -> bool {
    let anon = u128::from(referenced.negative_footprint_bytes(true))
        + u128::from(shared.map_or(0, |s| s.negative_footprint_bytes(false)));
    let (over, store) = recheck_routes_external(referenced_count, anon, budget);
    host.invocation.counter(
        "altw_auto_recheck_store_bytes",
        u64::try_from(store).unwrap_or(u64::MAX),
    );
    host.invocation.counter(
        "altw_auto_recheck_anon_bytes",
        u64::try_from(anon).unwrap_or(u64::MAX),
    );
    if over {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_recheck",
            "auto re-routed to index type external: the exact sparse store of \
             {:.1} GB ({referenced_count} referenced node IDs) plus {:.1} GB of \
             negative-ID sets exceeds {AUTO_STORE_BUDGET_PCT} % of the {:.1} GB \
             page-cache budget",
            store as f64 / 1e9,
            anon as f64 / 1e9,
            budget as f64 / 1e9,
        );
    }
    over
}

/// The recheck's rule, pure: `referenced_count` slots of 8 bytes and
/// `anon` bytes outside the allowance, against `budget`. Returns the verdict
/// and the store's bytes. Widened, so no input can wrap it into a verdict.
fn recheck_routes_external(referenced_count: u64, anon: u128, budget: u64) -> (bool, u128) {
    let store = u128::from(referenced_count) * 8;
    let pct = u128::from(AUTO_STORE_BUDGET_PCT);
    (store * 100 + anon * pct > u128::from(budget) * pct, store)
}

/// Refuse an output that carries placeholder vertices, unless the caller
/// explicitly asked for them.
///
/// Every backend that cannot resolve a way's node reference pushes `(0, 0)`
/// and bumps `missing_locations`. `(0, 0)` is Null Island - a legal
/// coordinate in the Gulf of Guinea - so the placeholder is not
/// distinguishable from real data by anything downstream: the file is
/// well-formed, the numbers are plausible, and no reader can tell. Absence
/// has to be reported as absence, so the run fails.
///
/// Each backend calls it as soon as its count is final. The sparse paths
/// (indexed passthrough and the decode-all fallback) count while they write,
/// so they call it after the write; the external join knows its count when
/// stage 2 returns and calls it there, before stages 3 and 4 encode and
/// write anything. Output written before a refusal is never published: it
/// sits at the [`Output`](crate::output::Output)'s staging path, and
/// [`add_locations_to_ways`] (the only public route to every backend)
/// publishes only after the run returns, so no caller is left holding a
/// file of fabricated coordinates.
///
/// The message deliberately does not name the output path: nothing is
/// published, so there is no output to point at. The error is the typed
/// [`ErrorKind::MissingNodeLocations`](crate::ErrorKind::MissingNodeLocations);
/// how to allow the placeholders is the front end's advice.
fn enforce_missing_locations_policy(
    missing_locations: u64,
    ignore_missing_nodes: bool,
) -> Result<()> {
    if missing_locations == 0 || ignore_missing_nodes {
        return Ok(());
    }
    // Every backend counts per reference: each unresolved position in each
    // way, so a node used twice counts twice.
    Err(crate::error::new_missing_node_locations(
        missing_locations,
        crate::error::Unresolved::WayNodeReferences,
    ))
}

// Auto-select: sparse unless the input qualifies for external (sorted +
// indexed) AND the estimated sparse store exceeds a known page-cache budget.
// The threshold is relative to runtime RAM, never a hardcoded byte
// constant: the knee is defined by store-vs-cache, and a constant fitted
// to one host misroutes every other machine.
//
// Eligibility comes from the header and the first OsmData blob's indexdata,
// the same hint the sparse path's indexdata check reads from this front, so
// the two can never disagree (a header-only file is indexed to both; an
// extension frame before the data is skipped by both). Selection judges
// header claims, not blob payloads: a malformed data blob is the business
// of whichever pass reads it.
//
// Returns the backend and, when it is sparse chosen against a known budget
// on an input external could take, that budget for the post-pass-0 recheck.
fn auto_select_backend(
    input: &PinnedInput,
    front: &mut InputFront<'_>,
    allow_eager: bool,
    host: &crate::host::Entered<'_>,
) -> Result<(ResolvedBackend, SparseRoute)> {
    let sorted = front.is_sorted()?;
    let has_index = front.indexed()?;

    if !(sorted && has_index) {
        // External cannot run (it requires sorted + indexdata); no
        // estimate needed.
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_select",
            "auto-selected index type sparse (sorted={sorted}, indexed={has_index})"
        );
        return Ok((
            ResolvedBackend::Sparse,
            SparseRoute::for_eager(has_index && allow_eager, host),
        ));
    }

    // Both backends can run. The budget is probed first: when it cannot
    // justify leaving sparse, the estimate walk would decide nothing and is
    // skipped.
    let available = host.resources.memory_available();
    host.invocation.counter(
        "altw_auto_memory_available_bytes",
        available.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
    );
    let Some(budget) = page_cache_budget_bytes(available) else {
        let why = match available {
            None => "available memory unknown".to_string(),
            Some(bytes) => format!(
                "{:.1} GB available does not exceed the {:.1} GB anon allowance",
                bytes as f64 / 1e9,
                AUTO_ANON_HEADROOM_BYTES as f64 / 1e9,
            ),
        };
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_select",
            "auto-selected index type sparse (sorted=true, indexed=true; {why}, so no \
             page-cache budget can justify the external index)"
        );
        return Ok((ResolvedBackend::Sparse, SparseRoute::default()));
    };

    // Route on scale. The walk reads headers the selected backend will also
    // need, so it can warm those pages for the following pass rather than
    // adding another cold header walk.
    host.invocation.marker("ALTW_AUTO_ESTIMATE_WALK_START");
    let walk = route_walk(input, host)?;
    host.invocation.marker("ALTW_AUTO_ESTIMATE_WALK_END");
    let store = walk.store_estimate;

    // An input the external join would refuse on its indexdata, or would
    // need its wide record for, runs sparse, which joins it, and is not
    // rechecked: there is nothing auto re-routes it to.
    if let Err(refusal) = &walk.external {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_select",
            "auto-selected index type sparse (sorted=true, indexed=true; auto does not \
             route this input to the external index: {refusal})"
        );
        return Ok((
            ResolvedBackend::Sparse,
            SparseRoute {
                budget: Some(budget),
                walk: Some(walk),
                recheck: false,
            },
        ));
    }
    let chosen = route_sorted_indexed(store, budget);

    host.invocation.counter(
        "altw_auto_store_estimate_bytes",
        store.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
    );
    host.invocation.counter(
        "altw_auto_cache_budget_bytes",
        i64::try_from(budget).unwrap_or(i64::MAX),
    );

    if let Some(store) = store {
        let rel = if chosen == ResolvedBackend::External {
            ">"
        } else {
            "<="
        };
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_select",
            "auto-selected index type {chosen} (store estimate {:.1} GB {rel} \
             {AUTO_STORE_BUDGET_PCT} % of {:.1} GB page-cache budget)",
            store as f64 / 1e9,
            budget as f64 / 1e9,
        );
    } else {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "altw.auto_select",
            "auto-selected index type {chosen} (sorted=true, indexed=true; \
             store estimate unavailable)"
        );
    }
    let recheck = chosen == ResolvedBackend::Sparse;
    Ok((
        chosen,
        SparseRoute {
            budget: Some(budget),
            walk: Some(walk),
            recheck,
        },
    ))
}

#[derive(Default)]
struct SparseRoute {
    budget: Option<u64>,
    walk: Option<RouteWalk>,
    recheck: bool,
}

impl SparseRoute {
    fn for_eager(candidate: bool, host: &crate::host::Entered<'_>) -> Self {
        let budget = if candidate {
            page_cache_budget_bytes(host.resources.memory_available())
        } else {
            None
        };
        Self {
            budget,
            ..Self::default()
        }
    }

    fn fits_all_nodes(&self, input: &PinnedInput, host: &crate::host::Entered<'_>) -> Result<bool> {
        let Some(budget) = self.budget else {
            return Ok(false);
        };
        let walk;
        let counts = if let Some(walk) = &self.walk {
            walk
        } else {
            host.invocation.marker("ALTW_SPARSE_ESTIMATE_WALK_START");
            walk = route_walk(input, host)?;
            host.invocation.marker("ALTW_SPARSE_ESTIMATE_WALK_END");
            &walk
        };
        Ok(counts.saw_way
            && counts
                .store_estimate
                .is_some_and(|bytes| eager::fits(bytes, counts.node_blobs, budget)))
    }
}

/// Pure routing decision for an input both backends can handle (sorted +
/// indexed), given a known page-cache budget. External iff the estimated
/// store exceeds [`AUTO_STORE_BUDGET_PCT`] of the budget. An unavailable
/// store estimate keeps sparse, the default before auto: external is
/// chosen only on positive evidence, since it imposes scratch-disk demand
/// the caller did not ask for.
fn route_sorted_indexed(store_bytes: Option<u64>, budget_bytes: u64) -> ResolvedBackend {
    match store_bytes {
        Some(store)
            if store.saturating_mul(100) > budget_bytes.saturating_mul(AUTO_STORE_BUDGET_PCT) =>
        {
            ResolvedBackend::External
        }
        _ => ResolvedBackend::Sparse,
    }
}

/// Estimate the sparse store size for an indexed input: total node count
/// from per-blob indexdata, times 8 bytes per rank slot. The store is
/// sized by the way-referenced IDs, so the estimate is close when most
/// nodes are way-referenced, high when many are standalone, and low by one
/// slot for each referenced ID the input does not hold. Overestimating routes toward external,
/// which is not free: a file of mostly standalone nodes can be sent to
/// external, whose scratch demand (per way reference, see
/// `reference/pipeline.md`) a tiny real store would never have needed, and
/// whose scratch check can then refuse a run sparse would complete.
/// Indexdata counts elements, not references, so no tighter bound is
/// available before pass 0 decodes the ways; the post-pass-0 recheck then
/// judges the exact store, which also counts the referenced IDs the input
/// does not hold. The estimate is `None` when the first OsmData blob lacks
/// indexdata, since the sum would silently undercount. Any missing indexdata
/// disables the estimate, leaving the consuming pass to apply its policy.
///
/// The same walk asks [`external::HeaderAdmission`] whether the external
/// join would admit the input's indexdata, so auto never routes, up front
/// or on the recheck, to an engine that then refuses those claims.
fn route_walk(input: &PinnedInput, host: &crate::host::Entered<'_>) -> Result<RouteWalk> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let mut node_count: u64 = 0;
    let mut node_blobs: u64 = 0;
    let mut saw_way = false;
    let mut admission = external::HeaderAdmission::new();
    while let Some(meta) = walker.next_header()? {
        if meta.blob_type != crate::blob::BlobKind::OsmData {
            continue;
        }
        match meta.index.as_ref() {
            Some(idx) => {
                if idx.kind == crate::ElementKind::Node {
                    node_count = node_count.saturating_add(idx.count);
                    node_blobs += 1;
                }
                saw_way |= idx.kind == crate::ElementKind::Way;
                admission.observe(idx);
            }
            None => {
                return Ok(RouteWalk {
                    store_estimate: None,
                    node_blobs,
                    saw_way,
                    external: Err(crate::error::new_missing_indexdata(
                        "an OsmData blob carries no indexdata",
                    )),
                });
            }
        }
    }
    Ok(RouteWalk {
        store_estimate: Some(store_estimate_bytes(node_count, saw_way)),
        node_blobs,
        saw_way,
        external: admission.finish(),
    })
}

/// The header estimate of the sparse store. Only way node references take
/// store slots (relation node members are collected separately and never
/// enter it), so a file with no way blob has an empty store whatever its
/// node count, and is judged after the whole walk so a late way blob still
/// counts. Otherwise every indexed node is charged.
///
/// No tighter bound on the referenced-ID count exists in the headers:
/// indexdata gives element counts and ID ranges, which cannot tell ways
/// that share nodes from ways that reference disjoint ones, and a bound
/// from way payload sizes (at least one byte per reference) is far looser
/// than the node count on any real file. Running sparse's reference pass
/// before an external route to judge the exact store was weighed and
/// refused: at planet scale the referenced-ID count is within a few percent
/// of the node count, so the pass would add its full cost to every large
/// external run to correct only standalone-heavy files, which
/// `--index-type sparse` already serves.
fn store_estimate_bytes(node_count: u64, saw_way: bool) -> u64 {
    if saw_way {
        node_count.saturating_mul(8)
    } else {
        0
    }
}

/// What auto's header walk learned: the store estimate, and whether the
/// external join would admit the input's indexdata (its refusal if not,
/// which auto reports and routes around).
struct RouteWalk {
    store_estimate: Option<u64>,
    node_blobs: u64,
    saw_way: bool,
    external: Result<()>,
}

/// Page-cache budget for the sparse store: the host's available memory
/// (`MachineProbe`: `MemAvailable`, capped on Linux by visible cgroup v2
/// headroom) minus the anon allowance the sparse run itself needs. The
/// caller owns the memory facts: fixed resources can route on any platform
/// without procfs.
///
/// `None` when memory is unknown, and also when it does not exceed the
/// allowance. The second is a compatibility rule, not a resource model:
/// the fixed allowance was measured at large scale, so below it the model
/// would send even a store of a few megabytes to external, and it cannot
/// justify departing from sparse, the default before auto. The rule is not
/// monotonic in memory (a large store routes external just above the
/// allowance and sparse just below it), and it is documented as such in
/// `reference/pipeline.md`.
///
/// Making it monotonic (external whenever a known budget is small) was
/// weighed and refused: the model charges only sparse's page-cache store,
/// and neither backend's input-dependent anon. External's anon grows with
/// reference occurrences per bucket at capped worker concurrency (recorded
/// peaks about 3 GB germany, 6.9 GB europe, 11.6 GB planet), while sparse's
/// store is a file-backed map that is evicted rather than killed, so routing
/// large inputs external under severe memory pressure is not the safe side.
/// Sparse's own anon can also grow with the shape of the referenced-ID set
/// (scattered negative IDs promote whole bitmap chunks). A monotone rule
/// needs a footprint model of both backends, not a different threshold.
fn page_cache_budget_bytes(available: Option<u64>) -> Option<u64> {
    available
        .filter(|&bytes| bytes > AUTO_ANON_HEADROOM_BYTES)
        .map(|bytes| bytes.saturating_sub(AUTO_ANON_HEADROOM_BYTES))
}

/// Per-run instrumentation for the injected-prepass producer.
///
/// The values feed brokkr sidecar counters only (a no-op without the FIFO),
/// so they are diagnostic and never surface in `Stats` or the CLI summary.
///
/// Each run owns one [`InjectMetrics`], so two runs in one process - one
/// after another or concurrently - never fold into each other's totals. The
/// counting is two-level to keep it off the per-way hot path: each reframe
/// worker tallies its current blob into a plain [`PinTally`] held in its
/// reusable scratch, and publishes that tally into the run's atomics once
/// per blob. This is the same blob-local pattern external stage 4 uses for
/// its `WayReframeCounters`; the previous process-global statics took two
/// contended atomic adds per pinned way.
pub(super) mod inject_metrics {
    use std::sync::atomic::Ordering::Relaxed;

    /// One blob's field-20 tally, accumulated by a single worker without
    /// atomics. Reset at the start of every way-blob reframe, so a blob
    /// that fails part-way never leaks its partial count into the next.
    #[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct PinTally {
        pinned_refs: u64,
        field20_ways: u64,
    }

    impl PinTally {
        /// Fold one way's field-20 pin bitmap: total set bits into
        /// `pinned_refs`, and one into `field20_ways` when any bit is set.
        /// A full-width all-zero bitmap is not emitted and counts nothing.
        #[inline]
        pub(super) fn record_way(&mut self, pins: &[u8]) {
            let popcount: u32 = pins.iter().map(|b| b.count_ones()).sum();
            if popcount > 0 {
                self.pinned_refs += u64::from(popcount);
                self.field20_ways += 1;
            }
        }
    }

    crate::counters::counter_set! {
        /// One run's injected-prepass totals, shared by reference across that
        /// run's reframe workers.
        #[derive(Debug)]
        pub(super) struct InjectMetrics {
            pinned_refs => "altw_pinned_refs",
            field20_ways => "altw_field20_ways_emitted",
            field5_bytes => "altw_field5_bytes",
        }
    }

    impl InjectMetrics {
        /// Publish one successfully reframed way blob: its pin tally and
        /// the length of its field-5 payload. A blob with no pinned refs
        /// touches only the field-5 total.
        pub(super) fn record_blob(&self, pins: PinTally, field5_bytes: usize) {
            if pins.pinned_refs > 0 {
                self.pinned_refs.fetch_add(pins.pinned_refs, Relaxed);
                self.field20_ways.fetch_add(pins.field20_ways, Relaxed);
            }
            self.field5_bytes.fetch_add(field5_bytes as u64, Relaxed);
        }

        /// Emit the four injected-prepass counters. `member_ways` is the
        /// size of the relation member-way IdSet, computed by the caller.
        pub(super) fn emit(&self, member_ways: u64, invocation: &crate::host::Recorder) {
            invocation.counter("altw_member_ways", member_ways);
            self.emit_all(invocation);
        }

        #[cfg(test)]
        fn totals(&self) -> (u64, u64, u64) {
            (
                self.pinned_refs.load(Relaxed),
                self.field20_ways.load(Relaxed),
                self.field5_bytes.load(Relaxed),
            )
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{InjectMetrics, PinTally};

        #[test]
        fn tally_counts_set_bits_and_only_ways_with_a_pin() {
            let mut tally = PinTally::default();
            tally.record_way(&[0b1010_0001, 0b0000_0001]);
            tally.record_way(&[0, 0, 0]);
            tally.record_way(&[]);
            tally.record_way(&[0b1000_0000]);
            assert_eq!(
                tally,
                PinTally {
                    pinned_refs: 5,
                    field20_ways: 2,
                }
            );
        }

        #[test]
        fn runs_do_not_share_totals() {
            let mut tally = PinTally::default();
            tally.record_way(&[0b11]);

            let first = InjectMetrics::default();
            first.record_blob(tally, 7);
            first.record_blob(tally, 3);

            let second = InjectMetrics::default();
            second.record_blob(PinTally::default(), 4);

            assert_eq!(first.totals(), (4, 2, 10));
            assert_eq!(second.totals(), (0, 0, 4));
        }

        #[test]
        fn concurrent_publishers_into_one_run_sum_exactly() {
            let mut tally = PinTally::default();
            tally.record_way(&[0b1]);
            let run = InjectMetrics::default();
            let other = InjectMetrics::default();
            std::thread::scope(|s| {
                for _ in 0..4 {
                    s.spawn(|| {
                        for _ in 0..1000 {
                            run.record_blob(tally, 2);
                            other.record_blob(tally, 1);
                        }
                    });
                }
            });
            assert_eq!(run.totals(), (4000, 4000, 8000));
            assert_eq!(other.totals(), (4000, 4000, 4000));
        }
    }
}

fn emit_stats_counters(stats: &Stats, invocation: &crate::host::Recorder) {
    invocation.counter("altw_nodes_read", stats.nodes_read);
    invocation.counter("altw_nodes_written", stats.nodes_written);
    invocation.counter("altw_nodes_dropped", stats.nodes_dropped);
    invocation.counter("altw_ways_written", stats.ways_written);
    invocation.counter("altw_relations_written", stats.relations_written);
    invocation.counter("altw_missing_locations", stats.missing_locations);
    invocation.counter("altw_blobs_passthrough", stats.blobs_passthrough);
    invocation.counter("altw_blobs_decoded", stats.blobs_decoded);
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

/// Reserve pass 0's sets for the worker-side atomic insertion, from the
/// node blobs' claimed highest ID capped by the input's size
/// ([`ALTW_PASS0_PREALLOC_IDS_PER_BYTE`](crate::tunables::ALTW_PASS0_PREALLOC_IDS_PER_BYTE),
/// shared by both sets under prepass). Both sets get the same bound, which
/// is what lets one repeat batch write them.
fn reserve_pass0_sets(
    file: &crate::read::input::InputFile,
    claim: i64,
    referenced: &mut IdSet,
    shared: Option<&mut IdSet>,
) -> Result<()> {
    let file_len = file
        .file()
        .metadata()
        .map_err(|e| {
            crate::Error::from(e).context(format!("failed to stat {}", file.path().display()))
        })?
        .len();
    let per_byte =
        crate::tunables::ALTW_PASS0_PREALLOC_IDS_PER_BYTE / if shared.is_some() { 2 } else { 1 };
    // Below 2^43 a reservation is lazy (chunks materialise on first write);
    // above it `pre_allocate` would allocate eagerly.
    let bound = IdSet::file_bounded_max_id(claim, file_len, per_byte).min((1i64 << 43) - 1);
    referenced.pre_allocate(bound);
    if let Some(shared) = shared {
        shared.pre_allocate(bound);
    }
    Ok(())
}

/// Insert one admitted blob's (closed-ring-trimmed) refs into the reserved
/// sets, returning the refs the reservation does not cover, in order, for
/// the serial union. With no reservation every ref spills, so the buffer
/// itself is handed over rather than a copy held beside it.
fn insert_admitted_refs(
    refs: &mut Vec<i64>,
    reserved: bool,
    referenced: &IdSet,
    shared: Option<&IdSet>,
) -> Vec<i64> {
    if !reserved {
        return std::mem::take(refs);
    }
    let mut spill = Vec::new();
    match shared {
        Some(shared) => {
            let mut batch = crate::idset::AtomicRepeatBatch::new(referenced, shared);
            for &node_id in refs.iter() {
                if !batch.insert(node_id) {
                    spill.push(node_id);
                }
            }
            batch.finish();
        }
        None => {
            let mut batch = crate::idset::AtomicWordBatch::new(referenced);
            for &node_id in refs.iter() {
                if !batch.insert(node_id) {
                    spill.push(node_id);
                }
            }
        }
    }
    spill
}

/// Collect all node IDs referenced by ways (pass 0).
///
/// Uses `build_classify_schedules_split` to obtain the way blob schedule
/// and the node blobs' claimed highest ID via the pread-only
/// `HeaderWalker`, then fans work out to `parallel_scan_blobs_raw`
/// workers. Each worker decompresses one blob and walks the wire format
/// directly via `scan_way_refs` - skipping `PrimitiveBlock` construction
/// (no StringTable parse, no `(u32, u32)` group_ranges allocation) - and,
/// once the blob is admitted, inserts its refs into the shared sets
/// word-batched and atomically, as far as the claim-sized reservation
/// covers them. Uncovered refs come back per blob and the main thread
/// unions them serially. Store size and peak memory depend on the input; see
/// `reference/performance/add-locations-to-ways.md` for measurements.
///
/// `direct_io` is intentionally dropped on this path: the blob bodies are
/// pread from the shared file handle on worker threads, which is
/// incompatible with `O_DIRECT`'s alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_way_referenced_node_ids(
    input: &PinnedInput,
    _direct_io: bool,
    inject_prepass: bool,
    host: &crate::host::Entered<'_>,
) -> Result<(IdSet, Option<IdSet>)> {
    let (schedules, shared_file, max_ids) = crate::scan::classify::build_classify_schedules_split(
        input,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;
    // The node blobs' claimed highest ID sizes the sets the workers write
    // atomically; an unindexed file (or one without node blobs) has no
    // claim, reserves nothing, and every ref takes the serial union below.
    let (schedule, node_claim) = match schedules {
        crate::scan::classify::KindSchedules::Indexed { nodes, ways, .. } => {
            (ways, (!nodes.is_empty()).then_some(max_ids[0]))
        }
        crate::scan::classify::KindSchedules::Unindexed(blobs) => (blobs, None),
    };
    let mut referenced = IdSet::new();
    // Prepass needs two independent facts: whether an ID has been seen at
    // all, and whether it was referenced again (a closed ring's repeated last
    // ref is stripped in the scan closure below and does not count).
    // A single membership bit cannot retain both. Opting into injection thus
    // allocates a second IdSet over the same node-ID range; its populated
    // chunks can approach referenced's footprint, roughly doubling pass-0
    // set memory in the worst case. The default path allocates no second set
    // and keeps its cheaper set-only union loop below. Retain this explicit
    // opt-in cost rather than replacing measured default-path membership
    // with a multi-state encoding or a per-worker duplicate set.
    let mut shared = inject_prepass.then(IdSet::new);
    let reserved = node_claim.is_some();
    if let Some(claim) = node_claim {
        reserve_pass0_sets(&shared_file, claim, &mut referenced, shared.as_mut())?;
    }
    // Refs the reservation does not cover (negative, above the bound, or
    // every ref of an unindexed file) are unioned serially into these, by
    // the loop the parallel path replaced; coverage depends only on the ID,
    // so each ID's occurrences all land on one side and the two halves
    // merge disjointly after the scan.
    let mut spill_referenced = IdSet::new();
    let mut spill_shared = inject_prepass.then(IdSet::new);
    // Union-side attribution: `altw_pass0_union_ms` is the serial union of
    // spilled refs on the calling thread, `altw_pass0_worker_insert_ms`
    // the worker-side pass over each admitted blob's refs - atomic inserts
    // plus routing uncovered refs to the spill - summed over workers (it
    // can exceed the phase wall).
    let mut union_ns: u64 = 0;
    let mut union_refs_total: u64 = 0;
    let mut spilled_refs_total: u64 = 0;
    let worker_insert_ns = std::sync::atomic::AtomicU64::new(0);
    let (referenced_ref, shared_ref) = (&referenced, shared.as_ref());
    crate::scan::classify::parallel_scan_blobs_raw(
        &shared_file,
        &schedule,
        None,
        || {
            (
                Vec::<i64>::new(),
                Vec::<(usize, usize)>::new(),
                Vec::<i64>::new(),
            )
        },
        |_position, decompressed, (refs_buf, group_starts, blob_refs), gate| {
            blob_refs.clear();
            let kinds = crate::scan::way::scan_way_refs(
                decompressed,
                refs_buf,
                group_starts,
                |_way_id, refs| {
                    let refs = if refs.len() >= 4 && refs.first() == refs.last() {
                        &refs[..refs.len() - 1]
                    } else {
                        refs
                    };
                    // Every ref, either sign: the set is signed, and the
                    // store ranks negatives like any other ID.
                    blob_refs.extend_from_slice(refs);
                },
            )?;
            // The walker reads only ways, so nodes or relations under a way
            // claim would be copied through the reframe unindexed and
            // unscanned. The way claim's range and count are checked where
            // the reframe counts them (`passthrough::decode_one`).
            let admitted = gate.admit(kinds)?;
            // Shared writes only after admission: the refs were buffered
            // above so that a blob whose kinds contradict its claim writes
            // nothing.
            let blob_ref_count = blob_refs.len() as u64;
            let t_insert = crate::counters::Stopwatch::start();
            let spill = insert_admitted_refs(blob_refs, reserved, referenced_ref, shared_ref);
            worker_insert_ns.fetch_add(t_insert.elapsed_ns(), std::sync::atomic::Ordering::Relaxed);
            Ok((admitted, (blob_ref_count, spill)))
        },
        |_seq, (blob_ref_count, refs_vec)| {
            let t_union = crate::counters::Stopwatch::start();
            union_refs_total += blob_ref_count;
            spilled_refs_total += refs_vec.len() as u64;
            // Split on `shared` rather than branching per ref: the plain
            // path must not pay the shared-detection `get`, which only
            // exists to feed prepass injection. Branching inside the loop
            // was measured at ~2.3 ns/ref on every ref regardless of the
            // flag (union 6.6 ns/ref vs 4.3), since `get` is a real probe of
            // a large set and the branch predictor cannot elide it.
            match &mut spill_shared {
                Some(spill_shared) => {
                    for &node_id in &refs_vec {
                        if spill_referenced.get(node_id) {
                            spill_shared.insert(node_id);
                        } else {
                            spill_referenced.insert(node_id);
                        }
                    }
                }
                None => {
                    for &node_id in &refs_vec {
                        spill_referenced.insert(node_id);
                    }
                }
            }
            union_ns += t_union.elapsed_ns();
        },
        host,
    )?;
    referenced.merge(spill_referenced);
    if let (Some(shared), Some(spill_shared)) = (&mut shared, spill_shared) {
        shared.merge(spill_shared);
    }
    host.invocation
        .counter("altw_pass0_union_ms", union_ns / 1_000_000);
    host.invocation.counter(
        "altw_pass0_worker_insert_ms",
        worker_insert_ns.into_inner() / 1_000_000,
    );
    host.invocation
        .counter("altw_pass0_refs_total", union_refs_total);
    host.invocation
        .counter("altw_pass0_spilled_refs", spilled_refs_total);
    if let Some(shared_set) = &shared {
        host.invocation
            .counter("altw_pass0_shared_node_ids", shared_set.len());
    }
    Ok((referenced, shared))
}

/// Collect all node IDs referenced by relation members.
///
/// Per-blob node-id streaming via `parallel_classify_phase`: workers emit
/// `Vec<i64>` of relation-member node ids per blob through the bounded
/// result channel; the main thread unions them into one shared `IdSet`.
/// Memory is bounded to one IdSet plus per-blob transient vectors rather
/// than one `IdSet` per worker (the previous shape, which measured +9.7 GB
/// anon at europe scale). Same
/// migration template as `tags_filter::collect_way_node_dependencies`
/// (commit `17b116c`). Set-union is commutative, so the worker-arrival
/// order does not affect the final IdSet contents.
///
/// `direct_io` is intentionally dropped on this path: blob bodies are
/// pread from the shared file handle on worker threads, incompatible
/// with `O_DIRECT` alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn collect_relation_member_ids(
    input: &PinnedInput,
    _direct_io: bool,
    want_nodes: bool,
    want_ways: bool,
    host: &crate::host::Entered<'_>,
) -> Result<(Option<IdSet>, Option<IdSet>)> {
    // Every relation blob is decoded here, so its claim is held to what it
    // holds: a foreign kind by the pool (`KindClaims::Refuse`), range and
    // count in the merge, which knows the blob's position. Order is not
    // checked: the sparse path does not depend on it.
    let (schedule, claims, shared_file) =
        crate::scan::classify::build_classify_schedule_with_claims(
            input,
            Some(crate::ElementKind::Relation),
            host,
        )?;
    let mut member_node_ids = want_nodes.then(IdSet::new);
    let mut member_way_ids = want_ways.then(IdSet::new);
    let mut refused: Option<Error> = None;
    crate::scan::classify::parallel_classify_phase_abortable(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _state| {
            let mut ids: Vec<(bool, i64)> = Vec::new();
            let mut walk = crate::blob_meta::ClaimWalk::new(None, crate::blob_meta::Order::Free);
            relation_member_ids(block, want_nodes, want_ways, &mut walk, |way, id| {
                ids.push((way, id));
            })?;
            Ok((ids, walk.tally()))
        },
        |seq, (ids, tally)| {
            if let Some(claim) = &claims[seq]
                && let Err(e) = tally.refuse_inexact(claim)
            {
                refused = Some(shared_file.locate_blob(&schedule[seq], e));
                return std::ops::ControlFlow::Break(());
            }
            for (way, id) in ids {
                // A kind that was not requested has no set and nothing to
                // record.
                let set = if way {
                    &mut member_way_ids
                } else {
                    &mut member_node_ids
                };
                if let Some(set) = set {
                    set.insert(id);
                }
            }
            std::ops::ControlFlow::Continue(())
        },
        host,
    )?;
    if let Some(e) = refused {
        return Err(e);
    }
    Ok((member_node_ids, member_way_ids))
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------

enum SparseNodes {
    AllNodes,
    Referenced(IdSet),
}

#[allow(clippy::too_many_arguments)]
fn write_output_checked(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    scratch: &crate::scratch::RunScratch,
    nodes: SparseNodes,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    inject_metrics: &inject_metrics::InjectMetrics,
    write: &WriteOptions,
    indexdata_present: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<Stats> {
    // Backstop only: `AltwOptions::validate` refuses `force` with
    // `inject_prepass`, so an unindexed input is refused by the indexdata
    // probe before pass 0. Kept so the decode-all writer can never be asked
    // for metadata it cannot attach.
    if inject_prepass && !indexdata_present {
        return Err(crate::error::new_missing_indexdata(
            "add-locations-to-ways with inject_prepass requires blob-level indexdata, even \
             when forced: the decode-all fallback cannot attach per-blob WayMembers metadata",
        )
        .locate(crate::error::Location::in_file(input.path())));
    }
    if indexdata_present {
        let store = match nodes {
            SparseNodes::AllNodes => None,
            SparseNodes::Referenced(referenced) => {
                Some(sparse::StoreBuild::create(scratch, referenced, host)?)
            }
        };
        write_output_passthrough(
            input,
            output,
            store,
            keep_untagged_nodes,
            relation_member_node_ids,
            relation_member_way_ids,
            shared_node_ids,
            inject_prepass,
            inject_metrics,
            write,
            overrides,
            host,
        )
    } else {
        let SparseNodes::Referenced(referenced) = nodes else {
            return Err(Error::internal("all-node store requires indexed input"));
        };
        host.invocation.marker("ALTW_PASS1_START");
        let index = build_node_index_sparse(input, write.direct_reads, scratch, referenced, host)?;
        host.invocation.marker("ALTW_PASS1_END");
        host.invocation.marker("ALTW_PASS2_START");
        let stats = write_output_decode_all(
            input,
            output,
            &index,
            keep_untagged_nodes,
            relation_member_node_ids,
            write,
            overrides,
            host,
        )?;
        host.invocation.marker("ALTW_PASS2_END");
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    index: &SparseArrayIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let reader = ElementReader::open_in(input, write.direct_reads, host)?;
    let mut writer = writer_from_header(
        output,
        write.compression,
        reader.header(),
        &altw_output_facts(false),
        overrides,
        write.backend.parallel_by_default(),
        host,
    )?;

    let mut decoded_blocks = 0_u64;
    let limits = &crate::commands::split::Limits::new(writer.compression());
    reader.for_each_fused_block(
        |block| {
            let mut bb = BlockBuilder::new();
            let mut output = Vec::new();
            let mut refs_buf = Vec::new();
            let mut locations_buf = Vec::new();
            let block_stats = process_block(
                &block,
                &mut bb,
                &mut output,
                Some(&NodeIndex::Sparse(index)),
                keep_untagged_nodes,
                relation_member_node_ids,
                &mut refs_buf,
                &mut locations_buf,
            )?;
            flush_local(&mut bb, &mut output)?;
            let (output, _) = fit_output(output, limits)?;
            Ok((output, block_stats))
        },
        |(blocks, block_stats)| {
            for block in blocks {
                writer.write_primitive_block_owned(block)?;
            }
            stats.merge(&block_stats)?;
            decoded_blocks += 1;
            if decoded_blocks.is_multiple_of(64) {
                host.invocation.counter("altw_pass2_blocks", decoded_blocks);
            }
            Ok(())
        },
    )?;
    host.invocation.counter("altw_pass2_blocks", decoded_blocks);

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Per-block fused transform
// ---------------------------------------------------------------------------

use super::flush_local;
use crate::owned::{dense_node_metadata, element_metadata};

/// Process a single `PrimitiveBlock`, writing elements into the thread-local
/// `BlockBuilder` and flushing complete blocks into `output`.
///
/// `node_index` is `None` in the indexed node pass, which rebuilds node
/// blobs while the store is still being filled; its caller has refused any
/// foreign kind first, so a way reaching the node-only rebuild is a broken
/// invariant.
///
/// Way refs resolve via inline `SparseArrayIndex::get`. The earlier
/// `resolve_batch_locations` pre-pass that converted random sparse mmap
/// I/O to a sorted sequential scan was removed in favour of straight
/// parallel inline lookups: the parallelism win (sparse pass 2 went from
/// avg cores ~4 with the serial resolve to ~16 with inline lookups)
/// dominates whatever cache-friendliness the sort provided. Pages get
/// faulted in either way; only the order changes, and the amortised
/// page-touch cost is the same once each page is hot.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node_index: Option<&NodeIndex<'_>>,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
) -> crate::Result<Stats> {
    let mut stats = Stats::default();

    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(dn.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    // A retained node keeps its content at the OSM data
                    // model's precision, not its source framing: a rebuilt
                    // block is default-framed (docs/cli/commands.md "PBF
                    // rebuild precision"). The external engine rebuilds
                    // retained nodes by the same conversion, so the engines
                    // stay digest-equal.
                    let meta = dense_node_metadata(dn)?;
                    bb.add_node_dense(
                        dn.id(),
                        dn.decimicro_lat()?,
                        dn.decimicro_lon()?,
                        dn.tags(),
                        meta.as_ref(),
                    );
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Node(n) => {
                stats.nodes_read += 1;
                let has_tags = n.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(n.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                    bb.add_node_partial(
                        n.id(),
                        n.decimicro_lat()?,
                        n.decimicro_lon()?,
                        n.tags(),
                        meta.as_ref(),
                    );
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
                let Some(node_index) = node_index else {
                    return Err(Error::internal(
                        "a way reached the node-only rebuild past its blob's kind check",
                    ));
                };
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for node_id in refs_buf.iter() {
                    match node_index.get(*node_id) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                }
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                bb.add_way_with_locations_partial(
                    w.id(),
                    w.tags(),
                    refs_buf,
                    locations_buf,
                    meta.as_ref(),
                );
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                r.members_into(&mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation_partial(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unwrap_in_result)]
mod tests {
    use super::{
        AUTO_ANON_HEADROOM_BYTES, AUTO_STORE_BUDGET_PCT, ResolvedBackend, page_cache_budget_bytes,
        recheck_routes_external, route_sorted_indexed,
    };
    use super::{AltwOptions, IndexType, add_locations_to_ways};
    use crate::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
    use crate::host::{Fixed, Host, Invocation};
    use crate::writer::{Compression, PbfWriter};
    use std::num::NonZeroUsize;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};

    const GB: u64 = 1_000_000_000;

    struct SparseMode {
        all_nodes: AtomicI64,
        collected_refs: AtomicBool,
    }

    impl crate::debug::InstrumentSink for SparseMode {
        fn marker(&self, name: &str) {
            if name == "ALTW_PASS0_START" {
                self.collected_refs.store(true, Relaxed);
            }
        }

        fn counter(&self, name: &str, value: i64) {
            if name == "altw_sparse_all_nodes" {
                self.all_nodes.store(value, Relaxed);
            }
        }
    }

    fn sparse_test_host(memory: Option<u64>) -> (Host, Arc<SparseMode>) {
        let mut host = Host::for_tests();
        host.resources = Arc::new(Fixed {
            parallelism: NonZeroUsize::new(4),
            memory_available: memory,
        });
        let mode = Arc::new(SparseMode {
            all_nodes: AtomicI64::new(-1),
            collected_refs: AtomicBool::new(false),
        });
        host.invocation = Invocation::with_sink(Arc::clone(&mode) as _);
        (host, mode)
    }

    fn write_eager_fixture(path: &Path, refs: &[i64]) {
        let mut writer = PbfWriter::to_path_sync(path, Compression::None).unwrap();
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        let mut builder = BlockBuilder::new();
        // An identical duplicate, a genuine (0, 0), and an unreferenced
        // duplicate whose first coordinate is outside the domain.
        for (id, lat, lon) in [
            (-7, 201, -102),
            (-7, 201, -102),
            (0, 0, 0),
            (5, 910_000_000, 100),
            (5, 500, 200),
            (20, 900, 100),
        ] {
            builder.add_node(id, lat, lon, [], None);
        }
        writer
            .write_primitive_block_owned(builder.take_owned().unwrap().unwrap())
            .unwrap();
        builder.add_way(100, [("highway", "residential")], refs, None);
        writer
            .write_primitive_block_owned(builder.take_owned().unwrap().unwrap())
            .unwrap();

        // Indexed but interleaved: a way can refer to a later node blob.
        // The negative ID repeats across blobs, and ID 20 conflicts across
        // the overlapping ranges. The pair well above u32::MAX forces the
        // ID dictionary to keep full-width keys.
        for (id, lat, lon) in [(1 << 40, 505, 601), (-7, 201, -102), (20, 700, 100)] {
            builder.add_node(id, lat, lon, [("name", "retained")], None);
        }
        writer
            .write_primitive_block_owned(builder.take_owned().unwrap().unwrap())
            .unwrap();
        builder.add_relation(
            101,
            [],
            &[MemberData {
                id: crate::MemberId::Node(0),
                role: "member",
            }],
            None,
        );
        writer
            .write_primitive_block_owned(builder.take_owned().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();
    }

    fn embed_fixture(
        host: &Host,
        input: &Path,
        output: &Path,
        keep_untagged_nodes: bool,
        inject_prepass: bool,
        ignore_missing_nodes: bool,
    ) -> crate::Result<super::Stats> {
        let scratch = crate::scratch::RunScratch::create(input.parent().unwrap(), host).unwrap();
        let output =
            crate::output::Output::new(output, crate::output::Overwrite::Refuse, &host.diagnostics)
                .unwrap();
        add_locations_to_ways(
            input,
            output,
            &AltwOptions {
                keep_untagged_nodes,
                write: crate::write::WriteOptions::from_flags(
                    Compression::None,
                    false,
                    false,
                    false,
                ),
                index_type: IndexType::Sparse,
                inject_prepass,
                ignore_missing_nodes,
            },
            &super::HeaderOverrides::default(),
            host,
            scratch,
        )
    }

    #[test]
    fn all_node_sparse_output_matches_reference_selected_bytes() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("fixture.pbf");
        write_eager_fixture(&input, &[-7, 0, 1 << 40, -6, -6, -7]);
        for keep in [false, true] {
            let (eager_host, eager_mode) = sparse_test_host(Some(8 << 30));
            let (ranked_host, ranked_mode) = sparse_test_host(None);
            let eager = dir.path().join(format!("eager-{keep}.pbf"));
            let ranked = dir.path().join(format!("ranked-{keep}.pbf"));
            let eager_stats =
                embed_fixture(&eager_host, &input, &eager, keep, false, true).unwrap();
            let ranked_stats =
                embed_fixture(&ranked_host, &input, &ranked, keep, false, true).unwrap();
            assert_eq!(eager_mode.all_nodes.load(Relaxed), 1);
            assert!(!eager_mode.collected_refs.load(Relaxed));
            assert_eq!(ranked_mode.all_nodes.load(Relaxed), 0);
            assert!(ranked_mode.collected_refs.load(Relaxed));
            assert_eq!(eager_stats.missing_locations, 2);
            assert_eq!(
                eager_stats.missing_locations,
                ranked_stats.missing_locations
            );
            assert_eq!(eager_stats.nodes_written, if keep { 9 } else { 4 });
            assert_eq!(
                std::fs::read(eager).unwrap(),
                std::fs::read(ranked).unwrap()
            );
        }
    }

    #[test]
    fn sparse_prepass_collects_refs_even_with_memory_for_all_nodes() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("fixture.pbf");
        write_eager_fixture(&input, &[-7, 0, -7]);
        let (host, mode) = sparse_test_host(Some(8 << 30));
        embed_fixture(
            &host,
            &input,
            &dir.path().join("prepass.pbf"),
            true,
            true,
            false,
        )
        .unwrap();
        assert!(mode.collected_refs.load(Relaxed));
        assert_eq!(mode.all_nodes.load(Relaxed), 0);
    }

    /// A file without ways has an empty store, so even a node count far
    /// over any budget keeps sparse; with ways every node is charged.
    #[test]
    fn wayless_input_estimates_an_empty_store() {
        let budget = 8 * GB;
        let nodes = 10 * GB;
        assert_eq!(super::store_estimate_bytes(nodes, false), 0);
        assert_eq!(
            route_sorted_indexed(Some(super::store_estimate_bytes(nodes, false)), budget),
            ResolvedBackend::Sparse
        );
        assert_eq!(
            route_sorted_indexed(Some(super::store_estimate_bytes(nodes, true)), budget),
            ResolvedBackend::External
        );
    }

    /// A claimed count that overflows a statistic is refused, and the
    /// refused merge leaves the receiver as it was.
    #[test]
    fn stats_merge_refuses_overflow_and_leaves_receiver_intact() {
        let mut stats = super::Stats {
            nodes_read: 1,
            ways_written: 7,
            ..super::Stats::default()
        };
        let claim = super::Stats {
            nodes_read: u64::MAX,
            ways_written: 1,
            ..super::Stats::default()
        };
        crate::test_support::assert_err_kind!(
            stats.merge(&claim),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("overflow in nodes_read"),
        );
        assert_eq!((stats.nodes_read, stats.ways_written), (1, 7));
        stats
            .merge(&super::Stats {
                nodes_dropped: 2,
                ..super::Stats::default()
            })
            .expect("fits");
        assert_eq!(stats.nodes_dropped, 2);
    }

    /// The recheck uses the estimate route's threshold on the exact store,
    /// and takes extra anon off the budget: `store <= 80 % of (budget -
    /// anon)` stays sparse.
    #[test]
    fn recheck_judges_the_exact_store_and_charges_extra_anon() {
        let budget = 10 * GB;
        let threshold_slots = budget * AUTO_STORE_BUDGET_PCT / 100 / 8;
        assert!(!recheck_routes_external(threshold_slots, 0, budget).0);
        assert!(recheck_routes_external(threshold_slots + 1, 0, budget).0);
        // 1 GB of anon off a 10 GB budget leaves 7.2 GB for the store.
        let anon = u128::from(GB);
        let fits = 7_200_000_000 / 8;
        assert!(!recheck_routes_external(fits, anon, budget).0);
        assert!(recheck_routes_external(fits + 1, anon, budget).0);
        assert_eq!(recheck_routes_external(3, 0, 1).1, 24);
        assert!(
            recheck_routes_external(u64::MAX, u128::from(u64::MAX), u64::MAX).0,
            "widened arithmetic: the largest store never wraps to sparse"
        );
        assert!(!recheck_routes_external(0, 0, 0).0);
    }

    /// The store sizes and budget below are the measured pair the threshold
    /// was chosen between: the smaller store ran linear on sparse, the larger
    /// thrashed. The measurements live in `reference/performance/add-locations-to-ways.md`; these
    /// values are fixed test inputs, not a claim about today's datasets.
    #[test]
    fn route_reproduces_the_measured_grid() {
        let budget = 25 * GB;
        assert_eq!(
            route_sorted_indexed(Some(18_750_000_000), budget),
            ResolvedBackend::Sparse,
            "north-america-shaped store must route sparse"
        );
        assert_eq!(
            route_sorted_indexed(Some(28_940_000_000), budget),
            ResolvedBackend::External,
            "europe-shaped store must route external"
        );
    }

    /// Known memory above the allowance routes on the store; unknown memory,
    /// or memory at or below the allowance, yields no budget, which
    /// `auto_select_backend` turns into sparse without an estimate walk.
    #[test]
    fn budget_follows_available_memory() {
        let store = Some(18_750_000_000);
        for (memory, expected) in [
            (32 * GB, ResolvedBackend::Sparse),
            (8 * GB, ResolvedBackend::External),
            (AUTO_ANON_HEADROOM_BYTES + 1, ResolvedBackend::External),
        ] {
            let budget = page_cache_budget_bytes(Some(memory)).expect("above the allowance");
            assert_eq!(route_sorted_indexed(store, budget), expected, "{memory}");
        }
        for memory in [None, Some(0), Some(AUTO_ANON_HEADROOM_BYTES)] {
            assert_eq!(
                page_cache_budget_bytes(memory),
                None,
                "{memory:?} cannot justify leaving sparse"
            );
        }
        assert_eq!(
            page_cache_budget_bytes(Some(AUTO_ANON_HEADROOM_BYTES + 1)),
            Some(1)
        );
    }

    #[test]
    fn route_threshold_is_strictly_greater_than() {
        let budget = 10 * GB;
        let threshold = budget * AUTO_STORE_BUDGET_PCT / 100;
        assert_eq!(
            route_sorted_indexed(Some(threshold), budget),
            ResolvedBackend::Sparse,
            "exactly at the threshold stays sparse"
        );
        assert_eq!(
            route_sorted_indexed(Some(threshold + 1), budget),
            ResolvedBackend::External,
            "one byte over the threshold routes external"
        );
    }

    /// An unknown store keeps sparse: external is chosen only on positive
    /// evidence.
    #[test]
    fn route_unknown_store_keeps_sparse() {
        assert_eq!(route_sorted_indexed(None, 25 * GB), ResolvedBackend::Sparse);
        assert_eq!(route_sorted_indexed(None, 1), ResolvedBackend::Sparse);
    }

    #[test]
    fn route_survives_extreme_inputs_without_overflow() {
        assert_eq!(
            route_sorted_indexed(Some(u64::MAX), 25 * GB),
            ResolvedBackend::External,
            "a store big enough to overflow the pct math must saturate \
             toward external, never wrap into a sparse verdict"
        );
        assert_eq!(
            route_sorted_indexed(Some(0), 1),
            ResolvedBackend::Sparse,
            "zero store is never greater-than"
        );
    }
}
