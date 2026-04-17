# parallel_classify_phase regression - investigation summary

**Status (2026-04-11): CLOSED.** Wall-time improvement shipped (~29% on
Europe smart extract, confirmed +17% on complete and +15% on simple via
the same `0b085b1` schedule reuse). Planet smart extract measured at
**11.17 GB peak anon / 279 s wall** on commit `cadc3e6` (UUID `2d028196`,
plantasjen, 27.9 GB avail, Europe bbox) - the Europe×2.6 = 26-28 GB
projection was wrong by ~2.4×, because peak anon is dominated by PASS3
write work (bbox-sized) and not by PASS1 scanning the input file.
Per the decision tree in "Planet ceiling" below, planet < 25 GB → **ship
as-is**; no mitigation-menu item needs to be implemented. The round-4
menu (packet pool, compact payload, malloc_trim-at-boundary, bumpalo
arena) is preserved below as historical investigation context, not as
outstanding work.

## What we shipped

Five commits across two days, in order:

| Commit | What | Wall on Europe smart |
|---|---|---|
| `5ca2df9` | (baseline before investigation) | 254 sec |
| `cc19d26` | `extract.rs:2813`: convert smart PASS2 way-deps from `parallel_classify_accumulate` to per-blob send via `parallel_classify_phase<(), Vec<i64>>`. Architecturally correct; PASS2 wall improvement was real but planet-blocker framing was wrong. | ~263 sec |
| `51f820d` | Marker rename (`EXTRACT_PASS*` → strategy-prefixed `SMART_*` / `COMPLETE_*` / `SIMPLE_UNSORTED_*`); sub-phase markers in smart PASS1/2/3; hotpath annotations on `build_classify_schedule`, `build_blob_schedule`, `build_blob_schedule_with_passthrough`, `pread_write_pass`. | similar |
| `eba72a9` | Notes-only update flagging the round-2 finding and setting up reviewer round 3. | n/a |
| `d4ea760` | **Smart PASS2 schedule reuse**: plumb `full_way_schedule` out of `collect_pass1_generic` via `Pass1Result`; `extract_smart` PASS2 uses it via `mem::take` instead of calling `build_classify_schedule` again. Falls back to `build_classify_schedule` for the unsorted-fallback path. | ~209 sec (−16%) |
| `0b085b1` | **Smart PASS3 + complete PASS2 schedule reuse**: plumb full `Vec<BlobDesc>` (`pass3_blob_schedule`) out of `collect_pass1_generic` alongside the way schedule. Add `pread_write_pass_with_schedule` variant that takes a pre-built schedule. Both `extract_smart` and `extract_complete_ways` use it via `mem::take`. Also adds `emit_mallinfo2` helper and snapshots at marker boundaries. | **~180 sec (−29%)** |

**The pattern across the wall improvements:** every header scan that ran
*after* PASS1's parallel allocator work was redundant - `collect_pass1_generic`
already scans the whole file once. By plumbing the per-phase schedules out of
PASS1's existing scan, smart extract now does ONE file scan instead of
THREE. That alone is the 29% wall improvement.

## The actual mechanism: cold-arena-page residency cascade

**The 6 GB anon transient observed in `SMART_PASS2_SCHEDULE` (and later in
`PREAD_WRITE_BLOB_SCHEDULE`) is NOT new heap allocation.** It's existing
glibc arena pages becoming resident as post-PASS1 code touches them for the
first time.

