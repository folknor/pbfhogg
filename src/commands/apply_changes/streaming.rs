//! Worker pool for the descriptor-first streaming pipeline.
//!
//! Spawns `worker_count` long-lived threads that pull [`ScannedBlob`]
//! candidates from the scanner over `candidate_rx` and emit
//! [`DrainItem`]s to the drain over `drain_tx`. Each worker owns a
//! thread-local [`BlockBuilder`], decompress and parse scratch buffers,
//! and (under `--locations-on-ways`) a private `FxHashMap` accumulator
//! published into a per-worker `Arc<Mutex<_>>` slot for the drain to
//! merge at the node→way barrier.
//!
//! ## Per-blob worker logic
//!
//! - [`ScannedBlob::Candidate`]: pread the Blob protobuf body via
//!   `(frame_start + blob_offset, data_size)`, decompress through the
//!   thread-local scratch, extract node coords (checked, dense and plain
//!   encodings, see `node_coords.rs`) if the blob is a Node and
//!   `--locations-on-ways` is active, parse via
//!   [`PrimitiveBlock::from_vec_with_scratch`], precise-check via
//!   [`block_overlaps_diff`]. False positives emit a `DrainItem::CopyRange`
//!   carrying the original descriptor so the drain can `copy_file_range`
//!   the raw frame. True overlaps compute
//!   the inline-upsert range from the descriptor's `id_range` against the
//!   diff's sorted upsert vector and call [`rewrite_block_parallel`] on the
//!   worker's persistent `BlockBuilder`, emitting
//!   `DrainItem::Rewritten`.
//! - [`ScannedBlob::Passthrough`]: only routed through workers under
//!   `--direct-io` output (where the drain can't `copy_file_range`).
//!   Workers pread the **full framed bytes** via
//!   `(frame_start, frame_len)` and emit
//!   `DrainItem::OwnedBytes`.
//!
//! ## Failure
//!
//! Every error the pool returns - setup or per-blob - is published into
//! the shared [`FailureSlot`] while the failing thread still holds its
//! drain sender. The drain returns it as its own error, which unblocks the
//! scanner and the surviving workers. Per-blob errors carry the blob's
//! seq and frame offset.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::blob::decompress_blob_raw;
use crate::blob_meta::ElemKind;
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::error::{Error, ErrorKind, Result, new_error};
use crate::writer::{Compression, frame_blob_pipelined};

use super::classify::block_overlaps_diff;
use super::descriptor::{BlobDescriptor, DrainItem, ScannedBlob};
use super::diff_ranges::DiffRanges;
use super::drain::LocMapHandle;
use super::failure::FailureSlot;
use super::loc_mode::LocMode;
use super::node_coords::extract_node_coords;
use super::rewrite_block::rewrite_block_parallel;

use crate::osc::CompactDiffOverlay;

/// One worker's shared coord accumulator. Workers hold their own; the
/// drain holds clones of every slot.
pub(super) type CoordSlot = Arc<Mutex<FxHashMap<i64, (i32, i32)>>>;

/// Per-worker shared coord accumulator slots. The drain holds clones of
/// every slot; each worker writes only into its own. At the node→way
/// barrier the drain acquires every mutex, drains the maps, merges into
/// `Arc<loc_map>`, and signals the scanner.
pub(super) type CoordSlots = Vec<CoordSlot>;

