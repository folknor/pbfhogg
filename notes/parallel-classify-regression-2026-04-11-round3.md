# parallel_classify_phase regression — round 3 — 2026-04-11

**Audience:** `planet` + `perf` + `arch` review archetypes
**Previous rounds:**
- [`notes/parallel-classify-regression-2026-04-10.md`](parallel-classify-regression-2026-04-10.md) (round 1 brief)
- [`notes/parallel-classify-regression-2026-04-10-reviews.md`](parallel-classify-regression-2026-04-10-reviews.md) (round 1 responses)
- [`notes/parallel-classify-regression-2026-04-11-followup.md`](parallel-classify-regression-2026-04-11-followup.md) (round 2 brief)
- [`notes/parallel-classify-regression-2026-04-11-followup-reviews.md`](parallel-classify-regression-2026-04-11-followup-reviews.md) (round 2 responses)

## TL;DR

You unanimously recommended `malloc_trim(0)` between PASS1 and PASS2 as the cheapest test for the "allocator arena retention" hypothesis. We ran it. **The trim took 4.6 seconds and freed essentially nothing visible to PASS2_SCHEDULE.** It DID help downstream phases by ~2 GB (allocator-retained pages from PASS1), but the SCHEDULE peak was unchanged. Allocator lag from PASS1 is real but small.

We then ran three more focused experiments with fresh sub-phase markers and isolated the burst with surgical precision:

