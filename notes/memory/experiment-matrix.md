# Planet-Scale Memory Experiment Matrix (64 GB Target)

## Goal
- Make planet-scale ingest + merge reliable on a 64 GB RAM host.
- Optimize for **peak RSS stability** first, throughput second.
- Validate each change with measured impact before proceeding.

## Success Criteria
- No OOM or swap-thrashing on full planet merge workflows.
- Peak RSS stays below an agreed safety ceiling (recommended: <= 52 GB).
- Throughput regression budget per accepted change: <= 10% unless explicitly approved.

## Measurement Protocol (applies to every experiment)
1. Capture:
- Peak RSS (`VmHWM`)
- Wall time
- CPU utilization
- Input/output throughput
- Rewrite ratio (passthrough vs rewrite blobs)
2. Run each scenario at least 3 times and report min/median/max.
3. Record commit hash, hostname, dataset/diff set, and exact command line.
4. Keep benchmark runs strictly sequential (no parallel benchmark/verify execution).

## Datasets / Scenarios
- `S1` Denmark + 1 daily diff (sanity/fast iteration)
- `S2` Germany + 7 daily diffs (mid-scale stress)
- `S3` North America + 7 daily diffs (high memory stress)
- `S4` Planet + 1 daily diff (production baseline)
- `S5` Planet + 7 to 30 coalesced daily diffs (backlog stress)

## Phase 0: Baseline and Attribution
### E0.1 Memory attribution by pipeline phase
- Hypothesis: Peak RSS is dominated by `DiffOverlay` + merge batch/rewrite buffering.
- Method: instrument phase boundaries (OSC parse, classify, rewrite, writer drain) and log phase-local high-water marks.
- Metrics: per-phase RSS delta, global `VmHWM`.
- Exit criteria: clear top-2 memory contributors identified.

### E0.2 Blob-size and rewrite-ratio sensitivity
- Hypothesis: RSS spikes correlate with high `raw_size` blobs and high rewrite ratio windows.
- Method: capture p50/p95/p99 blob raw_size and rolling rewrite ratio during merge.
- Metrics: RSS vs blob-size/rewrite-ratio correlation.
- Exit criteria: quantified trigger thresholds for memory spikes.

## Phase 1: High-ROI, Low-Risk Refactors
### E1.1 Compact DiffOverlay model — DONE (commit 1d291f1)
- Hypothesis: replacing object-heavy OSC representation yields the largest RSS reduction.
- Change:
- Replace per-entity heap-heavy structs with arenas + `id -> offset` indexes.
- Intern strings (keys/values/roles) across entire diff load.
- Convert node coords to `i32` decimicro at parse time.
- Metrics: peak RSS, overlay heap size estimate, parse time.
- Exit criteria: >= 25% peak RSS reduction on `S3` or `S4` with <= 10% time regression.
- **Result (S2 Germany):** RSS 710→652 MB (-8.2% zlib), 635→601 MB (-5.4% none). Overlay heap 60→26 MB (-56%). Time within budget. Decision: KEEP.

### E1.2 Replace inline upsert `Vec` copies with range views — DONE (commit 041d79f)
- Hypothesis: eliminating `to_vec()` per rewrite job reduces alloc churn in rewrite-heavy windows.
- Change: `RewriteJob.inline_upserts: Vec<i64>` → `upsert_range: (usize, usize)` indexing into shared `DiffRanges` arrays. `DiffRanges` wrapped in `Arc` for `rayon::spawn` closures. Eliminates per-job heap allocation entirely.
- Metrics: allocation count, peak RSS, rewrite phase wall time.
- Exit criteria: measurable allocation drop and non-negative throughput.
- **Result (S2 Germany):** Throughput and RSS within noise of E1.3 baseline (no regression). Benefit is structural: eliminates ~11K heap allocations per Germany merge (2.3M at planet scale), removes cross-thread alloc/dealloc churn. Decision: KEEP.

### E1.3 Bound DecompressPool retention — DONE (commit b9da254)
- Hypothesis: unbounded pooled buffers retain worst-case capacities and inflate tail RSS.
- Change:
- Single capacity threshold (`MAX_RETAINED_CAPACITY = 4 MB`) on `put()`: drop oversized buffers instead of recycling.
- Count cap (`MAX_POOL_SIZE = 64`) as defense-in-depth.
- Capacity check is before the mutex lock — zero overhead for typical buffers.
- Metrics: post-spike RSS decay behavior, allocation churn, throughput.
- Exit criteria: improved RSS recovery with <= 5% throughput regression.
- **Result (S2 Germany):** Throughput and RSS within noise of E2.2 baseline (no regression). Benefit is structural: prevents outlier blobs (up to 32 MB) from permanently inflating pool memory. Worst-case pool retention drops from 41×16 MB = 656 MB to 41×4 MB = 164 MB. Decision: KEEP.

