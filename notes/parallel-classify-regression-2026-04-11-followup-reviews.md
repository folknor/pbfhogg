=== planet ===

--- claude ---
OK, `handler` is still alive at PASS2 start (referenced via `extra_way_ids_ref` on line 2826), so nothing about it is freed.

**One interesting observation I just noticed:** the new per-blob-send classify at line 2848 does `scratch.drain(..).collect::<Vec<i64>>()`. This **creates a fresh Vec** from the scratch's contents, then returns it. The scratch itself is retained as worker state (line 2837: `Vec::<i64>::new`). But every call creates a new Vec from drain + collect. This is a new allocation per blob in the hot path.

Actually wait — `scratch.drain(..).collect::<Vec<i64>>()` allocates a new Vec with the exact size, then moves elements from scratch into it. The scratch is cleared (drain removes all), but its capacity is retained. The returned Vec is a fresh allocation. **So the fix you shipped at extract.rs:2813 introduces per-blob Vec allocation in the return path.**

At 20K way blobs, at average ~100 refs per matched way × ~50 matched ways per blob = ~5000 i64s × 8 bytes = 40 KB per returned Vec. ×20K blobs = ~800 MB of cumulative Vec allocation throughput (small, quickly freed). Not 6 GB, and small-allocation so won't show in anon peak, but notable as a missed optimization.

This also doesn't explain the 6 GB.

### Moving to the actual answer

Honestly, after reading the code carefully, I can't find a source of 6 GB allocation in `build_classify_schedule` or its callees. The function body is too simple. I have to default to **diagnostic recommendations** rather than a confident root cause.

**The cheapest diagnostic by far** (which the brief mentions in Q3): temporarily disable `set_parse_indexdata(true)` in `build_classify_schedule` and re-bench. Yes, this changes behavior (no way kind filter), but for a 2-minute Europe bench just to see whether the SCHEDULE peak drops. **If peak drops to ~4 GB, indexdata parsing is somehow responsible.** If peak stays at 10 GB, indexdata parsing is NOT the source.

Even cheaper than that: **add a sub-phase marker pair immediately around just the scan loop** and rerun. Commit a new marker:

```rust
crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
let mut scanner = BlobReader::seekable_from_path(input)?;
scanner.set_parse_indexdata(true);
scanner.next_header_skip_blob().ok_or(...)??;
crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
let mut schedule: Vec<(usize, u64, usize)> = Vec::new();
let mut seq: usize = 0;
while let Some(result_item) = scanner.next_header_with_data_offset() {
    ...
}
crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_END");

crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_START");
drop(scanner);
let shared_file = Arc::new(File::open(input)?);
crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_END");
```

Re-bench Europe smart. One of these three sub-phases will show the 6 GB peak. If it's OPEN: suspect `BufReader::with_capacity(256 * 1024, f)` or File::open allocations. If it's SCAN_LOOP: the per-header parse IS allocating despite my analysis. If it's DROP: the scanner drop or Arc construction is doing something weird.

**My strong prior: the peak will NOT be in the scan loop itself. It will be in a side effect of OPEN or DROP, or in something outside the marker scope that's getting attributed to it due to the 100ms sidecar sampling granularity.**

### Possibility C expanded: sidecar attribution

The sidecar samples every 100 ms. The marker-to-phase attribution uses "max anon during the interval [START_marker_ts, END_marker_ts]". If a phase is 100 ms long, it has at most one sample. If a phase is 19 seconds long, it has ~190 samples.

**What if the 6 GB transient is from kernel-side page cache manipulation, not application heap?** posix_fadvise(DONTNEED) from the previous phase's workers hitting the PBF file could cause page cache churn that shows up on the /proc/self/status RssAnon field? No — page cache is RssFile, not RssAnon. Rule out.

**What if it's from a memory-mapped file being touched?** mmap'd files show up in RssFile, not RssAnon. Rule out.

**What if glibc is calling sbrk() to grow the arena during build_classify_schedule and touching the new pages?** This would show as anon RSS growth. The question is: why would glibc sbrk() during this phase?

glibc grows the arena via sbrk when a small-allocation request can't be satisfied from existing free lists. During PASS1, the arena was pushed to its high-water mark. At PASS1 end, the arena has many coalesced free blocks. During PASS2 SCHEDULE, any small allocation can be satisfied from these free blocks WITHOUT growing the arena.

**Unless** the small allocations in SCHEDULE are large enough to exceed mmap_threshold. glibc's dynamic mmap_threshold starts at 128 KB and can grow to 32 MB based on observed allocation patterns. If PASS1 saw lots of 4 MB allocations (IdSetDense chunks), glibc may have set mmap_threshold ≈ 4 MB. Then a 1 MB allocation in PASS2 goes through the arena (brk), not mmap. If the arena is fragmented and doesn't have a contiguous 1 MB block, sbrk() is called to extend.

This is getting too speculative. **I can't confidently diagnose this from static reading.**

### Answer to Q3 — Should you add narrower hotpath annotations?

Yes, but **use the cheaper marker-based approach first** (the sub-phase markers I outlined above). Hotpath annotations track allocations per function frame and require `--alloc` mode which is expensive. Adding three sub-phase markers costs nothing at runtime and runs in normal `--bench` mode.

The specific recommendation: **add the 3 sub-phase markers (OPEN, SCAN_LOOP, DROP) in `build_classify_schedule`, rebuild, and run Europe smart `--bench 1` with the sidecar.** This isolates which third of the function is responsible for the 6 GB.

If the sub-markers show the peak is in SCAN_LOOP (which I doubt), THEN add hotpath annotations on `WireBlobHeader::parse`, `read_blob_header`, and `next_header_with_data_offset`.

If the sub-markers show the peak is NOT in any of the three sub-phases (i.e., transient growth is spread across all three), then the root cause is not in `build_classify_schedule` — it's in something happening concurrently or outside the marker scope. In that case, investigate sidecar attribution itself: look at the raw timeline samples around the SCHEDULE marker and see when anon actually grew. The sidecar has per-sample data at 100ms granularity.

**Alternative cheap diagnostic: call `malloc_trim(0)` just before `SMART_PASS2_START` and re-bench.** If anon drops significantly, the "transient" is glibc arena bookkeeping and can be completely resolved by a `malloc_trim` call at phase boundaries.

```rust
#[cfg(target_os = "linux")]
unsafe { libc::malloc_trim(0); }
crate::debug::emit_marker("SMART_PASS2_START");
```

`malloc_trim(0)` forces glibc to return all free pages at the top of the arena to the OS (via `sbrk(-n)`). If this drops the PASS2 SCHEDULE peak from 10 GB to ~4 GB, **the 6 GB is glibc arena waste and the fix is to call `malloc_trim` periodically or at phase boundaries.** If it has no effect, the 6 GB is real allocation.

**This is the experiment I'd run first.** One line change, one re-bench, definitive answer on whether the root cause is live allocation or allocator bookkeeping.

### Answer to Q4 — Should you keep the `extract.rs:2813` fix?

**Keep it.** The architectural rationale remains valid independent of the planet-scale puzzle:

1. `parallel_classify_accumulate` over an unbounded per-worker `IdSetDense` is unsound in principle. The 10.72 GB PASS2 peak in the pre-fix measurement wasn't fully from the accumulate path (we now know), but some fraction of it was — the fix dropped peak by 0.6 GB and wall by 23%. Real improvements.

2. The chunk-spread model I endorsed yesterday was correct in structure but wrong about the dominant contributor to PASS2 peak. I apologize for the overconfidence — I should have questioned "10 GB of IdSetDense from one call site" harder. Your measurement debunks it. The fix is still architecturally correct for the workload.