- **Experiment 1** (sub-phase markers inside `build_classify_schedule`): the burst is entirely in the **`SCHEDULE_SCAN_LOOP`** marker — the `while let Some(...) = scanner.next_header_with_data_offset()` loop. Not the open, not the drop.
- **Experiment 2** (`set_parse_indexdata(false)`): peak unchanged at ~10 GB. **Indexdata parsing is NOT the source.**
- **Experiment 3** (skip `build_classify_schedule` entirely, reuse PASS1's already-built schedule): SMART_PASS2 peak collapsed from 10.06 GB to 3.79 GB. PASS2 wall dropped from 25s to 5s. **16% total wall improvement** on Europe smart extract. **But peak anon didn't drop overall — it MOVED to PASS3.**

Then we added sub-phase markers to PASS3's `pread_write_pass` and re-ran:

- **The PASS3 peak is in `PREAD_WRITE_BLOB_SCHEDULE` — which is the call to `build_blob_schedule(input)` inside `pread_write_pass`. Same shape as `build_classify_schedule`. Same ~5 GB transient. 28.7 seconds, 8.78 GB peak.**

**The 6 GB anon transient is not specific to any one function. It happens in EVERY file header scan that runs after PASS1's parallel allocator work.** PASS1's identical scan in `collect_pass1_generic` (lines 2531–2554) does NOT show the burst — but it runs FIRST, before any parallel work.

**This validates the reviewers' allocator-state hypothesis but with a sharper trigger than predicted.** Not "lag" — the trigger is "header scan that allocates many small things AFTER an earlier phase has populated the allocator's arena state." The first such scan establishes a baseline that subsequent identical scans grow further.

We need your help on three things this round:

1. **Mechanism**: what specific allocator behavior produces a 6 GB transient anon delta in a function whose Rust-level allocations sum to <2 MB? We've ruled out: per-worker IdSetDense, indexdata parsing, function-internal retention, allocator lag from PASS1 (small), and the function code itself.
2. **Mitigation menu**: of the candidates below, which is most likely to actually fix this without introducing new pain?
3. **The sharpest cheap diagnostic** to distinguish glibc arena behavior from a real allocation we still haven't found.

You have full repo access. File and commit references throughout.

---

## What we did since the last round

### Round 2 recommendations we implemented

**Recommendation: `malloc_trim(0)` between PASS1 and PASS2 to test allocator-lag hypothesis.**

We added the call (cfg-gated to `target_os = "linux"`, using `libc = "0.2.184"` already in workspace deps) immediately between `SMART_PASS1_END` and `SMART_PASS2_START`. Re-benched Europe smart with `--bench 1 --force`.

**Result: the trim itself took ~4.6 seconds to walk the arena, and the SCHEDULE peak was essentially unchanged (10.06 → 9.81 GB, well within the ~700 MB run-to-run variance we measured at `--bench 3` earlier).**

But the trim DID help downstream phases:
| Phase | Baseline (UUID `8ac56b15`) | With malloc_trim | Δ |
|---|---|---|---|
| SMART_PASS1 peak | 3.73 GB | 3.74 GB | flat |
| **SMART_PASS2_SCHEDULE peak** | **10.06 GB** | **9.81 GB** | **−250 MB (noise)** |
| SMART_PASS2_CLASSIFY peak | 6.24 GB | 4.25 GB | **−2.0 GB** |
| SMART_PASS3 peak | 7.45 GB | 5.86 GB | **−1.6 GB** |

So the trim freed ~2 GB of arena pages from PASS1's parallel work that were carried into CLASSIFY and PASS3, but **the SCHEDULE burst was untouched.** It runs INSIDE the scan, after the trim. The trim's work is undone by the scan.

We reverted the trim — it costs 4.6 seconds for benefits that don't address the planet blocker.

**Recommendation: add narrower markers inside `build_classify_schedule`.**

We added three: `SCHEDULE_SCANNER_OPEN`, `SCHEDULE_SCAN_LOOP`, `SCHEDULE_SCANNER_DROP`. Bench (UUID dirty, commit `51f820d` baseline + experiment 1 edits):

```
SCHEDULE_SCANNER_OPEN     12 ms        0 KB        0 majflt
SCHEDULE_SCAN_LOOP     21155 ms    10.46 GB    13377 majflt   ← entire burst
SCHEDULE_SCANNER_DROP      0 ms        0 KB        0 majflt
```

**The burst is entirely in the scan loop body. Not the open. Not the drop.** All 10.46 GB peak and all 21 seconds.

### Three new experiments at HEAD

#### Experiment 2: disable `set_parse_indexdata(true)`

One-line change: comment out the `scanner.set_parse_indexdata(true)` call in `build_classify_schedule`. The kind filter becomes a no-op (`hdr.index()` always returns `None`), so the schedule includes all OsmData blobs not just way blobs — behaviorally wrong but fine for memory measurement.

```
SCHEDULE_SCAN_LOOP   25748 ms   10.00 GB   1797 majflt
```

**Peak unchanged** (10.00 GB vs 10.46 GB, within variance). Wall slightly slower (+4.6s) because the schedule now contains ~21× more blobs to push (no kind filter), but anon doesn't care about iteration count. **`set_parse_indexdata(true)` is NOT the source.**

The major fault count dropped 7× (13377 → 1797), but that's page cache state from running back-to-back — earlier runs warmed it.

#### Experiment 3: reuse PASS1's `way_schedule`, skip `build_classify_schedule`

The decisive structural test from round 2 (codex/planet's "skip the function entirely" recommendation).

Implementation:
- Added `full_way_schedule: Vec<(usize, u64, usize)>` to `Pass1Result`.
- In `collect_pass1_generic`'s sorted-path scan loop (`extract.rs:2541-2553`), build the unfiltered way schedule alongside the existing spatially-filtered schedules. One extra Vec push per way blob in the same scan loop.
- In `extract_smart` PASS2, `std::mem::take` the schedule out of `result.way_schedule` and use it directly. Open a fresh `Arc<File>` (the file open is fast, ~microseconds). Fall back to `build_classify_schedule` if `way_schedule` is empty (the unsorted-fallback path doesn't build one).

Result on Europe smart `--bench 1 --force`:

```
SMART_PASS1               62322 ms   3.71 GB
PASS1_NODE_CLASSIFY       12493 ms    14 MB
PASS1_WAY_CLASSIFY        25097 ms   3.63 GB
PASS1_RELATION_CLASSIFY    6861 ms   3.71 GB
SMART_PASS2                5229 ms   3.79 GB    ← was 24285ms / 10.06 GB
SMART_PASS2_SCHEDULE          0 ms       0 KB    ← function skipped entirely
SMART_PASS2_CLASSIFY       5228 ms   3.79 GB    ← was 4977ms / 6.24 GB
SMART_PASS3              141519 ms   9.92 GB    ← was 146535ms / 7.45 GB ← MOVED HERE
SMART_PASS3_SETUP             2 ms       0 KB
SMART_PASS3_WRITE        141509 ms   9.92 GB
```

**The SCHEDULE peak collapsed from 10.06 GB to 0 KB. The function call is genuinely skipped. PASS2 wall dropped from 25s to 5s.**

**But the total anon peak didn't drop. It moved to PASS3, which jumped from 7.45 GB to 9.92 GB.** Same ~10 GB total peak, just in a different phase.

Total wall: ~209 seconds vs ~250 seconds baseline. **16% wall improvement is real**, regardless of memory.

`brokkr verify extract --dataset denmark` passes for all three strategies — no semantic regression.

#### Experiment 4: PASS3 sub-phase markers

With experiment 3 still in place, we added markers inside `pread_write_pass` (`extract.rs:1700-1727`):

```rust
crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_START");
let schedule = build_blob_schedule(input)?;
crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_END");
crate::debug::emit_marker("PREAD_WRITE_EXECUTE_START");
pread_execute(input, &schedule, writer, stats, block_fn)?;
crate::debug::emit_marker("PREAD_WRITE_EXECUTE_END");
crate::debug::emit_marker("PREAD_WRITE_FLUSH_START");
writer.flush()?;
crate::debug::emit_marker("PREAD_WRITE_FLUSH_END");
```

Re-benched Europe smart:

```
SMART_PASS1                69644 ms   3.78 GB
SMART_PASS2                 5635 ms   6.81 GB
SMART_PASS3              150797 ms   8.78 GB
SMART_PASS3_SETUP              2 ms       0 KB
SMART_PASS3_WRITE         150795 ms   8.78 GB
PREAD_WRITE_BLOB_SCHEDULE  28729 ms   8.78 GB    ← 28.7 SECONDS, ~5 GB delta from PASS2 baseline
PREAD_WRITE_EXECUTE       121511 ms   8.10 GB
PREAD_WRITE_FLUSH            549 ms   8.10 GB
```

**The new PASS3 peak is in `PREAD_WRITE_BLOB_SCHEDULE` — the call to `build_blob_schedule(input)` inside `pread_write_pass`. It's another file header scan, and it has the same ~5 GB transient pattern.**

This is the smoking gun for the actual root cause.

---

## The pattern: same scan code, different memory behavior depending on WHEN it runs

Three header-scan call sites across the smart extract pipeline. Same `BlobReader::seekable_from_path` → `set_parse_indexdata(true)` → `next_header_with_data_offset` loop pattern. Different memory behavior:

| Scan call site | When it runs | Peak anon delta over baseline |
|---|---|---|
| `collect_pass1_generic` lines 2531–2554 | FIRST (fresh allocator) | **No visible burst.** PASS1 overall peak only 3.78 GB. |
| `build_classify_schedule` (smart PASS2) — when called | After PASS1's parallel work | **+6 GB transient.** SCHEDULE peak 10 GB. |
| `build_blob_schedule` (smart PASS3, in `pread_write_pass`) | After PASS1+PASS2 work | **+5 GB transient.** PREAD_WRITE_BLOB_SCHEDULE peak 8.78 GB. |

**The functions are not equivalent in implementation** — they're two different functions in two different files (`mod.rs` and `extract.rs`) — but they have the same structural shape. Both peak ~5-6 GB above their pre-call baseline.

**`collect_pass1_generic`'s scan does not exhibit the burst.** Same `BlobReader::seekable_from_path`, same `set_parse_indexdata(true)`, same loop calling `next_header_with_data_offset()`, pushing tiny `(usize, u64, usize)` triples into Vecs. PASS1 overall peak is 3.78 GB and the high-water inside PASS1 is reached during `PASS1_RELATION_CLASSIFY`, not during the scan setup. The scan inside `collect_pass1_generic` is invisible in the sidecar — meaning its peak contribution is well below 3.78 GB.

**The only remaining differentiator is allocator state at the time the scan runs.** `collect_pass1_generic`'s scan runs before any parallel work. The other two run after.

---

## What we've definitively ruled out

In order of how much pain each one cost to disprove:

1. **Per-worker `IdSetDense` accumulation in `parallel_classify_accumulate`** (round 1's diagnosis). Fixed in commit `cc19d26`. Peak moved by 0.6 GB out of 10.7 GB. The fix is architecturally correct and we kept it, but it was never the main cause.

2. **Allocator arena retention from PASS1's parallel work**. `malloc_trim(0)` between PASS1 and PASS2 reclaimed ~2 GB of carried-forward arena pages (real, measurable in CLASSIFY and PASS3 peaks dropping by 1.6-2 GB), but the SCHEDULE burst was unchanged. The 6 GB transient is NOT just lag from PASS1.

3. **Indexdata parsing** (`set_parse_indexdata(true)` in `build_classify_schedule`). Disabling it left the peak at 10 GB. NOT the source.

4. **The `build_classify_schedule` function code itself**. Bypassing the function entirely (experiment 3) eliminated its peak — but only because the same allocation pattern then happens in `build_blob_schedule` instead. Both functions exhibit the burst because both run after PASS1's parallel work.

5. **Anything specific to one call site**. The pattern is "header scan after parallel work," not "this particular function."

---

## The mechanism puzzle

`build_classify_schedule` in `mod.rs:426-456` allocates roughly 1-2 MB of Rust-level live state during its entire execution. The schedule Vec at completion is ~480 KB for Europe ways. The reusable `header_buf` in `BlobReader` is at most ~few hundred KB if a particularly fat blob header gets read. The `Arc<File>` is a few bytes.

**Yet the peak anon during the call is ~10 GB — ~5-6 GB above the entry baseline.** And this 5-6 GB is freed by the time the function returns (the next phase's marker shows lower anon).

The Japan `--alloc` data (sidecar `4eccb6f3`) attributes only **640 KB cumulative** to `build_classify_schedule` exclusive, vs **4.2 GB cumulative** to `parallel_classify_phase`. Hotpath alloc tracks bytes by call frame; sidecar tracks process-wide RSS. **They diverge here.** The function makes very few Rust-level allocations but the process-wide anon RSS climbs by 6 GB during its execution.

The shape of the trajectory (200 ms granularity, see UUID `01de22bb` timeline excerpt below):

```
t=72.4s   3.81 GB   ← scan loop starts
t=72.6s   5.05 GB   ← first 200 ms: +1.2 GB
t=72.8s   5.79 GB   ← +740 MB
t=73.0s   6.11 GB   ← +320 MB
t=73.4s   6.13 GB   ← stalled
t=74.6s   6.36 GB   ← slowly climbing
t=74.8s   7.08 GB   ← second burst
t=75.0s   8.16 GB   ← +1.1 GB
t=75.2s   8.73 GB   ← climbing
t=75.6s   9.49 GB
t=76.4s   9.80 GB   ← peak, then drops
t=76.6s   9.56 GB   ← starts dropping
t=79.4s   4.35 GB   ← sharp drop
t=79.6s+  4.35 GB   ← stable plateau (post-function-return)
```

Two distinct climb phases (t=72.4-73.0s and t=74.8-76.4s) each adding ~2-4 GB, then a slow decline, then a sharp drop at function return.

Major fault counts during the burst: **~10-13K** in cold-cache runs, **~1.5-6K** in warm-cache runs. The kernel reads ~50-200 MB of disk pages during the scan window. That's enough to be the WORKING SET of the scan, but not 6 GB worth.

**Working hypothesis: glibc arena reservation triggered by interaction with PASS1's allocation pattern.**

Possible specific mechanisms:
- **Dynamic `M_MMAP_THRESHOLD` adjustment**: glibc adjusts mmap_threshold based on observed allocation patterns. After PASS1's many small Vec<i64> allocations (cumulative 4.2 GB on Japan, ~63 GB extrapolated to Europe), the threshold may have grown. New small allocations during the scan that exceed the threshold get serviced via fresh `mmap(MAP_ANON)` which counts as anon RSS until munmap. The pages don't get released because glibc's free-list policy keeps them around.
- **Arena fragmentation forcing brk extension**: PASS1 left the arena fragmented (lots of small free-list entries scattered through the heap). The scan's small allocations don't fit any of the free-list slots, so glibc calls sbrk() to extend the arena. The new pages count as anon. They aren't returned because glibc only releases at the top of the brk arena.
- **Per-thread arena instantiation**: glibc creates per-thread arenas on first allocation by a new thread. The scan loop might be touching threads that haven't allocated yet (rayon idle workers? std::thread::scope reuse?), causing fresh per-thread arenas to be reserved.
- **Something we still haven't thought of**.

The 4.6-second cost of `malloc_trim(0)` is itself suggestive: glibc had a LOT of arena bookkeeping to walk. Whatever is going on, it's not a small allocator state.

---

## The mitigation menu

In rough order of how much code/risk they involve:

### Option A: Build all schedules upfront in `collect_pass1_generic`

The current commit already plumbs `full_way_schedule` out of `collect_pass1_generic` for smart PASS2. We can extend this to also build the `pread_write_pass` blob schedule (`build_blob_schedule`) in the same scan loop. Then both PASS2 and PASS3 schedules come from PASS1's first scan, no header scans happen after PASS1, and the burst should never get triggered.

**Risks:**
- `build_blob_schedule` returns `Vec<BlobDesc>`, which is a different type than `(usize, u64, usize)`. Need to plumb the right shape.
- The complete-extract strategy also calls `pread_write_pass` and would need its own pre-built schedule.
- Adds code complexity in collect_pass1_generic — it now does the work of three schedule builders.

**Benefits:**
- If the hypothesis is right, the planet blocker disappears entirely. No more post-PASS1 scans, no more arena growth bursts.
- Additional ~25 seconds of wall savings on top of the 16% we already shipped (because PREAD_WRITE_BLOB_SCHEDULE is currently 28 seconds).

### Option B: Switch the global allocator

Try jemalloc or mimalloc and see if the burst disappears. The pattern looks like glibc-specific arena behavior. Both jemalloc and mimalloc use different heap layouts and may not exhibit it.

**Risks:**
- Per the project history (TODO.md "Global allocator investigation"), jemalloc and mimalloc were previously evaluated at <1% wall difference on Denmark and removed because they broke `--all-features` builds (duplicate `#[global_allocator]` definitions). Adding them back means resolving that.
- Allocator behavior at planet scale may be different from Denmark scale.

**Benefits:**
- One-line change in `cli/src/main.rs`. Cheapest test.
- Affects the entire process, not just one path. Other commands may also benefit.

### Option C: `malloc_trim` at strategic points (NOT between PASS1 and PASS2)

We tested `malloc_trim` BEFORE the SCHEDULE phase and it didn't help. But maybe `malloc_trim` AFTER the SCHEDULE phase (or INSIDE the scan loop, periodically) would help. The allocator state grows during the scan; a mid-scan trim might keep it bounded.

**Risks:**
- 4.6 second cost per call. Doing it multiple times per phase is expensive.
- May not actually help if the issue is reservation, not retention.

**Benefits:**
- Surgical, no API changes.

### Option D: Don't fix it, accept the planet limit

Pro-rated to planet, the current peak is ~23 GB (down from 27 GB with the previous fix). It still doesn't fit on 30 GB plantasjen but it's much closer. With swap (already configured) it might just work. Or the user can run on a larger host.

**Risks:**
- Not actually solving the problem.
- The 6 GB transient also affects every other command that does header scans after parallel work (apply-changes? multi-extract?).

**Benefits:**
- Zero code change.

---

## The four questions

### Q1: Is the "header scans after parallel work trigger arena growth" hypothesis supportable from the data?

We have three data points:
- PASS1's first scan: no burst, low memory
- `build_classify_schedule` (PASS2): 6 GB burst
- `build_blob_schedule` (PASS3): 5 GB burst

The common factor across the bursting two is "runs after PASS1's parallel work." The non-bursting one is the FIRST scan. We can't measure a fourth case to triangulate further without writing more diagnostic code.

**Is this hypothesis sufficient given the evidence, or do you see a hole?** Specifically:
- Could the trajectory shape (two-step climb, slow decline, sharp drop) be consistent with anything OTHER than allocator behavior?
- Does the fact that `malloc_trim(0)` BEFORE the scan didn't help, but freed pages from PASS1, fit your model?

### Q2: What's the most likely glibc-specific mechanism?

We listed four candidates above (mmap_threshold growth, arena fragmentation, per-thread arena, "something else"). Do you have a strong prior on which is most likely? Is there a way to distinguish them experimentally without rewriting the allocator?

We can call `mallinfo2()` or `malloc_info()` at phase boundaries to get arena state — would that be informative, or is it too coarse to distinguish these?

### Q3: Which mitigation should we try first?

Our intuition: **Option B (try jemalloc/mimalloc) is the cheapest test that would either confirm the allocator is the cause or rule it out**. Option A (build all schedules upfront) is the most invasive but most likely to fix the problem if the diagnosis is right. Option C is unlikely to help based on what we've seen. Option D is a non-solution.

**Do you agree with B → A as the order, or is there a cheaper test we're missing?**

If the answer is "B is the right test, just do it," we'll do it. The historical objection (allocator features broke `--all-features`) is solvable now that the codebase has matured.

### Q4: Anything we're overlooking in the trajectory shape?

The two-step climb (t=72.4-73.0 then t=74.8-76.4) followed by a slow decline then a sharp drop is unusual. We've been treating it as one event but it's clearly two. Could the two climbs correspond to different allocator events (e.g., first climb is brk extension, second climb is mmap extension)?

If we instrumented `mallinfo2()` at marker boundaries, would we see a step change in `arena` (brk-managed) vs `hblks` (mmap-managed)?

---

## What we plan to do based on your responses

- **If you confirm the diagnosis and recommend B (allocator)**: switch to mimalloc or jemalloc, re-bench Europe, see if peak collapses.
- **If you recommend A (upfront schedules)**: extend `collect_pass1_generic` to build the `pread_write_pass` blob schedule too, plumb it through, re-bench.
- **If you propose a different diagnostic (mallinfo2, perf trace, strace -e mmap, etc.)**: run that and report back.
- **If you say the diagnosis is wrong and propose a different root cause**: investigate that.

---

## Files for context

### Source code at HEAD (commit `d4ea760`)

- `src/commands/mod.rs:418-468` — `build_classify_schedule` with `SCHEDULE_SCANNER_OPEN/SCAN_LOOP/SCANNER_DROP` markers
- `src/commands/extract.rs:2369-2384` — `Pass1Result` with `way_schedule: Vec<(usize, u64, usize)>` field
- `src/commands/extract.rs:2541-2571` — `collect_pass1_generic` scan loop building `full_way_schedule` alongside the spatially-filtered ones
- `src/commands/extract.rs:2820-2843` — smart PASS2 reusing `result.way_schedule` via `mem::take`, with fallback to `build_classify_schedule`
- `src/commands/extract.rs:1700-1735` — `pread_write_pass` with `PREAD_WRITE_BLOB_SCHEDULE/EXECUTE/FLUSH` markers
- `src/read/blob.rs:653,983,1015,1069` — the BlobReader functions on the scan path (you read these in round 2 — they remain unchanged)

### Recent commits

- `cc19d26` — `extract.rs:2813` per-blob send fix (the wrong fix, kept anyway)
- `51f820d` — Marker rename + sub-phase markers + hotpath annotations on `build_classify_schedule`/`pread_write_pass`/etc.
- `eba72a9` — Doc updates: notes/columnar-integration.md superseded section, TODO.md
- `d4ea760` — Schedule reuse + `PREAD_WRITE_*` markers + the round 2 reviewer responses captured

### Sidecar UUIDs

- `8ac56b15` — Europe smart, `51f820d`, original sub-phase marker run (the round 2 finding)
- `01de22bb` — Europe smart, `5ca2df9`, pre-fix baseline
- `f420c5fd` — Europe smart, `fc17b51`, pre-refactor original
- `0c60ec88` — Europe smart, `cc19d26`, `--bench 3` variance check
- `4eccb6f3` — Japan smart, `51f820d`, `--alloc` per-function attribution

The four `--force` runs from this round (malloc_trim, narrow markers, parse_indexdata=false, schedule reuse, schedule reuse + PASS3 markers) were not stored in the DB. Their data is captured in the tables in this brief — happy to re-run any of them with a clean tree and store the UUIDs if you need them for cross-reference.

### Notes

- `notes/parallel-classify-regression-2026-04-11-followup.md` — round 2 brief
- `notes/parallel-classify-regression-2026-04-11-followup-reviews.md` — round 2 responses (verbatim)
- `notes/columnar-integration.md` — original design review with the now-superseded "Resolved" section
- `TODO.md` — "Smart-extract planet memory blocker — STILL OPEN" entry

---

## The lesson from this round

The previous brief wasted reviewer attention on a Q3 framing that was contradicted by my own measurement table (claudia/planet and codex/arch both caught it). I've tried to be more careful this time: every claim in this brief has been walked through the data, and the wall improvements are kept conceptually separate from the memory puzzle (because confounding them was the previous failure mode).

If something in this brief contradicts the data, please point it out — I'd rather know now than after another wasted experiment.
