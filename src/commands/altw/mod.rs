//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

pub mod external;

// Under the `test-hooks` feature, expose external-join fault-injection
// hooks so integration tests can arm them.
#[cfg(feature = "test-hooks")]
pub mod external_test_hooks {
    pub use super::external::test_hooks::stage3;
}
mod passthrough;
mod reframe;
mod sparse;

use std::path::Path;
use std::str::FromStr;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, ElementReader, MemberId, PrimitiveBlock};

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, require_indexdata, writer_from_header, writer_from_header_parallel,
};
use crate::idset::IdSet;

use super::Result;

use self::passthrough::write_output_passthrough;
use self::sparse::{SparseArrayIndex, build_node_index_sparse};

// ---------------------------------------------------------------------------
// Index type selection
// ---------------------------------------------------------------------------

/// Strategy for storing node coordinates during add-locations-to-ways.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexType {
    /// Rank-indexed flat sparse array. Pre-allocates
    /// `referenced.total_count() * 8` bytes (~29 GB at europe, ~60 GB at
    /// planet); workers store coords via relaxed `AtomicU64` mmap stores
    /// at byte offset `IdSet::rank_if_set(node_id) << 3` (not pwrites -
    /// no syscall per tuple; the distinction matters when evaluating
    /// write-path alternatives). Fast at small / medium scale,
    /// survives europe at ~6 minutes on a 27 GB-RAM host. Likely thrashes
    /// at planet (working set exceeds free page cache); use `external`
    /// for planet.
    #[default]
    Sparse,
    /// External join via double radix permutation. Bounded memory
    /// (~8.7 GB at planet), all sequential I/O. Uses ~224 GB temp disk at
    /// planet scale. The only mode that survives at planet on a
    /// memory-constrained host.
    External,
    /// Auto-select on scale. Sparse unless the input is sorted + indexed
    /// (external's precondition) AND the estimated sparse store exceeds
    /// the host's page-cache budget - the measured knee where sparse
    /// pass 2 goes nonlinear (5.3x per-ref cost at 116 % of budget vs
    /// linear at 75 %). Unsorted or unindexed inputs always route sparse;
    /// when the store or budget cannot be estimated, sorted + indexed
    /// inputs route external (the cheap direction to be wrong in:
    /// misrouting a small input to external wastes seconds, an oversized
    /// input on sparse costs minutes).
    Auto,
}

