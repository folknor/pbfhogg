# Planet-Scale Memory Measurement: Gap Analysis

Audit of measurement and profiling infrastructure for the planet-scale memory
optimization experiments (E0.1 through E3.2) described in
`PLANET_MEMORY_EXPERIMENT_MATRIX.md`.

## Section 1: Current Capabilities

### 1.1 Wall-clock timing (fully supported)

**pbfhogg CLI `bench-merge` subcommand** (`cli/src/main.rs:968`):
- Self-times the `merge()` call with `Instant::now()` and emits `elapsed_ms=NNN`
  to stderr.
- Also emits `base_nodes`, `base_ways`, `base_relations`, `diff_nodes`,
  `diff_ways`, `diff_relations`, `blobs_passthrough`, `blobs_rewritten`,
  `output_mb` as key=value pairs on stderr.

**brokkr bench merge** (`~/Programs/brokkr/src/pbfhogg/bench_merge.rs`):
- Runs `bench-merge` as a subprocess, parses the stderr key=value output via
  `parse_kv_stderr()` in `harness.rs`.
- Uses `run_external_with_kv()` which takes the subprocess-reported `elapsed_ms`
  (not external wall-clock) and stores it in SQLite.
- Best-of-N selection (minimum elapsed_ms).

### 1.2 Function-level timing and allocation tracking (supported via hotpath)

**hotpath crate** (v0.13, `hotpath = "0.13"` in `Cargo.toml`):
- Two features: `hotpath` (timing mode) and `hotpath-alloc` (allocation mode).
- `#[hotpath::measure]` attribute macro on ~35 functions across the codebase.
- **Timing mode**: per-function call count, avg/p50/p95/p99 duration,
  total duration, % of total. Stored in HDR histograms.
- **Allocation mode**: replaces the global allocator with `CountingAllocator`
  that tracks per-function allocation bytes and count (alloc + dealloc).
  Per-thread alloc/dealloc totals. Uses thread-local stacks with depth tracking.
- **Thread metrics** (Linux): reads `/proc/self/task/*/stat` for per-thread
  CPU user/sys time. Also reads current RSS via `/proc/self/statm`.
- **RSS snapshot**: `get_rss_bytes()` reads `/proc/self/statm` for current RSS
  at report time (end of process). This is a single snapshot, NOT peak RSS.
- **JSON output**: when `HOTPATH_OUTPUT_FORMAT=json` and `HOTPATH_OUTPUT_PATH`
  are set, the report is written as JSON. brokkr captures this and stores it
  in the `extra` column of the results DB.

**Key hotpath-annotated merge functions**:
- `merge()` (top-level, `merge.rs:942`)
- `read_raw_frame()` (`merge.rs:271`)
- `rewrite_block_parallel()` (`merge.rs:653`)
- Plus `decompress_blob_data_into`, `parse_primitive_block_from_bytes_owned`,
  `frame_blob_into`, `take`, `add_node_dense`, `add_way`, `add_relation`,
  `scan_block_ids`, `scan_block_tag_keys` across read/write paths.

### 1.3 Merge statistics counters (supported in merge output)

`MergeStats` struct (`merge.rs:40`) tracks:
- `blobs_passthrough`, `blobs_rewritten`, `blobs_skip_decompress`,
  `blobs_scan_only`, `blobs_index_hit`
- Per-type element counts: `base_nodes/ways/relations`, `diff_nodes/ways/relations`
- `deleted` count
- Periodic progress (every 500 blobs) to stderr with cumulative stats.

### 1.4 Results database (partially supported)

**Schema** (`~/Programs/brokkr/src/db.rs:73`):
```
runs(id, timestamp, hostname, commit, subject, command, variant,
     input_file, input_mb, elapsed_ms, cargo_features, cargo_profile,
     kernel, cpu_governor, avail_memory_mb, storage_notes,
     extra, uuid, cli_args, metadata)
```

- `elapsed_ms`: wall-clock timing (always populated).
- `extra`: JSON blob for hotpath reports, distribution stats, or arbitrary kv.
- `metadata`: JSON blob for benchmark-specific context (compression, io_mode, etc.).
- `avail_memory_mb`: system-wide available memory at bench start (from
  `/proc/meminfo`), NOT process peak RSS.
- `hostname`, `commit`, `kernel`, `cpu_governor` are always recorded.
- Indexed on commit, command, timestamp.
- Dirty-tree guard: results are NOT stored if the git tree is dirty.

