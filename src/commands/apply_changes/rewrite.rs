//! Apply-changes orchestrator: spawn scanner + workers + drain, join,
//! surface stats. The legacy batch-loop merge() is replaced by the
//! descriptor-first streaming pipeline assembled here.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use rustc_hash::FxHashMap;

use crate::blob::{BlobKind, decode_blob_to_headerblock};
use crate::file_reader::FileReader;
use crate::osc::parse_osc_file;
use crate::writer::Compression;

use crate::commands::{
    HeaderOverrides, build_output_header, require_indexdata, writer_from_header_bytes_parallel,
};
use crate::read::raw_frame::read_raw_frame;

use super::descriptor::DrainItem;
use super::diff_ranges::DiffRanges;
use super::drain::{
    self, DrainChannels, DrainConfig, DrainCounters, LocMapHandle, SeededLocations,
};
use super::node_locations::NodeLocationIndex;
use super::scanner::{self, ScannerChannels, ScannerConfig};
use super::stats::MergeStats;
use super::streaming::{self, CoordSlots, StreamingChannels, StreamingConfig, WorkerCounters};

use super::Result;

/// Channel capacity for the unified DrainItem stream from scanner +
/// workers. Sized for ~16 in-flight items per worker plus headroom; the
/// drain's reorder buffer absorbs out-of-order arrivals beyond this.
const DRAIN_CHANNEL_CAPACITY: usize = 256;

/// Channel capacity for the scanner→workers candidate dispatch. Workers
/// pull at decompress + parse pace (ms per blob); the producer (scanner)
/// runs at header-walk pace (~µs per blob), so a moderate buffer keeps
/// workers fed during scanner pauses.
const CANDIDATE_CHANNEL_CAPACITY: usize = 64;

/// Public command options.
pub struct MergeOptions {
    pub compression: Compression,
    pub direct_io: bool,
    pub io_uring: bool,
    pub force: bool,
    pub locations_on_ways: bool,
    /// Override the worker-pool size used by the descriptor-first
    /// pipeline. `None` (or `Some(0)`) keeps the default `nproc - 2`
    /// heuristic (leaves two cores for scanner + drain, minimum 2
    /// workers). `Some(1)` is rejected at runtime - a single worker
    /// has a deadlock hazard on mid-stream worker panic and no
    /// production use case (2+ workers is strictly faster).
    pub jobs: Option<usize>,
    /// Test-only: panic in the worker loop when a blob with this seq
    /// is dequeued, to exercise the worker-panic → scope-join →
    /// scratch-cleanup recovery path. Only compiled when the
    /// `test-hooks` feature is enabled. Set via
    /// `MergeOptions::with_panic_at_blob_seq` from tests; production
    /// callers never touch this field (and never see it on non-
    /// `test-hooks` builds).
    #[cfg(feature = "test-hooks")]
    pub panic_at_blob_seq: Option<u64>,
}

// clippy `derivable_impls` only holds when `test-hooks` is off. With the
// feature enabled the `panic_at_blob_seq` field is present and the
// derive would also need to default it - which Default *does* handle
// for Option, but we keep the manual impl for symmetry across feature
// configurations.
#[allow(clippy::derivable_impls)]
impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: false,
            locations_on_ways: false,
            jobs: None,
            #[cfg(feature = "test-hooks")]
            panic_at_blob_seq: None,
        }
    }
}

#[cfg(feature = "test-hooks")]
impl MergeOptions {
    /// Arm the worker-panic hook at the given seq. Only available with
    /// the `test-hooks` feature. See field docs on `panic_at_blob_seq`.
    #[must_use]
    pub fn with_panic_at_blob_seq(mut self, seq: u64) -> Self {
        self.panic_at_blob_seq = Some(seq);
        self
    }
}