3. Reverting would re-introduce the per-worker IdSetDense accumulation with no benefit. The puzzle we're now investigating exists independent of the fix.

4. The fix makes the code MORE correct (matches the two-parameter pattern used for dense paths) and does not regress anything.

I was wrong about the fix being the planet blocker solution — we now know it's not. But it's still the right fix for the site. Keep it and update `notes/columnar-integration.md` to note the chunk-spread model is too pessimistic for this specific workload type (selective filters), and that the PASS2 memory issue is still unresolved and under investigation.

### Regarding my previous overconfidence

My last review said the chunk-spread model was "confirmed" for extract-smart and that the 10.72 GB peak matched 6 workers × ~1.5-1.8 GB. I was wrong. The measurement now shows extract.rs:2813's classify phase peaks at 6.24 GB — only about 2.5 GB above the PASS1 baseline — NOT the 10.72 GB I attributed to it.

The real 10.06 GB peak is during SCHEDULE, which I didn't investigate and couldn't have predicted from the previous brief's data. The previous brief's data showed PASS2 at 10.72 GB but didn't have sub-phase markers to localize it. I shouldn't have assumed the 10.72 was all in the classify call.

**Lesson for future reviews:** when a measurement shows a ~10 GB peak, ask "can we get a marker inside this phase to localize?" before assuming the cause. The sub-phase markers you added are exactly the right diagnostic move — they should have been step 1, not step 5.

### Recommended next actions, in order

1. **Run the `malloc_trim(0)` experiment first.** One line before `SMART_PASS2_START`. Re-bench Europe smart `--bench 1`. If peak drops to ~4 GB: glibc arena waste, fix is `malloc_trim` at phase boundaries. If unchanged: live allocation, continue to step 2.

2. **Add 3 sub-phase markers inside `build_classify_schedule`** (SCANNER_OPEN, SCAN_LOOP, SCANNER_DROP). Re-bench. Isolate which third of the function is responsible.

3. **If sub-markers show SCAN_LOOP has the peak:** I'm wrong about the code being clean. Add hotpath annotations to `WireBlobHeader::parse` and `read_blob_header`. Run Japan `--alloc` to see per-function alloc attribution inside the scan.

4. **If sub-markers show OPEN or DROP has the peak:** investigate those specifically. `BufReader::with_capacity(256 * 1024, f)` is 256 KB; `File::open` is stack. Neither should allocate GB. If the peak is in DROP, look at what happens when the scanner is dropped — possibly the BlobReader's header_buf grew very large and is being returned to the allocator.

5. **If sub-markers show the peak is NOT in any of the three sub-phases** (transient growth is somehow outside the marker granularity): dump raw sidecar timeline samples around SCHEDULE with `brokkr results 8ac56b15 --timeline --fields t,anon --every 1 --phase SMART_PASS2_SCHEDULE`. Look for the shape: is it a step (1 sample ×6 GB), gradual growth (60 samples × 100 MB), or spiky? The shape distinguishes attribution artifact from real growth.

**The malloc_trim experiment is the single cheapest test and will resolve this in one run.** Do it first.

### Updating the design notes

Regardless of the outcome, update `notes/columnar-integration.md` with a correction note:

> **2026-04-11 correction:** The chunk-spread argument used to justify converting extract.rs:2813 from accumulate to per-blob send was partially wrong. Measurement with sub-phase markers (commit 51f820d, UUID 8ac56b15) showed the classify phase peak is only 6.24 GB, not 10.72 GB. The fix improved wall by 23% but only moved peak by 0.6 GB. The remaining ~3.8 GB transient during PASS2 is in `build_classify_schedule` and is under investigation (see notes/parallel-classify-regression-2026-04-11-followup.md). The architectural rationale for the fix (per-worker IdSetDense over unbounded workloads is unsound) remains valid, but the planet-blocker framing was incorrect.

--- codex ---
**Findings**

1. Yesterday’s main diagnosis is overturned. The new marker data makes it clear that [`extract.rs:2813`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L2813) was not the dominant source of the 10 GB burst. Converting it from [`parallel_classify_accumulate`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L561) to [`parallel_classify_phase`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L453) only moved the peak by about `0.6 GB`, so the old “worker-local `IdSetDense` explains the whole blowup” theory is no longer credible.

2. Code inspection strongly argues that [`build_classify_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L426) is **not** directly retaining 6.3 GB of live heap.
- The returned schedule is tiny.
- [`BlobReader::seekable_from_path`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L1058) allocates a `BufReader` with a `256 KB` buffer.
- [`read_blob_header`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L658) reuses one `header_buf`.
- [`WireBlobHeader::parse`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L216) stores `indexdata` inline as a `[u8; 42]`; `parse_tagdata` is off, so there is no per-blob `Box<[u8]>` churn from tagdata.
- [`BlobHeader::index`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L560) deserializes into a stack-sized `BlobIndex`, not a heap object.

There is no obvious retained-per-blob state on that path that could multiply to gigabytes.

3. The follow-up’s biggest unresolved puzzle is real: the same header-scan pattern inside [`collect_pass1_generic`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L2445) does **not** show the 6 GB transient, while the PASS2 schedule marker does. That is strong evidence that the schedule marker is catching process-wide heap behavior that happens during that window, not a simple “this function stores 14 KB per blob” bug.

4. The Japan `--alloc` results and the Europe sidecar are not contradictory once you separate cumulative allocation from peak RSS.
- Japan `--alloc`: [`parallel_classify_phase`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L453) dominates cumulative allocation.
- Europe sidecar: the 10 GB **peak anon** lands during `SMART_PASS2_SCHEDULE`.
That means the culprit is likely “few large live mappings / retained heap during that time window,” not “the schedule loop is doing lots of cumulative allocation.”

5. The current comment above [`extract.rs:2813`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L2821) is now misleading. It still says the accumulate variant there “blew up to 10.7 GB on Europe,” but the new evidence says that was only a minor component. The architectural rationale for the change is still fine; the causal claim is not.

**Assessment**

**Q1: Where is the 6.33 GB transient?**  
From the code, not in any scanner-held per-blob state inside [`build_classify_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L426) or the `BlobReader` header path.

High-confidence negative conclusions:
- not `set_parse_indexdata(true)` by itself
- not `BlobIndex::deserialize`
- not `BlobHeader` retention
- not `WireBlobHeader::parse` retaining tagdata
- not the schedule vector

So the most plausible classes of explanation now are:

1. Heap retention / arena behavior during that time window, not caused by schedule objects themselves.  
2. Marker attribution catching some other process-wide effect that happens while `SMART_PASS2_SCHEDULE` is open.  
3. A hidden allocation source outside the code you initially suspected, but still inside the same bracket.

If I had to bet on one family, I’d bet on allocator-retained heap behavior, not a logical “scanner caches all headers” bug. The code does not support the caching theory.

**Q2: Why doesn’t `collect_pass1_generic` show the same transient?**  
Because this strongly suggests the header scan itself is not the root cause.

`collect_pass1_generic` has essentially the same scan shape:
- same seekable reader
- same `set_parse_indexdata(true)`
- same `next_header_with_data_offset()` loop

If that scan intrinsically created a 6 GB transient, PASS1 would show it too. It doesn’t. That means the right framing is no longer “why does the same scan allocate differently,” but “what other process-wide memory behavior happens during PASS2 schedule and not during PASS1 schedule?”

That is the most important update from the follow-up.

**Q3: Should you add narrower hotpath annotations on the BlobReader path?**  
Only if you want to close the case formally, but I don’t think that is the fastest next diagnostic.

