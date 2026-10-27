//! Sidecar counters for the 3-stage pipelined PBF reader (`run_pipeline`).
//!
//! # Invocation totals, re-emitted at the end of every pipelined read
//!
//! [`PipelineMetrics`] is one set per invocation
//! (`crate::counters::InvocationMetrics`), never reset. Every pipelined read
//! of the invocation adds into it (sums for the counts and waits, the
//! maximum for the high-water and peak counters), and every pipelined read
//! emits the whole set once its stages have joined, on success and error
//! alike. Every counter only grows, so for pipelined reads that run one
//! after another each emission is a snapshot of the invocation so far and
//! the last is its total. Concurrent reads of one invocation can emit out of
//! order (one loads a value, the other emits a later total first), but the
//! maximum of each name is still the total, and that is what the sidecar
//! reads: `brokkr sidecar --stalls` takes the maximum of each `*_wait_ns`
//! name as its total. A command that ran several pipelined reads reports
//! their sum.
//!
//! One instance per pipeline was weighed and declined. Emitting per-run
//! values would make `--stalls` report only the largest pass of an
//! invocation that reads twice and change what every stored run's counters
//! mean; per-pass names would split each stall category by pass and break
//! name-keyed comparisons. Two invocations, concurrent or not, record into
//! two sets.
//!
//! Only the pipelined engines record here: `par_map_reduce` and the
//! sequential `for_each` have none of these stages and emit nothing.
//! Pool-drop counters are invocation totals of every decompression pool, not
//! only this pipeline's, emitted after dispatch through
//! [`crate::read::emit_read_counters`]. Existing `pipeline_*` counter names
//! remain stable for sidecar consumers.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::counters::record_max;

crate::counters::counter_set! {
    pub(crate) struct PipelineMetrics {
        /// Cumulative time stage-1 (I/O reader) blocked on a full
        /// `read_ahead` raw-blob channel.
        pub raw_send_wait_ns => "pipeline_raw_send_wait_ns",
        /// Cumulative time stage-2 dispatcher spent waiting for decode
        /// admission permits. This is where backpressure from the decode
        /// cap is accounted.
        pub decode_admit_wait_ns => "pipeline_decode_admit_wait_ns",
        /// Number of decode admission acquisitions that actually blocked.
        /// This is the gate-engaged signal; `decode_admit_wait_ns` can be
        /// nonzero from uncontended lock overhead alone.
        pub decode_admit_blocked => "pipeline_decode_admit_blocked",
        /// Cumulative time stage-2 decode workers blocked sending decoded
        /// blocks toward the reorder buffer (channel bound by `decode_ahead`).
        pub decoded_send_wait_ns => "pipeline_decoded_send_wait_ns",
        /// Cumulative time stage-3 (consumer thread, runs `block_fn`)
        /// spent waiting in `decoded_rx.recv()` for the next decoded block.
        /// Combined with `block_fn`'s own time this localises whether the
        /// consumer is starved by decode or by its own work.
        pub decoded_recv_wait_ns => "pipeline_decoded_recv_wait_ns",
        /// Maximum filled slots any in-order reorder buffer of the
        /// invocation has reached. This is the live decoded-block memory diagnostic. For
        /// cross-run comparison with pre-change UUIDs, use
        /// `reorder_window_high_water` instead; old runs recorded window
        /// length including gaps under this counter name.
        pub reorder_high_water => "pipeline_reorder_high_water",
        /// Maximum window length any in-order reorder buffer of the
        /// invocation has reached, including gaps. This preserves the old
        /// `reorder_high_water` meaning as a completion-skew diagnostic.
        pub reorder_window_high_water => "pipeline_reorder_window_high_water",
        /// Maximum retained capacity (bytes) of the per-decode-thread
        /// `ST_SCRATCH` Vec (string-table kv pairs in `parse_and_inline`):
        /// the largest any one decode thread has retained, not a sum across
        /// threads (see [`PipelineMetrics::record_scratch_capacity`]). The iter-5 alloc
        /// profile fingered this as the dominant residual alloc bucket
        /// (~70 % at Japan, retained per-thread max-block-size capacity).
        pub scratch_st_capacity_peak_bytes => "pipeline_scratch_st_capacity_peak_bytes",
        /// Maximum retained capacity (bytes) of the per-decode-thread
        /// `GR_SCRATCH` Vec (group-range kv pairs).
        pub scratch_gr_capacity_peak_bytes => "pipeline_scratch_gr_capacity_peak_bytes",
        /// Number of decode tasks dispatched (one per OsmData blob).
        pub decode_tasks => "pipeline_decode_tasks",
        /// Number of blobs skipped pre-decompression by the index/tag filter
        /// (e.g. `BlobFilter` rejects). Subtract from `decode_tasks` for
        /// actual decompression count.
        pub blobs_skipped_by_filter => "pipeline_blobs_skipped_by_filter",
    }
}

