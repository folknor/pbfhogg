//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

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
use crate::read::input::PinnedInput;

use super::Result;
use crate::error::Error;

use self::passthrough::write_output_passthrough;
use self::sparse::{SparseArrayIndex, build_node_index_sparse};

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

/// What ALTW states about its output: input order, and every way written with
/// inline coordinates (plus the injected prepass fields when asked).
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
fn relation_member_ids(
    block: &PrimitiveBlock,
    want_nodes: bool,
    want_ways: bool,
    mut emit: impl FnMut(bool, i64),
) {
    for element in block.elements_skip_metadata() {
        if let Element::Relation(r) = element {
            let complete = r
                .tags()
                .any(|(key, value)| key == "type" && matches!(value, "multipolygon" | "boundary"));
            for member in r.members() {
                match member.id {
                    MemberId::Node(id) if want_nodes && id >= 0 => emit(false, id),
                    MemberId::Way(id) if want_ways && complete && id >= 0 => emit(true, id),
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Index type selection
// ---------------------------------------------------------------------------

/// Strategy for storing node coordinates during add-locations-to-ways.
///
/// `Sparse` is the default, and the docs recommend no backend over it.
/// Sparse took the default when `dense` was removed; `Auto` stays opt-in
/// because its threshold is bracketed by only two measured inputs on one
/// host, and when an estimate is unavailable (every non-Linux target among
/// others) it routes sorted, indexed input to external, whose scratch-disk
/// demand a user who never asked for external would not expect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexType {
    /// Rank-indexed flat sparse array, sized at `referenced.total_count() * 8`
    /// bytes. Workers store coordinates with relaxed `AtomicU64` mmap stores
    /// at byte offset `IdSet::rank_if_set(node_id) << 3` (not pwrites, so no
    /// syscall per tuple). Works on any PBF. Runtime and page-cache behavior
    /// depend on the input and host; see `reference/performance.md` for
    /// measurements.
    #[default]
    Sparse,
    /// External join via double radix permutation. Uses bounded working
    /// memory and sequential I/O, and requires sorted input with indexdata.
    /// Its temporary-disk demand depends on the input; see
    /// `reference/performance.md` for measurements.
    External,
    /// Selects a backend at runtime. Unsorted or unindexed input uses sparse.
    /// For sorted, indexed input, external is selected when the estimated
    /// sparse store exceeds a fixed share of the page-cache budget; if
    /// either estimate is unavailable, external is selected. See
    /// `reference/pipeline.md` for the routing rule and
    /// `reference/performance.md` for measurements.
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

#[cfg(not(test))]
type NodeIndex = SparseArrayIndex;

// Keep the reframe wire-format tests independent of an mmap-backed index.
#[cfg(test)]
enum NodeIndex {
    Sparse(SparseArrayIndex),
    InMemory(std::collections::HashMap<i64, (i32, i32)>),
}

#[cfg(test)]
impl NodeIndex {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Sparse(index) => index.get(node_id),
            Self::InMemory(map) => map.get(&node_id).copied(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
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
    pub fn merge(&mut self, src: &Stats) {
        self.nodes_read += src.nodes_read;
        self.nodes_written += src.nodes_written;
        self.nodes_dropped += src.nodes_dropped;
        self.ways_written += src.ways_written;
        self.relations_written += src.relations_written;
        self.missing_locations += src.missing_locations;
        self.blobs_passthrough += src.blobs_passthrough;
        self.blobs_decoded += src.blobs_decoded;
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
/// Two-pass algorithm:
/// 1. Read all nodes and build a coordinate index.
/// 2. Re-read the input and write to output, attaching coordinates to ways.
///
/// If `keep_untagged_nodes` is false, nodes with zero tags are omitted from
/// the output (their coordinates are still used for ways).
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)): where a later
/// open cannot reach the pinned file, an input replaced mid-run fails with
/// `InputReplaced`.
pub fn add_locations_to_ways(
    input: &Path,
    output: crate::output::Output,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<Stats> {
    host.cancel.check()?;
    let result = add_locations_to_ways_to(input, &output, options, overrides, host, &scratch)?;
    drop(scratch);
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`add_locations_to_ways`] writing to a borrowed output it does not publish.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(crate) fn add_locations_to_ways_to(
    input: &Path,
    output: &crate::output::Output,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
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
    let pin = PinnedInput::pin(input, host)?;
    let input = &pin;
    // Every precondition that depends on the input (sorted, indexed, the
    // header) is answered by this one front read, whichever backend runs.
    // The header walks that follow are separate and deliberate (see
    // `passthrough::build_schedule`). Scratch free space is not judged here:
    // both backends size their scratch from counts only their first pass
    // learns (sparse from the referenced-node set, external from stage 1),
    // and check it then.
    let mut front = InputFront::open(input, options.write.direct_reads, host)?;
    // Auto-select: sparse unless the input qualifies for external AND the
    // estimated store exceeds the page-cache budget (see IndexType::Auto).
    let backend = match options.index_type {
        IndexType::Sparse => ResolvedBackend::Sparse,
        IndexType::External => ResolvedBackend::External,
        IndexType::Auto => auto_select_backend(input, &mut front, host)?,
    };

    // External join has its own pipeline - dispatch early.
    if backend == ResolvedBackend::External {
        let stats = external::external_join_in(
            input,
            front,
            output,
            options.keep_untagged_nodes,
            &options.write,
            overrides,
            options.inject_prepass,
            host,
            scratch,
        )?;
        enforce_missing_locations_policy(&stats, options.ignore_missing_nodes)?;
        return Ok(stats);
    }

    let indexdata_present = front.require_indexdata(
        options.write.force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed and re-encoded (significantly slower).",
        host,
    )?;

    // Suggest external index for sorted indexed PBFs on *explicit* sparse
    // selection only. When auto resolved to sparse it did so because the
    // store fits the page-cache budget; hinting external there would
    // contradict the routing it just explained on stderr.
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

    // Pass 0: collect the set of node IDs referenced by ways. Only these
    // nodes need coordinate lookups, so only these get indexed. The store
    // therefore follows the referenced-node working set rather than every
    // node in the input; see `reference/performance.md` for measurements.
    crate::debug::emit_marker("ALTW_PASS0_START");
    let (referenced, shared) = collect_way_referenced_node_ids(
        input,
        options.write.direct_reads,
        options.inject_prepass,
        host,
    )?;
    crate::debug::emit_marker("ALTW_PASS0_END");
    // Cheap one-shot iter().count(); surfaces the way-ref IdSet size before
    // the index build so the counter survives SIGKILL during pass 1.
    crate::debug::emit_counter("altw_referenced_node_ids", referenced.iter().count());
    crate::debug::emit_counter("altw_index_kind", 1);

    crate::debug::emit_marker("ALTW_PASS1_START");
    let index =
        build_node_index_sparse(input, options.write.direct_reads, scratch, referenced, host)?;
    #[cfg(test)]
    let index = NodeIndex::Sparse(index);
    crate::debug::emit_marker("ALTW_PASS1_END");

    let relation_scan = if options.keep_untagged_nodes && !options.inject_prepass {
        (None, None)
    } else {
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_START");
        let ids = collect_relation_member_ids(
            input,
            options.write.direct_reads,
            !options.keep_untagged_nodes,
            options.inject_prepass,
            host,
        )?;
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_END");
        crate::debug::emit_counter(
            "altw_relation_member_node_ids",
            ids.0.as_ref().map_or(0, |set| set.iter().count()),
        );
        ids
    };
    crate::debug::emit_marker("ALTW_PASS2_START");
    let inject_metrics = inject_metrics::InjectMetrics::default();
    let stats = write_output_checked(
        input,
        output,
        &index,
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
    crate::debug::emit_marker("ALTW_PASS2_END");
    emit_stats_counters(&stats);
    if options.inject_prepass {
        let member_ways = relation_scan
            .1
            .as_ref()
            .map_or(0, |set| set.iter().count() as u64);
        inject_metrics.emit(member_ways);
    }
    enforce_missing_locations_policy(&stats, options.ignore_missing_nodes)?;
    Ok(stats)
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
/// The check runs after the write rather than mid-stream because the
/// external backend only learns its resolved count when its final stage
/// completes; a single post-pass gate is the one place that provably covers
/// all three backends. The output written in the meantime is never published:
/// it sits at the [`Output`](crate::output::Output)'s staging path, and
/// [`add_locations_to_ways`] publishes only after this gate passes, so no
/// caller is left holding a file of fabricated coordinates.
///
/// The message deliberately does not name the output path: nothing is
/// published, so there is no output to point at. The error is the typed
/// [`ErrorKind::MissingNodeLocations`](crate::ErrorKind::MissingNodeLocations);
/// how to allow the placeholders is the front end's advice.
fn enforce_missing_locations_policy(stats: &Stats, ignore_missing_nodes: bool) -> Result<()> {
    if stats.missing_locations == 0 || ignore_missing_nodes {
        return Ok(());
    }
    Err(crate::error::new_missing_node_locations(
        stats.missing_locations,
    ))
}

// Auto-select: sparse unless the input qualifies for external (sorted +
// indexed) AND the estimated sparse store exceeds the page-cache budget.
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
fn auto_select_backend(
    input: &PinnedInput,
    front: &mut InputFront<'_>,
    host: &crate::host::Host,
) -> Result<ResolvedBackend> {
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
        return Ok(ResolvedBackend::Sparse);
    }

    // Both backends can run: route on scale. The walk reads headers the
    // selected backend will also need, so it can warm those pages for the
    // following pass rather than adding another cold header walk.
    crate::debug::emit_marker("ALTW_AUTO_ESTIMATE_WALK_START");
    let store = estimate_sparse_store_bytes(input, host)?;
    crate::debug::emit_marker("ALTW_AUTO_ESTIMATE_WALK_END");
    let budget = page_cache_budget_bytes(host);
    let chosen = route_sorted_indexed(store, budget);

    crate::debug::emit_counter(
        "altw_auto_store_estimate_bytes",
        store.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
    );
    crate::debug::emit_counter(
        "altw_auto_cache_budget_bytes",
        budget.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
    );

    if let (Some(store), Some(budget)) = (store, budget) {
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
             store estimate available={}, memory budget available={})",
            store.is_some(),
            budget.is_some()
        );
    }
    Ok(chosen)
}

/// Pure routing decision for an input both backends can handle (sorted +
/// indexed). External iff the estimated store exceeds
/// [`AUTO_STORE_BUDGET_PCT`] of the page-cache budget. An unavailable
/// store or budget estimate routes external, preserving a conservative
/// choice when the sparse working set cannot be estimated: misrouting a
/// small input to external costs less than running an oversized input on
/// sparse into page-cache thrashing.
fn route_sorted_indexed(store_bytes: Option<u64>, budget_bytes: Option<u64>) -> ResolvedBackend {
    match (store_bytes, budget_bytes) {
        (Some(store), Some(budget)) => {
            if store.saturating_mul(100) > budget.saturating_mul(AUTO_STORE_BUDGET_PCT) {
                ResolvedBackend::External
            } else {
                ResolvedBackend::Sparse
            }
        }
        _ => ResolvedBackend::External,
    }
}

/// Estimate the sparse store size for an indexed input: total node count
/// from per-blob indexdata, times 8 bytes per rank slot. Total nodes is
/// an upper bound on referenced nodes (the store is sized by
/// way-referenced ids only) - tight when most nodes are way-referenced and
/// looser when that share is lower, which errs toward external, the cheap
/// direction. Returns
/// `None` when the first OsmData blob lacks indexdata, since the sum would
/// silently undercount; an input that is indexed on its first OsmData blob
/// and not on a later one is refused by the walk.
fn estimate_sparse_store_bytes(
    input: &PinnedInput,
    host: &crate::host::Host,
) -> Result<Option<u64>> {
    let mut walker =
        crate::read::header_walker::HeaderWalker::open(input, host)?.refusing_partial_index();
    let mut node_count: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if meta.blob_type != crate::blob::BlobKind::OsmData {
            continue;
        }
        match meta.index.as_ref() {
            Some(idx) => {
                if idx.kind == crate::ElementKind::Node {
                    node_count = node_count.saturating_add(idx.count);
                }
            }
            None => return Ok(None),
        }
    }
    Ok(Some(node_count.saturating_mul(8)))
}

/// Page-cache budget for the sparse store: the host's available memory
/// (`MachineProbe`: `MemAvailable`, capped on Linux by visible cgroup v2
/// headroom) minus the anon headroom the sparse run itself needs. The caller
/// owns the memory facts: fixed resources can route on any platform without
/// procfs. Unknown memory retains the conservative external fallback
/// documented in reference/pipeline.md; small input alone cannot establish
/// that its random-access store fits RAM.
fn page_cache_budget_bytes(host: &crate::host::Host) -> Option<u64> {
    host.resources
        .memory_available()
        .map(|b| b.saturating_sub(AUTO_ANON_HEADROOM_BYTES))
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
        pub(super) fn emit(&self, member_ways: u64) {
            crate::debug::emit_counter("altw_member_ways", member_ways);
            self.emit_all();
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

fn emit_stats_counters(stats: &Stats) {
    crate::debug::emit_counter("altw_nodes_read", stats.nodes_read);
    crate::debug::emit_counter("altw_nodes_written", stats.nodes_written);
    crate::debug::emit_counter("altw_nodes_dropped", stats.nodes_dropped);
    crate::debug::emit_counter("altw_ways_written", stats.ways_written);
    crate::debug::emit_counter("altw_relations_written", stats.relations_written);
    crate::debug::emit_counter("altw_missing_locations", stats.missing_locations);
    crate::debug::emit_counter("altw_blobs_passthrough", stats.blobs_passthrough);
    crate::debug::emit_counter("altw_blobs_decoded", stats.blobs_decoded);
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

/// Collect all node IDs referenced by ways (pass 0).
///
/// Uses `build_classify_schedule(Way)` to obtain a way-only blob schedule
/// via the pread-only `HeaderWalker`, then fans work out to
/// `parallel_scan_blobs_raw` workers. Each worker decompresses one blob
/// and walks the wire format directly via `scan_way_refs` - skipping
/// `PrimitiveBlock` construction (no StringTable parse, no
/// `(u32, u32)` group_ranges allocation). The main thread unions
/// per-blob `Vec<i64>`s into a single `IdSet`.
///
/// Per-blob refs are emitted into fresh `Vec<i64>`s that the main thread
/// consumes and drops immediately after the union, so transient worker-side
/// memory stays bounded to in-flight blob refs rather than all completed
/// blobs. Store size and peak memory depend on the input; see
/// `reference/performance.md` for measurements.
///
/// `direct_io` is intentionally dropped on this path: the blob bodies are
/// pread from the shared file handle on worker threads, which is
/// incompatible with `O_DIRECT`'s alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_way_referenced_node_ids(
    input: &PinnedInput,
    _direct_io: bool,
    inject_prepass: bool,
    host: &crate::host::Host,
) -> Result<(IdSet, Option<IdSet>)> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::ElementKind::Way),
        crate::scan::classify::PartialIndex::Refuse,
        host,
    )?;
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
    // Union-side attribution: the drain below runs on the main thread
    // and is a suspected pass-0 bottleneck at scale. Timed per drained
    // blob so `altw_pass0_union_ms` vs the phase wall separates
    // serial-union cost from worker scan cost.
    let mut union_ns: u64 = 0;
    let mut union_refs_total: u64 = 0;
    crate::scan::classify::parallel_scan_blobs_raw(
        &shared_file,
        &schedule,
        None,
        || (Vec::<i64>::new(), Vec::<(usize, usize)>::new()),
        |_position, decompressed, (refs_buf, group_starts)| {
            let mut refs_vec: Vec<i64> = Vec::new();
            crate::scan::way::scan_way_refs(
                decompressed,
                refs_buf,
                group_starts,
                |_way_id, refs| {
                    let refs = if refs.len() >= 4 && refs.first() == refs.last() {
                        &refs[..refs.len() - 1]
                    } else {
                        refs
                    };
                    for &node_id in refs {
                        if node_id >= 0 {
                            refs_vec.push(node_id);
                        }
                    }
                },
            )?;
            Ok(refs_vec)
        },
        |_seq, refs_vec| {
            let t_union = crate::counters::Stopwatch::start();
            union_refs_total += refs_vec.len() as u64;
            // Split on `shared` rather than branching per ref: the plain
            // path must not pay the shared-detection `get`, which only
            // exists to feed prepass injection. Branching inside the loop
            // was measured at ~2.3 ns/ref on every ref regardless of the
            // flag (union 6.6 ns/ref vs 4.3), since `get` is a real probe of
            // a large set and the branch predictor cannot elide it.
            match &mut shared {
                Some(shared) => {
                    for &node_id in &refs_vec {
                        if referenced.get(node_id) {
                            shared.set(node_id);
                        } else {
                            referenced.set(node_id);
                        }
                    }
                }
                None => {
                    for &node_id in &refs_vec {
                        referenced.set(node_id);
                    }
                }
            }
            union_ns += t_union.elapsed_ns();
        },
        host,
    )?;
    crate::debug::emit_counter("altw_pass0_union_ms", union_ns / 1_000_000);
    crate::debug::emit_counter("altw_pass0_refs_total", union_refs_total);
    if let Some(shared_set) = &shared {
        crate::debug::emit_counter("altw_pass0_shared_node_ids", shared_set.iter().count());
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
    host: &crate::host::Host,
) -> Result<(Option<IdSet>, Option<IdSet>)> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::ElementKind::Relation),
        crate::scan::classify::PartialIndex::Refuse,
        host,
    )?;
    let mut member_node_ids = want_nodes.then(IdSet::new);
    let mut member_way_ids = want_ways.then(IdSet::new);
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _state| {
            let mut ids: Vec<(bool, i64)> = Vec::new();
            relation_member_ids(block, want_nodes, want_ways, |way, id| {
                ids.push((way, id));
            });
            Ok(ids)
        },
        |_seq, ids| {
            for (way, id) in ids {
                // A kind that was not requested has no set and nothing to
                // record.
                let set = if way {
                    &mut member_way_ids
                } else {
                    &mut member_node_ids
                };
                if let Some(set) = set {
                    set.set(id);
                }
            }
        },
        host,
    )?;
    Ok((member_node_ids, member_way_ids))
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_output_checked(
    input: &PinnedInput,
    output: &crate::output::Output,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    inject_metrics: &inject_metrics::InjectMetrics,
    write: &WriteOptions,
    indexdata_present: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
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
        write_output_passthrough(
            input,
            output,
            index,
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
        write_output_decode_all(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            write,
            overrides,
            host,
        )
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &PinnedInput,
    output: &crate::output::Output,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
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
                index,
                keep_untagged_nodes,
                relation_member_node_ids,
                &mut refs_buf,
                &mut locations_buf,
            )?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, block_stats))
        },
        |(blocks, block_stats)| {
            for block in blocks {
                writer.write_primitive_block_owned(block)?;
            }
            stats.merge(&block_stats);
            decoded_blocks += 1;
            if decoded_blocks.is_multiple_of(64) {
                crate::debug::emit_counter("altw_pass2_blocks", decoded_blocks);
            }
            Ok(())
        },
    )?;
    crate::debug::emit_counter("altw_pass2_blocks", decoded_blocks);

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Per-block fused transform
// ---------------------------------------------------------------------------