/// Streaming pool configuration. Channels are passed separately so this
/// struct can be `Clone` cheaply across worker startup.
pub(super) struct StreamingConfig {
    pub base_pbf: Box<Path>,
    pub ranges: Arc<DiffRanges>,
    pub diff: Arc<CompactDiffOverlay>,
    /// Number of worker threads. Should be `nproc - 2` (one each for the
    /// scanner and drain).
    pub worker_count: usize,
    /// `--locations-on-ways` mode. Workers extract node coords during
    /// the node phase only if true.
    pub locations_on_ways: bool,
    /// Sized to `worker_count`. Each worker writes into `slots[worker_id]`;
    /// the drain merges all slots at the node→way barrier. `None` when
    /// `locations_on_ways` is false.
    pub coord_slots: Option<CoordSlots>,
    /// Drain → workers post-barrier `loc_map`. `None` when
    /// `locations_on_ways` is false. Workers reading way/relation
    /// blobs check `get()` on this; the scanner barrier protocol
    /// guarantees the lock is set before any way descriptor is
    /// dispatched.
    pub loc_map_handle: Option<LocMapHandle>,
    /// Set of node IDs the workers need to extract coords for. Workers
    /// filter `extract_node_coords` output against this so we don't
    /// blow per-worker RSS by capturing every coord in the file.
    /// `None` when `locations_on_ways` is false.
    pub needed_set: Option<Arc<FxHashSet<i64>>>,
    /// Output compression. Workers frame each rewrite output via
    /// `frame_blob_pipelined` so the drain side avoids the writer's
    /// `rayon::spawn`-per-block dispatch (P1.5).
    pub compression: Compression,
    /// True when the output backend supports `copy_file_range` (so the
    /// drain accepts `DrainItem::CopyRange`). When false (consumer
    /// build without the `linux-direct-io` feature, or when
    /// `--direct-io` is selected), false-positive candidates must be
    /// routed through the owned-passthrough path instead - the drain
    /// rejects `CopyRange` items up front.
    pub use_copy_range: bool,
    /// Shared with the drain; see the module doc's "Failure" section.
    pub failure: Arc<FailureSlot>,
    /// Test-only: panic in the worker loop when the dequeued blob's
    /// `seq` matches. Only compiled under the `test-hooks` feature.
    #[cfg(feature = "test-hooks")]
    pub panic_at_blob_seq: Option<u64>,
}

/// Channels owned by the streaming pool.
pub(super) struct StreamingChannels {
    /// Candidate descriptors from the scanner. Closed when the scanner
    /// finishes (which signals workers to drain and exit).
    pub candidate_rx: mpsc::Receiver<ScannedBlob>,
    /// Drain inputs. Workers send `DrainItem`s directly so the drain has
    /// one unified input stream (the
    /// scanner's fast-path `DrainItem::CopyRange` and worker outputs
    /// merge into the same byte-budget reorder buffer keyed by global
    /// seq).
    pub drain_tx: mpsc::SyncSender<DrainItem>,
}

/// Atomic counters surfaced as sidecar counters at end of `merge()`.
pub(super) struct WorkerCounters {
    pub blobs_processed: AtomicU64,
    pub blobs_rewritten: AtomicU64,
    pub blobs_false_positive: AtomicU64,
    pub blobs_owned_passthrough: AtomicU64,
    /// Decompress wall summed across all workers (cumulative ns).
    pub decompress_ns: AtomicU64,
    /// Parse wall summed across all workers.
    pub parse_ns: AtomicU64,
    /// Precise-check wall summed across all workers.
    pub precise_ns: AtomicU64,
    /// Rewrite wall summed across all workers.
    pub rewrite_ns: AtomicU64,
    /// Coord-extraction wall summed across all workers (LOW only).
    pub coord_extract_ns: AtomicU64,
    /// Node coord pairs extracted across all workers (LOW only).
    pub coord_pairs_extracted: AtomicU64,
    /// Node blobs decompressed for coordinate extraction (LOW only).
    /// Bumped once per blob on the extraction branch, unlike
    /// `blobs_processed`, which counts every candidate and owned
    /// passthrough of every kind.
    pub loc_node_blobs_scanned: AtomicU64,
    /// In-line framing wall summed across all workers (P1.5: avoids
    /// the writer's rayon-spawn-per-block dispatch).
    pub frame_ns: AtomicU64,
}

impl WorkerCounters {
    pub(super) fn new() -> Self {
        Self {
            blobs_processed: AtomicU64::new(0),
            blobs_rewritten: AtomicU64::new(0),
            blobs_false_positive: AtomicU64::new(0),
            blobs_owned_passthrough: AtomicU64::new(0),
            decompress_ns: AtomicU64::new(0),
            parse_ns: AtomicU64::new(0),
            precise_ns: AtomicU64::new(0),
            rewrite_ns: AtomicU64::new(0),
            coord_extract_ns: AtomicU64::new(0),
            coord_pairs_extracted: AtomicU64::new(0),
            loc_node_blobs_scanned: AtomicU64::new(0),
            frame_ns: AtomicU64::new(0),
        }
    }