### 1.5 brokkr hotpath / profile commands (supported)

**`brokkr hotpath`** (`~/Programs/brokkr/src/pbfhogg/hotpath.rs`):
- Runs 4-6 test commands (tags-count, check-refs, cat, merge, optionally
  merge-no-indexdata, merge-none) with hotpath instrumentation.
- Each test: build with `--features hotpath`, run N times, capture JSON report,
  store in DB.
- `--alloc` flag: build with `--features hotpath-alloc` instead.

**`brokkr profile`** (`~/Programs/brokkr/src/pbfhogg/profile.rs`):
- Two-pass: first timing pass (hotpath), then allocation pass (hotpath-alloc).
- Single run per test. Results stored in DB.

### 1.6 Memory limit testing (partially supported)

**`brokkr run --mem <size>`** (`~/Programs/brokkr/src/main.rs:963`):
- Wraps the binary invocation with `systemd-run --scope -p MemoryMax=<size>`.
- This provides a hard OOM kill boundary but does NOT measure RSS.
- Useful for testing "does it survive under 64G?" but not for measurement.

### 1.7 Allocator selection (supported)

**CLI features** (`cli/Cargo.toml`, `cli/src/main.rs:1`):
- `jemalloc` and `mimalloc` features with `#[global_allocator]`.
- `brokkr bench allocator` compares default/jemalloc/mimalloc.

---

## Section 2: Gap Analysis

### Requirement 1: Peak RSS (VmHWM) per run

**Status: NOT SUPPORTED**

- The hotpath crate reads current RSS at process exit via `/proc/self/statm`.
  This is a point-in-time snapshot, not the peak (VmHWM). The high-water mark
  is only available from `/proc/self/status` (VmHWM field) or
  `getrusage(RUSAGE_SELF).ru_maxrss`.
- Neither pbfhogg nor brokkr reads VmHWM at any point.
- The `avail_memory_mb` in the DB is system-wide available memory at start,
  not process RSS.
- There is no column in the results DB schema for peak RSS or any per-process
  memory metric.

**What's missing:**
1. A function to read VmHWM from `/proc/self/status` (pbfhogg or CLI).
2. The CLI `bench-merge` command does not emit peak RSS to stderr.
3. The results DB has no `peak_rss_mb` column.
4. The harness `parse_kv_stderr()` would handle it automatically if the CLI
   emitted it, but nobody emits it.

**Where to add:**
- Read VmHWM after `merge()` returns in `cli/src/main.rs:run_bench_merge()`.
  Emit as `peak_rss_kb=NNN` to stderr. The kv parser picks it up into `extra`.
- Alternatively, add a `peak_rss_mb` column to the DB schema (cleaner, avoids
  burying it in JSON).

### Requirement 2: Per-phase RSS deltas

**Status: NOT SUPPORTED**

- The merge function has clear phase boundaries (OSC parse at line 954,
  classify at 1092, rewrite at 1132, output at 1158) but no RSS readings
  at these boundaries.
- The hotpath `#[hotpath::measure]` annotations provide per-function timing
  and allocation but NOT RSS snapshots at function entry/exit.
- There is no phase-boundary instrumentation hook in the merge pipeline.

**What's missing:**
1. RSS sampling at phase boundaries within `merge()`.
2. A mechanism to log or accumulate phase-local high-water marks.
3. The batch loop processes 64-blob batches, so phase boundaries repeat per
   batch -- need either per-batch RSS or rolling max per phase across batches.

**Where to add:**
- Inside `merge()` (`src/commands/merge.rs:943`), read `/proc/self/statm` or
  VmRSS at: (a) after `parse_osc_file`, (b) after each batch's Phase 1-4,
  (c) after writer flush.
- Gate behind `#[cfg(feature = "hotpath")]` to avoid overhead in production.
- Return phase RSS data as part of `MergeStats` or a new `MergeProfile` struct.

### Requirement 3: Allocation tracking

**Status: PARTIALLY SUPPORTED**

**What works:**
- hotpath-alloc mode provides total allocation volume and count per annotated
  function. Per-thread alloc/dealloc totals.
- histograms give p50/p95/p99 per-function allocation size.
- Per-thread tracking via `ThreadAllocStats` (up to 256 threads).

**What's missing:**
1. **Per-phase allocation attribution**: hotpath-alloc gives per-function totals
   across the entire run. It cannot distinguish "allocations during OSC parse"
   from "allocations during rewrite phase" because the same functions
   (e.g., `Vec::push`) are called in multiple phases.
