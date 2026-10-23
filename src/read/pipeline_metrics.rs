//! Sidecar counters for the 3-stage pipelined PBF reader (`run_pipeline`).
//!
//! Mirrors `src/write/metrics.rs` for the read side. All counters are
//! lightweight atomics; emitted at the end of `run_pipeline` via
//! [`PIPELINE_METRICS.emit()`]. The decompression-pool drop counters are also
//! emitted by `par_map_reduce`, which shares the pool without using that
//! pipeline. Names track `pipeline_*` so they sort next to the writer's
//! `writer_*` counters in `--counters` views.
//!
//! Bench scope: every command that consumes blocks via
//! `for_each_pipelined` / `into_blocks_pipelined` gets these counters
//! for free. Current production users include geocode pass 1, getid
//! referenced-node collection, tags-filter `-R`, the non-indexed
//! add-locations-to-ways fallback, and the history time-filter path.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) struct PipelineMetrics {
    /// Cumulative time stage-1 (I/O reader) blocked on a full
    /// `read_ahead` raw-blob channel.
    pub raw_send_wait_ns: AtomicU64,
    /// Cumulative time stage-2 dispatcher spent waiting for decode
    /// admission permits. This is where backpressure from the decode
    /// cap is accounted.
    pub decode_admit_wait_ns: AtomicU64,
    /// Number of decode admission acquisitions that actually blocked.
    /// This is the gate-engaged signal; `decode_admit_wait_ns` can be
    /// nonzero from uncontended lock overhead alone.
    pub decode_admit_blocked: AtomicU64,
    /// Cumulative time stage-2 decode workers blocked sending decoded
    /// blocks toward the reorder buffer (channel bound by `decode_ahead`).
    pub decoded_send_wait_ns: AtomicU64,
    /// Cumulative time stage-3 (consumer thread, runs `block_fn`)
    /// spent waiting in `decoded_rx.recv()` for the next decoded block.
    /// Combined with `block_fn`'s own time this localises whether the
    /// consumer is starved by decode or by its own work.
    pub decoded_recv_wait_ns: AtomicU64,
    /// Maximum filled slots the in-order reorder buffer reached during
    /// the run. This is the live decoded-block memory diagnostic. For
    /// cross-run comparison with pre-change UUIDs, use
    /// `reorder_window_high_water` instead; old runs recorded window
    /// length including gaps under this counter name.
    pub reorder_high_water: AtomicU64,
    /// Maximum window length the in-order reorder buffer reached during
    /// the run, including gaps. This preserves the old
    /// `reorder_high_water` meaning as a completion-skew diagnostic.
    pub reorder_window_high_water: AtomicU64,
    /// Maximum retained capacity (bytes) of the per-decode-thread
    /// `ST_SCRATCH` Vec (string-table kv pairs in `parse_and_inline`).
    /// Sum across all decode threads at end of run. The iter-5 alloc
    /// profile fingered this as the dominant residual alloc bucket
    /// (~70 % at Japan, retained per-thread max-block-size capacity).
    pub scratch_st_capacity_peak_bytes: AtomicU64,
    /// Maximum retained capacity (bytes) of the per-decode-thread
    /// `GR_SCRATCH` Vec (group-range kv pairs).
    pub scratch_gr_capacity_peak_bytes: AtomicU64,
    /// Number of decode tasks dispatched (one per OsmData blob).
    pub decode_tasks: AtomicU64,
    /// Number of blobs skipped pre-decompression by the index/tag filter
    /// (e.g. `BlobFilter` rejects). Subtract from `decode_tasks` for
    /// actual decompression count.
    pub blobs_skipped_by_filter: AtomicU64,
    /// Decompression buffers dropped because their capacity exceeded the
    /// retained-buffer limit.
    pub decompress_pool_oversize_drops: AtomicU64,
    /// Sum of capacities of oversized decompression buffers dropped.
    pub decompress_pool_oversize_drop_capacity_bytes: AtomicU64,
    /// Decompression buffers dropped because the retained pool was full.
    pub decompress_pool_full_drops: AtomicU64,
    /// Sum of capacities of buffers dropped because the retained pool was full.
    pub decompress_pool_full_drop_capacity_bytes: AtomicU64,
    /// Decompression buffers dropped because the pool mutex was poisoned.
    pub decompress_pool_poisoned_drops: AtomicU64,
    /// Sum of capacities of buffers dropped after the pool mutex was poisoned.
    pub decompress_pool_poisoned_drop_capacity_bytes: AtomicU64,
}

impl PipelineMetrics {
    const fn new() -> Self {
        Self {
            raw_send_wait_ns: AtomicU64::new(0),
            decode_admit_wait_ns: AtomicU64::new(0),
            decode_admit_blocked: AtomicU64::new(0),
            decoded_send_wait_ns: AtomicU64::new(0),
            decoded_recv_wait_ns: AtomicU64::new(0),
            reorder_high_water: AtomicU64::new(0),
            reorder_window_high_water: AtomicU64::new(0),
            scratch_st_capacity_peak_bytes: AtomicU64::new(0),
            scratch_gr_capacity_peak_bytes: AtomicU64::new(0),
            decode_tasks: AtomicU64::new(0),
            blobs_skipped_by_filter: AtomicU64::new(0),
            decompress_pool_oversize_drops: AtomicU64::new(0),
            decompress_pool_oversize_drop_capacity_bytes: AtomicU64::new(0),
            decompress_pool_full_drops: AtomicU64::new(0),
            decompress_pool_full_drop_capacity_bytes: AtomicU64::new(0),
            decompress_pool_poisoned_drops: AtomicU64::new(0),
            decompress_pool_poisoned_drop_capacity_bytes: AtomicU64::new(0),
        }
    }