/// Configuration for [`add_locations_to_ways`].
#[derive(Clone, Copy, Debug)]
pub struct AltwOptions {
    pub keep_untagged_nodes: bool,
    pub compression: Compression,
    pub direct_io: bool,
    pub force: bool,
    pub index_type: IndexType,
    /// Emit the private WayMembers-v1 and SharedNodePins-v1 prepass metadata.
    pub inject_prepass: bool,
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
            "dense" => Err(ParseIndexTypeError(
                "index type 'dense' was removed in favor of 'sparse'. Sparse \
                 (rank-indexed flat) is faster than dense at every measured \
                 scale and works in regimes dense doesn't. Use \
                 --index-type sparse instead."
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
// Unified node index
// ---------------------------------------------------------------------------

/// Unified node coordinate index. Currently a single-variant enum because
/// `external` builds its own coord representation inline (stage 4 reads
/// `coord_payloads` directly, never instantiates `NodeIndex`); `auto`
/// resolves to either Sparse (here) or External (separate path) before
/// the build step. The enum stays as a future-proofing shape - a follow-up
/// shrink encoding (planet-scale, ~17 GB working set) would add a variant
/// rather than reshape the dispatch.
enum NodeIndex {
    Sparse(SparseArrayIndex),
}

impl NodeIndex {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Sparse(idx) => idx.get(node_id),
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
    pub fn print_summary(&self) {
        eprintln!(
            "add-locations-to-ways: {} nodes read, {} written, {} dropped, \
             {} ways, {} relations, {} missing locations",
            self.nodes_read,
            self.nodes_written,
            self.nodes_dropped,
            self.ways_written,
            self.relations_written,
            self.missing_locations,
        );
        if self.blobs_passthrough > 0 {
            eprintln!(
                "  Blobs: {} passthrough, {} decoded",
                self.blobs_passthrough, self.blobs_decoded,
            );
        }
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
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub fn add_locations_to_ways(
    input: &Path,
    output: &Path,
    options: &AltwOptions,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    // Auto-select: sparse unless the input qualifies for external AND the
    // estimated store exceeds the page-cache budget (see IndexType::Auto).
    let index_type = if options.index_type == IndexType::Auto {
        auto_select_index_type(input, options.direct_io)?
    } else {
        options.index_type
    };

    // External join has its own pipeline - dispatch early.
    if index_type == IndexType::External {
        return external::external_join(
            input,
            output,
            options.keep_untagged_nodes,
            options.compression,
            options.direct_io,
            options.force,
            overrides,
            options.inject_prepass,
        );
    }

    let indexdata_present = require_indexdata(
        input,
        options.direct_io,
        options.force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed and re-encoded (significantly slower).",
    )?;

    // Suggest external index for sorted indexed PBFs on *explicit* sparse
    // selection only. When auto resolved to sparse it did so because the
    // store fits the page-cache budget; hinting external there would
    // contradict the routing it just explained on stderr.
    if options.index_type == IndexType::Sparse && indexdata_present {
        let reader = crate::ElementReader::open(input, options.direct_io)?;
        if reader.header().is_sorted() {
            eprintln!(
                "hint: this sorted indexed PBF is eligible for --index-type external, \
                 which uses bounded memory and sequential I/O. External wins at large \
                 scale (europe and up) and is the only mode that survives at planet on \
                 memory-constrained hosts; sparse is typically faster below that."
            );
        }
    }

    let scratch_dir = output.parent().unwrap_or(Path::new("."));

    // Pass 0: collect the set of node IDs referenced by ways. Only these
    // nodes need coordinate lookups, so only these get indexed. At planet
    // scale this reduces touched mmap pages from ~80 GB to ~16 GB.
    crate::debug::emit_marker("ALTW_PASS0_START");
    let (referenced, shared) =
        collect_way_referenced_node_ids(input, options.direct_io, options.inject_prepass)?;
    crate::debug::emit_marker("ALTW_PASS0_END");
    // Cheap one-shot iter().count(); surfaces the way-ref IdSet size before
    // the index build so the counter survives SIGKILL during pass 1.
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "altw_referenced_node_ids",
            i64::try_from(referenced.iter().count()).unwrap_or(i64::MAX),
        );
        crate::debug::emit_counter(
            "altw_index_kind",
            match index_type {
                IndexType::Sparse => 1,
                IndexType::External | IndexType::Auto => i64::MIN,
            },
        );
    }

    crate::debug::emit_marker("ALTW_PASS1_START");
    let index = build_node_index(
        input,
        options.direct_io,
        scratch_dir,
        referenced,
        index_type,
    )?;
    crate::debug::emit_marker("ALTW_PASS1_END");

    let relation_scan = if options.keep_untagged_nodes && !options.inject_prepass {
        (None, None)
    } else {
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_START");
        let ids = collect_relation_member_ids(
            input,
            options.direct_io,
            !options.keep_untagged_nodes,
            options.inject_prepass,
        )?;
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_END");
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter(
                "altw_relation_member_node_ids",
                i64::try_from(ids.0.as_ref().map_or(0, |set| set.iter().count()))
                    .unwrap_or(i64::MAX),
            );
        }
        ids
    };
    crate::debug::emit_marker("ALTW_PASS2_START");
    let stats = write_output_checked(
        input,
        output,
        &index,
        options.keep_untagged_nodes,
        relation_scan.0.as_ref(),
        relation_scan.1.as_ref(),
        shared.as_ref(),
        options.inject_prepass,
        options.compression,
        options.direct_io,
        indexdata_present,
        overrides,
    )?;
    crate::debug::emit_marker("ALTW_PASS2_END");
    emit_stats_counters(&stats);
    if options.inject_prepass {
        let member_ways = relation_scan
            .1
            .as_ref()
            .map_or(0, |set| set.iter().count() as u64);
        inject_metrics::emit(member_ways);
    }
    Ok(stats)
}

/// Route to external when the estimated sparse store exceeds this many
/// percent of the page-cache budget. The knee is bracketed, not
/// pinpointed: north-america's 18.75 GB store (~75 % of the measuring
/// host's ~25 GB budget) ran linear while europe's 28.94 GB (~116 %)
/// thrashed at 5.3x the per-ref cost, and 80 % is the
/// bias-toward-external choice inside that gap (notes/altw.md P1).
const AUTO_STORE_BUDGET_PCT: u64 = 80;

/// Anon footprint a sparse run holds outside the mmap store (IdSet, decode
/// buffers, writer queues): ~2-3 GB measured at europe. Subtracted from
/// `MemAvailable` before applying [`AUTO_STORE_BUDGET_PCT`], so the budget
/// reflects what the page cache can actually keep resident for the store.
const AUTO_ANON_HEADROOM_BYTES: u64 = 3 << 30;

// Auto-select: sparse unless the input qualifies for external (sorted +
// indexed) AND the estimated sparse store exceeds the page-cache budget.
// The threshold is relative to runtime RAM, never a hardcoded byte
// constant: the knee is defined by store-vs-cache, and a constant fitted
// to one host misroutes every other machine (notes/altw.md P1).
fn auto_select_index_type(input: &Path, direct_io: bool) -> Result<IndexType> {
    let reader = crate::ElementReader::open(input, direct_io)?;
    let sorted = reader.header().is_sorted();
    drop(reader);
    // Check indexdata presence without erroring (peek at first blob).
    let has_index = (|| -> Option<bool> {
        let mut r = crate::blob::BlobReader::open(input, direct_io).ok()?;
        r.set_parse_indexdata(true);
        r.next()?.ok()?; // skip header
        let blob = r.next()?.ok()?;
        Some(blob.index().is_some())
    })()
    .unwrap_or(false);

    if !(sorted && has_index) {
        // External cannot run (it requires sorted + indexdata); no
        // estimate needed.
        eprintln!("auto-selected --index-type sparse (sorted={sorted}, indexed={has_index})");
        return Ok(IndexType::Sparse);
    }

    // Both backends can run: route on scale. The walk is priced like any
    // other header walk (~50 us/blob cold, ~free warm) and self-amortizes:
    // whichever backend runs next starts with its own header walk, which
    // now rides warm on this one, so the command's total cold-walk count
    // does not increase (the P4 arithmetic, notes/altw.md).
    crate::debug::emit_marker("ALTW_AUTO_ESTIMATE_WALK_START");
    let store = estimate_sparse_store_bytes(input)?;
    crate::debug::emit_marker("ALTW_AUTO_ESTIMATE_WALK_END");
    let budget = page_cache_budget_bytes();
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
        let rel = if chosen == IndexType::External {
            ">"
        } else {
            "<="
        };
        eprintln!(
            "auto-selected --index-type {chosen} (store estimate {:.1} GB {rel} \
             {AUTO_STORE_BUDGET_PCT} % of {:.1} GB page-cache budget)",
            store as f64 / 1e9,
            budget as f64 / 1e9,
        );
    } else {
        eprintln!(
            "auto-selected --index-type {chosen} (sorted=true, indexed=true; \
             store or budget estimate unavailable)"
        );
    }
    Ok(chosen)
}

/// Pure routing decision for an input both backends can handle (sorted +
/// indexed). External iff the estimated store exceeds
/// [`AUTO_STORE_BUDGET_PCT`] of the page-cache budget. An unavailable
/// store or budget estimate routes external - the pre-scale-aware
/// behavior for these inputs, and the cheap direction to be wrong in
/// (seconds wasted vs minutes of pass-2 thrash).
fn route_sorted_indexed(store_bytes: Option<u64>, budget_bytes: Option<u64>) -> IndexType {
    match (store_bytes, budget_bytes) {
        (Some(store), Some(budget)) => {
            if store.saturating_mul(100) > budget.saturating_mul(AUTO_STORE_BUDGET_PCT) {
                IndexType::External
            } else {
                IndexType::Sparse
            }
        }
        _ => IndexType::External,
    }
}

/// Estimate the sparse store size for an indexed input: total node count
/// from per-blob indexdata, times 8 bytes per rank slot. Total nodes is
/// an upper bound on referenced nodes (the store is sized by
/// way-referenced ids only) - tight where the knee lives (95-98 % of
/// nodes are way-referenced at regional scale) and loose at planet
/// (~69 %), which errs toward external, the cheap direction. Returns
/// `None` when any OsmData blob lacks indexdata, since the sum would
/// silently undercount.
fn estimate_sparse_store_bytes(input: &Path) -> Result<Option<u64>> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let mut node_count: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if meta.blob_type != crate::blob::BlobKind::OsmData {
            continue;
        }
        match meta.index.as_ref() {
            Some(idx) => {
                if idx.kind == crate::blob_meta::ElemKind::Node {
                    node_count = node_count.saturating_add(idx.count);
                }
            }
            None => return Ok(None),
        }
    }
    Ok(Some(node_count.saturating_mul(8)))
}