2. **DiffOverlay size estimation**: no instrumentation to measure the actual
   heap size of the parsed DiffOverlay (nodes + ways + relations + delete sets).
   The experiment matrix specifically needs "overlay heap size estimate".
3. **Writer framing allocation**: `frame_blob_into` is annotated, but the
   pipelined writer's internal channel buffer accumulation is not tracked.
4. **hotpath-alloc replaces the global allocator**: this means it is mutually
   exclusive with jemalloc/mimalloc. Cannot measure "allocation behavior under
   jemalloc" with hotpath-alloc enabled.

**Where to add:**
- DiffOverlay size: add a `heap_size_estimate()` method to `DiffOverlay` in
  `src/osc.rs`. Call it after `parse_osc_file()` in merge and emit to stderr.
- Phase-scoped allocation: would require hotpath API changes or manual
  `ALLOCATIONS` depth manipulation at phase boundaries. Non-trivial.

### Requirement 4: Blob-size distribution and rolling rewrite ratio

**Status: NOT SUPPORTED**

- `MergeStats` tracks total `blobs_passthrough` and `blobs_rewritten` but not
  per-blob raw sizes.
- No blob-size histogram or distribution tracking exists.
- No rolling rewrite ratio (windowed passthrough/rewrite counts over time).
- The merge function does not record `raw_size` of individual blobs.
- The periodic progress log (every 500 blobs, line 1260) shows cumulative
  counts but not size distribution.

**What's missing:**
1. Per-blob `raw_size` recording (available from `RawBlobFrame.frame_bytes.len()`).
2. A histogram or percentile tracker for blob sizes (could use an HDR histogram
   or a simple sorted buffer).
3. Rolling window rewrite ratio tracker.
4. Emitting p50/p95/p99 blob sizes and rewrite ratio to stderr/MergeStats.

**Where to add:**
- In the batch processing loop (`merge.rs:1158`, Phase 4), record
  `batch[i].frame_bytes.len()` into a size tracker.
- Track per-batch rewrite fraction (rewrite jobs / batch size).
- Add fields to `MergeStats` for blob size distribution and rewrite ratio.
- Emit summary stats in `print_summary()` and `run_bench_merge()`.

### Requirement 5: Wall time (total and per-phase)

**Status: PARTIALLY SUPPORTED**

**What works:**
- Total wall time: fully supported (bench-merge self-times, DB stores it).
- Per-function timing: hotpath timing mode gives breakdown.

**What's missing:**
- Per-phase wall time within a single merge run. The hotpath `#[hotpath::measure]`
  on `merge()` gives the total, and annotations on `rewrite_block_parallel()`
  and `read_raw_frame()` give function-level breakdown, but there is no
  "OSC parse took X ms, classify phase took Y ms across all batches, rewrite
  phase took Z ms" rollup.
- The 4-phase batch pipeline repeats per batch, so per-phase timing needs
  accumulation across batches.

**Where to add:**
- Add per-phase `Instant` timing in the batch loop (gated on hotpath feature).
- Accumulate phase durations across batches into `MergeStats` or a separate
  timing struct.

### Requirement 6: CPU utilization and I/O throughput

**Status: PARTIALLY SUPPORTED**

**What works:**
- hotpath thread metrics collect per-thread CPU user/sys time from
  `/proc/self/task/*/stat` (at report time, i.e., end of process).
- Input file size is known (passed as `input_mb`).
- Total wall time is known.
- I/O throughput can be derived (input_mb / elapsed_s) but is not computed
  or stored explicitly.

**What's missing:**
1. CPU utilization as a time series or per-phase breakdown.
2. I/O throughput (MB/s) for read and write separately.
3. Output file size is captured in `bench-merge` (`output_mb`) but only in
   the stderr kv output, not as a first-class DB field.
4. No `iostat`-style per-device throughput measurement.

**Where to add:**
- Compute and emit `read_throughput_mbs` and `write_throughput_mbs` in
  `run_bench_merge()` using input_mb, output_mb, and elapsed_ms.
- These would flow into `extra` via kv parsing automatically.

### Requirement 7: Rewrite ratio (passthrough vs rewrite counts and bytes)

**Status: PARTIALLY SUPPORTED**

**What works:**
- `MergeStats` tracks `blobs_passthrough` and `blobs_rewritten` counts.
- `bench-merge` emits these as kv pairs to stderr.
- They end up in the DB `extra` JSON column.