#[allow(clippy::redundant_closure_for_method_calls)]
fn build_header_bytes(
    header: &crate::HeaderBlock,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    // apply-changes requires Sort.Type_then_ID on the base PBF
    // regardless of --locations-on-ways: the upsert slicing in
    // `streaming::upsert_slice` computes per-block `osm_id_key`
    // bounds that assume canonical ordering, and
    // `rewrite_block.rs::rewrite_block` advances its `upsert_cursor`
    // using `osm_id_cmp` against the iterating element, which only
    // produces correct output if elements arrive in canonical order.
    // Previously the check only fired for --locations-on-ways; the
    // general path silently dropped creates on malformed unsorted
    // input.
    if !header.is_sorted() {
        return Err(
            "apply-changes requires a sorted base PBF (Sort.Type_then_ID). \
             All nodes must precede all ways, and elements within a kind \
             must be ordered by ID."
                .into(),
        );
    }
    if locations_on_ways {
        if !header.has_locations_on_ways() {
            return Err(
                "merge --locations-on-ways requires the base PBF to have LocationsOnWays. \
                 Run add-locations-to-ways first to bootstrap coordinates."
                    .into(),
            );
        }
        build_output_header(header, false, overrides, |hb| {
            hb.sorted().optional_feature("LocationsOnWays")
        })
    } else {
        crate::commands::warn_locations_on_ways_loss(header);
        build_output_header(header, false, overrides, |hb| hb.sorted())
    }
}

fn read_header(
    base_pbf: &Path,
    direct_io: bool,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    let mut reader = FileReader::open(base_pbf, direct_io)?;
    let mut offset: u64 = 0;
    loop {
        match read_raw_frame(&mut reader, &mut offset)? {
            Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                return build_header_bytes(&header, locations_on_ways, overrides);
            }
            Some(_) => {}
            None => return Err("base PBF has no OSMHeader blob".into()),
        }
    }
}