## Phase 2: Bounded In-Flight Redesign
### E2.1 Adaptive batch sizing by bytes (not blob count) — DONE (commit e1099c4)
- Hypothesis: fixed `BATCH_SIZE=64` over-allocates in high-raw-size windows.
- Change: drive in-flight limit by estimated bytes budget (frames + decoded + rewrite outputs).
- Metrics: peak RSS stability across `S3-S5`, throughput.
- Exit criteria: materially lower peak RSS on stress scenarios with acceptable throughput loss.
- **Result (S2 Germany):** RSS 652→532 MB (-18.4% zlib), 601→388 MB (-35.4% none). Time 6381→5728 ms (-10.2% zlib). Decision: KEEP.
- **Cumulative E1.1+E2.1 vs original:** RSS 710→532 MB (-25.1% zlib), 635→388 MB (-38.9% none). Time -9.4% zlib, -20.8% none.

### E2.2 Stream rewrite outputs to writer — DONE (commit 1e03e5b)
- Hypothesis: collecting all rewrite outputs before phase 4 causes avoidable peak memory.
- Change: replace par_iter().collect() with rayon::spawn per job + streaming drain loop. Each rayon task owns its RewriteJob (PrimitiveBlock freed on task completion). Main thread receives results via bounded channel, processes slots in file order.
- Metrics: rewrite phase RSS, end-to-end `VmHWM`, channel backpressure behavior.
- Exit criteria: rewrite-window RSS reduced without ordering regressions.
- **Result (S2 Germany):** RSS 532→515 MB (-3.2% zlib), 388→390 MB (+0.6% none). Time 5728→5335 ms (-6.9% zlib), 3710→3420 ms (-7.8% none). Decision: KEEP.
- **Cumulative E1.1+E2.1+E2.2 vs original:** RSS 710→515 MB (-27.5% zlib), 635→390 MB (-38.6% none). Time -15.6% zlib, -27.0% none.

## Phase 3: Writer/Framing Memory Tightening
### E3.1 Buffer recycling pool for pipelined writer — DROPPED
- Hypothesis: removing final frame concatenation lowers transient allocations.
- Change: `Arc<Mutex<Vec<Vec<u8>>>>` buffer pool with capacity-capped retention (2 MB max, 16 buffers). `frame_blob_into` accepts `&mut Vec<u8>` out-param. Writer thread recycles buffers after write. Approach F from `notes/memory/p6-vectored-writer-framing.md`.
- Metrics: allocation volume, writer thread RSS, throughput.
- Exit criteria: lower allocator pressure and neutral/improved throughput.
- **Result (S1 Denmark, S2 Germany, dirty tree):** Regression across the board. Germany zlib: 5627ms/545 MB (+5.5%/+5.8%). Germany none: 3842ms/417 MB (+12.3%/+6.9%). Mutex contention on pool acquire/release offsets reuse benefit. Large blobs (>2 MB) rejected by capacity cap, paying lock overhead without reuse. Decision: DROP.

### E3.2 Coalescing policy tuning under memory budget
- Hypothesis: passthrough coalescing can overshoot memory target during long passthrough streaks.
- Change: dynamic flush threshold based on live memory budget rather than static behavior.
- Metrics: peak RSS, write syscall count, throughput.
- Exit criteria: RSS cap adherence with acceptable syscall overhead.

## Phase 4: Bigger Architectural Bets (Only if Needed)
### E4.1 Sharded diff application by ID ranges
- Hypothesis: partitioning working set by ID range lowers live diff footprint.
- Cost: high implementation complexity.
- Exit criteria: significant memory improvement beyond Phase 1-3 gains.

### E4.2 Disk-backed diff payload index
- Hypothesis: offloading cold diff payloads to disk can hard-cap RAM on backlog merges.
- Cost: potential random I/O penalties.
- Exit criteria: stable 64 GB behavior for `S5` even with large diff backlogs.

### E4.3 Two-pass merge planner/executor mode
- Hypothesis: planning first, executing second gives strict memory control.
- Cost: extra full-file read and complexity.
- Exit criteria: fallback mode that guarantees memory envelope when standard mode fails.

## Execution Order (Recommended)
1. `E0.1`, `E0.2` (establish attribution and guardrails)
2. `E1.1` (largest likely RAM win)
3. `E1.2`, `E1.3` (cheap cleanup wins)
4. `E2.1`, `E2.2` (in-flight memory envelope control)
5. `E3.1`, `E3.2` (writer-side tightening)
6. `E4.*` only if still above target

## Decision Gates
- Gate A (after Phase 1): If peak RSS on `S4` <= target ceiling, proceed to throughput polish only.
- Gate B (after Phase 2): If `S5` still exceeds ceiling, move to Phase 3 immediately.
- Gate C (after Phase 3): If still unstable on `S5`, schedule one Phase 4 redesign with strongest expected ROI.

## Reporting Template (per experiment)
- Experiment ID:
- Commit:
- Host:
- Scenario:
- Peak RSS (`VmHWM`):
- Wall time:
- Rewrite ratio:
- Result vs baseline:
- Regressions/risks:
- Decision: keep / iterate / drop