    pub(super) fn emit(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                let v = self.$field.load(Ordering::Relaxed);
                crate::debug::emit_counter($name, i64::try_from(v).unwrap_or(i64::MAX));
            };
        }
        emit!("merge_streaming_blobs_processed", blobs_processed);
        emit!("merge_streaming_blobs_rewritten", blobs_rewritten);
        emit!("merge_streaming_blobs_false_positive", blobs_false_positive);
        emit!(
            "merge_streaming_blobs_owned_passthrough",
            blobs_owned_passthrough
        );
        emit!("merge_streaming_decompress_ns", decompress_ns);
        emit!("merge_streaming_parse_ns", parse_ns);
        emit!("merge_streaming_precise_ns", precise_ns);
        emit!("merge_streaming_rewrite_ns", rewrite_ns);
        emit!("merge_streaming_coord_extract_ns", coord_extract_ns);
        emit!(
            "merge_streaming_coord_pairs_extracted",
            coord_pairs_extracted
        );
        emit!("merge_streaming_frame_ns", frame_ns);
    }
}

/// Run the worker pool to completion. Returns once the candidate channel
/// is closed and every worker has exited.
/// Called by `rewrite::merge` after the scanner and drain channels are ready.
///
/// Errors are not returned: every one, setup or per-blob, is published to
/// `cfg.failure` while the failing thread still holds a drain sender (the
/// drain's post-disconnect check relies on that ordering), and the drain
/// returns it. Worker panics are not caught and unwind through the scope.
///
/// The `drain_tx` clone owned by this function is dropped on return;
/// downstream drain sees `Disconnected` once every worker's clone is
/// also dropped.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn run_workers(
    cfg: StreamingConfig,
    channels: StreamingChannels,
    counters: &WorkerCounters,
) {
    let StreamingConfig {
        base_pbf,
        ranges,
        diff,
        worker_count,
        locations_on_ways,
        coord_slots,
        loc_map_handle,
        needed_set,
        compression,
        use_copy_range,
        failure,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq,
    } = cfg;
    let StreamingChannels {
        candidate_rx,
        drain_tx,
    } = channels;

    // Setup failures publish while `drain_tx` is still bound here; it is
    // dropped by the return that follows.
    if locations_on_ways {
        let slots_len = coord_slots.as_ref().map_or(0, Vec::len);
        if slots_len != worker_count {
            failure.publish(new_error(ErrorKind::Io(std::io::Error::other(format!(
                "streaming: locations_on_ways requires coord_slots.len()={worker_count}, got {slots_len}",
            )))));
            return;
        }
    }

    crate::debug::emit_marker("MERGE_STREAMING_START");

    let shared_file = match std::fs::File::open(&*base_pbf) {
        Ok(file) => file,
        Err(e) => {
            failure.publish(new_error(ErrorKind::Io(std::io::Error::other(format!(
                "streaming: failed to open {}: {e}",
                base_pbf.display(),
            )))));
            return;
        }
    };

    // mpsc::Receiver isn't Sync, so workers serialize their `recv()` calls
    // through this Mutex. The lock is only held for the duration of one
    // `recv()` (microseconds); workers spend their time in pread +
    // decompress + parse, not queued on the receiver lock.
    let candidate_rx = Mutex::new(candidate_rx);

    std::thread::scope(|scope| {
        for worker_id in 0..worker_count {
            let drain_tx = drain_tx.clone();
            let coord_slot = coord_slots.as_ref().map(|s| Arc::clone(&s[worker_id]));
            let loc_map_handle = loc_map_handle.as_ref().map(Arc::clone);
            let needed_set = needed_set.as_ref().map(Arc::clone);
            let file = &shared_file;
            let candidate_rx = &candidate_rx;
            let failure = &*failure;
            let ranges = &*ranges;
            let diff = &*diff;

            scope.spawn(move || {
                let result = worker_loop(
                    worker_id,
                    file,
                    candidate_rx,
                    &drain_tx,
                    counters,
                    ranges,
                    diff,
                    locations_on_ways,
                    coord_slot.as_ref(),
                    loc_map_handle.as_ref(),
                    needed_set.as_deref(),
                    &compression,
                    use_copy_range,
                    #[cfg(feature = "test-hooks")]
                    panic_at_blob_seq,
                );
                // Publish before this closure returns: `drain_tx` (moved in
                // above) is only dropped after it.
                if let Err(e) = result {
                    failure.publish(e);
                }
                drop(drain_tx);
            });
        }
    });

    // Drop our SyncSender clone explicitly so the drain sees end-of-input
    // once every worker's clone is also dropped.
    drop(drain_tx);

    crate::debug::emit_marker("MERGE_STREAMING_END");
    counters.emit();
}