Given the code, I would do these experiments before adding more hotpath instrumentation:

1. `malloc_trim(0)` immediately before `SMART_PASS2_SCHEDULE_START`.  
If the 10 GB peak collapses, the issue is allocator-retained heap from prior work, not schedule-build live state.

2. Reuse the way schedule from PASS1 as a one-off diagnostic.  
[`collect_pass1_generic`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L2494) already builds `way_schedule`. If you temporarily plumb that out and skip [`build_classify_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L426) in smart PASS2:
- if the 10 GB peak disappears, the trigger is truly in the schedule window
- if it does not, the schedule marker was incidental

3. Add allocator stats, not more hotpath first.  
`mallinfo2`, `malloc_info`, or even a coarse “heap bytes mapped before/after schedule” snapshot will tell you more than another exclusive alloc counter, because the code path itself already looks too small.

I would only annotate deeper into:
- [`BlobReader::seekable_from_path`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L1058)
- [`BlobReader::next_header_with_data_offset`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L1015)
- [`WireBlobHeader::parse`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L216)

if the allocator-trim / schedule-reuse experiments do not resolve it.

**Q4: Keep `cc19d26` or revert it?**  
Keep it.

Reason:
- it is still the architecturally correct API choice for an unbounded dense path
- it improved PASS2 wall materially
- it did not make memory worse
- reverting it would not restore the missing 6 GB, because that was never mainly from that call site

What should change is the narrative:
- keep the fix
- remove the claim that it solved or should solve the planet blocker
- continue the investigation from the new evidence

**Recommended next steps**

1. Keep `cc19d26`.
2. Update the comment/note around [`extract.rs:2813`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L2821) so it no longer claims that call site explained the 10.7 GB burst.
3. Run one of these two diagnostics next:
- `malloc_trim(0)` before `SMART_PASS2_SCHEDULE`
- reuse PASS1’s `way_schedule` to eliminate schedule build entirely
4. Only if those fail to explain it, instrument deeper inside `BlobReader`.

**Bottom line**

The follow-up successfully disproves the old chunk-spread diagnosis for the smart-extract peak. It does **not** show that `build_classify_schedule` is actually allocating 6 GB of live state. Code inspection points the other way: the header-scan path is too small and too non-retaining for that. The most likely next target is allocator-retained heap or some other process-wide effect that happens during the schedule window, not the schedule data structures themselves.

=== perf ===

--- claude ---
I have enough to write the report. Let me give a thorough analysis.

---

## Report

### Summary

Your previous framing of this as a classification path issue was wrong — I stand corrected with you. The 10 GB peak is in `SMART_PASS2_SCHEDULE`, not in the classify call. But reading the code carefully: **`build_classify_schedule` cannot be allocating 6 GB.** The body is bounded work. Something else is happening inside the marker interval and being attributed to SCHEDULE.

### Q1: Where is the 6.33 GB transient actually coming from?

**It's not in `build_classify_schedule`.** I traced every allocation in the scan path:

1. **`BlobReader::seekable_from_path`** (blob.rs:1069): opens `File`, wraps in `BufReader::with_capacity(256 * 1024, f)`. Total allocation: ~256 KB. Not the source.

2. **`set_parse_indexdata(true)`**: sets a bool. No allocation.

3. **`next_header_skip_blob`** (called once for OsmHeader): reads ~50 byte header, seeks past blob data. Allocates nothing new beyond `header_buf` reuse.

4. **`next_header_with_data_offset` loop** (blob.rs:1015): for each of ~430K blobs:
   - `read_blob_header` (blob.rs:658): reads 4-byte length + N-byte header into `self.header_buf` (reused Vec, cleared + reserved per iteration). No accumulation.
   - `WireBlobHeader::parse` with `parse_indexdata=true, parse_tagdata=false`: returns struct with `indexdata: Option<[u8; 42]>` (inline 42-byte array on stack) and `tagdata: None`. **Zero heap allocation.**
   - Returns `BlobHeader::new(header)` — a stack copy of the WireBlobHeader.
   - Caller does `hdr.index()` → `BlobIndex::deserialize(&d[..])` which returns a pure stack struct with `(i64, i64, u64, ElemKind, Option<BlobBbox>)`. No heap.
   - Caller does `schedule.push((seq, data_offset, data_size))` — 24 bytes per entry. At 20K way blobs = 480 KB. Total schedule allocation: under 1 MB.

**The total heap allocation from `build_classify_schedule` is ~1-2 MB, max.** The 6.33 GB transient is coming from somewhere else that's running concurrently or being attributed to the SCHEDULE marker bracket.

### The actual cause — look at what the compiler dropped BEFORE `build_classify_schedule` is called

Look at `extract_smart` lines 2806-2815:

```rust
let result = collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler)?;
let mut extra_node_ids = handler.extra_node_ids;
crate::debug::emit_marker("SMART_PASS1_END");

