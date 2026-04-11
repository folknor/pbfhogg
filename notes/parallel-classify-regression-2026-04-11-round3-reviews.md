=== planet ===

--- claude ---
Good, `build_classify_schedule` doesn't touch tag_index. Ruled out.

**One final thought on mechanism.** Look at the trajectory shape one more time:

```
t=72.4s → 72.6s: +1.24 GB  (first burst starts)
t=72.6s → 72.8s: +740 MB
t=72.8s → 73.0s: +320 MB   (first burst tails off over ~600ms)
t=73.0s → 73.4s: +20 MB    (400ms stall)
t=73.4s → 74.6s: +230 MB   (slow climb over 1200ms)
t=74.6s → 74.8s: +720 MB   (second burst starts)
t=74.8s → 75.0s: +1080 MB
t=75.0s → 75.2s: +570 MB
t=75.2s → 75.6s: +760 MB
t=75.6s → 76.4s: +310 MB   (second burst tails off over 800ms)
```

Two bursts, ~600-1800 ms each, ~2-4 GB each. Separated by ~1.6 seconds of stall.

**This is NOT the shape of a Vec doubling.** Vec doubling would produce ~15 tiny bumps as the Vec doubles, not two large bursts. Whatever is causing this is allocating in ~GB-scale batches.

**New hypothesis: the sidecar sampling is catching the kernel's deferred accounting of anon pages.** When glibc extends the arena via sbrk, the new pages are "reserved" but not faulted in. They don't count as RssAnon until they're written to. The scan loop writes to these pages progressively as the Vec grows. The kernel's accounting batches updates (via vmstat, which has its own sampling interval) and the sidecar's 200 ms reads of /proc/self/status catch the lag.

But this shouldn't produce 2-4 GB bursts. Unless the scan is actually writing 2-4 GB worth of pages.

**OK, final recommendation: instrument `mallinfo2()`. All my hypotheses are speculation until you have real allocator state data.**

---

## Final answer

1. **Your diagnosis direction is right, your list of mechanisms is right, but you're guessing which one.** The trajectory shape (two distinct bursts) doesn't match Vec doubling or any single-source hypothesis. Add `mallinfo2()` instrumentation at phase boundaries before choosing a mitigation.

2. **Reordered mitigation priority: mallinfo2() → Option B → Option A → Option D.** The mallinfo2 step is ~20 LoC and tells you unambiguously whether the growth is brk or mmap, plus whether it's fordblks or uordblks. That data constrains the mechanism enough to pick B or A confidently.

3. **Option B (jemalloc) is the most practical immediate workaround if B works.** It doesn't require understanding the glibc mechanism — it bypasses the question. The historical concern about `--all-features` breaking is solvable with mutually-exclusive cargo features.

4. **Option A (build all schedules upfront) is architecturally cleaner** regardless of the mechanism. Even if B fixes the symptom, A is still worth doing for the wall-time improvement (~25 more seconds saved on PASS3) and for reducing the number of file scans. Consider A as a follow-up even if B solves the memory issue.

5. **Keep the extract.rs:2813 fix.** It's the right pattern regardless of the mechanism puzzle.

6. **Your process discipline this round was excellent.** The surgical experiments, the explicit separation of wall improvements from memory improvements, the willingness to revert the malloc_trim when it didn't deliver — this is how these investigations should work. The remaining step is one more cheap diagnostic (mallinfo2) before committing to a mitigation.

**If you can only run one experiment before the next review: add mallinfo2() instrumentation and re-bench Europe smart once.** That single data set will either confirm the glibc hypothesis decisively (arena grew by 6 GB, hblks flat) or force a completely different investigation (mmap'd pages grew, allocator isn't the cause).

--- codex ---
**Findings**

1. Round 3 resolves the main ambiguity from round 2: the burst is not “in smart PASS2” generically. It is in **header-scan schedule construction after PASS1**, and it reproduces in two separate helpers:
- [`build_classify_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L426) during `SMART_PASS2_SCHEDULE`
- [`build_blob_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) during `PREAD_WRITE_BLOB_SCHEDULE`

That is the most important result in the note.

2. The experiments also rule out most of the earlier theories cleanly:
- not the old worker-local `IdSetDense` theory
- not simple carry-over allocator lag from PASS1 alone (`malloc_trim(0)` freed later-phase carryover, but not the schedule burst)
- not indexdata parsing (`set_parse_indexdata(false)` left the peak unchanged)
- not anything unique to `build_classify_schedule`

3. Reusing PASS1’s already-built way schedule is a real fix for that specific scan.
- It removes the `SMART_PASS2_SCHEDULE` burst entirely.
- It cuts PASS2 wall from about `25s` to `5s`.
- It improves total wall by about `16%` on Europe smart extract.

That is not a diagnostic-only result; it is already a meaningful mitigation.