/// Per-worker hot loop. Pulls candidates until the channel closes; for
/// each candidate dispatches to the appropriate handler.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn worker_loop(
    _worker_id: usize,
    file: &std::fs::File,
    candidate_rx: &Mutex<mpsc::Receiver<ScannedBlob>>,
    drain_tx: &mpsc::SyncSender<DrainItem>,
    counters: &WorkerCounters,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    locations_on_ways: bool,
    coord_slot: Option<&CoordSlot>,
    loc_map_handle: Option<&LocMapHandle>,
    needed_set: Option<&FxHashSet<i64>>,
    compression: &Compression,
    use_copy_range: bool,
    #[cfg(feature = "test-hooks")] panic_at_blob_seq: Option<u64>,
) -> Result<()> {
    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut bb = BlockBuilder::new();

    // Per-worker coord accumulator: gathered locally then flushed to the
    // shared slot in batches to amortise the mutex acquire. Empty when
    // `locations_on_ways` is false.
    let mut local_coords: FxHashMap<i64, (i32, i32)> = FxHashMap::default();
    let mut node_groups_scratch: Vec<(usize, usize)> = Vec::new();

    loop {
        let item = {
            let rx = candidate_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx.recv() {
                Ok(item) => item,
                Err(_) => break, // channel closed by scanner
            }
        };

        // Test-only: arm this from `MergeOptions::with_panic_at_blob_seq`
        // to trigger a mid-stream worker panic at a deterministic point,
        // exercising the worker-panic -> scope-join -> scratch-cleanup
        // recovery path. Production builds don't compile this block.
        #[cfg(feature = "test-hooks")]
        {
            let seq = match &item {
                ScannedBlob::Candidate(d) | ScannedBlob::Passthrough(d) => d.seq,
            };
            if panic_at_blob_seq == Some(seq) {
                panic!("test-hooks: panic_at_blob_seq={seq} triggered in worker {_worker_id}");
            }
        }

        match item {
            ScannedBlob::Candidate(mut desc) => {
                handle_candidate(
                    file,
                    &mut desc,
                    drain_tx,
                    counters,
                    ranges,
                    diff,
                    &mut bb,
                    &mut read_buf,
                    &mut decompress_buf,
                    &mut st_scratch,
                    &mut gr_scratch,
                    locations_on_ways,
                    &mut local_coords,
                    &mut node_groups_scratch,
                    coord_slot,
                    loc_map_handle,
                    needed_set,
                    compression,
                    use_copy_range,
                )
                .map_err(|e| blob_err(&desc, &e))?;
            }
            ScannedBlob::Passthrough(desc) => {
                // Only seen here under --direct-io output (scanner routes
                // splice-capable passthroughs straight to the drain).
                // Scanner only emits Passthrough for indexed blobs
                // (`scanner.rs::is_fastpath` requires `has_indexdata`),
                // so `desc.index.count` is authoritative and
                // desc.kind/id_range are authoritative (no override).
                let count = desc.index.as_ref().map_or(0, |i| i.count);
                handle_owned_passthrough(
                    file,
                    &desc,
                    drain_tx,
                    counters,
                    &mut read_buf,
                    count,
                    None,
                )
                .map_err(|e| blob_err(&desc, &e))?;
            }
        }
    }

    // Final flush of this worker's coord accumulator into the shared slot.
    // Safe regardless of whether the drain has merged yet - drain holds
    // the same Arc and acquires when ready.
    if locations_on_ways
        && !local_coords.is_empty()
        && let Some(slot) = coord_slot
    {
        flush_local_coords(slot, &mut local_coords);
    }

    Ok(())
}

