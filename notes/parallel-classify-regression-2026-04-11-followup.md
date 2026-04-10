# parallel_classify_phase regression — followup brief — 2026-04-11

**Audience:** `planet` + `perf` + `arch` review archetypes
**Previous round:** [`notes/parallel-classify-regression-2026-04-10.md`](parallel-classify-regression-2026-04-10.md) (the brief) and [`notes/parallel-classify-regression-2026-04-10-reviews.md`](parallel-classify-regression-2026-04-10-reviews.md) (your responses).

## TL;DR

You confidently endorsed converting `extract.rs:2813` from `parallel_classify_accumulate` to `parallel_classify_phase` per-blob send as the fix for the 10 GB PASS2 anon burst. We shipped it (commit `cc19d26`). **It moved the peak by 0.6 GB out of 10.7 GB.** The chunk-spread diagnosis was wrong — yours and mine both.

We then added sub-phase markers and hotpath annotations across the smart-extract path (commit `51f820d`) and re-measured. The new data shows **the burst is not in the classify path at all**. It's inside `build_classify_schedule` — a function whose body looks like it cannot possibly allocate 6 GB.

We need a fresh diagnosis. The puzzle is sharper now and the data is better.

---

## What we did since the last brief

### 1. Shipped the fix

Commit `cc19d26`: converted `extract.rs:2813` from `parallel_classify_accumulate` (per-worker `IdSetDense`) to `parallel_classify_phase<(), Vec<i64>>` (per-blob send). The architectural fix you endorsed.

**Europe extract-smart, single-run baseline (`fc17b51` 2026-03-30) → `cc19d26` post-fix (2026-04-10):**
- PASS2 wall: 33.8s → 26.0s (−23%)
- PASS2 peak anon: **10.72 GB → 10.09 GB (−6%)** ← only ~0.6 GB savings
- Total wall: 254s → 263s (essentially flat)
- Major faults: **discount entirely** — they were dominated by cold/warm page cache differences across runs, not memory pressure (see point 3 below).

### 2. Variance check with `--bench 3`

You specifically asked for `--bench 3` to confirm the post-fix numbers weren't single-run noise. We did it. Three independent runs of the post-fix code on Europe smart:

| Run | PASS2 wall | PASS2 anon | Wall total |
|---|---|---|---|
| 0 | 27.7s | **10.71 GB** | 254s |
| 1 | 24.4s | **9.97 GB** | 248s |
| 2 | 27.7s | **10.74 GB** | 246s |

**PASS2 anon clusters tightly at 10.5 ± 0.4 GB across three independent runs.** Not noise. The fix really did only move the peak by ~0.6 GB. Pro-rated to planet (~2.6×): peak still ~27 GB, still doesn't fit 30 GB plantasjen.

### 3. Major fault counts were noise

Per-run major fault counts varied wildly: PASS1 majflt 7637 / 0 / 37; PASS2 majflt 68 / 1641 / 5. Run 0 had a cold page cache (first run of the session) and Runs 1–2 had warm cache from preceding runs. **Discount the earlier "48× majflt regression" framing entirely** — it was an artifact of comparing cold-vs-warm runs across separate sessions. The wall regression on its own remains real but smaller (~22%) and is now clearly orthogonal to the memory issue.

### 4. Added sub-phase markers and hotpath annotations

Commit `51f820d`. Two changes:

**Marker rename** (the old `EXTRACT_PASS1/2/3` markers were emitted by THREE functions so phase reports were conflated):
- `extract_simple` unsorted fallback → `SIMPLE_UNSORTED_PASS1/2`
- `extract_complete_ways` → `COMPLETE_PASS1/2` (with new `COMPLETE_PASS2_SETUP/WRITE` sub-markers)
- `extract_smart` → `SMART_PASS1/2/3` (with new `SMART_PASS2_SCHEDULE/CLASSIFY` and `SMART_PASS3_SETUP/WRITE` sub-markers)
- Inside `collect_pass1_generic` (shared by smart and complete): `PASS1_NODE_CLASSIFY`, `PASS1_WAY_CLASSIFY`, `PASS1_RELATION_CLASSIFY`

**Hotpath annotations added** (cfg-gated, no semantic impact):
- `mod.rs` `build_classify_schedule`
- `extract.rs` `build_blob_schedule`
- `extract.rs` `build_blob_schedule_with_passthrough`
- `extract.rs` `pread_write_pass`

Previously these were absorbed into `extract_smart` / `extract_complete_ways` in `--alloc` runs, hiding ~6 seconds of schedule build (smart PASS2) and ~149 seconds of write loop (smart PASS3) under the wrong attribution.