// --- Pass 2: Resolve extra way node deps (parallel pread) ---
crate::debug::emit_marker("SMART_PASS2_START");
// ...
{
crate::debug::emit_marker("SMART_PASS2_SCHEDULE_START");
let (way_schedule, shared_file) = super::build_classify_schedule(
    input, Some(crate::blob_index::ElemKind::Way),
)?;
crate::debug::emit_marker("SMART_PASS2_SCHEDULE_END");
```

After `collect_pass1_generic` returns, the caller holds `result: Pass1Result { bbox_node_ids, matched_way_ids, all_way_node_ids, matched_relation_ids }` (4 IdSetDense) and `extra_node_ids: IdSetDense` (moved from handler). That's **5 IdSetDense live at PASS1_END**.

The `collect_pass1_generic` function itself just returned. Everything local to it — the scanner, the three schedule Vecs, the FILES, the worker_init state from each classify phase — is dropped by the compiler at the function return point. But the **drop order** matters: `let` bindings inside collect_pass1_generic are dropped in reverse order when the function returns. This happens AT the function return, but the memory freed during drop doesn't necessarily return to the OS immediately (it goes to the allocator's free list).

**Critical observation: the peak anon measurement includes allocator free-list pages, not just live allocations.** glibc's allocator holds freed pages in its arena for reuse. A large transient allocation (like a 6 GB IdSetDense that was briefly live during PASS1, then dropped) would leave 6 GB of pages in the allocator's arena that show up as "anon RSS" until the allocator returns them to the OS via `madvise(DONTNEED)` or `munmap`.

Let me check: **what was the PASS1_RELATION_CLASSIFY peak?** From the sidecar: 3.73 GB, same as PASS1 overall. The peak wasn't 10 GB during PASS1, so there wasn't a 6 GB allocation that got freed before PASS2.

**Alternative hypothesis: the 6.33 GB is the worker_init state from an earlier classify call that's held in the allocator arena but not live.**

Actually, let me re-read the sidecar numbers more carefully:

```
SMART_PASS1                72393ms    3.73 GB    3.73 GB         17
PASS1_NODE_CLASSIFY        13657ms    245 MB     242 MB          17
PASS1_WAY_CLASSIFY         25805ms    3.66 GB    3.65 GB          4
PASS1_RELATION_CLASSIFY     6730ms    3.73 GB    3.73 GB          0
SMART_PASS2                24285ms   10.06 GB   10.06 GB       1578
SMART_PASS2_SCHEDULE       19307ms   10.06 GB   10.06 GB       1578
SMART_PASS2_CLASSIFY        4977ms    6.24 GB    6.24 GB          3
```

**Wait — `SMART_PASS1` total peak is 3.73 GB, but `PASS1_WAY_CLASSIFY` peak is 3.66 GB.** The difference (70 MB) is the persistent IdSetDense carried forward. This means `PASS1_WAY_CLASSIFY` allocated up to 3.66 GB of transient state.

Now the critical question: **what does `parallel_classify_phase` allocate during PASS1_WAY_CLASSIFY?** From commit `51f820d`, PASS1 node/way/relation all use `parallel_classify_phase<S, R>` (per-blob send). Each per-blob result is `R = Vec<i64>` (the way refs). At 20K way blobs × ~8000 refs × 8 bytes = 1.3 GB of Vec<i64> flowing through the channel. Peak channel capacity × per-item size. The channel is bounded at 32 items, so ~32 × ~64 KB = 2 MB in flight at any moment.

But the PASS1_WAY_CLASSIFY peak is 3.66 GB. Where does that come from?

The answer: `parallel_classify_phase` sends results through a channel, and the consumer merges them into `matched_way_ids` and `all_way_node_ids` (both IdSetDense). `all_way_node_ids` accumulates ALL node refs from all ways in the bbox, which at Europe scale is potentially billions of refs. Each ref sets a bit in the IdSetDense. The chunks span the full node ID range → many chunks allocated → ~1.5-2 GB of `all_way_node_ids` alone.

Plus `matched_way_ids` (smaller, ~few hundred MB), `bbox_node_ids` (~500 MB), and worker scratch.

**Pass 1 peak of 3.66 GB is plausible from the IdSetDense accumulation in the consumer, not the channel.**

### Now the real diagnosis of Pass 2 SCHEDULE

After `SMART_PASS1_END`, the process holds:
- `result.bbox_node_ids`
- `result.matched_way_ids`
- `result.all_way_node_ids` ← potentially 1.5 GB
- `result.matched_relation_ids`
- `handler.extra_way_ids`
- `handler.extra_node_ids` → moved to `extra_node_ids`

5+ IdSetDense × ~500 MB-1.5 GB each = ~3-7 GB of persistent state.

The `SMART_PASS1` marker reports 3.73 GB peak. The `SMART_PASS2` marker reports 10.06 GB peak. The delta is ~6.33 GB — occurring between PASS1_END and PASS2_CLASSIFY.

**Between these markers, the only allocating operation is `build_classify_schedule`.** But that allocates ~1 MB.

**Unless... the 6.33 GB is the allocator's arena state being poked by `build_classify_schedule`'s small allocations.**

Here's the mechanism: glibc's allocator keeps arena free-list pages "reserved" (counted as anon RSS) even after they're freed. The pages are reclaimed by `madvise(DONTNEED)` only when the arena detects low fragmentation or on explicit `malloc_trim`. If PASS1 allocated 6 GB of transient state inside `parallel_classify_phase` (per-blob Vec<i64>, PrimitiveBlock, string tables, etc.), those pages remain in the arena after being freed. **The peak anon during PASS1 should have captured this** — but it didn't (PASS1 peak is 3.73 GB).

**The only way PASS2_SCHEDULE could show 10 GB when nothing is allocating 6 GB is if the 6 GB transient is happening in a background thread whose work is attributed to the SCHEDULE marker.**

Wait — is there a rayon thread pool from PASS1 that's still running cleanup in the background? `parallel_classify_phase` uses `std::thread::scope` which joins all workers before returning. There shouldn't be background threads from PASS1 alive during PASS2.

**BUT: the decompress pool, allocator coalescing, and rayon global pool internals might still be active.** Rust's rayon uses a global thread pool that persists across calls. The rayon workers hold thread-local state. Between PASS1 and PASS2, rayon threads may be idle but holding per-thread allocator arenas populated with PASS1 frees.

### Q1 answer: the 6.33 GB is NOT from `build_classify_schedule` — it's allocator lag

The sidecar's peak anon measurement samples RSS every 100ms. It doesn't measure "allocation events" — it measures the kernel's view of anon pages at each sample. The 10 GB peak during SCHEDULE is either:

1. **Allocator free-list pages from PASS1's transient allocations**, finally coalescing and showing up in RSS because the kernel's dirty page tracking is delayed.
2. **A real transient allocation in a function we haven't instrumented** — possibly inside `collect_pass1_generic`'s drop sequence. The function's locals are dropped after the `Ok(Pass1Result { ... })` expression evaluates, BEFORE control returns to the caller. If any of the worker_init closures captured large state that's dropped in sequence, the drop could occur across the `SMART_PASS1_END` → `SMART_PASS2_SCHEDULE_START` marker boundary.

**Hypothesis worth testing:** Add `malloc_trim` (via libc) or `drop(result.all_way_node_ids)` between `SMART_PASS1_END` and `SMART_PASS2_SCHEDULE_START`. If the 10 GB peak drops to 4 GB, the cause is allocator lag from PASS1 state still sitting in arena pages.

Actually, the cleanest test: **emit a marker IMMEDIATELY after `collect_pass1_generic` returns, before computing `extra_node_ids`:**

```rust
let result = collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler)?;
crate::debug::emit_marker("SMART_AFTER_COLLECT_PASS1");
let mut extra_node_ids = handler.extra_node_ids;
crate::debug::emit_marker("SMART_PASS1_END");
```

If `SMART_AFTER_COLLECT_PASS1` shows 10 GB peak, the transient is from `collect_pass1_generic`'s drop path. If it shows 3.73 GB, the transient is from `handler.extra_node_ids` move or later.

### Q2: Why doesn't `collect_pass1_generic`'s scan show the same transient peak?

Because `collect_pass1_generic` runs its scan at line 2526-2547, and PASS1_NODE_CLASSIFY doesn't start until line 2559. There's no marker bracketing the scan itself. The scan happens within the `SMART_PASS1` marker (line 2807) but before `PASS1_NODE_CLASSIFY_START`. 

Looking at the sidecar: `SMART_PASS1` peak is 3.73 GB, and the 3.73 GB is achieved during `PASS1_RELATION_CLASSIFY`. Nothing in `SMART_PASS1` shows a 10 GB peak. **The scan in `collect_pass1_generic` really doesn't allocate 6 GB.**

This confirms that `build_classify_schedule` itself cannot be allocating 6 GB either — same code, same conditions. **The 6.33 GB in PASS2_SCHEDULE is from something outside the function.**

### Q3: Should narrower hotpath annotations go on BlobReader?

**No — that's chasing the wrong thing.** The hotpath `--alloc` data from Japan already showed `build_classify_schedule` at 640 KB exclusive and `parallel_classify_phase` at 4.2 GB. The alloc tracking is measuring cumulative bytes through the function frame. `build_classify_schedule` is correctly measured as allocating almost nothing. Adding annotations inside BlobReader won't change this — the function truly doesn't allocate 6 GB.

**The gap between `--alloc` (cumulative bytes) and `--bench` (peak anon) is the key observation.** `parallel_classify_phase` cumulatively allocates 4.2 GB on Japan. It allocates many small Vec<i64>s through the channel. Each Vec is freed immediately after merge. Peak RSS during classify is bounded. But the allocator's free list grows to ~4.2 GB (the cumulative max). When PASS2 SCHEDULE is sampled, this free list is still sitting in the arena — showing up as anon RSS even though no Rust-level allocation is live.

### The fastest experiment: `malloc_trim` between phases

Add this between `SMART_PASS1_END` and `SMART_PASS2_SCHEDULE_START`:

```rust
#[cfg(target_os = "linux")]
unsafe { libc::malloc_trim(0); }
```

`malloc_trim(0)` forces glibc to release unused arena pages back to the OS. If the PASS2_SCHEDULE peak drops from 10 GB to ~4 GB after this, the diagnosis is confirmed: the "transient" is allocator free-list pages from PASS1, not from `build_classify_schedule`.

If `malloc_trim` doesn't help, the transient is a real live allocation somewhere, and you need narrower markers to find it — specifically, markers INSIDE `build_classify_schedule` before and after the loop, and inside the drop of `Pass1Result`.

### Q4: Should the `extract.rs:2813` fix stay?

**Keep it.** The architectural rationale is correct regardless of whether it solved the specific measurement. `parallel_classify_accumulate` with an unbounded `IdSetDense` is genuinely unsafe at planet scale — the chunk-spread model is right in principle even if the Europe workload didn't trigger the worst case. The 23% PASS2 wall improvement is real. Reverting would restore a latent planet-scale risk without any measurable benefit. Document in the commit message that the fix addresses a latent issue and is not the root cause of the 10 GB peak.

### What I got wrong yesterday

I diagnosed the 10 GB as chunk-spread IdSetDense accumulation. That was wrong — I extrapolated from the design review's prediction without questioning whether the prediction matched reality. When the tags-filter measurement showed flat memory with the same "unsafe" pattern, I should have flagged that as a falsification of the chunk-spread model, not rationalized it as workload selectivity. The model is wrong. The fact that both measurements defeat it is the important signal.

**The investigation should have started by reading the BlobReader code yesterday.** I treated `build_classify_schedule` as opaque and reasoned about it from its caller signature. If I had read the 30 lines of code, I would have seen immediately that no allocation larger than ~1 MB is possible in that function, and looked elsewhere.

### Recommended next steps

1. **Add two markers** around the `Pass1Result` destructuring:
   ```rust
   crate::debug::emit_marker("SMART_PASS1_RESULT_RETURNED");
   let mut extra_node_ids = handler.extra_node_ids;
   crate::debug::emit_marker("SMART_PASS1_END");
   ```
   This narrows the 6.33 GB window to either "during collect_pass1_generic drop" or "after collect_pass1_generic returns."

2. **Try `malloc_trim(0)` between PASS1 and PASS2.** If the SCHEDULE peak drops, the diagnosis is allocator lag and the "transient" is not a real live allocation.

3. **If malloc_trim doesn't help**, add a marker inside `build_classify_schedule` before and after the scan loop — if the peak is already 10 GB before the loop starts, the allocation is elsewhere. If it's 4 GB before and 10 GB after, something in the loop is actually allocating.

4. **Japan `--alloc` won't help for this diagnosis** — it measures cumulative bytes, not peak RSS. The discrepancy between the two is the entire point.

The planet blocker is still real, but it's not `extract.rs:2813`. It's whatever is sitting in ~6 GB of allocator arena state between PASS1 and PASS2. Finding a way to release that between phases — either structural (drop the Pass1Result fields earlier) or mechanical (malloc_trim) — is the fix.

--- codex ---
**Findings**

1. The new note convincingly overturns yesterday’s main diagnosis: `extract.rs:2813` was not the source of the 10 GB burst. The `cc19d26` fix improved PASS2 wall time materially, but moved peak anon only about 0.6 GB. That is strong evidence the original “worker-local `IdSetDense` blowup” explanation was at most a minor contributor.

2. Reading the current code strongly suggests `build_classify_schedule()` itself is **not capable** of allocating 6+ GB transiently.
   - In [mod.rs](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427), it builds only:
     - one `BlobReader`
     - one `Vec<(usize, u64, usize)>`
     - one shared `File`
   - In [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L658), `read_blob_header()` reuses a single `header_buf`.
   - In [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1015), `next_header_with_data_offset()` reads one header, computes offsets, skips the blob, and returns; it does not cache headers or blob data.
   - `set_parse_indexdata(true)` only enables a fixed-size 42-byte copy per blob header in [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L648).
   There is no visible mechanism here for multi-gigabyte retained anon growth.

3. The “same scan code behaves differently” puzzle is real and important. [collect_pass1_generic()](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2525) uses essentially the same `BlobReader::seekable_from_path` + `next_header_with_data_offset()` loop as [build_classify_schedule()](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427), yet PASS1 does not show the same transient burst. That strongly suggests the sidecar peak is being **attributed** to `SMART_PASS2_SCHEDULE`, but not necessarily **caused** by the code inside `build_classify_schedule()`.

4. The most likely remaining explanation is not “BlobReader caches 6 GB,” but “some other allocation pattern reaches peak during the schedule bracket.”
   Plausible candidates:
   - retained state from PASS1 that reaches its high-water mark just before / during the schedule bracket
   - allocator arena behavior / delayed page release that makes the RSS spike visible there
   - coarse sidecar sampling that lands the high-water in the schedule window even if the causative work straddles the boundary
   - an extraction-specific data structure outside the scanned code path that grows or is finalized between `SMART_PASS1_END` and `SMART_PASS2_CLASSIFY_START`

5. The wall-time story is now cleaner:
   - the memory issue and the wall issue are separate
   - the `cc19d26` fix improved PASS2 wall, so it was not useless
   - but it did not address the planet blocker
   So keeping that fix still makes sense, but not because it solved the original problem.

**What the code inspection rules out**

I think you can safely rule out the note’s hypothesis `(5)`:

- `BlobReader::seekable_from_path()` in [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1069) just opens a file and wraps it in a 256 KB `BufReader`.
- `next_header_skip_blob()` and `next_header_with_data_offset()` in [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L983) and [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1015) do not retain per-blob state.
- `WireBlobHeader::parse()` is fed a reused `header_buf`; it does not imply schedule-wide retention in this path.

So my strongest fresh conclusion is:

**The 6.33 GB transient is almost certainly not “inside BlobReader” in the literal sense.**

**Implications for the four questions**

1. **Q1: Where is the 6.33 GB transient?**  
Not obviously in `build_classify_schedule()` or the `BlobReader` functions you flagged. The code does not support that hypothesis. I would now suspect marker attribution rather than the schedule builder itself.

2. **Q2: Why doesn’t `collect_pass1_generic` show it?**  
Because the scan path is probably not the real source. The fact that PASS1’s near-identical scan does not reproduce the burst is the strongest evidence against the “schedule build allocates 6 GB” theory.

3. **Q3: Add narrower hotpath annotations?**  
Yes, but I would not start with more `BlobReader` markers alone. They may only confirm what the code already suggests.
   Better next instrumentation:
   - mark the code immediately before and after `build_classify_schedule()` in `extract_smart`
   - add counters for sizes / chunk counts of the large PASS1 products entering PASS2:
     - `result.bbox_node_ids`
     - `result.matched_way_ids`
     - `handler.extra_way_ids`
     - `extra_node_ids`
   - if possible, emit per-structure chunk counts or heap estimates right before `SMART_PASS2_SCHEDULE_START` and right after `SMART_PASS2_SCHEDULE_END`
   That will tell you whether the “burst” is really new allocation or just existing state becoming fully resident / visible.

4. **Q4: keep `cc19d26`?**  
Yes, keep it.
   Reasons:
   - it improved PASS2 wall by ~23%
   - it is architecturally sound
   - it does not worsen the real blocker
   The fact that it did not solve the blocker is a diagnosis issue, not a reason to revert a valid improvement.

**Recommended next steps**

1. **Do not spend more time blaming `build_classify_schedule` without stronger evidence.**  
The code does not support it.

2. **Instrument the PASS2 inputs, not just the schedule builder.**  
Right before `SMART_PASS2_SCHEDULE_START`, log or sidecar-count:
   - `handler.extra_way_ids` chunk count / total_count if available
   - `extra_node_ids` chunk count / total_count
   - `result.matched_way_ids` and `bbox_node_ids` chunk counts
   If one of these is already near the missing 6 GB, the puzzle is solved.

3. **If you want one cheap experiment, move the schedule build earlier.**  
Build the way schedule before PASS1 completes, or at least before materializing PASS1 outputs into their final structures. If the peak moves with the marker instead of with the code, that confirms attribution error.

4. **Keep the `cc19d26` fix in place and investigate separately.**  
It is now a performance improvement, not the memory fix.

**Bottom line**

The follow-up note successfully falsifies the previous root cause, but the code review here also falsifies the new tentative one: the `BlobReader` schedule-build path does not look capable of holding 6 GB. The most likely next answer is that the peak is being observed during `SMART_PASS2_SCHEDULE`, not caused by it. The right next move is to instrument the large PASS1/PASS2 state objects around that boundary, not to keep drilling into `BlobReader`.

=== arch ===

--- claude ---
Now I have both briefs. Let me provide a thorough report.

---

## Report: parallel_classify_phase Regression — Original Brief + Follow-up

### Summary of what the two documents tell us

The original brief (2026-04-10) described a measured post-refactor regression: extract-smart peak anon jumped 4.71 GB → 10.72 GB and wall time +22%. The hypothesis, endorsed by me and other reviewers, was that `extract.rs:2813` (smart-extract way-dep `IdSetDense` accumulation) was the cause, matching the "disputed" design-review prediction of 6 workers × 1.5 GB per-worker IdSetDense.

The follow-up brief (2026-04-11) reports that the fix was implemented (commit `cc19d26`) and shipped. **It didn't work.** The PASS2 peak dropped only 0.6 GB out of 10.7 GB. The chunk-spread diagnosis — the framework both the reviewers and I reasoned from — was wrong. New instrumentation (commit `51f820d`) with sub-phase markers reveals the 10 GB peak is during `SMART_PASS2_SCHEDULE`, which is the wrapper around `build_classify_schedule(input, Some(ElemKind::Way))`. The actual `parallel_classify_phase` call that followed (SMART_PASS2_CLASSIFY) peaks at only 6.24 GB — barely above PASS1 baseline. **The classify path was never the problem.**

### The new puzzle

`build_classify_schedule` is a ~30-line function (`mod.rs:426-456`) that:
1. Opens a seekable BlobReader
2. Sets `parse_indexdata(true)`
3. Skips the header blob
4. Loops over blob headers via `next_header_with_data_offset()`
5. Pushes `(seq, data_offset, data_size)` tuples into a Vec when the kind matches
6. Returns `(schedule, shared_file)`

The final schedule Vec is ~480 KB for Europe (~20K way entries × 24 bytes). But peak anon during this function grows by 6.33 GB transiently, then drops ~3.8 GB when the function returns.

The critical puzzle: **the same scan code in `collect_pass1_generic` (called at the start of PASS1) does NOT produce this 6 GB transient**. PASS1's overall peak is 3.73 GB. If the scan inside `collect_pass1_generic` behaved the same way, PASS1 peak would be ≥6.33 GB. It isn't.

Differences between the two call sites:
- `build_classify_schedule` has a single kind filter; `collect_pass1_generic` dispatches to three per-kind schedules
- `collect_pass1_generic` runs first (cold page cache); `build_classify_schedule` runs second
- `collect_pass1_generic` runs with fresh allocator state; `build_classify_schedule` runs after PASS1's parallel work has populated glibc arenas

### Questions asked

**Q1:** Where is the 6.33 GB transient inside `build_classify_schedule`'s call path? Candidates: `seekable_from_path`, `set_parse_indexdata`, `next_header_skip_blob`, `next_header_with_data_offset`. None have been read yet.

**Q2:** Why doesn't `collect_pass1_generic`'s scan show the same transient? Same code, different behavior.

**Q3:** Should they add narrower hotpath annotations on the BlobReader path, or try a faster diagnostic (temporarily disable `set_parse_indexdata(true)` and re-bench)?

**Q4:** Should the shipped fix at `extract.rs:2813` stay or be reverted? It's architecturally correct but doesn't solve the planet blocker it was framed to solve.

### What stands out to me

**1. The `--alloc` and sidecar results tell different stories because they measure different things.**

Japan `--alloc` (UUID `4eccb6f3`) says `parallel_classify_phase` is 90% of cumulative bytes (4.2 GB), and `build_classify_schedule` is only 640 KB exclusive. Europe sidecar (UUID `8ac56b15`) says the peak anon is during `SMART_PASS2_SCHEDULE`, which wraps `build_classify_schedule`.

The follow-up correctly identifies this: cumulative throughput vs. instantaneous peak. A function that churns many small Vecs through a channel has huge cumulative throughput but flat peak (because each Vec dies immediately). A function that briefly allocates a few large things has small cumulative but big peak.

The sidecar is more reliable for identifying the planet blocker (peak anon is what causes OOM). The allocator profile tells you where alloc/free churn lives. Both are valuable, but for this question, the sidecar is the ground truth.

**2. `build_classify_schedule` itself is almost certainly not the allocator.**

The body has one `Vec::new()`, one `Arc::new(File::open(...))`, and the scanner. At 480 KB for the schedule, the function's exclusive allocations are tiny. The 6.33 GB must come from something called inside it, most likely inside the iterator loop.

**3. The decisive clue is `next_header_with_data_offset` being called ~430K times.**

Europe has ~430K total blobs. Each iteration calls this function once. If the function has any per-call allocation that isn't immediately freed, it multiplies by 430K. 6 GB / 430K = ~14 KB per call. That's suspiciously close to the size of a parsed indexdata blob (typical PBF indexdata is ~5-20 KB when the blob has spatial bboxes and per-element ID ranges).

**4. The `collect_pass1_generic` puzzle points at something subtle.**

If `next_header_with_data_offset` was unconditionally allocating 14 KB/call and holding it, both call sites would see the same peak. Since only `build_classify_schedule` shows the 6 GB transient, the difference is in:
- What happens to the parsed indexdata after it's read
- How the scanner state accumulates across iterations
- Some state in the enclosing context (BlockBuilder, IdSetDense, rayon pool) that colludes with the scan

The follow-up's hypothesis 5 — "`BlobReader::seekable_from_path` allocates per-blob structures during the constructor" — is worth checking but probably wrong, because the scanner is created in both call sites with the same constructor call. What differs is what happens during iteration.

**5. Strongest hypothesis: the scanner retains parsed indexdata across iterations.**

If `next_header_with_data_offset` parses indexdata into a heap-allocated structure that's owned by the scanner (not returned to the caller), and the scanner holds a `Vec<BlobHeaderInfo>` or similar that grows per iteration, the accumulated state would be ~6 GB at the end.

But why would `collect_pass1_generic` not show this? Answer: **it might**, but the peak is masked. `collect_pass1_generic` scans, dispatches into three `node_schedule`/`way_schedule`/`relation_schedule` Vecs, then drops the scanner immediately after building them. The scan's transient growth is contained within `PASS1_NODE_CLASSIFY`/`PASS1_WAY_CLASSIFY`/`PASS1_RELATION_CLASSIFY` brackets… wait, no — the schedule is built BEFORE the classify markers fire. Let me re-read.

Actually, looking more carefully at the follow-up's phase table: `PASS1_NODE_CLASSIFY 242 MB`, `PASS1_WAY_CLASSIFY 3.65 GB`, `PASS1_RELATION_CLASSIFY 3.73 GB`. These are cumulative — `PASS1_WAY_CLASSIFY` peak includes whatever was built up in `PASS1_NODE_CLASSIFY` and the schedule build. If the scan built up 6 GB, it would show up in `PASS1_NODE_CLASSIFY` (since the schedule is built before the first classify). It doesn't.

So `collect_pass1_generic`'s scan genuinely does not accumulate 6 GB. And `build_classify_schedule`'s scan genuinely does. Same code. Different context. The difference is what state exists at the time the scan runs.

**6. My best new hypothesis: the scanner is reading the file but the 6 GB isn't from the scanner at all — it's from the previous PASS2 setup.**

Let me re-read the SMART_PASS2_SCHEDULE measurement:
```
SMART_PASS2_SCHEDULE       19307ms   10.06 GB   10.06 GB       1578
```

19.3 seconds to scan the file headers is slow. Too slow. At 430K blobs × header-read + indexdata parse, I'd expect maybe 2-5 seconds on a warm file. 19 seconds suggests something else is happening. And 1578 major faults during the SCHEDULE marker — those aren't noise, they're real page faults into file-backed pages (or anon pages that were swapped).

The 3.8 GB drop when SCHEDULE returns (`10.06 GB → 6.24 GB`) is suggestive. If the schedule function truly returned ~480 KB of Vec data, the drop wouldn't be 3.8 GB. Something ~3.8 GB was allocated during SCHEDULE and freed at function exit. That fits the profile of a dropped intermediate collection — maybe the scanner's per-blob buffer, maybe a cached indexdata Vec, maybe something holding onto the full header bytes.

**7. The simplest diagnostic is the one the follow-up already proposed: comment out `set_parse_indexdata(true)`.**

This is the fastest path to narrow the source. If peak collapses from 10 GB to 4 GB, indexdata parsing is the culprit. If it doesn't collapse, the issue is in `next_header_with_data_offset` itself (reading headers and/or blob framing), not indexdata.

The concern about breaking the kind filter is valid but easy to work around: temporarily remove the kind filter too, accept that the schedule includes all OsmData blobs instead of just Way blobs, and measure the peak. You're not checking correctness, you're isolating the allocation source. 19 seconds → maybe 10 seconds; the extra blobs in the schedule won't affect the 6 GB transient if the transient is in the scan itself.

**8. The `extract.rs:2813` fix was shipped under false pretenses — but should stay.**

The architectural justification was valid: `parallel_classify_accumulate` over an unbounded `IdSetDense` is a correctness landmine even if the specific workload didn't trigger it as severely as predicted. The 23% PASS2 wall improvement is real and not controversial. The only problem is that the PR framing said "this fixes the planet blocker" and it doesn't.

Keep the fix, update the commit message / notes to correct the framing, and continue investigating the real cause. Reverting would lose a measurable improvement and leave the landmine in place.

### My answers to the questions

**Q1:** I can't point at the exact line without reading `src/read/blob.rs`. But based on the shape of the regression (~14 KB/blob × 430K blobs = ~6 GB) and the 3.8 GB drop on function return, my strongest hypothesis is that `next_header_with_data_offset` parses indexdata per-blob and the scanner or an intermediate buffer retains the parsed data (or the raw header bytes that contain indexdata) across iterations. The 3.8 GB drop is the scanner being dropped at the end of the function.

Less likely but possible: `BlobReader::seekable_from_path` constructs something that grows during iteration — e.g., a buffer or cache for read-ahead. But the constructor is identical to the one in `collect_pass1_generic`, so this only fits if the construction is cheap and the growth happens during iteration.

**Q2:** The structural difference between `build_classify_schedule` and `collect_pass1_generic` is in what the caller does around the scan, not in the scan code itself. My best guess is that the 6 GB transient is being attributed to the SCHEDULE marker bracket but is actually caused by state that exists BEFORE SCHEDULE starts and gets accounted to SCHEDULE because the marker captures "max anon during the bracket." Specifically: if PASS1 leaves behind a large BlockBuilder scratch, rayon decode pool state, or a cached DenseNodeColumns buffer, and that state is kept alive through PASS2 setup but freed during the scan (or shortly after), the peak would appear in SCHEDULE even though the allocation was from an earlier phase.

This hypothesis fits the drop pattern: 10.06 GB during SCHEDULE → 6.24 GB during CLASSIFY. If the 3.8 GB drop is "PASS1 leftover finally getting freed," the scan itself might only add ~2-3 GB, not 6 GB.

**Q3:** Both diagnostics are valuable but at different costs:

- **Temporarily disable `set_parse_indexdata(true)`** — cheapest, fastest feedback. ~5 minute turnaround. Accept the behavioral break for the measurement run.
- **Narrower hotpath annotations on BlobReader** — more work, but gives a reusable instrumentation layer. Maybe 30 minutes to add, another ~10 minutes to bench on Japan.

I'd do both, in that order. The disable-indexdata bench answers "is this in the indexdata parser" in one run. If the answer is yes, the annotations become unnecessary. If the answer is no (peak doesn't collapse), the annotations are the next step.

**Also consider a third diagnostic: measure peak anon BEFORE the SCHEDULE marker fires.** Add a marker `SMART_PASS2_PRE_SCHEDULE` right before `build_classify_schedule` is called and another `SMART_PASS2_SCHEDULE_ENTRY` as the first line of the function. If `SMART_PASS2_PRE_SCHEDULE` shows 10 GB peak already, the burst isn't in `build_classify_schedule` at all — it's in something happening between PASS1 and PASS2 that the current marker bracket includes. This is the cheapest test for hypothesis 8 (PASS1 leftover).

**Q4:** Keep the fix. The architectural justification is sound, the 23% PASS2 wall improvement is real, and reverting loses progress without fixing the framing problem. Update the notes to acknowledge that the shipped fix addressed a correctness landmine but did NOT solve the planet blocker, and continue investigating.

### What I got wrong in the previous round

Looking back at my earlier reasoning:

I accepted the "6 workers × 1.5 GB IdSetDense = 9 GB" framing from the design review and reasoned within it. The framing was seductive because it matched the measured peak (10.7 GB) closely and the disputed site (`extract.rs:2813`) was structurally consistent with the model. I treated the structural match as evidence without questioning whether the measurement actually implicated that site.

The right move would have been to ask for sub-phase markers FIRST — to narrow the measurement to the actual source — before committing to a diagnosis. The follow-up's instrumentation work is exactly what should have happened before the fix shipped. The brief even mentioned that the pre-refactor sidecar didn't have CLOSURE/WAYDEPS as named markers — that was a red flag for trusting any attribution within pre-refactor phases, and I should have flagged it.

The general lesson: when a model's prediction matches a measurement but the model has been questioned (disputed in the design review), treat the match as correlation, not causation. Require attribution at finer granularity before committing to a fix. The chunk-spread model fit tags-filter poorly (9 GB predicted, 2 GB measured), which should have been enough to distrust it for extract-smart too.

### What I'd do now (if it were my call)

1. **Cheapest test first:** add a `SMART_PASS2_PRE_SCHEDULE` marker right before the `build_classify_schedule` call, run Europe smart once, check whether the 10 GB is already present before entering the function. This takes ~5 minutes and rules out (or confirms) the "PASS1 leftover" hypothesis.

2. **If the peak is already present before SCHEDULE:** the burst isn't in the scanner. Look at what's between `SMART_PASS1` end and `SMART_PASS2_SCHEDULE` start. Something is holding ~3.8 GB that doesn't get freed until PASS2 starts. Candidates: PASS1's `all_way_node_ids` IdSetDense, the columnar decode worker scratch buffers, a leftover BlockBuilder state.

3. **If the peak is not yet present before SCHEDULE:** it really is inside the function. Next step is the temporary `set_parse_indexdata(false)` bench. If peak collapses → indexdata parser. If peak stays → header reader.

4. **If the indexdata parser is implicated:** read `src/read/blob.rs` to find where parsed indexdata lives. If it's a `Box<BlobIndex>` returned per call that the caller drops, that's fine. If the scanner caches it internally, that's the bug. Alternatively, if indexdata parsing allocates temporary buffers that aren't returned to a pool, that could be the cumulative growth.

5. **Planet blocker unblocks when peak is understood and reduced.** The 27 GB extrapolation (10 GB Europe × 2.6×) is clearly not fitting 30 GB. But if we can reduce the 10 GB Europe peak to ~4 GB (matching pre-refactor), planet becomes 10-11 GB, well within 30 GB.

The original brief and the follow-up are both well-written — the follow-up in particular models the right epistemic move: "we shipped what the reviewers said, it didn't work, here's sharper data, here's where our previous reasoning broke." That's exactly the right loop. My previous review contributed to the wrong answer; the instrumentation work is what corrected it.

--- codex ---
**Findings**

1. The follow-up convincingly falsifies the previous root-cause theory. The `extract.rs:2813` change was a reasonable architectural cleanup, but the new measurements show it was not the source of the 10 GB burst. The strongest evidence is the new sub-phase split:
- `SMART_PASS2_SCHEDULE`: 10.06 GB peak
- `SMART_PASS2_CLASSIFY`: 6.24 GB peak  
at [followup.md:107-123](/home/folk/Programs/pbfhogg/notes/parallel-classify-regression-2026-04-11-followup.md#L107).

2. Code inspection of the current schedule-build path does not reveal any mechanism that can plausibly retain 6.33 GB of anonymous memory.
- [`build_classify_schedule()`](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427) only builds `Vec<(usize, u64, usize)>` plus one `Arc<File>`.
- [`BlobReader::seekable_from_path()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1069) opens a file and wraps it in a 256 KB `BufReader`.
- [`next_header_skip_blob()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L983) and [`next_header_with_data_offset()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1015) read headers and seek past blob data; they do not retain per-blob state.
- [`WireBlobHeader::parse()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L216) copies at most 42 bytes of `indexdata` per header when `parse_indexdata` is enabled, and `tagdata` is not parsed on this path.

From the code alone, candidate “BlobReader caches 430K headers/indexes” is ruled out.

3. The new evidence also weakens the “the burst is literally inside `build_classify_schedule`” wording. The marker bracket is correct, but the code inside it is too small. That means the sidecar is almost certainly attributing a process-wide RSS event that happens during that bracket, not a large live object graph owned by the schedule builder itself.

4. The “same scan code behaves differently in PASS1 vs PASS2” puzzle is real and important. [`collect_pass1_generic()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2525) uses the same header-only scan pattern and does not show a comparable transient. That strongly suggests context-sensitive allocator behavior or phase-attribution effects, not a direct “this function allocates 6 GB” bug.