/// Slow-path candidate: pread body, decompress, coord extraction
/// (Node + LOW), parse, precise check, false-positive or rewrite.
#[allow(clippy::too_many_arguments)]
// Sequential slow-path handler - the stages (pread, decompress, coord
// extract, parse, precise check, rewrite) share enough scratch state
// (read_buf, decompress_buf, group scratch, local_coords) that a split
// would fragment the descriptor pipeline without clarifying it.
#[allow(clippy::too_many_lines)]
fn handle_candidate(
    file: &std::fs::File,
    desc: &mut BlobDescriptor,
    drain_tx: &mpsc::SyncSender<DrainItem>,
    counters: &WorkerCounters,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    read_buf: &mut Vec<u8>,
    decompress_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
    locations_on_ways: bool,
    local_coords: &mut FxHashMap<i64, (i32, i32)>,
    node_groups_scratch: &mut Vec<(usize, usize)>,
    coord_slot: Option<&CoordSlot>,
    loc_map_handle: Option<&LocMapHandle>,
    needed_set: Option<&FxHashSet<i64>>,
    compression: &Compression,
    use_copy_range: bool,
) -> Result<()> {
    counters.blobs_processed.fetch_add(1, Ordering::Relaxed);

    let body_offset = desc.frame_start + desc.blob_offset as u64;
    pread_into(file, read_buf, body_offset, desc.data_size).map_err(io_err)?;

    let t_decompress = std::time::Instant::now();
    decompress_blob_raw(read_buf, decompress_buf).map_err(io_err)?;
    counters
        .decompress_ns
        .fetch_add(elapsed_ns(t_decompress), Ordering::Relaxed);

    if locations_on_ways && desc.kind == ElemKind::Node {
        let t_extract = std::time::Instant::now();
        let mut hits = 0u64;
        // Every node is decoded and validated; only the ones an OSC way
        // references are kept, so the per-worker map doesn't grow with
        // the whole base PBF's coord population. An extraction error is
        // fatal: dropping this blob's coordinates would surface later
        // only as an unexplained count of unresolved references.
        extract_node_coords(decompress_buf, node_groups_scratch, |id, lat, lon| {
            if needed_set.is_some_and(|ns| !ns.contains(&id)) {
                return;
            }
            local_coords.insert(id, (lat, lon));
            hits += 1;
        })?;
        counters
            .loc_node_blobs_scanned
            .fetch_add(1, Ordering::Relaxed);
        counters
            .coord_pairs_extracted
            .fetch_add(hits, Ordering::Relaxed);
        counters
            .coord_extract_ns
            .fetch_add(elapsed_ns(t_extract), Ordering::Relaxed);
    }

    let t_parse = std::time::Instant::now();
    let raw = std::mem::take(decompress_buf);
    let block = crate::PrimitiveBlock::from_vec_with_scratch(raw, st_scratch, gr_scratch)
        .map_err(io_err)?;
    counters
        .parse_ns
        .fetch_add(elapsed_ns(t_parse), Ordering::Relaxed);

    let t_precise = std::time::Instant::now();
    let overlaps = block_overlaps_diff(&block, ranges);
    counters
        .precise_ns
        .fetch_add(elapsed_ns(t_precise), Ordering::Relaxed);

    // Sync per-worker coords into the shared slot before emitting the
    // DrainItem. This guarantees the drain's barrier merge (which fires
    // once `next_seq > last_node_seq`) sees coords for every node blob
    // whose output has been processed - the `local_coords` map is
    // private and the drain reads only the shared `CoordSlot`s. For
    // non-Node blobs `local_coords` is empty so this is a no-op.
    if let Some(slot) = coord_slot
        && !local_coords.is_empty()
    {
        flush_local_coords(slot, local_coords);
    }

    // On the --force path the scanner fills `desc` with a placeholder
    // `kind=Node` and `id_range=None` because it can't distinguish blob
    // kinds without decompressing. Recover both from the parsed block so
    // the drain credits `base_*` on the real kind, the rewrite slice
    // lookup and create dispatch see correct values, and type-transition
    // logic doesn't fire spurious Way->Node/Relation->Node flushes that
    // would drain remaining upserts as trailing creates. Indexed
    // descriptors carry authoritative values already (the scanner sets
    // `id_range` exactly when `index` is present).
    let (effective_kind, effective_min_id, effective_max_id) = match desc.id_range {
        Some((min, max)) => (desc.kind, min, max),
        None => infer_kind_and_range(&block),
    };
    let inline_upserts = upsert_slice(ranges, effective_kind, effective_min_id, effective_max_id);

    // Passthrough is only sound when the blob needs no edit AND no create
    // lands inside its ID span. `block_overlaps_diff` answers the first
    // question only: a pure create whose ID falls in the blob's interior
    // matches no base element, so `overlaps` is false while
    // `inline_upserts` is non-empty. Shipping the blob verbatim then
    // leaves that create to the *next* blob's gap-create pass (or to the
    // type transition), i.e. emitted after larger IDs - and
    // `build_header_bytes` stamps `Sort.Type_then_ID` unconditionally, so
    // the result is a PBF that claims an ordering it does not have.
    // Forcing the rewrite lets `rewrite_block_parallel` interleave the
    // create at its sorted position and lets the drain advance the upsert
    // cursor past this blob's range.
    if !overlaps && inline_upserts.is_empty() {
        counters
            .blobs_false_positive
            .fetch_add(1, Ordering::Relaxed);
        // The drain needs an element count for per-kind `base_*` stats
        // (gap #7). Indexed blobs carry it in `desc.index.count`; for
        // non-indexed blobs (-force path) we walk the already-parsed
        // block. Counting is cheap next to the decompress + parse we
        // just did.
        let blob_count = desc
            .index
            .as_ref()
            .map_or_else(|| count_block_elements(&block), |i| i.count);
        let effective_id_range = (effective_min_id, effective_max_id);
        if use_copy_range {
            desc.kind = effective_kind;
            desc.id_range = Some(effective_id_range);
            return send_drain(drain_tx, desc.take_drain_item(blob_count));
        }
        // Consumer build (no `linux-direct-io` feature) or `--direct-io`
        // output: the drain rejects `DrainItem::CopyRange` up front, so
        // route the false-positive through the owned-passthrough path
        // (pread the full frame, ship the bytes via
        // `DrainItem::OwnedBytes`). `handle_owned_passthrough` does
        // exactly that for the scanner-side passthrough case.
        return handle_owned_passthrough(
            file,
            desc,
            drain_tx,
            counters,
            read_buf,
            blob_count,
            Some((effective_kind, effective_id_range)),
        );
    }

    // The drain publishes `loc_map` at the node→way barrier; the scanner
    // barrier protocol guarantees it is set before any way/relation
    // descriptor is dispatched to workers. Node blobs run before it, so
    // they see `LocMode::Pending`, under which writing a way is an error:
    // a way in a blob routed as nodes must never be written stripped of
    // its coordinates under a LocationsOnWays header.
    let loc_arc = if locations_on_ways {
        loc_map_handle.and_then(|h| h.get().cloned())
    } else {
        None
    };
    let loc = LocMode::new(locations_on_ways, loc_arc.as_deref());

    let t_rewrite = std::time::Instant::now();
    let output = rewrite_block_parallel(&block, diff, bb, inline_upserts, effective_kind, loc)
        .map_err(|e| new_error(ErrorKind::Io(std::io::Error::other(e.to_string()))))?;
    counters
        .rewrite_ns
        .fetch_add(elapsed_ns(t_rewrite), Ordering::Relaxed);

    // P1.5: frame each output block in-line via per-thread scratch so the
    // drain side can `write_raw_owned` without the writer's
    // `rayon::spawn`-per-block dispatch (which was the dominant
    // contributor to `writer_pipeline_send_wait_ns` at planet).
    let t_frame = std::time::Instant::now();
    let mut framed_chunks: Vec<Vec<u8>> = Vec::with_capacity(output.blocks.len());
    for OwnedBlock {
        bytes: block_bytes,
        index,
        tagdata,
        way_members,
    } in output.blocks
    {
        let indexdata = index.serialize();
        let parts = frame_blob_pipelined(
            &block_bytes,
            compression,
            Some(&indexdata),
            tagdata.as_deref(),
            way_members.as_deref(),
        )
        .map_err(io_err)?;
        framed_chunks.push(parts.into_vec());
    }
    counters
        .frame_ns
        .fetch_add(elapsed_ns(t_frame), Ordering::Relaxed);

    counters.blobs_rewritten.fetch_add(1, Ordering::Relaxed);
    send_drain(
        drain_tx,
        DrainItem::Rewritten {
            seq: desc.seq,
            framed_chunks,
            kind: effective_kind,
            id_range: (effective_min_id, effective_max_id),
            stats: output.stats,
        },
    )
}