### 5. Ran `--alloc` (Japan, since Europe failed the mem preflight at 141 GB estimated)

UUID `4eccb6f3`, commit `51f820d`, Japan smart extract, 7 seconds wall.

**Function-level alloc attribution (exclusive bytes):**

| Function | Total | % |
|---|---|---|
| `parallel_classify_phase` | **4.2 GB** | **90.07%** |
| `block_builder::take_owned` | 264.6 MB | 5.57% |
| `frame_blob_into` | 103.9 MB | 2.19% |
| `parse_and_inline_with_scratch` | 47.2 MB | 0.99% |
| `block_builder::add_node` | 36.0 MB | 0.76% |
| `build_blob_schedule_with_passthrough` | 8.2 MB | 0.17% |
| `write_primitive_block_owned` | 7.4 MB | 0.16% |
| `collect_pass1_generic` | 3.9 MB | 0.08% |
| `build_classify_schedule` | **640 KB** | 0.01% |
| `parallel_classify_accumulate` | 16.2 KB | 0.00% |

**Per-thread:**
```
RSS: 2.0 GB, Alloc: 5.2 GB, Dealloc: 5.0 GB, Diff: 222.8 MB

Main thread:    Alloc 4.2 GB, Dealloc 4.1 GB, Diff 67 MB
Worker (×4):    Alloc ~40-47 MB each, Dealloc ~13-21 MB each
```

`parallel_classify_phase` is 90% of cumulative allocations. Workers are tiny — main thread does almost all of the allocation. **`build_classify_schedule` shows only 640 KB exclusive.** That's the puzzle: cumulative alloc says `parallel_classify_phase` dominates, but the sidecar (point 6) says the burst is during `build_classify_schedule`.

The two views measure different things. Hotpath/alloc tracks **cumulative bytes through the function frame** (exclusive of nested calls). Sidecar tracks **process-wide peak anon RSS**. They diverge when:
- A function allocates many small things that are immediately freed → big cumulative, small peak.
- A function allocates a few big things that live briefly → small cumulative, big peak.

`parallel_classify_phase` looks like the first (lots of small Vec<i64>s flowing through the channel). `build_classify_schedule` looks like the second.

### 6. Ran `--bench 1` on Europe with the new sub-phase markers

UUID `8ac56b15`, commit `51f820d`, Europe smart, 255s wall. **This is the decisive measurement.**

```
Phase                      Duration   Peak RSS  Peak Anon  Peak Mflt
SMART_PASS1                72393ms    3.73 GB    3.73 GB         17
PASS1_NODE_CLASSIFY        13657ms    245 MB     242 MB          17
PASS1_WAY_CLASSIFY         25805ms    3.66 GB    3.65 GB          4
PASS1_RELATION_CLASSIFY     6730ms    3.73 GB    3.73 GB          0
SMART_PASS2                24285ms   10.06 GB   10.06 GB       1578
SMART_PASS2_SCHEDULE       19307ms   10.06 GB   10.06 GB       1578   ← peak here
SMART_PASS2_CLASSIFY        4977ms    6.24 GB    6.24 GB          3   ← already dropped 3.8 GB
SMART_PASS3               146535ms    7.45 GB    7.44 GB       2152
SMART_PASS3_SETUP              1ms       0kB       0kB           0
SMART_PASS3_WRITE         146533ms    7.45 GB    7.44 GB       2152
```

**The 10 GB peak is during `SMART_PASS2_SCHEDULE`, the wrapper around `build_classify_schedule(input, Some(ElemKind::Way))`.**
- Duration: 19.3 seconds
- Peak anon: 10.06 GB
- Baseline at PASS2 entry: 3.73 GB (carried from PASS1)
- **Transient delta during SCHEDULE: ~6.33 GB**
- After SCHEDULE returns and CLASSIFY starts: 6.24 GB peak (so ~3.8 GB freed at function return)

`SMART_PASS2_CLASSIFY` (the actual `parallel_classify_phase` call we just spent two days analyzing) peaks at only **6.24 GB** — barely above the PASS1 baseline. **The classify path is not the problem. It never was.**

---

## What `build_classify_schedule` actually does

`mod.rs:426-456`:

```rust
pub(crate) fn build_classify_schedule(
    input: &Path,
    kind_filter: Option<ElemKind>,
) -> Result<(Vec<(usize, u64, usize)>, Arc<File>)> {
    let mut scanner = BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob().ok_or(...)??;

    let mut schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), BlobType::OsmData) { continue; }
        if let Some(filter_kind) = kind_filter {
            if let Some(idx) = hdr.index() {
                if idx.kind != filter_kind { continue; }
            }
        }
        schedule.push((seq, data_offset, data_size));
        seq += 1;
    }

    let shared_file = Arc::new(File::open(input)?);
    Ok((schedule, shared_file))
}
```