/// Page-cache budget for the sparse store: `MemAvailable` minus the anon
/// headroom the sparse run itself needs. `None` when `/proc/meminfo` is
/// unavailable (non-Linux) or unparsable.
fn page_cache_budget_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(
        kb.saturating_mul(1024)
            .saturating_sub(AUTO_ANON_HEADROOM_BYTES),
    )
}

/// Process-global instrumentation for the injected-prepass producer.
///
/// The values feed brokkr sidecar counters only (a no-op without the FIFO),
/// so they are diagnostic and never surface in `Stats` or the CLI summary.
/// Both the sparse and external backends fold into these accumulators during
/// the parallel reframe and each emits once at the end of its run; the
/// counters start at zero per process, which matches brokkr's one-process-per
/// -run benchmark model.
pub(super) mod inject_metrics {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static PINNED_REFS: AtomicU64 = AtomicU64::new(0);
    static FIELD20_WAYS: AtomicU64 = AtomicU64::new(0);
    static FIELD5_BYTES: AtomicU64 = AtomicU64::new(0);

    /// Fold one way's field-20 pin bitmap: total set bits into `pinned_refs`,
    /// and one into `field20_ways` when the way emits a (non-empty) bitmap.
    pub(super) fn record_pins(pins: &[u8]) {
        let popcount: u32 = pins.iter().map(|b| b.count_ones()).sum();
        if popcount > 0 {
            PINNED_REFS.fetch_add(u64::from(popcount), Relaxed);
            FIELD20_WAYS.fetch_add(1, Relaxed);
        }
    }

