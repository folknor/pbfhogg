# parallel_classify_phase regression — investigation summary

**Status (2026-04-11):** Wall-time improvement shipped (~29% on Europe smart
extract). Memory peak unchanged but now well-understood. Planet-scale memory
ceiling remains tight (~26-27 GB pro-rated) but no longer described by the
original "per-worker IdSetDense blowup" framing. Allocator swap (jemalloc/
mimalloc) is **not** being pursued — see "The unresolved planet blocker"
below.

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
*after* PASS1's parallel allocator work was redundant — `collect_pass1_generic`
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
doesn't grow. `uordblks` (live allocations) grows by 33 MB — that's exactly
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
   allocations come from glibc's free-list — which is scattered across the
   entire 8 GB free area in arbitrary places.
4. **Each "fresh" small allocation from a previously-untouched free-list
   slot triggers a page fault**, the kernel allocates a physical page, the
   page becomes resident, `RssAnon` ticks up.
5. **Over the scan loop's many iterations, most of the free-list area gets
   touched and becomes resident.** That's the "burst": 6 GB of cold pages
   becoming hot.
6. **glibc's accounting doesn't change** because nothing new is being
   allocated — the same fordblks pages that were already in the free-list
   are still in the free-list, just now resident in physical memory.

**Two implications for what the 6 GB transient actually is:**

- **It's not allocation churn.** The total bytes flowing through the
  allocator during the scan are tiny.
- **It's not glibc bookkeeping growth.** The arena and free-list sizes
  are static across the burst.
- **It's the working set of any post-PASS1 phase that touches the
  pre-existing free-list.** Header scans trigger it because they make many
  small allocations from various free-list slots. `parallel_classify_phase`
  triggers it for the same reason — per-blob `Vec<i64>` results flowing
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
   sense — `malloc_trim` only releases the *top* of the arena, not scattered
   free-list pages.

3. **Indexdata parsing** (`set_parse_indexdata(true)` in
   `build_classify_schedule`). Disabling it left the `SCAN_LOOP` peak
   essentially unchanged. Not the source.

4. **The `build_classify_schedule` function code itself**. Bypassing the
   function entirely (commit `d4ea760`'s schedule reuse) eliminated its
   peak — but only because the same allocation pattern then surfaced in
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
ruled-out hypotheses: the 6 GB is *not* an allocation at all — it's first-
touch faulting of pre-existing reserved address space.

## The unresolved planet blocker

**Memory ceiling on Europe smart extract:** ~10–11 GB peak anon, regardless
of which phase reports it (`PREAD_WRITE_BLOB_SCHEDULE`, `PREAD_WRITE_EXECUTE`,
or `SMART_PASS2_CLASSIFY` depending on the run). Pro-rated to planet
(~2.6× Europe): ~26–28 GB. **Tight on the 30 GB plantasjen host but does
not OOM in practice.**

The mechanism — cold-arena-page residency cascade — means the ceiling is
gated by **how much fordblks bloat PASS1 leaves behind** (8–11 GB on
Europe), not by what any single post-PASS1 phase does. Eliminating one
consumer of the cold free-list helps wall time but doesn't shrink the
free-list.

**Allocator swap (jemalloc/mimalloc) is NOT being pursued.** Per project
history (TODO.md "Global allocator investigation"), jemalloc and mimalloc
have been benchmarked multiple times in this project and don't behave the
way reasoning about their `madvise(DONTNEED)` policy would suggest. They
were also removed from the CLI feature flags because they broke
`--all-features` builds via duplicate `#[global_allocator]` definitions.
**Meta has restarted active jemalloc development** — revisit when that
work matures.

**Remaining mitigation directions** (none currently planned):

1. **Reduce PASS1's cumulative allocation footprint.** The `parallel_classify_phase`
   per-blob `Vec<i64>` channel pattern is the dominant source of small-allocation
   churn (~4.2 GB cumulative on Japan, ~63 GB on Europe extrapolated). Switching
   back to per-worker accumulation for some paths would reduce churn but reintroduces
   the planet-safety concerns the round-1 design review was about.
2. **Custom arena allocator** (e.g., bumpalo) for PASS1's parallel work that
   gets explicitly munmap'd at PASS1 end. Larger structural change. See
   TODO.md "Custom allocators (per-block arena)" entry.
3. **Restructure PASS1 to use pooled fixed-size buffers** instead of per-blob
   `Vec` allocations. Would require redesigning the `parallel_classify_phase`
   API to expose buffer recycling. Significant refactor.
4. **Accept the ceiling and document it.** Pro-rated to planet, the ceiling
   is 26–28 GB. Not an OOM in production (the pipeline runs as one cycle,
   not concurrently with anything heavy), just tight headroom on plantasjen.

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

- `src/commands/extract.rs:2397` — `Pass1Result` struct with `way_schedule`
  and `pass3_blob_schedule` fields
- `src/commands/extract.rs:~2566` — `collect_pass1_generic`'s sorted-path
  scan loop building both schedules
- `src/commands/extract.rs:~2898` — `extract_smart` PASS2 consuming the
  way schedule via `mem::take`, with fallback to `build_classify_schedule`
- `src/commands/extract.rs:~2992` — `extract_smart` PASS3 consuming the
  blob schedule via `mem::take`, with fallback to `pread_write_pass`
- `src/commands/extract.rs:~2384` — `extract_complete_ways` PASS2 doing
  the same blob-schedule reuse
- `src/commands/extract.rs:1700-1750` — `pread_write_pass` and
  `pread_write_pass_with_schedule`
- `src/commands/mod.rs:418-468` — `build_classify_schedule` with the
  three sub-phase markers
- `src/debug.rs:24-66` — `emit_counter` and `emit_mallinfo2` helpers

## Stored sidecar UUIDs (in `.brokkr/results.db`)

- `f420c5fd` — Europe smart, `fc17b51`, original pre-refactor baseline
- `01de22bb` — Europe smart, `5ca2df9`, post-`parallel_classify_phase`
  refactor (the original "10 GB peak" measurement that started the
  investigation)
- `0c60ec88` — Europe smart, `cc19d26`, `--bench 3` variance check showing
  the post-fix peak is real, not noise
- `8ac56b15` — Europe smart, `51f820d`, first run with the new sub-phase
  markers (the "burst is in PREAD_WRITE_BLOB_SCHEDULE" finding)
- `9b6fe6cc` — Europe smart, `d4ea760` + `MALLOC_ARENA_MAX=1` env var
  (the per-thread arena falsification)
- `4eccb6f3` — Japan smart, `51f820d`, `--alloc` per-function attribution

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
   nearly free at runtime — the bias should be toward adding them, not
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
   wrong — the question to ask is "which one is the planet blocker."

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