4. But reusing only the PASS2 schedule does **not** solve the memory problem globally. The peak simply moves to PASS3, where [`build_blob_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) shows the same post-PASS1 scan burst pattern. So the real class of problem is now clear:

> header-only scans that run after PASS1’s parallel work can trigger a ~5-6 GB anon burst

5. Code structure still strongly argues that these scan helpers are not directly retaining gigabytes of Rust objects.
- [`build_classify_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/mod.rs#L426) builds a tiny schedule of `(usize, u64, usize)`.
- [`build_blob_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) builds a larger but still modest `Vec<BlobDesc>`.
- [`BlobReader::seekable_from_path`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L1058), [`read_blob_header`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L658), and [`WireBlobHeader::parse`](file:///home/folk/Programs/pbfhogg/src/read/blob.rs#L216) still do not contain an obvious “retain 6 GB” path.

So the round-3 data strengthens the allocator-state hypothesis substantially.

**Assessment**

**Q1: Is the “header scans after parallel work trigger arena growth” hypothesis supportable?**  
Yes. At this point it is the best fit to the evidence.

Why:
- PASS1’s first scan does not burst.
- PASS2 schedule scan bursts.
- PASS3 schedule scan bursts.
- Skipping one scan removes that one burst and shifts the peak to the next scan.
- `malloc_trim` before PASS2 proves there is some carry-over from PASS1, but the scan itself still regrows the heap.

That combination is much stronger than round 2. I think the hypothesis is now supportable enough to act on, even though the exact allocator mechanism is still unresolved.

The main caveat: this still does **not** prove the precise libc mechanism. It proves “post-parallel header scans are the trigger,” not “which allocator subsystem is responsible.”

**Q2: Most likely glibc-specific mechanism?**  
My strongest prior is now:

1. arena / heap-segment growth during the scan  
2. driven by allocator state established by prior parallel phases  
3. not by large logical live objects in the scan code itself

I would rank the specific sub-mechanisms:
- most likely: arena fragmentation / heap growth behavior
- plausible: mmap-backed heap segment growth
- less likely: dynamic `mmap_threshold` as the main story
- least likely: per-thread arena creation, because the bursting scans themselves are single-threaded

The two-step climb pattern is consistent with multiple heap-growth events rather than one steady retained object. That makes “allocator expanding in chunks” more plausible than “one big forgotten allocation.”

If you want to distinguish `brk`-arena growth from mmap-backed growth, `mallinfo2()` can help:
- `arena` / `uordblks` / `fordblks` for brk-managed heap behavior
- `hblks` / `hblkhd` for mmap-backed blocks

But the sharper diagnostic is probably syscall-level:
- `strace -e brk,mmap,munmap,madvise` around the Europe smart run
If the scan window is issuing large `brk` or `mmap` growth, that will show up directly.

**Q3: Which mitigation should you try first?**  
I no longer think `B -> A` is the best order if the goal is to ship a fix.

My updated order is:
1. **Option A first** for the product fix
2. **Option B second** if you still want allocator confirmation / broader cleanup

Reason:
- Option A already has partial proof from experiment 3.
- It gives deterministic benefit independent of allocator choice.
- It removes real wall time immediately.
- Extending it from PASS2 schedule reuse to PASS3 blob-schedule reuse is the most direct continuation of what already worked.

Option B is still valuable as a diagnostic and possibly as a broader runtime fix, but it is less certain:
- prior allocator experiments in this project were noisy or low-yield
- it broadens the blast radius
- it may fix this issue, but you do not yet know that

So if the question is “what should we do next in the code,” I would do:
- build all needed schedules up front in PASS1
- eliminate all later header scans in smart extract
- remeasure

If the question is “what is the cheapest experiment to validate allocator causality,” then yes, allocator swap is attractive. But as a mitigation order, I would pick A before B now.

**Q4: Anything overlooked in the two-step climb?**  
The two-step climb reinforces allocator behavior rather than weakening it.

It looks like:
- one growth event
- brief plateau / slower growth
- another growth event
- then a drop later

That shape fits:
- multiple heap expansions
- multiple heap segments
- different allocator subpaths activating at different sizes

I would not over-interpret the exact two-step structure yet, but it is consistent with allocator internals far more than with a single Rust object graph.

Yes, `mallinfo2()` at marker boundaries could be useful:
- if `arena` jumps, think brk-managed heap growth
- if `hblkhd` jumps, think mmap-backed growth
But if you only do one extra diagnostic, I would still choose `strace` on `brk/mmap/munmap` during the scan window over more in-process counters.

**Recommended next steps**

1. Keep the PASS2 schedule reuse change. It is a real win.
2. Extend the same idea to PASS3:
- prebuild the `BlobDesc` schedule during the initial PASS1 scan
- plumb it through instead of calling [`build_blob_schedule`](file:///home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) later
3. Re-bench Europe smart after both schedule builders are eliminated post-PASS1.
4. In parallel, if you want allocator proof, run:
- `strace -e brk,mmap,munmap,madvise`
or
- `mallinfo2()` snapshots at the schedule markers

**Bottom line**

Round 3 upgrades the diagnosis from “plausible allocator issue” to “high-confidence post-PASS1 header-scan allocator issue.” The most effective next move is no longer “keep probing `build_classify_schedule`.” It is:
- stop doing later header scans when you already have the information from PASS1
- use allocator diagnostics as confirmation, not as the first mitigation path

If you want the highest-confidence path to a user-visible fix, it is now **Option A first**.

=== perf ===

--- claude ---
**FOUND IT.** Line 219: `let mut blob_type = BlobKind::Unknown(String::new());`

This allocates a new `BlobKind::Unknown(String::new())` **on every call to `WireBlobHeader::parse`**. `String::new()` itself is zero-allocation (empty Vec). But:

Line 229-236 — the match arm for field 1 (blob type) REPLACES `blob_type` with `BlobKind::OsmHeader` or `BlobKind::OsmData` for standard blobs. The initial `BlobKind::Unknown(String::new())` is dropped at that point. No leak.

This is not the source.

Let me look more carefully at what happens per iteration. The initial `BlobKind::Unknown(String::new())` → empty String → no heap. Then field 1 is encountered, `read_len_delimited` returns a `&[u8]` slice of the header data, the match on the bytes creates a `BlobKind::OsmData` (stack). The old `BlobKind::Unknown(empty_string)` is dropped — no heap to free. Net: zero heap per parse.

### I genuinely cannot identify the source from the Rust code

Every allocation site I've traced is bounded. The 6 GB transient has no Rust-level explanation I can find. This strongly suggests the brief's hypothesis is correct: **it's glibc-specific allocator behavior**, and the mechanism is invisible at the Rust level.

---

## Report

### Q1: Is the "header scan after parallel work" hypothesis supportable?

**Partially, but with a critical correction.** The data supports "something is allocating 5-6 GB of anon pages during the scan loop AND freeing them at function return." It does NOT support "glibc arena retention from PASS1."

Evidence against retention:
1. `malloc_trim(0)` before the scan didn't help → the burst is NEW allocation, not lag
2. The sharp 5.45 GB drop at t=79.4 (function return) → the memory is genuinely freed when the scanner goes out of scope
3. The two-step climb trajectory → not a steady accumulation pattern

Evidence for "new allocation during scan":
1. The peak rises inside the marker interval, not before it
2. Function return frees the peak
3. `malloc_trim` didn't pre-clear the burst, so it's not arena pages from before

**But here's the problem for the hypothesis: nothing in the Rust code should allocate 5-6 GB.** I've traced every path — `BlobReader::seekable_from_path`, `next_header_with_data_offset`, `WireBlobHeader::parse`, `BlobIndex::deserialize`, `self.header_buf.reserve`. Every allocation is bounded at KB-MB scale, not GB.

### The two-step climb needs a different explanation

The trajectory:
- Step 1: t=72.4-73.0, +2.3 GB over 600 ms (~4 GB/s growth rate)
- Plateau: t=73.0-74.6, flat at ~6.1 GB for 1.6 seconds
- Step 2: t=74.8-76.4, +3.7 GB over 1.6 seconds (~2.3 GB/s growth rate)
- Slow decline: t=76.4-79.4, dropping to 4.35 GB over 3 seconds
- Plateau: t=79.4+ stable at 4.35 GB

**A 1.6-second plateau between two climbs is significant.** The scan loop processes ~430K blobs in 21 seconds, meaning ~50 μs per iteration. 1.6 seconds of plateau is ~32K iterations of "no memory change." The loop is still running (blobs are being processed), but the anon RSS isn't growing.

**Hypothesis: the two climbs correspond to file readahead.** The kernel pre-fetches file pages via `fadvise(SEQUENTIAL)` or `POSIX_FADV_WILLNEED`. When the BufReader is created at line 1080 (`BufReader::with_capacity(256 * 1024, f)`), linux-direct-io feature may fadvise the file. The scan reads sequentially through ~1 GB of the PBF (file size on Europe ~40 GB, but only headers are read — ~200 MB).

**Wait — file page cache doesn't count as anon RSS.** This can't be the mechanism for anon growth.

### The REAL mechanism I missed

Let me re-read the sidecar's `anon` field carefully. The brief reports:

```
SMART_PASS2_SCHEDULE       19307ms   10.06 GB   10.06 GB       1578
```

Where the columns are `Duration | Peak RSS | Peak Anon | Peak Mflt`. **Peak RSS and Peak Anon are both 10.06 GB.**

The sidecar is reading `/proc/self/status` for `RssAnon` (anonymous mapped memory) and `VmRSS` (total RSS). If they're both 10.06 GB and equal, then all 10 GB is anonymous — not file-backed.

But file readahead would increase `RssFile`, not `RssAnon`. So the sampler IS correctly reporting anonymous memory.

**I genuinely can't find the 6 GB Rust-level allocation.** It has to be allocator behavior, but the specific mechanism is beyond what I can determine from the code alone.

### Q2: Most likely glibc-specific mechanism

Of the four candidates in the brief, I'd rank them:

**1. Most likely — per-thread arena instantiation from rayon worker idle-state cleanup.** When `parallel_classify_phase` returns at the end of PASS1_RELATION_CLASSIFY, its `std::thread::scope` joins all workers. The worker threads in the rayon pool don't terminate — they return to the rayon global pool and become idle. But their per-thread glibc arenas still exist. When PASS2 SCHEDULE runs, something causes these per-thread arenas to grow or consolidate. The two-step climb might correspond to two distinct arena growth events as background glibc bookkeeping runs.

**Diagnostic:** set `MALLOC_ARENA_MAX=1` environment variable before launching. This forces glibc to use a single arena, eliminating per-thread arena behavior. If the burst disappears, per-thread arenas are the cause.

**2. Less likely — dynamic `M_MMAP_THRESHOLD` adjustment.** glibc can grow mmap_threshold up to 32 MB based on observed allocation patterns. After PASS1's many allocations, the threshold might be elevated. New allocations that previously went to mmap now go to brk, extending the main arena. But this wouldn't explain the 6 GB growth — the main arena's brk segment can grow without bound, but only in response to actual allocations.

**3. Unlikely — arena fragmentation forcing brk extension.** Fragmentation affects efficiency but doesn't directly cause 6 GB growth for a function that does ~1 MB of actual allocation.

**4. Mystery option — a Rust-level allocation I haven't found.** Possible but my trace of every function is consistent with small allocations only.

### Q3: Which mitigation to try first?

**Option A first, not B.** Here's why:

**Option A (build all schedules upfront) is the *correct* fix regardless of root cause.** It eliminates a redundant file scan — `collect_pass1_generic` already reads every blob header, and the smart PASS2/PASS3 schedules are computable from that same scan. Building them upfront saves ~47 seconds of wall time (19s PASS2 SCHEDULE + 28s PREAD_WRITE_BLOB_SCHEDULE) regardless of the memory issue. If the memory burst also disappears, that's a bonus.

**Option B (allocator swap) is a speculative test that might show nothing.** If the hypothesis about per-thread arenas is wrong, jemalloc/mimalloc won't help and you'll have added a dependency for no benefit. Even if it does help, you still have redundant file scans costing 47 seconds of wall time.

**The correct order: A first (fixes the wall time, may fix the memory), then B if needed (addresses residual memory if A doesn't).**

### Alternative cheap diagnostic before Option A

Before implementing Option A, run the `MALLOC_ARENA_MAX=1` experiment. This is a zero-code change (just set the env var in the bench invocation). If it collapses the peak, you've confirmed per-thread arenas are the cause AND gained a workaround that doesn't require code changes.

```bash
MALLOC_ARENA_MAX=1 brokkr extract --dataset europe --bbox ... --strategy smart --bench 1 --force
```

If this shows SMART_PASS2_SCHEDULE peak drop from 10 GB to ~4 GB, the allocator arena hypothesis is confirmed and you can either:
- Set `MALLOC_ARENA_MAX=1` globally in the CLI startup (one line)
- Switch to mimalloc/jemalloc (more invasive but better multi-thread performance)
- Still do Option A for the wall-time savings

### Q4: The trajectory shape

**The two-step climb is consistent with sequential file reading triggering readahead-then-allocation cycles.** When the BufReader reads 256 KB at a time, the kernel reads up to `RA_PAGES` pages ahead (default 128 KB on most filesystems). As each 256 KB chunk is consumed, the next 256 KB is prefetched. But this is file-backed I/O — it affects `RssFile`, not `RssAnon`.

**If Peak RSS and Peak Anon are both 10.06 GB**, then Peak File = 0. No page cache contribution. The memory really is anonymous.

The two-step climb might correspond to:
- Step 1: Initial arena extension as `next_header_with_data_offset` runs its first few thousand iterations. Each iteration allocates ~a few bytes of transient state, but glibc extends the arena in larger chunks to amortize sbrk syscalls.
- Plateau: Arena has enough space for subsequent allocations, no new extension needed.
- Step 2: A specific iteration triggers a larger allocation (maybe a blob with unusual header structure, or a mmap_threshold crossing), causing a second arena extension.

**Without `mallinfo2()` instrumentation at sub-phase granularity, I can't distinguish between these.** The brief's suggestion to call `mallinfo2()` at marker boundaries IS valuable — it would show `arena` (brk-managed bytes), `hblkhd` (mmap bytes), `fsmblks` (fastbin free), and `uordblks` (total allocated). A step change in `hblkhd` would indicate mmap-based allocations; a step change in `arena` would indicate brk extension.

### Recommended action

1. **Run the `MALLOC_ARENA_MAX=1` experiment.** Zero code change, 20 minutes of bench time. If it collapses the peak, you have your answer and a workaround.

2. **If MALLOC_ARENA_MAX=1 doesn't help**, instrument `mallinfo2()` at marker boundaries in `build_classify_schedule`. Print `arena`, `hblkhd`, `uordblks` at start and end of the scan loop. This will tell you whether the growth is brk or mmap.

3. **Regardless of the mechanism**, implement Option A (build all schedules upfront in `collect_pass1_generic`). This saves 47 seconds of wall time and is architecturally cleaner — fewer file opens, single source of truth for blob schedules. Even if it doesn't fix the memory issue, it's a wall-time win worth shipping.

4. **Don't switch allocators yet.** Option B is speculative and won't address the redundant scans. Defer until after A is shipped and MALLOC_ARENA_MAX experiment clarifies the mechanism.

### What I'm most uncertain about

The trajectory with two distinct climbs and a 1.6-second plateau between them doesn't fit any simple allocator behavior I know. It might indicate:
- A background task running concurrently (rayon pool cleanup, decompress pool state, something else)
- A specific blob structure in the middle of the file triggering a second allocation event
- A threshold crossing (glibc's `M_MXFAST` or `M_TRIM_THRESHOLD` being re-evaluated)

The only way to know is to run with instrumentation. `mallinfo2()` at 1-second intervals during the SCAN_LOOP would tell you whether the arena state is matching the trajectory.

### The pragmatic recommendation

Stop investigating the mechanism and ship Option A. The time you've spent diagnosing this is approaching the time you'd spend implementing the fix. The root cause is interesting but not blocking if Option A makes the symptom go away (by eliminating the trigger).

If Option A doesn't eliminate the peak (e.g., the same pattern appears elsewhere), THEN come back and instrument `mallinfo2()` to find the glibc mechanism.

--- codex ---
**Findings**

1. The round-3 note convincingly falsifies the original root cause. The `cc19d26` change at [extract.rs:2813](/home/folk/Programs/pbfhogg/src/commands/extract.rs:2813) was not the main source of the 10.7 GB burst. Its effect on peak memory was small, while its wall-time improvement was real. That means the earlier “worker-local `IdSetDense` blowup” diagnosis was materially wrong.

2. The current code does not support the idea that `build_classify_schedule()` or `BlobReader` is literally allocating and retaining 5-6 GB of Rust-level state. The relevant path in [mod.rs:427](/home/folk/Programs/pbfhogg/src/commands/mod.rs:427) and [blob.rs:658](/home/folk/Programs/pbfhogg/src/read/blob.rs:658), [blob.rs:1015](/home/folk/Programs/pbfhogg/src/read/blob.rs:1015), [blob.rs:1069](/home/folk/Programs/pbfhogg/src/read/blob.rs:1069) only:
- reuses one header buffer
- reads one header at a time
- skips blob payloads
- pushes small schedule tuples
There is no obvious mechanism there for multi-gigabyte retained anon growth.

3. The strongest new evidence is that the same burst pattern moved from PASS2 to PASS3 when PASS2 schedule-building was skipped. That is the key result. It means the problem is not specific to [build_classify_schedule](/home/folk/Programs/pbfhogg/src/commands/mod.rs:427); it is something about these later header-scan phases in general. The fact that the analogous scan inside [collect_pass1_generic](/home/folk/Programs/pbfhogg/src/commands/extract.rs:2525) does not show the same spike is especially important.

4. Disabling `set_parse_indexdata(true)` not changing the peak is strong evidence that header indexdata parsing is not the culprit. The fixed-size copy in [blob.rs:648](/home/folk/Programs/pbfhogg/src/read/blob.rs:648) was already a weak candidate, and the experiment seems to rule it out.

5. `malloc_trim(0)` not materially reducing the schedule-local burst weakens the “this is just delayed release from PASS1” theory. It does not fully eliminate allocator effects as an explanation, but it does make “PASS1 left garbage around” a poor sole root cause.

**Assessment**

The note narrows the problem substantially:

- It is probably not `extract.rs:2813`.
- It is probably not `BlobReader` retaining schedule-wide state.
- It is probably not indexdata parsing.
- It is probably not just stale PASS1 heap that `malloc_trim` could fix.

What remains most plausible is a broader process/allocator interaction that becomes visible during these later header-scan windows, plus marker attribution landing the high-water there. The repeated appearance in both PASS2 and PASS3 schedule-building strongly supports that interpretation.

**Recommendations**

1. Keep `cc19d26`. It improved PASS2 wall time and is architecturally better, even though it was not the memory fix.

2. The best practical mitigation from the experiments is to eliminate late schedule-building passes where possible.
The note’s “reuse PASS1 schedule” result is the first thing that both:
- removes the schedule-local burst
- materially improves wall time
That makes “build schedules earlier and reuse them later” the highest-confidence direction.

3. Do not spend more time drilling into `BlobReader` internals unless new evidence appears. The current code in [blob.rs](/home/folk/Programs/pbfhogg/src/read/blob.rs) is too simple and too non-retaining to justify that focus.

4. If you continue diagnosing, instrument process/allocator state around the phase boundaries rather than adding more fine-grained markers inside the header-scan loop. The code already suggests the loop itself is not the literal allocator.

**Bottom line**

Round 3 successfully disproves the old diagnosis and makes a direct `BlobReader` bug unlikely. The real signal is that post-PASS1 header scans correlate with the burst regardless of which schedule builder runs. The most useful next move is to avoid those late scans by reusing schedules built earlier, not to keep chasing `IdSetDense` or header parsing.

=== arch ===

--- claude ---
Good. `std::thread::scope` ensures workers are joined before the function returns. Worker state (including `DecompressPool` buffers, `st_scratch`, `gr_scratch`) is freed on the worker thread at function end. After PASS1 returns, all worker-thread state is gone and its heap is returned to glibc.

Now I've verified everything I can from the code. Let me give my actual answers.

---

## My Answers

### Q1: Is "header scans after parallel work trigger arena growth" supportable?

**Yes, it's the best available hypothesis given the evidence, but the evidence does NOT uniquely identify arena growth as the cause.** Specifically:

- Three call sites with the same scan code, three different memory behaviors, differentiated only by "when" they run: strong evidence that the burst is contextual, not code-local.
- `malloc_trim(0)` before scan doesn't help: rules out "pre-existing free-list entries carried into the scan."
- Indexdata parsing doesn't matter: rules out a specific Rust-level allocation pattern in header parsing.
- Bypassing `build_classify_schedule` moves the burst to the next scan: rules out "this specific function has a bug."

The consistency of these findings is strong. But there's a gap: **I can read the code and see that Rust-level allocation in the scan is negligible**. 640 KB exclusive for Japan's scan. Something between the 640 KB of Rust allocations and the 6 GB of process anon is doing the work. Glibc arena behavior is the only layer I can point at, but I can't prove it from the code.

The trajectory shape (two-step climb, slow decline, sharp drop at function return) is consistent with glibc's `sbrk` extension followed by arena release at some later point. The sharp drop at function return suggests something is being explicitly freed at that moment — not just the schedule Vec (480 KB is undetectable at this scale). This is the piece that doesn't fit clean "arena growth" either. If glibc's arena was extended, it would shrink only on `malloc_trim` or on the next large `free`. A sharp 3.8 GB drop at exactly the function return is too precise for a lazy glibc shrink.

**The sharp drop at function return is suspicious.** It suggests something in the function's scope is being explicitly dropped that holds ~3.8 GB. But the only things in scope are `scanner`, `schedule`, `shared_file`, and the stack. The `scanner` (BlobReader<BufReader<File>>) is the most plausible candidate — but I've read its fields and none of them should be 3.8 GB.

### Q2: Most likely glibc mechanism

I'll commit to a guess: **dynamic M_MMAP_THRESHOLD ratcheting followed by arena extension via sbrk**. Here's the specific story I'd want to test:

1. PASS1's decode pool `malloc`s and `free`s many 1-4 MB buffers. Each `free` of a >128 KB chunk that was allocated via `mmap` (because threshold was 128 KB default) tells glibc's dynamic threshold logic: "this size is common, stop using mmap for it." M_MMAP_THRESHOLD ratchets up to ~4 MB.
2. After PASS1, the threshold is at ~4 MB. Future large allocations go through the brk arena, not mmap.
3. The scan runs. The `header_buf.reserve()` calls are small (few hundred bytes) and go through the brk arena.
4. But here's the kicker: the brk arena has a `M_TOP_PAD` (default 128 KB) worth of padding at the top. Every time `sbrk` needs to extend, it grows by at least M_TOP_PAD. With fragmentation and many small allocations, the arena can grow much faster than the sum of live allocations.
5. Between pass 1's 1-4 MB decompress buffers being freed into the arena free-list (not mmap'd) and the scan's small allocations, glibc's coalescing may keep marking pages as "in use" that aren't, causing incremental sbrk extensions.

This doesn't cleanly explain 6 GB either. I don't have a confident answer for Q2.

**To distinguish hypotheses experimentally, instrument `mallinfo2()` at phase boundaries.** The specific fields that matter:
- `arena` (total brk heap size): if this grows 6 GB, it's arena extension.
- `hblks` / `hblkhd` (count/bytes of mmap'd chunks): if this grows 6 GB, it's mmap_threshold not ratcheting.
- `fordblks` (total free space in arena): if this stays high while arena grows, fragmentation is the mechanism.
- `uordblks` (total allocated in use): should match Rust's view.

If `uordblks` stays near 640 KB while `arena` grows to 6 GB, that's direct confirmation of arena fragmentation/extension.

### Q3: Which mitigation first?

I agree with the brief's ordering but for different reasons:

**B (try jemalloc/mimalloc) is the right first test, but not because it's cheap to implement.** It's the right first test because it's the only way to confirm or rule out the glibc-specific hypothesis without building a test harness for mallinfo2. If jemalloc shows the same burst, the problem isn't glibc arena behavior — it's something Rust or pbfhogg is doing at a higher level that we haven't found. If jemalloc makes the burst disappear, the hypothesis is confirmed and the question becomes "ship jemalloc or find the glibc-specific fix."

Practical note: the previous objection ("duplicate `#[global_allocator]` with `--all-features`") is solvable by a feature flag that excludes jemalloc/mimalloc from conflicting builds. Standard pattern.

**A (build all schedules upfront in collect_pass1_generic) is the right actual fix, regardless of B's outcome.** Even if jemalloc fixes the memory burst, `build_classify_schedule` is 19-28 seconds of wall time that's duplicative work — PASS1 already scanned the file and knows which blobs are ways. Removing the redundant scan is a pure win on wall time. The memory fix is a bonus.

The brief's proposed order (B first to confirm diagnosis, A second as the actual fix) is correct.

**One thing I'd add: before doing B or A, add `mallinfo2()` instrumentation at SCHEDULE enter/exit.** It's ~20 lines of code via libc, takes one run to collect the data, and will give you a definitive answer on whether the 6 GB is in `arena` vs `hblks` vs `fordblks`. This is cheaper than switching allocators and produces more useful information.

### Q4: Trajectory shape

The two-step climb is interesting but I can't interpret it confidently without more data. Possibilities:

1. **First climb = brk extension phase, second climb = mmap extension phase.** If the first climb fills up whatever free space glibc had after PASS1, and the second climb is fresh mmap chunks when sbrk hits a limit (e.g., rlimit_as or memory pressure), you'd get two distinct sources. `mallinfo2` would show `arena` growing in climb 1 and `hblks` growing in climb 2.

2. **Two different file regions trigger different allocation patterns.** Maybe the scan hits a transition where blobs change shape (e.g., node blobs vs way blobs have different header sizes or per-header metadata). That would mean `header_buf` has to grow at the transition.

3. **Artifact of the 200ms sidecar granularity.** If the actual growth is one smooth curve sampled at 200ms intervals, what looks like two steps could be one linear growth over 3.6 seconds (t=72.4 to t=76.0). The "slow decline" phase (t=76.4 to t=79.4) is 3 seconds of ~5 GB drop, which is ~1.7 GB/s — consistent with glibc gradually walking its free lists and returning pages during the scan's final iterations, then a cliff at function return when the `scanner` is dropped.

I don't have enough data to distinguish these. `mallinfo2()` at 200ms granularity during the scan would.

### One thing the brief doesn't consider: the scanner's lifetime

The sharp drop at function return (t=79.4s: 4.35 GB, after being 9.56 GB at t=76.6s) is too sharp to be glibc lazy shrink. Something is being explicitly dropped at that moment that holds ~5 GB.

The only candidates in `build_classify_schedule`'s scope at return time are:
- `scanner: BlobReader<BufReader<File>>` — explicitly dropped at line 455
- `schedule: Vec<(usize, u64, usize)>` — returned to caller (not dropped)
- `shared_file: Arc<File>` — returned to caller (not dropped)

When `scanner` is dropped, its `BufReader` is dropped, which drops the `File`. File drop shouldn't free user memory. BufReader has a 256 KB buffer.

Unless — and this is speculative — the BufReader's internal buffer has grown somehow. Let me check if there's any way BufReader's buffer can grow beyond 256 KB.

Standard `BufReader::with_capacity(256 * 1024, ...)` allocates a fixed 256 KB buffer. It doesn't grow. Not the source.

What about `self.header_buf` inside `BlobReader`? That's a `Vec<u8>` that grows via `reserve()` on every header read. The capacity is retained across iterations. What if a single fat blob header (e.g., a rare one with huge tagdata) caused the buffer to grow to several MB, and it stays at that capacity for the rest of the scan? 

But `parse_tagdata` is `false`, so tagdata isn't allocated. And the brief confirmed `set_parse_indexdata(false)` didn't change the peak. So the buffer growing due to parsing is ruled out.

**I'm stuck on explaining the sharp drop at function return from the code I can see.** The brief is right to ask for help.

### Recommendation

Before doing either B (allocator swap) or A (schedule reuse everywhere), **spend 30 minutes adding mallinfo2 instrumentation and running one more Europe smart bench**. This is the cheapest high-information diagnostic. Add `mallinfo2()` calls at:
- PASS1 entry
- PASS1 exit (after worker drop)
- SCHEDULE entry
- Inside the scan loop every 10K iterations
- SCHEDULE exit (before return)
- PASS2 classify entry
- PASS3 entry
- PASS3 `build_blob_schedule` exit

Print `arena`, `hblks`, `hblkhd`, `uordblks`, `fordblks` at each point. The difference between entry and exit of each phase tells you exactly where the bytes are going and whether they're in mmap chunks or the brk arena. This is the definitive test.

If `arena` grows 6 GB during the scan, that's sbrk extension — the fix is either `malloc_trim` inside the scan or switching allocators. If `hblks` grows 6 GB, that's mmap chunks — something is allocating large chunks that we can't see at the Rust level, and the fix is finding that source. If `fordblks` grows 6 GB, that's fragmentation — something is freeing into the arena faster than it can be reused, and `malloc_trim` should help at cleanup time.

This single measurement would save multiple rounds of guessing. The brief mentioned this as a possible diagnostic in Q2 but defaulted to B/A in the recommendation. I'd flip that: measure first, then act.

### The lesson carried forward

The brief's closing line — "every claim in this brief has been walked through the data" — is visible in the writing. Each experiment tests exactly one variable. Each conclusion states what's ruled in and what's ruled out. The wall improvement from the fix is kept separate from the memory puzzle. This is the epistemic discipline the previous rounds lacked, and it's paying off: the investigation has narrowed from "10 paths, which one?" in round 1 to "one mechanism, which layer?" in round 3. The remaining question is fundamental enough that the answer (glibc behavior vs a hidden Rust allocation) determines the fix strategy, so it's worth getting right before shipping any more code.

The one thing I'd push back on: the brief's framing treats the glibc hypothesis as near-certain ("this validates the reviewers' allocator-state hypothesis but with a sharper trigger than predicted"). That's consistent with the evidence but not proved. The sharp drop at function return is the piece that doesn't fit cleanly. mallinfo2 will either close the gap or reveal that the real mechanism is something else entirely.

--- codex ---
**Findings**

1. Round 3 makes the core conclusion much stronger: the 5-10 GB anon bursts are not owned by any obvious logical state in the header-scan code itself. The new peak moved from [`build_classify_schedule()`](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427) to [`build_blob_schedule()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) once PASS2 reused PASS1’s schedule, and both functions are structurally the same kind of header-only scan.

2. The new experiments rule out the two most plausible scan-local explanations:
- disabling `set_parse_indexdata(true)` did not change the peak, so indexdata parsing is not the source
- `malloc_trim(0)` between PASS1 and PASS2 did not materially reduce the scan-time burst, so this is not just “PASS1 left reclaimable junk behind and the next phase happened to measure it”

3. The narrow markers are the most important result in the note. They show the burst is inside the scan loops themselves, not:
- scanner construction / file open
- scanner drop
- schedule vector post-processing

That is consistent with process-wide allocator state changing while the loop runs, not with a large retained Rust object graph inside the schedule builders.

4. The code still does not support a “scanner is caching per-blob metadata” theory.
- [`BlobReader::seekable_from_path()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L1069) creates one `BufReader` with a 256 KB buffer.
- [`BlobReader::read_blob_header()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L658) reuses a single `header_buf`.
- [`WireBlobHeader::parse()`](/home/folk/Programs/pbfhogg/src/read/blob.rs#L216) only parses the current header; on these paths `tagdata` is not being parsed.
- [`BlobIndex::deserialize()`](/home/folk/Programs/pbfhogg/src/blob_index.rs#L109) is tiny and non-persistent.
- [`build_classify_schedule()`](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427) and [`build_blob_schedule()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) only accumulate small schedule vectors.

So the note’s updated framing is right: this is a scan-correlated anon spike, not a large scan-owned data structure.

5. Reusing PASS1’s way schedule is the first mitigation that is both effective and well-supported by evidence.
- [`Pass1Result`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2375) now carries the schedule.
- [`collect_pass1_generic()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2538) already builds it.
- [`extract_smart()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L2820) reuses it.
This eliminated the PASS2 schedule burst and improved wall time materially. That is the strongest practical result in the note.

6. The remaining peak moving to PASS3 is informative. It says the bug is not “one broken function”; it is “repeat a large header scan after PASS1’s heavy parallel allocator activity, and anon jumps again.” That pattern now appears in both schedule-building helpers.

**Assessment of the hypothesis**

The allocator-state hypothesis is now the best explanation, but still not pinned to one exact glibc mechanism.

What the evidence supports:
- not per-worker `IdSetDense`
- not indexdata parsing
- not a hidden `BlobReader` metadata cache
- not merely stale reclaimable memory that `malloc_trim` would have dropped
- yes, something process/allocator-wide that is triggered during repeated post-PASS1 scan loops

What is still uncertain:
- whether this is arena growth, fragmentation, threshold behavior between `brk` and `mmap`, or something similar
- why the growth has the reported stepped profile instead of a simple smooth ramp

So I would say the note is now strong on “what it is not” and moderately strong on “allocator/process-state effect,” but not yet strong on the exact submechanism.

**What I think is happening**

My best current model is:

- PASS1’s parallel decode/classify phases leave the allocator in a state where a later long-running main-thread loop that performs many small transient allocations causes glibc to grow anon-backed heap state aggressively.
- The scan loops are the trigger context, not the owner of a giant live structure.
- Because both [`build_classify_schedule()`](/home/folk/Programs/pbfhogg/src/commands/mod.rs#L427) and [`build_blob_schedule()`](/home/folk/Programs/pbfhogg/src/commands/extract.rs#L1479) show the same behavior, the root cause is likely below the Rust logical data model.

That matches the code better than any “retained headers/indexes” theory.

**Implications for next steps**

1. Option A from the note, “build and retain schedules up front in PASS1,” is now the best engineering mitigation.
- It already worked for PASS2.
- It directly removes repeated post-PASS1 header scans.
- It improves wall time as well as memory behavior.
- It is localized and deterministic.

If you can also prebuild the PASS3 blob schedule during PASS1 or otherwise before the allocator enters the problematic state, that looks like the most promising product fix.

2. Option B, allocator swap, is still useful, but more as diagnosis than as first product action.
- If `jemalloc` or `mimalloc` makes the spike disappear, that would strongly confirm the allocator-specific theory.
- But it is a coarser, cross-cutting change, and the note already has one mitigation with real evidence behind it.

So my ordering would be:
1. pursue schedule reuse / upfront schedule construction first
2. use allocator swap as a confirmation experiment if you still want root-cause confidence

**What I would instrument next**

If you want to understand the allocator behavior more precisely, the next high-signal step is not more extract logic instrumentation. It is allocator-state instrumentation:

- `mallinfo2()` snapshots:
  - before PASS1
  - after PASS1
  - mid schedule scan
  - after schedule scan
  - after PASS3
- or `malloc_info()` dumps at the same points if the XML output is acceptable

Those would tell you whether the growth is in arena heap vs mmap-backed allocations.

If external tooling is acceptable later, `strace` on `brk`, `mmap`, `munmap`, and `madvise` around one reproducer would also be very informative.

**Bottom line**

Round 3 substantially improves the diagnosis.

- The scan helpers themselves still do not plausibly own multi-gigabyte live state.
- The problem is now clearly “post-PASS1 repeated header scans trigger a large anon spike,” not “`build_classify_schedule` is broken.”
- Reusing PASS1-built schedules is the first mitigation with strong direct evidence.
- The remaining unknown is the exact allocator mechanism, not the practical direction for the fix.

If you want the shortest actionable conclusion: keep the current PASS2 reuse change, extend the same idea to PASS3, and treat allocator swapping as a validation experiment rather than the primary fix.