**What's missing:**
1. **Byte-level rewrite ratio**: total bytes of passthrough frames vs total
   bytes of rewritten output. Currently only counts are tracked, not sizes.
2. **Rolling/windowed ratio**: only cumulative totals, not per-batch or
   per-region-of-file distribution.

**Where to add:**
- Add `bytes_passthrough` and `bytes_rewritten` fields to `MergeStats`.
- Accumulate frame sizes in Phase 4 of the batch loop.
- Emit in `print_summary()` and `run_bench_merge()`.

### Requirement 8: Results stored in SQLite with commit/hostname/dataset/CLI

**Status: MOSTLY SUPPORTED**

**What works:**
- Results DB stores: hostname, commit, subject, command, variant, input_file,
  input_mb, elapsed_ms, cargo_features, cargo_profile, kernel, cpu_governor,
  avail_memory_mb, storage_notes, extra (JSON), cli_args, metadata (JSON).
- Commit hash, hostname, dataset (via input_file), and CLI args are all
  captured.

**What's missing:**
1. No dedicated `peak_rss_mb` column (would need to be buried in `extra` or
   a schema migration is needed).
2. No dedicated columns for merge-specific metrics (blobs_passthrough,
   blobs_rewritten, rewrite_ratio, etc.) -- these go into `extra` JSON, which
   works but makes querying harder.
3. The experiment matrix wants per-experiment structured comparison. The current
   `brokkr results --compare A B` compares two commits but is limited to
   timing and hotpath diff formatting. No memory-focused comparison view.

---

## Section 3: Critical Gaps for Phase 0

### E0.1: Memory attribution by pipeline phase

**Can it be run today?** No.

**Blockers:**
1. No peak RSS measurement (VmHWM) at any point in the pipeline.
2. No per-phase RSS sampling within merge.
3. The hotpath end-of-process RSS snapshot from `/proc/self/statm` is the
   only memory reading, and it captures current RSS (not peak) at a single
   point in time.

**Minimum work to unblock:**

1. **Read VmHWM from `/proc/self/status`** (small effort):
   Add a `read_peak_rss_kb()` function that parses VmHWM from
   `/proc/self/status`. Call it after `merge()` returns in
   `cli/src/main.rs:run_bench_merge()`. Emit as `peak_rss_kb=NNN`.
   This gives per-run peak RSS immediately.

2. **Phase-boundary RSS sampling** (medium effort):
   In `merge()`, add RSS reads at:
   - After `parse_osc_file()` (captures diff overlay footprint)
   - After each batch's Phase 3 (rewrite complete, before output drain)
   - After writer flush
   Gate behind `#[cfg(feature = "hotpath")]`. Track rolling max per phase.
   Return as new fields on `MergeStats`.

3. **DiffOverlay heap size estimate** (small effort):
   Add `DiffOverlay::heap_size_estimate()` that sums:
   `nodes.capacity() * size_of_entry + ways... + tags string bytes...`
   Emit after parse. This directly attributes memory to the diff model.

### E0.2: Blob-size and rewrite-ratio sensitivity

**Can it be run today?** Partially -- rewrite ratio by count is available,
but blob-size distribution and byte-level ratio are not.

**Minimum work to unblock:**

1. **Blob-size tracking** (small effort):
   Add a simple accumulator in the Phase 4 loop that records
   `frame_bytes.len()` for each blob. Track min/max/sum/count, or use
   a compact histogram (sorted Vec of sizes, compute percentiles post-run).
   Emit p50/p95/p99/max to stderr.

2. **Byte-level rewrite ratio** (small effort):
   Add `bytes_passthrough: u64` and `bytes_rewritten: u64` to `MergeStats`.
   Accumulate in Phase 4. Emit in summary and bench-merge output.

3. **Rolling rewrite ratio** (small-medium effort):
   Track a sliding window (e.g., last 100 blobs) of passthrough/rewrite
   decisions. Log the rewrite fraction at periodic intervals (already have
   a 500-blob progress log at line 1260). Emit max windowed rewrite ratio
   in the final summary.

---

## Section 4: Recommendations

### Priority 1: VmHWM capture in bench-merge (SMALL effort)

Add `read_peak_rss_kb()` that reads `/proc/self/status` VmHWM line. Call it
in `run_bench_merge()` after `merge()` returns. Emit to stderr as
`peak_rss_kb=NNN`. The existing kv parser puts it into `extra` automatically.

This single change unblocks basic before/after RSS comparison for every
experiment. Estimated: ~20 lines of code in `cli/src/main.rs`.

