//! Merge statistics, per-phase timers, and RSS tracking.

/// Statistics from a merge operation.
pub struct MergeStats {
    pub base_nodes: u64,
    pub base_ways: u64,
    pub base_relations: u64,
    pub diff_nodes: u64,
    pub diff_ways: u64,
    pub diff_relations: u64,
    pub deleted: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    pub blobs_skip_decompress: u64,
    pub blobs_scan_only: u64,
    pub blobs_index_hit: u64,
    /// Bytes of raw passthrough frames (wire size including framing).
    pub bytes_passthrough: u64,
    /// Bytes of rewritten blocks (pre-compression protobuf size).
    pub bytes_rewritten: u64,
    /// Heap bytes used by the CompactDiffOverlay after OSC parsing.
    pub diff_heap_bytes: u64,
    /// Per-blob frame sizes in bytes for percentile computation.
    pub(super) blob_sizes: Vec<u32>,
    // -- locations-on-ways stats (only populated when flag is on) --
    /// Total node IDs needed for OSC ways.
    pub loc_nodes_needed: u64,
    /// Node coordinates found in OSC (pre-seeded).
    pub loc_nodes_from_diff: u64,
    /// Node coordinates found in base PBF during merge.
    pub loc_nodes_from_base: u64,
    /// Node coordinates not found anywhere (0,0 fallback).
    pub loc_missing: u64,
    /// Passthrough node blobs decompressed for coordinate extraction.
    pub loc_node_blobs_scanned: u64,
}

impl MergeStats {
    pub(super) fn new() -> Self {
        Self {
            base_nodes: 0,
            base_ways: 0,
            base_relations: 0,
            diff_nodes: 0,
            diff_ways: 0,
            diff_relations: 0,
            deleted: 0,
            blobs_passthrough: 0,
            blobs_rewritten: 0,
            blobs_skip_decompress: 0,
            blobs_scan_only: 0,
            blobs_index_hit: 0,
            bytes_passthrough: 0,
            bytes_rewritten: 0,
            diff_heap_bytes: 0,
            blob_sizes: Vec::new(),
            loc_nodes_needed: 0,
            loc_nodes_from_diff: 0,
            loc_nodes_from_base: 0,
            loc_missing: 0,
            loc_node_blobs_scanned: 0,
        }
    }

    pub fn total_elements(&self) -> u64 {
        self.base_nodes
            + self.base_ways
            + self.base_relations
            + self.diff_nodes
            + self.diff_ways
            + self.diff_relations
    }

    pub(super) fn merge_from(&mut self, other: &MergeStats) {
        self.base_nodes += other.base_nodes;
        self.base_ways += other.base_ways;
        self.base_relations += other.base_relations;
        self.diff_nodes += other.diff_nodes;
        self.diff_ways += other.diff_ways;
        self.diff_relations += other.diff_relations;
        self.deleted += other.deleted;
        self.bytes_passthrough += other.bytes_passthrough;
        self.bytes_rewritten += other.bytes_rewritten;
    }