/// Count the elements in an already-parsed block. Used on the
/// non-indexed false-positive path to supply a `base_<kind>` count to
/// the drain when `desc.index.count` is unavailable (gap #7 parity).
fn count_block_elements(block: &crate::PrimitiveBlock) -> u64 {
    u64::try_from(block.elements_skip_metadata().count()).unwrap_or(u64::MAX)
}

/// Recover a non-indexed blob's dominant element kind and (min, max)
/// id range by walking the already-parsed block. Used on the --force
/// path where the scanner has to emit a placeholder `kind=Node` and
/// `id_range=None` because it runs off the blob header alone; the
/// values it returns are authoritative for drain cursor advancement.
///
/// For homogeneous blobs (the usual shape for writers that emit one
/// kind per block - our own `PbfWriter` included) `block.block_type()`
/// is the dominant kind and the walk returns tight (min, max) id
/// bounds for that kind. For mixed blocks this picks the first-seen
/// element's kind and its own min/max - other-kind elements are
/// ignored by the range, leaving their upserts to the drain's
/// trailing-creates path. Empty blocks return a degenerate sentinel
/// range that the upsert cursor computation treats as "no range".
fn infer_kind_and_range(block: &crate::PrimitiveBlock) -> (ElemKind, i64, i64) {
    use crate::{BlockType, Element};

    let primary = match block.block_type() {
        BlockType::DenseNodes | BlockType::Nodes => Some(ElemKind::Node),
        BlockType::Ways => Some(ElemKind::Way),
        BlockType::Relations => Some(ElemKind::Relation),
        BlockType::Mixed | BlockType::Empty => None,
    };

    let mut kind: Option<ElemKind> = primary;
    let mut min_id = i64::MAX;
    let mut max_id = i64::MIN;

    for element in block.elements_skip_metadata() {
        let (elem_kind, id) = match &element {
            Element::DenseNode(dn) => (ElemKind::Node, dn.id()),
            Element::Node(n) => (ElemKind::Node, n.id()),
            Element::Way(w) => (ElemKind::Way, w.id()),
            Element::Relation(r) => (ElemKind::Relation, r.id()),
        };
        match kind {
            Some(k) if k == elem_kind => {
                min_id = min_id.min(id);
                max_id = max_id.max(id);
            }
            Some(_) => {
                // Different kind in a block whose block_type() said
                // homogeneous - ignore for the range (trailing-creates
                // handle other-kind upserts).
            }
            None => {
                kind = Some(elem_kind);
                min_id = id;
                max_id = id;
            }
        }
    }

    match kind {
        Some(k) => (k, min_id, max_id),
        // Empty block (or homogeneous-classified block whose elements are
        // all a different kind): return the reversed sentinel range
        // `(i64::MAX, i64::MIN)`. `upsert_slice` keys on
        // `blob_osm_first_key`/`blob_osm_last_key` and yields an empty
        // slice. The drain-side `process_item` detects `min > max` and
        // skips `handle_gap_creates`; without that guard,
        // `blob_osm_first_id` would return `i64::MAX` and gap-creates
        // would fire for every remaining upsert of `kind`. Any upserts
        // of any kind get trailing-created.
        None => (ElemKind::Node, i64::MAX, i64::MIN),
    }
}