The body does nothing that obviously allocates 6 GB. The schedule itself is small: ~20K Way entries × 24 bytes = ~480 KB for Europe. The 6 GB transient must be inside one of:
- `BlobReader::seekable_from_path` (file open, scanner construction)
- `set_parse_indexdata(true)` (just sets a flag, almost certainly not the source)
- `next_header_skip_blob` (read the OsmHeader blob)
- `next_header_with_data_offset` (read each subsequent blob header) ← the loop body

These are in `src/read/blob.rs` lines 653, 983, 1015, 1069. We have NOT yet read the implementations.

---

## The puzzle that broke our previous reasoning

`collect_pass1_generic` (`extract.rs:2519-2541`) has nearly identical scan code that runs at the start of PASS1. It uses the same `BlobReader::seekable_from_path`, the same `set_parse_indexdata(true)`, the same `next_header_with_data_offset()` loop, and pushes to three Vec<(usize, u64, usize)> instead of one. It happens BEFORE the `PASS1_NODE_CLASSIFY` marker fires.

If that scan also added 6.33 GB of transient state, **PASS1 overall peak would be ~6.33 GB or higher**, since the markers bracket the entire function and capture the max anon during the bracket.

But **PASS1 overall peak is 3.73 GB**, achieved during `PASS1_RELATION_CLASSIFY`. The scan inside PASS1 demonstrably does NOT add 6.33 GB of transient memory.

**Why does the same scan code behave differently in two contexts?** That's the question we can't answer alone.

Hypotheses we can think of:
1. **It's not the same code.** `collect_pass1_generic` uses `filter.wants_index(&idx)` (a `BlobFilter`), `build_classify_schedule` uses `if idx.kind != filter_kind`. Maybe `BlobFilter` short-circuits something that the simpler match doesn't. Or vice versa — maybe parsing indexdata and discarding the result is cheaper than parsing and keeping it.
2. **Page cache effects.** PASS1's scan is the first read of the file (after parallel build setup); PASS2's scan is the second. Different page cache state, but anon RSS shouldn't be affected by file mmap.
3. **Allocator state.** glibc's allocator might fragment differently after PASS1's parallel work runs. PASS2's scan hits a fragmented arena and triggers a different allocation pattern.
4. **Pre-existing state collusion.** The 6.33 GB transient during PASS2 SCHEDULE isn't actually from `build_classify_schedule` at all — it's some background work (decompress pool, allocator coalescing, rayon thread pool init) that happens to fall within the SCHEDULE marker bracket and gets attributed to it.
5. **`BlobReader::seekable_from_path` allocates per-blob structures during the constructor.** Maybe it pre-scans something and caches per-blob state. The constructor returns an iterator that lazily yields headers but the underlying reader holds all of them.

We need someone with eyes on `src/read/blob.rs` to either confirm or rule out (5), and on rayon/glibc to weigh in on (3) and (4).

---

## Questions for the reviewers

### Q1 — Where is the 6.33 GB transient inside `build_classify_schedule`'s call path?

The function body itself can't be it (small loop, no allocations). The candidates are:
- `BlobReader::seekable_from_path` (`src/read/blob.rs:1069`)
- `set_parse_indexdata` (`src/read/blob.rs:653`) — almost certainly not
- `next_header_skip_blob` (`src/read/blob.rs:983`)
- `next_header_with_data_offset` (`src/read/blob.rs:1015`)

What in those functions could plausibly allocate 6 GB on Europe (~430K total blobs, ~20K of which are way blobs)? The natural unit of work is per-blob, so 6 GB / 430K = ~14 KB per blob. That's roughly the size of a parsed indexdata structure with tagdata, or the size of a decompressed header. Not implausible.

Specific question: does parsing indexdata for every blob accumulate state that lives until the scanner is dropped? If `next_header_with_data_offset` returns an owned `BlobHeader` containing parsed indexdata, and the scanner internally caches them, that could grow ~14 KB × 430K = ~6 GB.

We have not yet read these functions. Please look — your collective context window is much wider than ours and you'll spot the allocation pattern faster than we can.

### Q2 — Why doesn't `collect_pass1_generic`'s scan show the same transient peak?