5. The `--alloc` results and the sidecar results are not contradictory once you separate cumulative allocation from peak residency. The note is right on that point:
- `parallel_classify_phase`: huge cumulative alloc, small retained diff
- `build_classify_schedule`: tiny exclusive alloc, large peak attributed in sidecar  
This is exactly the signature of “the sidecar peak is not explained by retained objects local to this function.”

**What I think is happening**

My current best read is:

- The 6.33 GB delta is not a real header/index cache in `BlobReader`.
- It is more likely allocator-state-driven resident memory growth that is being triggered or exposed during the schedule scan bracket.

Why I say that:
- There is no persistent data structure in the scanned code large enough.
- The peak drops by about 3.8 GB before `SMART_PASS2_CLASSIFY` ends, which is consistent with transient allocator residency, not with the schedule vector itself.
- The pass-2 schedule scan runs after a large, highly allocating pass-1 pipeline, so allocator state is very different from the earlier pass-1 scan.

I do not think page cache explains it, because the metric is anon, not file-backed RSS.

**Answers**

**Q1. Where is the 6.33 GB transient?**

Not in any obvious retained object inside the current `build_classify_schedule` / `BlobReader` header path.

After reading the code, I can rule out:
- `seekable_from_path()` as a large allocator
- `set_parse_indexdata(true)` as anything meaningful
- `next_header_skip_blob()` / `next_header_with_data_offset()` as retaining per-blob objects
- `BlobIndex::deserialize()` as a source of significant heap growth