### Priority 2: Blob-size and byte-level rewrite stats (SMALL effort)

Extend `MergeStats` with:
- `bytes_passthrough: u64`, `bytes_rewritten: u64`
- `blob_sizes: Vec<u32>` (or a compact histogram)
- `max_blob_raw_size: u32`

Accumulate in Phase 4. Emit p50/p95/p99 blob sizes and byte-level rewrite
ratio in `print_summary()` and `run_bench_merge()`.

This unblocks E0.2 entirely. Estimated: ~50 lines in `merge.rs`, ~10 lines
in `cli/src/main.rs`.

### Priority 3: DiffOverlay heap size estimate (SMALL effort)

Add `DiffOverlay::heap_size_estimate(&self) -> usize` in `src/osc.rs`.
Sum: each HashMap's capacity * entry size + each Vec/String's heap bytes.
Does not need to be exact -- an estimate within 20% is sufficient for
attribution.

Call after `parse_osc_file()` in `merge()`, emit as eprintln and in
`MergeStats`. This directly validates the hypothesis that DiffOverlay
dominates peak RSS.

Estimated: ~40 lines in `osc.rs`, ~5 lines in `merge.rs`.

### Priority 4: Phase-boundary RSS sampling (MEDIUM effort)

Add RSS reads at phase boundaries in `merge()`, gated behind
`#[cfg(feature = "hotpath")]`:
- After OSC parse
- After batch Phase 1 (classify)
- After batch Phase 3 (rewrite)
- After batch Phase 4 (output drain)
- After final writer flush

Track per-phase rolling max RSS. Add to `MergeStats` or a new
`MergeProfile` struct. Emit in `bench-merge` output.

This is the core of E0.1 but can be deferred slightly because Priority 1
(VmHWM) gives a useful first signal.

Estimated: ~80 lines in `merge.rs`, ~20 lines reading `/proc/self/statm`.

### Priority 5: Results DB schema for memory metrics (SMALL effort)

Add `peak_rss_kb INTEGER` column to the `runs` table. Populate from the
bench-merge kv output. This makes memory metrics queryable alongside timing
without digging into JSON.

Alternatively, since `extra` JSON already captures arbitrary kv pairs, this
is optional -- but a dedicated column is cleaner for cross-commit comparison
queries.

Estimated: ~10 lines in `db.rs` (schema + insert + select).

### Priority 6: I/O throughput computation (SMALL effort)

Compute `read_mbs` and `write_mbs` in `run_bench_merge()` from input_mb,
output_mb, and elapsed_ms. Emit to stderr. Flows into `extra` automatically.

Estimated: ~5 lines in `cli/src/main.rs`.

### Priority 7: Per-phase wall time accumulation (MEDIUM effort)

Add per-phase `Instant` timers in the batch loop of `merge()`. Accumulate
across batches. Add to `MergeStats`:
- `osc_parse_ms`, `classify_total_ms`, `rewrite_total_ms`,
  `output_total_ms`, `gap_creates_ms`

Gate behind hotpath feature. Emit in bench-merge output.

Estimated: ~60 lines in `merge.rs`.

### Priority 8: brokkr comparison view for memory (MEDIUM effort)

Extend `brokkr results --compare` to display memory metrics side-by-side
when available (peak_rss_kb, blob size distribution, rewrite ratio).
Currently it only shows elapsed_ms and hotpath function diffs.

Estimated: ~80 lines in `db.rs` and `hotpath_fmt.rs`.

---

## Summary

| Requirement | Status | Blocking E0? | Fix Effort |
|---|---|---|---|
| 1. Peak RSS (VmHWM) | Not supported | YES (E0.1) | Small |
| 2. Per-phase RSS deltas | Not supported | YES (E0.1) | Medium |
| 3. Allocation tracking | Partial | No (hotpath-alloc works) | Small for overlay estimate |
| 4. Blob-size distribution | Not supported | YES (E0.2) | Small |
| 5. Wall time total+phase | Partial | No (total works) | Medium for per-phase |
| 6. CPU util + I/O throughput | Partial | No | Small |
| 7. Rewrite ratio (bytes) | Partial (counts only) | YES (E0.2) | Small |
| 8. SQLite storage | Mostly supported | No | Small for dedicated column |

**Critical path**: Priorities 1-3 (VmHWM + blob stats + overlay size estimate)
are all small-effort changes that together unblock both E0.1 and E0.2.
Total estimated: ~120 lines of new code across 3 files.