/// `--direct-io` (or consumer-build false-positive) passthrough: pread
/// the **full framed bytes** so the drain can `write_raw_owned` without
/// re-framing. `count` is the blob's element count; the drain uses it
/// to credit `base_<kind>` stats and must not be zero on the indexed
/// path or per-kind merge totals drift (gap #7).
///
/// `override_kind_range`: scanner-side passthrough (indexed blob) leaves
/// this `None` and lets desc's authoritative kind + range flow through.
/// Worker-side false-positive on a non-indexed blob supplies the kind +
/// range walked from the parsed block, overriding desc's placeholder
/// `kind=Node` and `id_range=None` so the drain routes on the real kind.
fn handle_owned_passthrough(
    file: &std::fs::File,
    desc: &BlobDescriptor,
    drain_tx: &mpsc::SyncSender<DrainItem>,
    counters: &WorkerCounters,
    read_buf: &mut Vec<u8>,
    count: u64,
    override_kind_range: Option<(ElemKind, (i64, i64))>,
) -> Result<()> {
    counters.blobs_processed.fetch_add(1, Ordering::Relaxed);
    counters
        .blobs_owned_passthrough
        .fetch_add(1, Ordering::Relaxed);

    pread_into(file, read_buf, desc.frame_start, desc.frame_len).map_err(io_err)?;

    // Move the bytes into the DrainItem; reset read_buf for the next
    // pread without keeping the per-blob allocation alive.
    let frame_bytes = std::mem::take(read_buf);
    let (kind, id_range) =
        override_kind_range.unwrap_or_else(|| (desc.kind, desc.id_range.unwrap_or((0, 0))));

    send_drain(
        drain_tx,
        DrainItem::OwnedBytes {
            seq: desc.seq,
            frame_bytes,
            kind,
            id_range,
            count,
        },
    )
}