Same code, same file, same flag. The only differences:
- `build_classify_schedule` builds 1 schedule with a kind filter; `collect_pass1_generic` builds 3 schedules without a kind filter (it dispatches to the right one based on `idx.kind`).
- `collect_pass1_generic`'s scan happens FIRST (cold page cache); `build_classify_schedule`'s happens SECOND (warm page cache).
- `collect_pass1_generic` runs after `BlobReader::open(input, direct_io)` was already opened+dropped at line 2459 (before the manual scan); `build_classify_schedule` runs after the parallel classify phases have populated the allocator with their patterns.

Are any of these differences enough to explain why one allocates 6 GB transiently and the other doesn't? Or is there a third explanation we're missing — e.g., the transient isn't in `build_classify_schedule` at all but in something running concurrently or asynchronously that we're attributing wrong?

### Q3 — Should we add narrower hotpath annotations on the BlobReader path?

We already added annotations to `build_classify_schedule`, `build_blob_schedule`, `build_blob_schedule_with_passthrough`, and `pread_write_pass`. The next layer of attribution would be on:
- `BlobReader::seekable_from_path`
- `BlobReader::next_header_with_data_offset`
- Any indexdata parser (`parse_indexdata`, `BlobIndex::parse`, etc.)

These would let `--alloc` show whether the bytes are in the constructor, the per-iteration call, or the indexdata parser. Worth doing? Or is there a faster diagnostic — e.g., comment out `set_parse_indexdata(true)` temporarily and re-bench, see if peak collapses?

The cheapest experiment we can think of: **temporarily disable `set_parse_indexdata(true)` in `build_classify_schedule`** and re-bench Europe smart. If the SCHEDULE peak drops from 10 GB to ~4 GB, the indexdata parser is the source. (We'd then revert and add proper instrumentation.) But this would break PASS2's filter-by-kind logic — we'd need to skip the kind filter too, which changes behavior. The alternative is adding the markers and re-running.

### Q4 — Are we still confident the fix at extract.rs:2813 should ship?

It's already shipped (commit `cc19d26`). The architectural rationale is correct (`parallel_classify_accumulate` over an unbounded `IdSetDense` is unsafe in principle even if this specific workload didn't trigger it as hard as predicted). PASS2 wall improved 23%. So there's some justification. But it does NOT solve the planet blocker, and the previous brief framed it as if it would.

**Should we leave the fix in place and continue investigating, or revert it?** Arguments for keeping: architectural correctness, ~23% wall improvement, doesn't make anything worse. Arguments for reverting: we shipped it under false pretenses (the planet blocker framing), and the wall improvement doesn't compound across the larger picture (total wall flat at 254s).

I lean keep, but want your read.

---

## What we plan to do once you respond

Depending on your answers:

- **If you find the source in BlobReader (Q1):** add narrower hotpath annotations or fix the allocation pattern directly. Re-bench, confirm collapse, update notes, commit.
- **If you can't find it from the code (Q1) but suggest a diagnostic (Q3):** run that diagnostic. We have `--alloc` working on Japan; Europe `--alloc` is blocked by mem preflight (141 GB estimated). We could `--no-mem-check` Europe and accept OOM risk, but Japan-scale + narrower instrumentation might be enough.
- **If you say the puzzle (Q2) implies the transient ISN'T in `build_classify_schedule`:** pursue whatever the alternative is.
- **If you say revert the fix (Q4):** we revert and put the planet investigation back to step 0.

---

## Files for context

- `notes/parallel-classify-regression-2026-04-10.md` — original brief (with post-review correction note)
- `notes/parallel-classify-regression-2026-04-10-reviews.md` — your previous responses, verbatim
- `notes/columnar-integration.md` — original design review and "Resolved" (now-questionable) section
- `src/commands/mod.rs:426-456` — `build_classify_schedule`
- `src/commands/extract.rs:2773-2891` — `extract_smart` with new sub-phase markers
- `src/commands/extract.rs:2445-2647` — `collect_pass1_generic` (the puzzle: same scan code, different memory behavior)
- `src/read/blob.rs:653, 983, 1015, 1069` — BlobReader functions on the schedule-build path (we have NOT read these yet)
- `.brokkr/results.db` UUIDs:
  - `8ac56b15` — Europe smart, HEAD `51f820d`, with new sub-phase markers (the decisive measurement)
  - `4eccb6f3` — Japan smart, `--alloc`, HEAD `51f820d` (per-function allocation distribution)
  - `0c60ec88` — Europe smart, post-fix `cc19d26`, `--bench 3` (variance check showing 10 GB is real)
  - `01de22bb` — Europe smart, pre-fix `5ca2df9`, baseline
  - `f420c5fd` — Europe smart, `fc17b51`, original pre-refactor baseline