impl PipelineMetrics {
    /// Maxima for reorder-buffer fill and window levels. Called from the
    /// consumer thread on every push, so kept lock-free.
    pub fn record_reorder_levels(&self, filled: usize, window: usize) {
        record_max(&self.reorder_high_water, filled as u64);
        record_max(&self.reorder_window_high_water, window as u64);
    }

    /// Maximum for the named scratch field. Each decode worker calls this
    /// at the end of every blob with its thread-local Vec's current
    /// capacity; the global max ends up reflecting the thread that touched
    /// the largest blob.
    ///
    /// We aggregate as a peak (single thread's worst case) rather
    /// than a sum across threads because the per-thread retention is
    /// what dominates - a sum would over-count if threads serially
    /// touched smaller blobs after a big one.
    pub fn record_scratch_capacity(&self, st_bytes: usize, gr_bytes: usize) {
        record_max(&self.scratch_st_capacity_peak_bytes, st_bytes as u64);
        record_max(&self.scratch_gr_capacity_peak_bytes, gr_bytes as u64);
    }

    /// Emit the invocation totals so far (see the module docs), after every
    /// `run_pipeline` has joined its stages.
    pub fn emit(&self, invocation: &crate::host::Invocation) {
        self.emit_all(invocation);
    }
}

crate::counters::counter_set! {
    /// Decompression-pool drop counters: invocation totals across every pool
    /// the invocation ran (classify workers, command-specific readers, altw
    /// passthrough), not only the pipelined reader, so no per-pipeline path
    /// emits them. Their only way out is [`emit_decompress_pool_counters`],
    /// which only the end-of-invocation emitter calls; the generated emitter
    /// is private to this module. A pool records into the invocation it was
    /// built for, also when a block it handed out is dropped after the call.
    pub(crate) struct DecompressPoolMetrics {
        /// Decompression buffers dropped because their capacity exceeded the
        /// retained-buffer limit.
        pub(crate) oversize_drops => "pipeline_decompress_pool_oversize_drops",
        /// Sum of capacities of oversized decompression buffers dropped.
        pub(crate) oversize_drop_capacity_bytes => "pipeline_decompress_pool_oversize_drop_capacity_bytes",
        /// Decompression buffers dropped because the retained pool was full.
        full_drops => "pipeline_decompress_pool_full_drops",
        /// Sum of capacities of buffers dropped because the retained pool was full.
        full_drop_capacity_bytes => "pipeline_decompress_pool_full_drop_capacity_bytes",
        /// Decompression buffers dropped because the pool mutex was poisoned.
        poisoned_drops => "pipeline_decompress_pool_poisoned_drops",
        /// Sum of capacities of buffers dropped after the pool mutex was poisoned.
        poisoned_drop_capacity_bytes => "pipeline_decompress_pool_poisoned_drop_capacity_bytes",
    }
}

impl DecompressPoolMetrics {
    /// Record a decompression buffer that could not be retained by the pool.
    /// These calls occur only on drop paths, so they add no atomic operation
    /// to the normal buffer-reuse path.
    fn record_drop(count: &AtomicU64, bytes: &AtomicU64, capacity: usize) {
        count.fetch_add(1, Relaxed);
        bytes.fetch_add(u64::try_from(capacity).unwrap_or(u64::MAX), Relaxed);
    }

    pub fn record_oversize_drop(&self, capacity: usize) {
        Self::record_drop(
            &self.oversize_drops,
            &self.oversize_drop_capacity_bytes,
            capacity,
        );
    }

    pub fn record_full_drop(&self, capacity: usize) {
        Self::record_drop(&self.full_drops, &self.full_drop_capacity_bytes, capacity);
    }

    pub fn record_poisoned_drop(&self, capacity: usize) {
        Self::record_drop(
            &self.poisoned_drops,
            &self.poisoned_drop_capacity_bytes,
            capacity,
        );
    }
}

/// Emit an invocation's pool-drop totals after all its work has joined.
/// Only `crate::read::emit_read_counters` calls this.
pub(crate) fn emit_decompress_pool_counters(
    metrics: &DecompressPoolMetrics,
    invocation: &crate::host::Invocation,
) {
    metrics.emit_all(invocation);
}