use super::flush_local;
use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};

/// Process a single `PrimitiveBlock`, writing elements into the thread-local
/// `BlockBuilder` and flushing complete blocks into `output`.
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
    node_index: &NodeIndex,
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
                    let meta = dense_node_metadata(dn)?;
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
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
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
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
                bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                relation_members_into(r, &mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::{AUTO_STORE_BUDGET_PCT, ResolvedBackend, route_sorted_indexed};

    const GB: u64 = 1_000_000_000;

    /// The store sizes and budget below are the measured pair the threshold
    /// was chosen between: the smaller store ran linear on sparse, the larger
    /// thrashed. The measurements live in `reference/performance.md`; these
    /// values are fixed test inputs, not a claim about today's datasets.
    #[test]
    fn route_reproduces_the_measured_grid() {
        let budget = Some(25 * GB);
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

    #[test]
    fn route_uses_injected_memory_budget() {
        let mut host = crate::host::Host::for_tests();
        for (memory, expected) in [
            (Some(32 * GB), ResolvedBackend::Sparse),
            (Some(8 * GB), ResolvedBackend::External),
            (Some(0), ResolvedBackend::External),
            (None, ResolvedBackend::External),
        ] {
            host.resources = std::sync::Arc::new(crate::host::Fixed {
                parallelism: None,
                memory_available: memory,
            });
            assert_eq!(
                route_sorted_indexed(Some(18_750_000_000), super::page_cache_budget_bytes(&host)),
                expected,
            );
        }
    }

    #[test]
    fn route_threshold_is_strictly_greater_than() {
        let budget = 10 * GB;
        let threshold = budget * AUTO_STORE_BUDGET_PCT / 100;
        assert_eq!(
            route_sorted_indexed(Some(threshold), Some(budget)),
            ResolvedBackend::Sparse,
            "exactly at the threshold stays sparse"
        );
        assert_eq!(
            route_sorted_indexed(Some(threshold + 1), Some(budget)),
            ResolvedBackend::External,
            "one byte over the threshold routes external"
        );
    }

    /// Unknown store or budget routes external for sorted + indexed input,
    /// since the sparse working set cannot be compared with the available
    /// page-cache budget.
    #[test]
    fn route_unknowns_fall_back_to_external() {
        assert_eq!(
            route_sorted_indexed(None, Some(25 * GB)),
            ResolvedBackend::External
        );
        assert_eq!(
            route_sorted_indexed(Some(GB), None),
            ResolvedBackend::External
        );
        assert_eq!(route_sorted_indexed(None, None), ResolvedBackend::External);
    }

    #[test]
    fn route_survives_extreme_inputs_without_overflow() {
        assert_eq!(
            route_sorted_indexed(Some(u64::MAX), Some(25 * GB)),
            ResolvedBackend::External,
            "a store big enough to overflow the pct math must saturate \
             toward external, never wrap into a sparse verdict"
        );
        assert_eq!(
            route_sorted_indexed(Some(0), Some(0)),
            ResolvedBackend::Sparse,
            "zero store on a zero budget is not greater-than"
        );
    }
}