The only heapful things on this path are tiny:
- one reusable `header_buf` in [`BlobReader`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L588)
- one temporary `WireBlobHeader` / `BlobHeader` per iteration
- a 42-byte copied `indexdata`
- the final schedule vec

So the transient is almost certainly not “inside the call path” in the sense of retained logical state. It is more likely:
- allocator arena growth / retention caused by repeated small allocations in that bracket, or
- a process-wide RSS event coinciding with the bracket.

**Q2. Why doesn’t the PASS1 scan show the same transient?**

That is the strongest argument that this is contextual, not intrinsic to the scan code.

The pass-1 scan at [`extract.rs:2525-2548`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2525) is materially the same style of scan. If the header path itself were building a 6 GB cache, PASS1 should also show it.

The meaningful differences are environmental:
- PASS1 scan happens before the three classify phases.
- PASS2 schedule happens after PASS1 has already built large `IdSetDense` state and exercised parallel decode/alloc paths.
- PASS2 therefore runs in a very different allocator state.

So my answer is: the code is similar enough that the difference is almost certainly not in the scanner logic. It is in surrounding allocator/process state.

**Q3. Should you add narrower hotpath annotations on the BlobReader path?**

Yes, but with the right expectation.

They are worth adding to prove the negative:
- `read_blob_header`
- `next_header_with_data_offset`
- `WireBlobHeader::parse`
- maybe `BlobHeader::index`