/// Resize `buf` to `len` and pread `len` bytes at `offset`.
fn pread_into(
    file: &std::fs::File,
    buf: &mut Vec<u8>,
    offset: u64,
    len: usize,
) -> std::io::Result<()> {
    buf.resize(len, 0);
    file.read_exact_at(buf, offset)
}

/// Compute the inline-upsert slice for one blob: upserts whose OSM ID
/// key falls in `[blob_osm_first(min,max), blob_osm_last(min,max)]`.
fn upsert_slice(ranges: &DiffRanges, kind: ElemKind, min_id: i64, max_id: i64) -> &[i64] {
    let upserts = ranges.upserts(kind);
    let first = crate::osm_id::blob_osm_first_key(min_id, max_id);
    let last = crate::osm_id::blob_osm_last_key(min_id, max_id);
    let start = upserts.partition_point(|&id| crate::osm_id::osm_id_key(id) < first);
    let end = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_key(id) <= last) + start;
    &upserts[start..end]
}

/// Drain `local` into the worker's shared coord slot. Acquires the mutex
/// for the duration of the merge; the drain reads via the same Arc so
/// the publish is atomic from its perspective.
fn flush_local_coords(
    slot: &Arc<Mutex<FxHashMap<i64, (i32, i32)>>>,
    local: &mut FxHashMap<i64, (i32, i32)>,
) {
    if local.is_empty() {
        return;
    }
    let mut shared = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if shared.is_empty() {
        std::mem::swap(&mut *shared, local);
    } else {
        shared.extend(local.drain());
    }
}

/// Send to the drain. Returns `Err` only if the drain has already exited
/// (channel closed) - treat as graceful shutdown.
fn send_drain(drain_tx: &mpsc::SyncSender<DrainItem>, item: DrainItem) -> Result<()> {
    drain_tx.send(item).map_err(|_| {
        new_error(ErrorKind::Io(std::io::Error::other(
            "streaming: drain closed worker→drain channel",
        )))
    })
}

/// Prefix a per-blob error with the blob's seq and frame offset so the
/// caller can find the blob that failed.
fn blob_err(desc: &BlobDescriptor, e: &Error) -> Error {
    new_error(ErrorKind::Io(std::io::Error::other(format!(
        "apply-changes: base blob seq {} at byte offset {}: {e}",
        desc.seq, desc.frame_start,
    ))))
}

#[allow(clippy::needless_pass_by_value)]
fn io_err<E: std::fmt::Display>(e: E) -> Error {
    new_error(ErrorKind::Io(std::io::Error::other(e.to_string())))
}

fn elapsed_ns(t: std::time::Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