    pub fn print_summary(&self) {
        let total_blobs =
            self.blobs_passthrough + self.blobs_rewritten + self.blobs_skip_decompress;
        eprintln!("Merge complete: {} elements written", self.total_elements());
        eprintln!(
            "  Base: {} nodes, {} ways, {} relations",
            self.base_nodes, self.base_ways, self.base_relations,
        );
        eprintln!(
            "  Diff: {} nodes, {} ways, {} relations",
            self.diff_nodes, self.diff_ways, self.diff_relations,
        );
        eprintln!("  Deleted: {}", self.deleted);
        eprintln!(
            "  Blobs: {} passthrough ({} index-hit, {} scan-only, {} skip-decompress), {} rewritten (of {total_blobs} total)",
            self.blobs_passthrough + self.blobs_skip_decompress,
            self.blobs_index_hit,
            self.blobs_scan_only,
            self.blobs_skip_decompress,
            self.blobs_rewritten,
        );
        let total_bytes = self.bytes_passthrough + self.bytes_rewritten;
        if total_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rewrite_pct = (self.bytes_rewritten as f64 / total_bytes as f64) * 100.0;
            eprintln!(
                "  Bytes: {} passthrough, {} rewritten ({rewrite_pct:.1}% rewrite ratio)",
                self.bytes_passthrough, self.bytes_rewritten,
            );
        }
        if !self.blob_sizes.is_empty() {
            let mut sizes = self.blob_sizes.clone();
            let (p50, p95, p99) = percentiles_u32(&mut sizes);
            eprintln!("  Blob sizes: p50={p50}, p95={p95}, p99={p99} bytes");
        }
        if self.diff_heap_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let mb = self.diff_heap_bytes as f64 / (1024.0 * 1024.0);
            eprintln!("  CompactDiffOverlay heap: {mb:.1} MB");
        }
        if self.loc_nodes_needed > 0 {
            eprintln!(
                "  Locations-on-ways: {} nodes needed, {} from diff, {} from base, {} missing, {} node blobs scanned",
                self.loc_nodes_needed, self.loc_nodes_from_diff,
                self.loc_nodes_from_base, self.loc_missing,
                self.loc_node_blobs_scanned,
            );
        }
    }
}

/// Compute p50, p95, p99 from a mutable slice. Returns `(0, 0, 0)` if empty.
fn percentiles_u32(data: &mut [u32]) -> (u32, u32, u32) {
    if data.is_empty() {
        return (0, 0, 0);
    }
    data.sort_unstable();
    let len = data.len();
    (data[len / 2], data[len * 95 / 100], data[len * 99 / 100])
}

/// Per-phase wall time accumulation across all batches. Always on - the
/// accumulation cost is a handful of `Instant::now()` calls per batch
/// (~10 ns each), amortised to zero against ~100 batches at planet scale.
/// Emitted as sidecar counters at the end of `merge()`.
pub(super) struct PhaseTimers {
    pub(super) osc_parse: std::time::Duration,
    pub(super) diffranges: std::time::Duration,
    pub(super) prefill: std::time::Duration,
    pub(super) header_read: std::time::Duration,
    pub(super) writer_setup: std::time::Duration,
    pub(super) classify_total: std::time::Duration,
    pub(super) phase2_inline_total: std::time::Duration,
    pub(super) rewrite_spawn_total: std::time::Duration,
    pub(super) rewrite_recv_total: std::time::Duration,
    pub(super) output_write_total: std::time::Duration,
    pub(super) passthrough_write_total: std::time::Duration,
    pub(super) trailing_creates: std::time::Duration,
    pub(super) final_flush: std::time::Duration,
}

impl PhaseTimers {
    pub(super) fn new() -> Self {
        Self {
            osc_parse: std::time::Duration::ZERO,
            diffranges: std::time::Duration::ZERO,
            prefill: std::time::Duration::ZERO,
            header_read: std::time::Duration::ZERO,
            writer_setup: std::time::Duration::ZERO,
            classify_total: std::time::Duration::ZERO,
            phase2_inline_total: std::time::Duration::ZERO,
            rewrite_spawn_total: std::time::Duration::ZERO,
            rewrite_recv_total: std::time::Duration::ZERO,
            output_write_total: std::time::Duration::ZERO,
            passthrough_write_total: std::time::Duration::ZERO,
            trailing_creates: std::time::Duration::ZERO,
            final_flush: std::time::Duration::ZERO,
        }
    }
}