    /// Fold one way blob's field-5 payload length.
    pub(super) fn record_field5_bytes(len: usize) {
        FIELD5_BYTES.fetch_add(len as u64, Relaxed);
    }

    /// Emit the four injected-prepass counters. `member_ways` is the size of
    /// the relation member-way IdSet, computed by the caller.
    #[allow(clippy::cast_possible_wrap)]
    pub(super) fn emit(member_ways: u64) {
        crate::debug::emit_counter("altw_member_ways", member_ways as i64);
        crate::debug::emit_counter("altw_pinned_refs", PINNED_REFS.load(Relaxed) as i64);
        crate::debug::emit_counter(
            "altw_field20_ways_emitted",
            FIELD20_WAYS.load(Relaxed) as i64,
        );
        crate::debug::emit_counter("altw_field5_bytes", FIELD5_BYTES.load(Relaxed) as i64);
    }
}

#[allow(clippy::cast_possible_wrap)]
fn emit_stats_counters(stats: &Stats) {
    crate::debug::emit_counter("altw_nodes_read", stats.nodes_read as i64);
    crate::debug::emit_counter("altw_nodes_written", stats.nodes_written as i64);
    crate::debug::emit_counter("altw_nodes_dropped", stats.nodes_dropped as i64);
    crate::debug::emit_counter("altw_ways_written", stats.ways_written as i64);
    crate::debug::emit_counter("altw_relations_written", stats.relations_written as i64);
    crate::debug::emit_counter("altw_missing_locations", stats.missing_locations as i64);
    crate::debug::emit_counter("altw_blobs_passthrough", stats.blobs_passthrough as i64);
    crate::debug::emit_counter("altw_blobs_decoded", stats.blobs_decoded as i64);
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

fn build_node_index(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: IdSet,
    index_type: IndexType,
) -> Result<NodeIndex> {
    match index_type {
        IndexType::Sparse => {
            // Sparse takes ownership: the rank-indexed flat layout
            // builds rank index on the IdSet and carries it through to
            // SparseArrayIndex so pass 2 lookups can do
            // rank_if_set(node_id) -> mmap read at rank * 8.
            build_node_index_sparse(input, direct_io, scratch_dir, referenced)
                .map(NodeIndex::Sparse)
        }
        IndexType::External | IndexType::Auto => unreachable!("resolved before build_node_index"),
    }
}

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
/// At planet scale (~2 B unique node refs) the union bitset costs ~1.6 GB.
/// Per-blob refs are emitted into fresh `Vec<i64>`s that the main thread
/// consumes and drops immediately after the union, so the transient
/// worker-side memory stays bounded to per-blob refs (~8 k ways × ~20
/// refs each × 8 B = ~1.3 MB) regardless of how many blobs have completed.
///
/// `direct_io` is intentionally dropped on this path: the blob bodies are
/// pread from the shared file handle on worker threads, which is
/// incompatible with `O_DIRECT`'s alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_way_referenced_node_ids(
    input: &Path,
    _direct_io: bool,
    inject_prepass: bool,
) -> Result<(IdSet, Option<IdSet>)> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Way),
    )?;
    let mut referenced = IdSet::new();
    let mut shared = inject_prepass.then(IdSet::new);
    // Union-side attribution: the drain below runs on the main thread
    // and is the suspected pass-0 wall at scale (~4.7 B set calls at
    // europe). Timed per drained blob so `altw_pass0_union_ms` vs the
    // phase wall separates serial-union cost from worker scan cost.
    let mut union_ns: u64 = 0;
    let mut union_refs_total: u64 = 0;
    crate::scan::classify::parallel_scan_blobs_raw(
        &shared_file,
        &schedule,
        None,
        || (Vec::<i64>::new(), Vec::<(usize, usize)>::new()),
        |decompressed, (refs_buf, group_starts)| {
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
            let t_union = std::time::Instant::now();
            union_refs_total += refs_vec.len() as u64;
            // Split on `shared` rather than branching per ref: the plain
            // path must not pay the shared-detection `get`, which only
            // exists to feed prepass injection. Branching inside the loop
            // cost ~2.3 ns/ref on every ref regardless of the flag
            // (union 6.6 ns/ref vs 4.3), since `get` is a real probe of a
            // multi-GB set and the branch predictor cannot elide it.
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
            #[allow(clippy::cast_possible_truncation)]
            {
                union_ns += t_union.elapsed().as_nanos() as u64;
            }
        },
    )?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("altw_pass0_union_ms", (union_ns / 1_000_000) as i64);
        crate::debug::emit_counter("altw_pass0_refs_total", union_refs_total as i64);
    }
    if let Some(shared_set) = &shared {
        crate::debug::emit_counter(
            "altw_pass0_shared_node_ids",
            i64::try_from(shared_set.iter().count()).unwrap_or(i64::MAX),
        );
    }
    Ok((referenced, shared))
}

