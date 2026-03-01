# Merge Measurement Infrastructure — Status

All measurement gaps identified for the memory optimization work have been resolved.

## Capabilities

| Capability | Status | Implementation |
|---|---|---|
| Peak RSS (VmHWM) | Done | `read_peak_rss_kb()` in CLI, `peak_rss_mb` DB column |
| Per-phase RSS deltas | Done | `PhaseRss` struct, gated on `hotpath` feature |
| Per-phase wall time | Done | `PhaseTimers` struct, gated on `hotpath` feature |
| Allocation tracking | Done | hotpath-alloc + `DiffOverlay::heap_size_estimate()` |
| Blob-size distribution | Done | `MergeStats::blob_sizes`, p50/p95/p99 in summary |
| Rewrite ratio (bytes) | Done | `bytes_passthrough` / `bytes_rewritten` in `MergeStats` |
| SQLite storage | Done | `peak_rss_mb` column, schema v3 |
| I/O throughput | Not done | ~5 lines, non-blocking |
| Memory comparison view | Not done | ~80 lines in brokkr, non-blocking |