I do not expect them to reveal a 6 GB exclusive allocation source. I expect them to show the path is cheap, which would strengthen the case that the sidecar peak is attribution/context, not parser retention.

If the goal is fastest diagnosis of the RSS peak, I would prioritize sidecar-style diagnostics over hotpath alloc:
- marker inside `build_classify_schedule` around just scanner construction
- marker around the header-skip
- marker around the loop body
- optionally periodic anon snapshots every N blobs

That will tell you whether anon rises steadily during the loop or appears as a step from something else.

**Q4. Should the `extract.rs:2813` fix stay?**

Yes, keep it.

Reasons:
- The follow-up disproves the “planet blocker fixed” claim, but not the architectural rationale.
- [`parallel_classify_accumulate()`](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L561) is still the wrong helper for an unbounded node-ref `IdSetDense` in principle.
- The shipped change improved PASS2 wall by about 23% per the note at [followup.md:22-26](/home/folk/Programs/pbfhogg/notes/parallel-classify-regression-2026-04-11-followup.md#L22).
- It does not appear to make memory worse.

So I would keep the code change and correct the narrative in the notes: it was a sound cleanup and a local speed win, not the solution to the PASS2 peak.

**What I would do next**

1. Stop treating `build_classify_schedule` as the root cause. Treat it as the phase bracket where the peak is observed.
2. Add narrower markers inside that function and the `BlobReader` header path.
3. Instrument anon during the loop, not just around it.
4. If that still points nowhere, investigate allocator behavior directly rather than more extract logic.

**Bottom line**

The follow-up successfully overturns the previous diagnosis. The current code does not support a theory where `build_classify_schedule` or `BlobReader` header parsing literally owns 6 GB of live state. The most likely remaining explanation is allocator/process-state behavior whose peak happens during the schedule bracket. The `extract.rs:2813` fix should stay, but the planet blocker is elsewhere.