/// Collect all node IDs referenced by relation members.
///
/// Per-blob node-id streaming via `parallel_classify_phase`: workers emit
/// `Vec<i64>` of relation-member node ids per blob through the bounded
/// 32-slot result channel; the main thread unions them into one shared
/// `IdSet`. Memory is bounded to one IdSet plus per-blob transient vectors
/// rather than N-workers x per-worker `IdSet` (the previous shape, which
/// hit +9.7 GB anon at europe scale). Same
/// migration template as `tags_filter::collect_way_node_dependencies`
/// (commit `17b116c`). Set-union is commutative, so the worker-arrival
/// order does not affect the final IdSet contents.
///
/// `direct_io` is intentionally dropped on this path: blob bodies are
/// pread from the shared file handle on worker threads, incompatible
/// with `O_DIRECT` alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn collect_relation_member_ids(
    input: &Path,
    _direct_io: bool,
    want_nodes: bool,
    want_ways: bool,
) -> Result<(Option<IdSet>, Option<IdSet>)> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Relation),
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
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = element {
                    let complete = r.tags().any(|(key, value)| {
                        key == "type" && matches!(value, "multipolygon" | "boundary")
                    });
                    for member in r.members() {
                        match member.id {
                            MemberId::Node(id) if want_nodes && id >= 0 => ids.push((false, id)),
                            MemberId::Way(id) if want_ways && complete && id >= 0 => {
                                ids.push((true, id));
                            }
                            _ => {}
                        }
                    }
                }
            }
            ids
        },
        |_seq, ids| {
            for (way, id) in ids {
                if way {
                    member_way_ids.as_mut().expect("requested").set(id);
                } else {
                    member_node_ids.as_mut().expect("requested").set(id);
                }
            }
        },
    )?;
    Ok((member_node_ids, member_way_ids))
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_output_checked(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    relation_member_way_ids: Option<&IdSet>,
    shared_node_ids: Option<&IdSet>,
    inject_prepass: bool,
    compression: Compression,
    direct_io: bool,
    indexdata_present: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    if inject_prepass && !indexdata_present {
        return Err("--inject-prepass requires blob-level indexdata; --force cannot enable the decode-all fallback because it cannot attach per-blob WayMembers metadata".into());
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
            compression,
            direct_io,
            overrides,
        )
    } else {
        write_output_decode_all(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            compression,
            direct_io,
            overrides,
        )
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let reader = ElementReader::open(input, direct_io)?;
    let mut writer = writer_from_header_parallel(
        output,
        compression,
        reader.header(),
        true,
        overrides,
        |hb| hb.optional_feature("LocationsOnWays"),
        direct_io,
        false,
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
            for OwnedBlock {
                bytes,
                index,
                tagdata,
                way_members,
            } in blocks
            {
                writer.write_primitive_block_owned(
                    bytes,
                    index,
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )?;
            }
            stats.merge(&block_stats);
            decoded_blocks += 1;
            if decoded_blocks.is_multiple_of(64) {
                crate::debug::emit_counter(
                    "altw_pass2_blocks",
                    i64::try_from(decoded_blocks).unwrap_or(i64::MAX),
                );
            }
            Ok(())
        },
    )?;
    crate::debug::emit_counter(
        "altw_pass2_blocks",
        i64::try_from(decoded_blocks).unwrap_or(i64::MAX),
    );

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
/// Way refs resolve via inline `NodeIndex::get`. The earlier
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
) -> std::result::Result<Stats, String> {
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
                    let meta = dense_node_metadata(dn);
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
                    let meta = element_metadata(&n.info());
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
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::{AUTO_STORE_BUDGET_PCT, IndexType, route_sorted_indexed};

    const GB: u64 = 1_000_000_000;

    /// The measured grid this routing rule was fitted to (notes/altw.md
    /// P1, all at `dcc445e`): north-america's 18.75 GB store against the
    /// measuring host's ~25 GB budget ran linear (sparse won by 26 %),
    /// europe's 28.94 GB thrashed (external won by 21 %).
    #[test]
    fn route_reproduces_the_measured_grid() {
        let budget = Some(25 * GB);
        assert_eq!(
            route_sorted_indexed(Some(18_750_000_000), budget),
            IndexType::Sparse,
            "north-america-shaped store must route sparse"
        );
        assert_eq!(
            route_sorted_indexed(Some(28_940_000_000), budget),
            IndexType::External,
            "europe-shaped store must route external"
        );
    }

    #[test]
    fn route_threshold_is_strictly_greater_than() {
        let budget = 10 * GB;
        let threshold = budget * AUTO_STORE_BUDGET_PCT / 100;
        assert_eq!(
            route_sorted_indexed(Some(threshold), Some(budget)),
            IndexType::Sparse,
            "exactly at the threshold stays sparse"
        );
        assert_eq!(
            route_sorted_indexed(Some(threshold + 1), Some(budget)),
            IndexType::External,
            "one byte over the threshold routes external"
        );
    }

    /// Unknown store or budget routes external: the pre-scale-aware
    /// behavior for sorted + indexed inputs, and the direction where a
    /// wrong guess costs seconds rather than minutes of pass-2 thrash.
    #[test]
    fn route_unknowns_fall_back_to_external() {
        assert_eq!(
            route_sorted_indexed(None, Some(25 * GB)),
            IndexType::External
        );
        assert_eq!(route_sorted_indexed(Some(GB), None), IndexType::External);
        assert_eq!(route_sorted_indexed(None, None), IndexType::External);
    }

    #[test]
    fn route_survives_extreme_inputs_without_overflow() {
        assert_eq!(
            route_sorted_indexed(Some(u64::MAX), Some(25 * GB)),
            IndexType::External,
            "a store big enough to overflow the pct math must saturate \
             toward external, never wrap into a sparse verdict"
        );
        assert_eq!(
            route_sorted_indexed(Some(0), Some(0)),
            IndexType::Sparse,
            "zero store on a zero budget is not greater-than"
        );
    }
}