    /// Record a decompression buffer that could not be retained by the pool.
    /// These calls occur only on drop paths, so they add no atomic operation
    /// to the normal buffer-reuse path.
    fn record_decompress_pool_drop(count: &AtomicU64, bytes: &AtomicU64, capacity: usize) {
        count.fetch_add(1, Relaxed);
        bytes.fetch_add(u64::try_from(capacity).unwrap_or(u64::MAX), Relaxed);
    }

    pub fn record_decompress_pool_oversize_drop(&self, capacity: usize) {
        Self::record_decompress_pool_drop(
            &self.decompress_pool_oversize_drops,
            &self.decompress_pool_oversize_drop_capacity_bytes,
            capacity,
        );
    }

    pub fn record_decompress_pool_full_drop(&self, capacity: usize) {
        Self::record_decompress_pool_drop(
            &self.decompress_pool_full_drops,
            &self.decompress_pool_full_drop_capacity_bytes,
            capacity,
        );
    }

    pub fn record_decompress_pool_poisoned_drop(&self, capacity: usize) {
        Self::record_decompress_pool_drop(
            &self.decompress_pool_poisoned_drops,
            &self.decompress_pool_poisoned_drop_capacity_bytes,
            capacity,
        );
    }

    /// Compare-and-swap maxima for reorder-buffer fill and window levels.
    /// Called from the consumer thread on every push, so kept lock-free.
    pub fn record_reorder_levels(&self, filled: usize, window: usize) {
        cas_max(&self.reorder_high_water, filled as u64);
        cas_max(&self.reorder_window_high_water, window as u64);
    }

    /// Compare-and-swap maximum for the named scratch field. Each
    /// decode worker calls this at the end of every blob with its
    /// thread-local Vec's current capacity; the global max ends up
    /// reflecting the thread that touched the largest blob.
    ///
    /// We aggregate as a peak (single thread's worst case) rather
    /// than a sum across threads because the per-thread retention is
    /// what dominates - a sum would over-count if threads serially
    /// touched smaller blobs after a big one.
    pub fn record_scratch_capacity(&self, st_bytes: usize, gr_bytes: usize) {
        cas_max(&self.scratch_st_capacity_peak_bytes, st_bytes as u64);
        cas_max(&self.scratch_gr_capacity_peak_bytes, gr_bytes as u64);
    }

    pub fn emit(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                crate::debug::emit_counter(
                    $name,
                    i64::try_from(self.$field.load(Relaxed)).unwrap_or(i64::MAX),
                );
            };
        }
        emit!("pipeline_raw_send_wait_ns", raw_send_wait_ns);
        emit!("pipeline_decode_admit_wait_ns", decode_admit_wait_ns);
        emit!("pipeline_decode_admit_blocked", decode_admit_blocked);
        emit!("pipeline_decoded_send_wait_ns", decoded_send_wait_ns);
        emit!("pipeline_decoded_recv_wait_ns", decoded_recv_wait_ns);
        emit!("pipeline_reorder_high_water", reorder_high_water);
        emit!(
            "pipeline_reorder_window_high_water",
            reorder_window_high_water
        );
        emit!(
            "pipeline_scratch_st_capacity_peak_bytes",
            scratch_st_capacity_peak_bytes
        );
        emit!(
            "pipeline_scratch_gr_capacity_peak_bytes",
            scratch_gr_capacity_peak_bytes
        );
        emit!("pipeline_decode_tasks", decode_tasks);
        emit!("pipeline_blobs_skipped_by_filter", blobs_skipped_by_filter);
        self.emit_decompress_pool_counters();
    }

    /// Emit pool-drop counters from the `par_map_reduce` path, which uses the
    /// shared decompression pool but does not run `run_pipeline`.
    pub fn emit_decompress_pool_counters(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                crate::debug::emit_counter(
                    $name,
                    i64::try_from(self.$field.load(Relaxed)).unwrap_or(i64::MAX),
                );
            };
        }
        emit!(
            "pipeline_decompress_pool_oversize_drops",
            decompress_pool_oversize_drops
        );
        emit!(
            "pipeline_decompress_pool_oversize_drop_capacity_bytes",
            decompress_pool_oversize_drop_capacity_bytes
        );
        emit!(
            "pipeline_decompress_pool_full_drops",
            decompress_pool_full_drops
        );
        emit!(
            "pipeline_decompress_pool_full_drop_capacity_bytes",
            decompress_pool_full_drop_capacity_bytes
        );
        emit!(
            "pipeline_decompress_pool_poisoned_drops",
            decompress_pool_poisoned_drops
        );
        emit!(
            "pipeline_decompress_pool_poisoned_drop_capacity_bytes",
            decompress_pool_poisoned_drop_capacity_bytes
        );
    }
}

fn cas_max(field: &AtomicU64, candidate: u64) {
    let mut current = field.load(Relaxed);
    while candidate > current {
        match field.compare_exchange_weak(current, candidate, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

pub(crate) static PIPELINE_METRICS: PipelineMetrics = PipelineMetrics::new();

#[inline]
pub(crate) fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