/// Apply an OSC diff to a base PBF, producing an updated sorted PBF.
///
/// Descriptor-first streaming pipeline: scanner walks blob headers via
/// `HeaderWalker` and emits `DrainItem::CopyRange` (splice fast-path)
/// or `ScannedBlob::Candidate` (overlap candidates, routed to workers).
/// Workers pread bodies, decompress, precise-check, and either rewrite
/// inline (`DrainItem::Rewritten`) or convert false positives to
/// `DrainItem::CopyRange`. The drain reorders the unified stream by seq
/// and writes the output PBF, coalescing contiguous `copy_file_range`
/// runs.
///
/// Under `--locations-on-ways`, workers extract node coords during the
/// node phase into per-worker `Arc<Mutex<FxHashMap>>` slots; the drain
/// merges them at the node→way barrier and publishes the
/// `Arc<FxHashMap>` for way-phase workers via `LocMapHandle`.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the
/// output file cannot be written, or any PBF parsing/encoding fails.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::cast_precision_loss
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn merge(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: &Path,
    opts: &MergeOptions,
    overrides: &HeaderOverrides,
) -> Result<MergeStats> {
    let MergeOptions {
        compression,
        direct_io,
        io_uring,
        force,
        locations_on_ways,
        jobs,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq,
    } = *opts;
    let has_base_indexdata = require_indexdata(
        base_pbf,
        direct_io,
        force,
        "base PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to classify elements (significantly slower).",
    )?;

    // --force --locations-on-ways on a non-indexed base PBF is unsound:
    // under --force the scanner tags every blob as a placeholder `Node`
    // (real kind is not known until decompress), which defeats the
    // Node->Way barrier that would otherwise gate the worker pool
    // until the node-coord loc_map is published. The consequence is
    // that way workers run without a loc_map and `rewrite_block_local`
    // silently falls back to `write_base_way_local` (stripping
    // LocationsOnWays from every base way). Reject the combination up
    // front with a clear migration path rather than produce a silently
    // lossy output.
    if force && locations_on_ways && !has_base_indexdata {
        return Err(
            "apply-changes --force --locations-on-ways on a non-indexed PBF would \
             silently strip LocationsOnWays data from base ways. Generate an indexed \
             PBF first:\n\n\
             \x20 pbfhogg cat <input.osm.pbf> -o indexed.osm.pbf\n\n\
             Then run apply-changes against the indexed output."
                .into(),
        );
    }

    let osc_start = std::time::Instant::now();
    eprintln!("Parsing OSC diff: {}", osc_file.display());
    let diff = Arc::new(parse_osc_file(osc_file)?);
    eprintln!(
        "Diff: {} nodes, {} ways, {} relations ({} del nodes, {} del ways, {} del rels)",
        diff.node_count(),
        diff.way_count(),
        diff.relation_count(),
        diff.deleted_nodes.len(),
        diff.deleted_ways.len(),
        diff.deleted_relations.len(),
    );
    let diff_heap_bytes = diff.heap_size_estimate() as u64;
    let osc_parse_ms = osc_start
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);

    let diffranges_start = std::time::Instant::now();
    let ranges = Arc::new(DiffRanges::from_diff(&diff));
    let diffranges_ms = diffranges_start
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    eprintln!(
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(),
        ranges.way_ids.len(),
        ranges.rel_ids.len(),
    );

    // --locations-on-ways setup. The descriptor-first pipeline fuses
    // prefill into the worker pool, so we no longer call
    // `prefill_from_base` here. Build_from_diff still produces the
    // OSC-pre-seeded coords (`locations`) and the still-needed-from-base
    // ID set (`needed_set`).
    let (seeded_locations, needed_set, loc_stats_pre) = if locations_on_ways {
        let idx = NodeLocationIndex::build_from_diff(&diff);
        let total_needed = idx.locations.len() + idx.needed_set.len();
        let seeded = idx.locations.len();
        let still_needed = idx.needed_set.len();
        eprintln!(
            "  --locations-on-ways: {total_needed} node IDs needed, {seeded} from diff, {still_needed} from base"
        );
        (
            Some(idx.locations),
            Some(Arc::new(idx.needed_set)),
            (total_needed as u64, seeded as u64, still_needed as u64),
        )
    } else {
        (None, None, (0, 0, 0))
    };

    let header_start = std::time::Instant::now();
    let header_bytes = read_header(base_pbf, direct_io, locations_on_ways, overrides)?;
    let header_read_ms = header_start
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);

    let writer_setup_start = std::time::Instant::now();
    let mut writer = writer_from_header_bytes_parallel(
        output_pbf,
        compression,
        &header_bytes,
        direct_io,
        io_uring,
    )?;
    let writer_setup_ms = writer_setup_start
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);

    // copy_file_range fd setup. Mirrors the legacy path so direct-io
    // and io_uring routing remain correct.
    #[cfg(feature = "linux-direct-io")]
    let (_copy_fd_file, input_fd, use_copy_range) = {
        let f = FileReader::buffered(base_pbf)?;
        let fd = f.raw_fd();
        (f, fd, io_uring || !direct_io)
    };
    #[cfg(not(feature = "linux-direct-io"))]
    let (input_fd, use_copy_range) = (0i32, false);

    // Worker count: `-j N` override if supplied (non-zero), else the
    // default `nproc - 2` heuristic (leaves two cores for the scanner
    // and drain threads).
    //
    // Minimum is 2 workers, not 1. With a single worker, a worker
    // panic mid-stream deadlocks the pipeline: the scanner blocks on
    // a full `candidate_rx` with no one draining, and the drain
    // blocks waiting on senders that never drop. With 2+ workers the
    // surviving one keeps draining `candidate_rx`, the scanner
    // finishes normally, and the drain surfaces a clean error.
    // Rejecting `-j 1` explicitly is loud but honest; silently
    // bumping to 2 would mask a user's intent without benefit
    // (single-threaded apply-changes has no production use case -
    // it's strictly slower than 2+ workers on every host).
    let worker_count = match jobs {
        Some(1) => {
            return Err("apply-changes requires at least 2 worker threads \
                 (`--jobs N` with N >= 2, or omit `--jobs` for the \
                 default). A single worker has a deadlock hazard on \
                 mid-stream worker panic (scanner blocks on a full \
                 candidate channel with no one draining). \
                 Single-threaded operation has no production use \
                 case here - 2 workers is strictly faster on every \
                 host."
                .into());
        }
        Some(n) if n > 1 => n,
        _ => std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(2))
            .unwrap_or(4),
    };
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_worker_count", worker_count as i64);

    // Channels.
    let (drain_tx, drain_rx) = mpsc::sync_channel::<DrainItem>(DRAIN_CHANNEL_CAPACITY);
    let (candidate_tx, candidate_rx) =
        mpsc::sync_channel::<super::descriptor::ScannedBlob>(CANDIDATE_CHANNEL_CAPACITY);
    // Only clone for the scanner if we're going to use it (splice path).
    // Otherwise the unused clone keeps `drain_rx` alive past worker exit
    // and the drain thread deadlocks waiting for a sender that will
    // never drop until merge() returns - which only happens after the
    // drain joins.
    let drain_tx_for_scanner = if use_copy_range {
        Some(drain_tx.clone())
    } else {
        None
    };

    // --locations-on-ways barrier wiring.
    let (barrier_tx_opt, barrier_rx_opt, last_node_tx_opt, last_node_rx_opt) = if locations_on_ways
    {
        let (b_tx, b_rx) = mpsc::sync_channel::<()>(1);
        let (n_tx, n_rx) = mpsc::sync_channel::<u64>(1);
        (Some(b_tx), Some(b_rx), Some(n_tx), Some(n_rx))
    } else {
        (None, None, None, None)
    };

    let coord_slots: Option<CoordSlots> = if locations_on_ways {
        Some(
            (0..worker_count)
                .map(|_| Arc::new(Mutex::new(FxHashMap::default())))
                .collect(),
        )
    } else {
        None
    };
    let loc_map_handle: Option<LocMapHandle> = if locations_on_ways {
        Some(Arc::new(OnceLock::new()))
    } else {
        None
    };

    let worker_counters = Arc::new(WorkerCounters::new());
    let drain_counters = Arc::new(DrainCounters::new());

    let scanner_cfg = ScannerConfig {
        base_pbf,
        ranges: &ranges,
        use_copy_range,
        locations_on_ways,
        channels: ScannerChannels {
            candidate_tx,
            drain_tx: drain_tx_for_scanner,
        },
        barrier_rx: barrier_rx_opt,
        last_node_seq_tx: last_node_tx_opt,
    };

    let streaming_cfg = StreamingConfig {
        base_pbf: Box::from(base_pbf),
        ranges: Arc::clone(&ranges),
        diff: Arc::clone(&diff),
        worker_count,
        locations_on_ways,
        coord_slots: coord_slots
            .as_ref()
            .map(|s| s.iter().map(Arc::clone).collect()),
        loc_map_handle: loc_map_handle.as_ref().map(Arc::clone),
        needed_set: needed_set.as_ref().map(Arc::clone),
        compression,
        use_copy_range,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq,
    };
    let streaming_channels = StreamingChannels {
        candidate_rx,
        drain_tx,
    };

    let drain_cfg = DrainConfig {
        ranges: &ranges,
        diff: &diff,
        use_copy_range,
        input_fd,
        locations_on_ways,
        coord_slots,
        barrier_tx: barrier_tx_opt,
        loc_map_out: loc_map_handle,
        seeded_locations: seeded_locations.map(|l| SeededLocations::new(Some(l))),
    };
    let drain_channels = DrainChannels {
        drain_rx,
        last_node_seq_rx: last_node_rx_opt,
    };

    crate::debug::emit_marker("MERGE_LOOP_START");
    let pipeline_start = std::time::Instant::now();

    // Drain runs in this (the merge() caller's) thread because it owns
    // `&mut writer`. Scanner and worker pool run in scoped threads.
    //
    // Error ordering on a mid-stream fault: if the scanner errors out
    // after sending some candidates but before all are dispatched, the
    // drain will hit its "channel closed with N items in reorder buffer"
    // path first and the `?` below surfaces *that* error before the
    // scanner's real cause surfaces via `scanner_handle.join()` further
    // down. A similar pattern applies to a worker-pool panic. The user
    // sees two errors for one fault with the misleading one first; the
    // true cause lands via the scope join. Accepted as diagnostic-quality
    // - the failure mode is rare (corrupt PBF header mid-stream or an
    // internal `unwrap` on the worker path), both errors surface, and
    // plumbing a shared "first cause" slot is engineering we haven't
    // needed in practice.
    let stats = std::thread::scope(|scope| -> Result<MergeStats> {
        let scanner_handle = scope.spawn(move || scanner::run_scanner(scanner_cfg));
        let workers_counters_inner = Arc::clone(&worker_counters);
        let workers_handle = scope.spawn(move || {
            streaming::run_workers(streaming_cfg, streaming_channels, &workers_counters_inner)
        });

        let drain_stats =
            drain::run_drain(drain_cfg, drain_channels, &mut writer, &drain_counters)?;

        scanner_handle
            .join()
            .map_err(|_| -> Box<dyn std::error::Error> { "scanner thread panicked".into() })?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        workers_handle
            .join()
            .map_err(|_| -> Box<dyn std::error::Error> { "worker pool thread panicked".into() })?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        Ok(drain_stats)
    })?;

    let pipeline_ms = pipeline_start
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    crate::debug::emit_marker("MERGE_LOOP_END");

    let mut final_stats = stats;
    final_stats.diff_heap_bytes = diff_heap_bytes;
    if locations_on_ways {
        final_stats.loc_nodes_needed = loc_stats_pre.0;
        final_stats.loc_nodes_from_diff = loc_stats_pre.1;
        // from_base / missing populated by drain via worker-side counters
        // and the published map (post-barrier inspection).
        let extracted = worker_counters
            .coord_pairs_extracted
            .load(std::sync::atomic::Ordering::Relaxed);
        final_stats.loc_nodes_from_base = extracted;
        final_stats.loc_missing = loc_stats_pre.2.saturating_sub(extracted);
        final_stats.loc_node_blobs_scanned = worker_counters
            .blobs_processed
            .load(std::sync::atomic::Ordering::Relaxed);
    }

    final_stats.print_summary();

    // Surface phase counters that callers expect from the legacy shape.
    crate::debug::emit_counter("merge_osc_parse_ms", osc_parse_ms);
    crate::debug::emit_counter("merge_diffranges_ms", diffranges_ms);
    crate::debug::emit_counter("merge_header_read_ms", header_read_ms);
    crate::debug::emit_counter("merge_writer_setup_ms", writer_setup_ms);
    crate::debug::emit_counter("merge_pipeline_ms", pipeline_ms);

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter(
            "merge_blobs_passthrough",
            final_stats.blobs_passthrough as i64,
        );
        crate::debug::emit_counter("merge_blobs_rewritten", final_stats.blobs_rewritten as i64);
        crate::debug::emit_counter("merge_blobs_index_hit", final_stats.blobs_index_hit as i64);
        crate::debug::emit_counter("merge_total_elements", final_stats.total_elements() as i64);
        crate::debug::emit_counter("merge_deleted", final_stats.deleted as i64);
        crate::debug::emit_counter("merge_diff_nodes", final_stats.diff_nodes as i64);
        crate::debug::emit_counter("merge_diff_ways", final_stats.diff_ways as i64);
        crate::debug::emit_counter("merge_diff_relations", final_stats.diff_relations as i64);
        crate::debug::emit_counter(
            "merge_bytes_passthrough",
            final_stats.bytes_passthrough as i64,
        );
        crate::debug::emit_counter("merge_bytes_rewritten", final_stats.bytes_rewritten as i64);
        crate::debug::emit_counter("merge_diff_heap_bytes", final_stats.diff_heap_bytes as i64);
        if locations_on_ways {
            crate::debug::emit_counter("merge_loc_needed", final_stats.loc_nodes_needed as i64);
            crate::debug::emit_counter(
                "merge_loc_from_diff",
                final_stats.loc_nodes_from_diff as i64,
            );
            crate::debug::emit_counter(
                "merge_loc_from_base",
                final_stats.loc_nodes_from_base as i64,
            );
            crate::debug::emit_counter("merge_loc_missing", final_stats.loc_missing as i64);
            crate::debug::emit_counter(
                "merge_loc_node_blobs_scanned",
                final_stats.loc_node_blobs_scanned as i64,
            );
        }
    }

    Ok(final_stats)
}