/// Per-path classify instrumentation. Accumulated across rayon workers by
/// `classify_only`; emitted as sidecar counters at end of `merge()`.
///
/// Blob counts split `Passthrough` into the three paths that produced it
/// (fast-path vs scan-path vs fall-through-via-parse), disambiguating the
/// existing `blobs_index_hit`/`blobs_scan_only` which are keyed on
/// `has_indexdata` rather than which code path fired. FalsePositive is
/// its own count instead of being derived by subtraction.
///
/// Cumulative nanoseconds (summed across all rayon workers) attribute
/// classify wall to its sub-steps. Not instrumented: the fast-path range
/// check (ns per call, dominated by atomic-add overhead if measured).
pub(super) struct ClassifyCounters {
    // Blob-count per path
    pub(super) blobs_fastpath: std::sync::atomic::AtomicU64,
    pub(super) blobs_scan_pass: std::sync::atomic::AtomicU64,
    pub(super) blobs_false_positive: std::sync::atomic::AtomicU64,
    pub(super) blobs_rewrite: std::sync::atomic::AtomicU64,
    // Cumulative CPU per sub-step (summed across workers)
    pub(super) decompress_ns: std::sync::atomic::AtomicU64,
    pub(super) scan_ns: std::sync::atomic::AtomicU64,
    pub(super) parse_ns: std::sync::atomic::AtomicU64,
    pub(super) precise_ns: std::sync::atomic::AtomicU64,
}

impl ClassifyCounters {
    pub(super) fn new() -> Self {
        Self {
            blobs_fastpath: std::sync::atomic::AtomicU64::new(0),
            blobs_scan_pass: std::sync::atomic::AtomicU64::new(0),
            blobs_false_positive: std::sync::atomic::AtomicU64::new(0),
            blobs_rewrite: std::sync::atomic::AtomicU64::new(0),
            decompress_ns: std::sync::atomic::AtomicU64::new(0),
            scan_ns: std::sync::atomic::AtomicU64::new(0),
            parse_ns: std::sync::atomic::AtomicU64::new(0),
            precise_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// Cross-thread stall accumulator. The reader thread bumps `reader_send_us`
/// when a `try_send` fails and it blocks on the full channel; the main thread
/// bumps `consumer_recv_us` when `collect_batch` blocks on an empty channel,
/// and `rewrite_recv_us` when the output loop blocks waiting for a rewrite
/// result. All three surface as sidecar counters for attribution of
/// reader-bound vs consumer-bound vs writer-bound wall time.
pub(super) struct StallAccumulator {
    pub(super) reader_send_us: std::sync::atomic::AtomicU64,
    pub(super) consumer_recv_us: std::sync::atomic::AtomicU64,
    pub(super) rewrite_recv_us: std::sync::atomic::AtomicU64,
    pub(super) writer_call_us: std::sync::atomic::AtomicU64,
}

impl StallAccumulator {
    pub(super) fn new() -> Self {
        Self {
            reader_send_us: std::sync::atomic::AtomicU64::new(0),
            consumer_recv_us: std::sync::atomic::AtomicU64::new(0),
            rewrite_recv_us: std::sync::atomic::AtomicU64::new(0),
            writer_call_us: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// Read current RSS in kilobytes from `/proc/self/statm`.
/// Returns 0 on failure (non-Linux, read error, parse error).
#[cfg(feature = "hotpath")]
pub(super) fn read_rss_kb() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let Some(resident_str) = statm.split_whitespace().nth(1) else {
        return 0;
    };
    let Ok(pages) = resident_str.parse::<u64>() else {
        return 0;
    };
    pages * 4 // pages × 4096 / 1024 = pages × 4
}

/// Per-phase RSS tracking (rolling max across batches, in KB).
#[cfg(feature = "hotpath")]
pub(super) struct PhaseRss {
    pub(super) after_osc_parse: u64,
    pub(super) classify_max: u64,
    pub(super) rewrite_max: u64,
    pub(super) output_max: u64,
    pub(super) after_flush: u64,
}

#[cfg(feature = "hotpath")]
impl PhaseRss {
    pub(super) fn new() -> Self {
        Self {
            after_osc_parse: 0,
            classify_max: 0,
            rewrite_max: 0,
            output_max: 0,
            after_flush: 0,
        }
    }
}