**Decisive evidence (commit `0b085b1`'s mallinfo2 instrumentation):**

```
MI_PASS1_END:           arena=10.10 GB   hblkhd=3.16 GB   uordblks=1.94 GB   fordblks=8.16 GB
MI_PRE_BLOB_SCHEDULE:   arena=10.16 GB   hblkhd=3.16 GB   uordblks=1.94 GB   fordblks=8.21 GB
MI_POST_BLOB_SCHEDULE:  arena=10.16 GB   hblkhd=3.16 GB   uordblks=1.97 GB   fordblks=8.18 GB
```

**Glibc's tracked heap state across the burst window is essentially
constant.** `arena` (brk-managed) doesn't grow. `hblkhd` (mmap chunks)
doesn't grow. `uordblks` (live allocations) grows by 33 MB - that's exactly
the schedule Vec we expected. The allocator's view is steady.

**But the sidecar's `RssAnon` (from `/proc/self/status`) shows the same
window peaking at ~10 GB anon.** That's because:

1. **At PASS1 end, glibc has reserved ~13 GB of address space via `sbrk`**
   (10 GB arena + 3 GB mmap chunks). Of that, only ~1.94 GB is live
   (`uordblks`), and ~8 GB is sitting in the free-list (`fordblks`).
2. **Most of those 8 GB of free-list pages have never been written to.**
   They're reserved address space but not yet faulted in. The kernel doesn't
   count them as `RssAnon` until something writes to them.
3. **The post-PASS1 scan loop allocates ~few MB of small Vecs**, but those
   allocations come from glibc's free-list - which is scattered across the
   entire 8 GB free area in arbitrary places.
4. **Each "fresh" small allocation from a previously-untouched free-list
   slot triggers a page fault**, the kernel allocates a physical page, the
   page becomes resident, `RssAnon` ticks up.
5. **Over the scan loop's many iterations, most of the free-list area gets
   touched and becomes resident.** That's the "burst": 6 GB of cold pages
   becoming hot.
6. **glibc's accounting doesn't change** because nothing new is being
   allocated - the same fordblks pages that were already in the free-list
   are still in the free-list, just now resident in physical memory.

**Two implications for what the 6 GB transient actually is:**

- **It's not allocation churn.** The total bytes flowing through the
  allocator during the scan are tiny.
- **It's not glibc bookkeeping growth.** The arena and free-list sizes
  are static across the burst.
- **It's the working set of any post-PASS1 phase that touches the
  pre-existing free-list.** Header scans trigger it because they make many
  small allocations from various free-list slots. `parallel_classify_phase`
  triggers it for the same reason - per-blob `Vec<i64>` results flowing
  through the channel are many small allocations. Both produce the same
  cascade.

**Why `collect_pass1_generic`'s manual scan does NOT show the burst:** it
runs FIRST, before any parallel work has populated the free-list. The arena
is ~zero, fordblks is ~zero, no cold pages exist to be brought into
residence.

## Why simpler diagnoses failed

In rough chronological order, with the experiments that disproved each:

1. **Per-worker `IdSetDense` accumulation in `parallel_classify_accumulate`**
   (the round-1 design-review prediction). Fixed in `cc19d26` by converting
   `extract.rs:2813` to per-blob send. **Peak moved by 0.6 GB out of 10.7 GB.**
   The architectural fix is correct (and was kept) but it's not the cause.
   The chunk-spread model (6 workers × 1.5 GB = 9 GB) was workload-dependent
   and overestimated.

2. **Allocator arena retention from PASS1's parallel work**. `malloc_trim(0)`
   between PASS1 and PASS2 reclaimed ~2 GB of carried-forward arena pages
   (visible in subsequent phases as ~2 GB lower peaks), but the
   `SMART_PASS2_SCHEDULE` burst was unchanged. The 4.6-second cost of the
   trim wasn't worth the modest savings. Not "allocator lag" in the simple
   sense - `malloc_trim` only releases the *top* of the arena, not scattered
   free-list pages.

3. **Indexdata parsing** (`set_parse_indexdata(true)` in
   `build_classify_schedule`). Disabling it left the `SCAN_LOOP` peak
   essentially unchanged. Not the source.

4. **The `build_classify_schedule` function code itself**. Bypassing the
   function entirely (commit `d4ea760`'s schedule reuse) eliminated its
   peak - but only because the same allocation pattern then surfaced in
   `build_blob_schedule` instead. Both functions are header scans, both
   trigger the cascade, the burst follows whichever scan happens first
   after PASS1.

5. **Per-thread glibc arenas**. `MALLOC_ARENA_MAX=1` env var forced glibc
   to a single arena. **The `PREAD_WRITE_BLOB_SCHEDULE` peak was unchanged**
   (8.78 → 8.87 GB). And the constrained arena made `PREAD_WRITE_EXECUTE`
   97 seconds slower (worker thread contention on one lock). Per-thread
   arena instantiation is not the trigger.

6. **A hidden Rust-level allocation in the `BlobReader` scan path**. Three
   reviewers independently read `src/read/blob.rs:653,983,1015,1069` and
   confirmed the path's total Rust-level live state during the scan is
   ~1-2 MB. There's no obvious allocation site for 6 GB.

The mallinfo2 data finally produced a confident answer that fits all the
ruled-out hypotheses: the 6 GB is *not* an allocation at all - it's first-
touch faulting of pre-existing reserved address space.

## Planet ceiling - MEASURED 2026-04-11, ship as-is

**Planet smart extract, commit `cadc3e6`, plantasjen 32 GB host, Europe
bbox, `--bench 1` single sample, UUID `2d028196`:**

| Metric | Europe-dataset (`48ca6bbb`) | Planet+EU bbox (`2d028196`) |
|---|---|---|
| Input file | 35.3 GB | 92.0 GB (2.6×) |
| Wall total | 181 s | **279 s** (+54%) |
| **Peak anon RSS** | **10.71 GB** | **11.17 GB** (+4%) |
| SMART_PASS1 wall | 55.7 s | 89.4 s (+60%) |
| SMART_PASS3 peak anon | 10.71 GB | 11.17 GB |
| fordblks @ POST_EXECUTE | 10.93 GB | 13.41 GB (+23%) |
| arena @ POST_EXECUTE | 13.21 GB | 19.07 GB (+44%) |

**Per the round-4 decision tree (step 0, `:160-176`):**

- < ~25 GB → ship as-is, ceiling is fine in practice ← **we are here**
- 25-28 GB → accept (Option 4) defensible
- \> 28 GB → implement reusable packet pool

**Why the Europe×2.6 projection was wrong by 2.4×:** peak anon is
dominated by PASS3 *write* work, which scales with **bbox** (identical
Europe bbox in both runs), not by PASS1 scanning the input file. PASS1's
cold-arena cascade added only +0.46 GB of anon going from europe-dataset
to planet-dataset, even though `fordblks` bloat grew from 10.93 GB →
13.41 GB and total arena reservation grew from 13.21 GB → 19.07 GB. Most
of the extra reserved free-list pages never got first-touched - the
cascade mechanism is real but its magnitude is gated by what downstream
consumers actually touch, not by the size of the reserved pool.

**Caveats on the measurement:**

- Single `--bench 1` sample; no variance bounds. The gap to the 25 GB
  threshold (14 GB of headroom) is large enough that sample noise can't
  flip the decision.
- Europe bbox specifically. A substantially larger bbox (say, "most of
  the planet minus one small island") would grow PASS3's touched
  working set and could push peak anon higher by faulting in more of
  the 13.4 GB fordblks pool. If extract-on-planet ever becomes a
  recurring operation for bboxes larger than Europe, re-measure.
  Whole-planet bbox isn't a real workload - `cat` passthrough is the
  right tool for that.
- 32 GB host (27.9 GB avail at run start). Smaller hosts (e.g., 16 GB)
  would need a re-measurement; the headroom calculation is host-specific.

**Allocator swap (jemalloc/mimalloc) remains NOT pursued.** Per project
history (TODO.md "Global allocator investigation"), jemalloc and mimalloc
have been benchmarked multiple times in this project and don't behave the
way reasoning about their `madvise(DONTNEED)` policy would suggest. They
were also removed from the CLI feature flags because they broke
`--all-features` builds via duplicate `#[global_allocator]` definitions.
Meta has restarted active jemalloc development - revisit if the planet
measurement becomes tight for some other reason.

## Historical mitigation menu (NOT NEEDED - preserved as investigation context)

The round-4 reviewer consensus below predates the planet measurement
above. None of these options need to be implemented given the 11.17 GB
actual peak. The text is kept because it captures why we *would* have
picked the packet pool had the measurement gone the other way, and
because the mechanism analysis behind it (cold-arena-page residency
cascade, fordblks reservation dynamics) remains correct and worth
referencing in future investigations.

### Step 0: measure planet before implementing anything ✅ DONE 2026-04-11

**Result: 11.17 GB peak anon, 279 s wall, commit `cadc3e6`, UUID
`2d028196`** - see "Planet ceiling" section above for the full table
and mechanism analysis. Decision tree landed in the < 25 GB bucket
(ship as-is). Round-4 reviewers were correct that step 0 was the most
important action: the original projection was off by 2.4× and would
have motivated implementing the packet pool below for no benefit.

Attribution: perf/claude, round 4, concurred by planet/codex, perf/codex,
arch/codex, planet/claude. The collective "measure first, don't build a
fix for a problem you haven't measured" instinct was vindicated.

### Primary recommendation: reusable result packets in parallel_classify_phase

4 of 5 round-4 reviewers (planet/codex, perf/claude, perf/codex,
arch/codex) independently converged on this with nearly identical
language. The shape:

- Worker-local reusable result buffer, owned, fixed capacity sized for
  a typical blob's output
- Worker fills the buffer, sends ownership through the channel
- Consumer merges, clears, returns the buffer to a recycle pool
  (`crossbeam::ArrayQueue<Packet>` or similar)
- The same ~12 buffers (≈ 2× `decode_threads`) cycle through all ~20K
  blobs instead of spawning 20K fresh `Vec<i64>` allocations

**Why this attacks the cold-arena cascade directly:** the current code
allocates a fresh `Vec<i64>` for each blob's classification result and
drops it after the consumer merges. The allocator scatters these
small-Vec lifetimes across the arena, growing the `fordblks` reservation
to ~8-11 GB. A pool bounded at ~12 buffers of ~64 KB each has a touched-
page footprint of ~3 MB. The current cumulative channel allocation is
~1.3 GB of Vec churn → pool version is ~3 MB retained. Expected
`fordblks` reduction: 3-5 GB.

**Specific implementation shape (from perf/codex and arch/codex):**
make the result type an explicit reusable packet struct, not a generic
`Vec<i64>`. Something like `IdPacket { ids: Box<[i64; N]>, len: usize }`
or a slab-backed variable-length packet with fixed capacity. Workers
fill a packet, send ownership, consumer drains and returns via a
recycle channel. This is easier to optimize, instrument, and A/B
against the current code than generic `R: Send` pooling.

**What to watch for:** `R` crossing the thread boundary is the tricky
part. The packet must be `Send` but the current `parallel_classify_phase`
signature already handles that. The refactor is localized to the
worker loop body, the channel send, and the consumer's merge call.
Estimated ~200-400 LoC plus call-site updates. Planet-safe because
the pool size is bounded, worker memory is bounded by the packet
capacity, and the consumer's merge pattern doesn't change.

### Paired fifth option: compact packet payload

Two reviewers (perf/codex and arch/codex) independently proposed this
as a partner to the packet pool:

- Use `u32` where legal instead of `i64` for IDs that fit (node/way IDs
  within a 32-bit range - many real datasets)
- Delta-pack sorted IDs within a blob (classify results are monotonic
  within a blob because blobs are sorted)
- Or bucket local IDs/ranges before sending

**Why this is attractive on top of the pool:** smaller per-packet
capacity means smaller pool footprint (fewer cache lines touched,
lower memory bandwidth), and monotonic runs compress well. Even at
small pool sizes this is a measurable win on cache pressure and
consumer-side merge cost.

**This should be designed into the packet format from the start**, not
retrofitted. If the packet is opaque (just `Box<[i64; N]>`) the
compact payload becomes a separate refactor; if the packet type is a
protocol (`IdPacket { format: Compact | Raw, payload: ... }`) the
compact path is a drop-in optimization.

### Rejected: revert to per-worker accumulation for dense paths (Option 3)

4 of 5 reviewers rejected this for the dense paths. **The new mechanism
understanding does NOT rescue per-worker `IdSetDense` accumulation.**
Per-worker accumulation replaces "cold-page residency cascade" with
"real live state that scales with result cardinality and chunk spread."
For dense node/way classify paths, that live state still hits the
planet-safety concerns from round 1 - it just does so via a different
mechanism.

The only reviewer dissent (planet/claude) framed Option 3 as a 30-
minute validation experiment to re-check whether round 1's conclusion
was wrong under the new mechanism, not as a shipping recommendation.
If anyone wants to do that experiment: revert one dense-path call
site (e.g., `extract.rs`'s Pass 1 way classify) back to `parallel_classify_accumulate`,
add `mallinfo2` print at PASS1 end, measure the `fordblks` delta. If
`fordblks` drops from ~11 GB to ~1-2 GB, round 1's conclusion needs
revisiting. Otherwise this option is closed.

### Downgraded: custom arena / bumpalo for PASS1 (original Option 1)

Three reviewers (planet/codex, perf/codex, arch/codex) classified this
as "a bigger hammer than you need right now" and "a second-line
implementation strategy for the same basic idea." The reasoning: if
you build the ownership transfer semantics for moving arena-backed
buffers safely between workers and the consumer, **you've almost built
the packet pool already**. Arena-first reintroduces lifetime complexity
(R crosses the thread boundary, must not borrow from the per-worker
Bump) without producing anything the pool doesn't also produce.

The one case where arena-first wins is if PASS1 has MANY allocation
sources beyond the channel Vecs (PrimitiveBlock scratch, string
tables, decompress buffers) and you want to capture all of them in
one bump. But that's a much broader refactor affecting read-path code
that has nothing to do with this bug.

**Keep bumpalo in reserve as a narrower slab allocator for packet
storage if the pool approach isn't sufficient.** Not as the first
thing to try.

### Accept the ceiling (Option 4)

Only 2 of 4 reviewers recommended this as a primary path. perf/claude
framed it as defensible if the operational constraint is loose and the
planet bench shows the ceiling is actually fit-able with swap. The
other three said "only later if nothing else works."

This is still a valid choice given:
- 29% wall improvement is already shipped
- The production pipeline doesn't use smart extract (tile gen uses
  ALTW → elivagar → PMTiles; geocoding uses the geocode index)
- Extract-smart on planet is an ad-hoc user operation, not a recurring
  automated job

**Take this option if the planet bench from step 0 shows the ceiling
fits, or if implementation cost of the packet pool exceeds the
operational pain.**

### Opportunistic: malloc_trim(0) at PASS1/PASS2 boundary

perf/claude flagged this as a cheap partial mitigation we reverted
prematurely. The round 3 experiment showed `malloc_trim(0)` between
PASS1 and PASS2 freed ~2 GB of carried-forward arena pages (visible
as ~2 GB lower peaks in subsequent CLASSIFY and PASS3 phases) at a
cost of 4.6 seconds wall time. We reverted it because "it costs
4.6 seconds for benefits that don't address the planet blocker."

With the 29% wall improvement already shipped (254s → 180s), 4.6
seconds is ~2.5% of total wall, and the 2 GB reclaim is meaningful
headroom on a 30 GB host. **Worth reconsidering as a shipped partial
mitigation** - not a fix for the ceiling, but a guaranteed ~2 GB
cushion at a 2.5% wall cost. Take this if the planet bench shows the
ceiling is tight-but-fitting and you want a safety margin.

## What's instrumented now

`commit 51f820d` and `commit 0b085b1` together added:

**Sub-phase markers** (visible via `brokkr results <uuid> --markers --phases`):
- `SMART_PASS1_*`, `COMPLETE_PASS1_*`, `SIMPLE_UNSORTED_PASS1_*` (was
  conflated under one `EXTRACT_PASS1` name across three functions)
- `PASS1_NODE_CLASSIFY` / `PASS1_WAY_CLASSIFY` / `PASS1_RELATION_CLASSIFY`
  inside `collect_pass1_generic` (shared by smart and complete)
- `SMART_PASS2_SCHEDULE` / `SMART_PASS2_CLASSIFY` inside `extract_smart`
- `SMART_PASS3_SETUP` / `SMART_PASS3_WRITE` inside `extract_smart`
- `COMPLETE_PASS2_SETUP` / `COMPLETE_PASS2_WRITE` inside `extract_complete_ways`
- `SCHEDULE_SCANNER_OPEN` / `SCHEDULE_SCAN_LOOP` / `SCHEDULE_SCANNER_DROP`
  inside `build_classify_schedule`
- `PREAD_WRITE_BLOB_SCHEDULE` / `PREAD_WRITE_EXECUTE` / `PREAD_WRITE_FLUSH`
  inside `pread_write_pass`

**Hotpath annotations** (cfg-gated, visible via `brokkr extract --alloc`):
- `mod.rs::build_classify_schedule`
- `extract.rs::build_blob_schedule`
- `extract.rs::build_blob_schedule_with_passthrough`
- `extract.rs::pread_write_pass`
- `extract.rs::pread_write_pass_with_schedule`

**`mallinfo2` snapshots** (commit `0b085b1`, Linux only, via the new
`crate::debug::emit_mallinfo2(prefix)` helper in `src/debug.rs`):
- `MI_PASS1_END` (after PASS1 finishes)
- `MI_PRE_BLOB_SCHEDULE` (entering `pread_write_pass`'s scan path)
- `MI_POST_BLOB_SCHEDULE` (after the scan returns)
- `MI_POST_EXECUTE` (after the write loop)

The `mallinfo2` helper is reusable: pass any prefix and it emits
`<prefix>_arena`, `<prefix>_hblks`, `<prefix>_hblkhd`, `<prefix>_uordblks`,
`<prefix>_fordblks`, `<prefix>_keepcost` as counters. Useful for any future
investigation that needs to distinguish glibc state changes from RssAnon
changes.

## Code references

For future investigators, the relevant call sites and data structures:

- `src/commands/extract.rs:2397` - `Pass1Result` struct with `way_schedule`
  and `pass3_blob_schedule` fields
- `src/commands/extract.rs:~2566` - `collect_pass1_generic`'s sorted-path
  scan loop building both schedules
- `src/commands/extract.rs:~2898` - `extract_smart` PASS2 consuming the
  way schedule via `mem::take`, with fallback to `build_classify_schedule`
- `src/commands/extract.rs:~2992` - `extract_smart` PASS3 consuming the
  blob schedule via `mem::take`, with fallback to `pread_write_pass`
- `src/commands/extract.rs:~2384` - `extract_complete_ways` PASS2 doing
  the same blob-schedule reuse
- `src/commands/extract.rs:1700-1750` - `pread_write_pass` and
  `pread_write_pass_with_schedule`
- `src/commands/mod.rs:418-468` - `build_classify_schedule` with the
  three sub-phase markers
- `src/debug.rs:24-66` - `emit_counter` and `emit_mallinfo2` helpers

## Stored sidecar UUIDs (in `.brokkr/results.db`)

- `f420c5fd` - Europe smart, `fc17b51`, original pre-refactor baseline
- `01de22bb` - Europe smart, `5ca2df9`, post-`parallel_classify_phase`
  refactor (the original "10 GB peak" measurement that started the
  investigation)
- `0c60ec88` - Europe smart, `cc19d26`, `--bench 3` variance check showing
  the post-fix peak is real, not noise
- `8ac56b15` - Europe smart, `51f820d`, first run with the new sub-phase
  markers (the "burst is in PREAD_WRITE_BLOB_SCHEDULE" finding)
- `9b6fe6cc` - Europe smart, `d4ea760` + `MALLOC_ARENA_MAX=1` env var
  (the per-thread arena falsification)
- `4eccb6f3` - Japan smart, `51f820d`, `--alloc` per-function attribution

The four `--force` runs from rounds 2-3 (malloc_trim, narrow markers,
parse_indexdata=false, schedule reuse) are not in the DB. Their data is
captured in commit messages and the prior version of the round-by-round
notes (now consolidated into this file).

## Lessons for future investigations

A few process points that emerged from the wrong turns:

1. **When a measurement shows a multi-GB peak, ask "can we localize it
   with sub-phase markers?" before guessing the cause.** The original
   round-1 brief skipped straight to "this must be the per-worker
   `IdSetDense` accumulation." The sub-phase markers added in round 3
   immediately showed the burst was elsewhere. Sub-phase markers are
   nearly free at runtime - the bias should be toward adding them, not
   reasoning about peaks attributed to coarse phases.

2. **Walk every claim in a brief through your own measurement table
   before sending to reviewers.** The round-2 brief framed the wall
   regression as "shared accumulate-mode wall regression" but the
   measurement table showed tags-filter PASS1 used `parallel_classify_phase`,
   not accumulate. Two reviewers caught the contradiction within minutes.
   This is the cheapest kind of feedback to avoid.

3. **Cumulative-allocation tracking (`--alloc`) and peak-RSS tracking
   (sidecar) measure different things, and they can disagree
   spectacularly.** The Japan `--alloc` data attributed 90% of cumulative
   bytes to `parallel_classify_phase` (4.2 GB), but the sidecar said the
   peak was during `build_classify_schedule` (640 KB cumulative). Both
   are correct. Cumulative bytes flow through one function while peak
   anon RSS climbs in another, because the peak is about *touching*
   pages, not allocating them. When the two views disagree, neither is
   wrong - the question to ask is "which one is the planet blocker."

4. **`mallinfo2` is the right diagnostic for "is this allocator state or
   first-touch faulting".** It's ~20 LoC of instrumentation and gives a
   binary answer in one bench run. Should have been added in round 1
   instead of waiting until round 4.

5. **Reviewers can be confidently wrong about glibc behavior.** All four
   reviewers in round 2 endorsed "allocator arena retention from PASS1"
   as the most likely mechanism. `malloc_trim(0)` falsified that in
   round 3. Mechanism guesses about glibc internals are not a substitute
   for measurement.

6. **Allocator swap (jemalloc/mimalloc) is the kind of fix that sounds
   right but doesn't behave the way reasoning predicts in this project's
   history.** Multiple prior attempts have shown <1% wall difference at
   small scale and broke the build. Don't propose it without checking
   prior measurements first.
