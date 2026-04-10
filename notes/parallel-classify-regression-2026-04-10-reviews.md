# Review responses — parallel_classify_phase regression brief

**Source:** `review planet,perf,arch` invocation on 2026-04-10, pointer to `notes/parallel-classify-regression-2026-04-10.md`.
**Reviewers responding:** planet/claude, planet/codex, perf/codex, arch/codex (4 of 6 — perf/claude and arch/claude exited with error, no detail).

This file is a verbatim capture of the reviewer output. The brief itself is at `notes/parallel-classify-regression-2026-04-10.md`.

---

## === planet ===

### --- claude ---

Now I have the complete picture. Let me write the report.

---

## Report: parallel_classify_phase regression brief

I've read the brief, the two suspect call sites (`extract.rs:2813`, `tags_filter.rs:1000`), both parallel_classify helpers in `commands/mod.rs`, and the tags-filter PASS1 path for contrast. Here are my findings on the three questions.

### Q1: Is `extract.rs:2813` the cause?

**Very likely yes, but not for the chunk-spread reason the design review stated.** The real cause is the interaction between the two passes, and the mechanism is not what the 9 GB chunk-spread model predicts.

The structural facts:
- `extract.rs:2813` (smart PASS2) and `tags_filter.rs:1000` (WAYDEPS) are **byte-identical in shape**: same `parallel_classify_accumulate(... IdSetDense::new ... node_ids.set(r) ... merge)` pattern on way blobs.
- Both scan a filtered subset of ways (tags-filter: `included_way_ids.get(wid)`; extract: `extra_way_ids_ref.get(wid) && !matched_way_ids_ref.get(wid)`).
- Tags-filter's peak is flat at ~2 GB; extract-smart's PASS2 peak is 10.72 GB. Same helper, same pattern, 5× different peak.

The hypothesis (a) from the brief — "smart PASS2 ways scan the FULL way blob set with very poor locality" — is **incorrect**. Both paths process the **same schedule** (all way blobs from `build_classify_schedule(ElemKind::Way)`). The per-worker `IdSetDense` sees every way blob in both cases. The filter `extra_way_ids_ref.get(wid)` vs `included_way_ids.get(wid)` only determines which ways contribute refs, not which blobs are visited.

The difference must be in **how many ways pass the filter**, which determines how many `node_ids.set(r)` calls happen per worker, which determines the chunk touch pattern.

For tags-filter `highway=primary` on Europe: `included_way_ids` is small (~500K ways, narrow ID range for a tag-selective filter). Way refs from those 500K ways have limited chunk spread — maybe a few hundred chunks, ~100-500 MB per worker IdSetDense.

For extract-smart on a regional bbox: `extra_way_ids` is **relation-driven**. Smart PASS1 walks matched relations and collects their member way IDs. For a region with many multipolygons (coastlines, administrative boundaries, forests, lakes), `extra_way_ids` can include ways from across the entire continent — these are ways referenced by relations whose OTHER members fall in the bbox. The extra-way set can be millions of ways spanning the full node ID space.

At Europe scale: extra ways from relation members × ~8 refs/way × 6 workers, each populating a worker-local IdSetDense with chunks scattered across all ~500 node ID chunks = 6 × ~1.5 GB = ~9 GB. This matches the measured +6.6 GB PASS2 delta exactly.

**But tags-filter's way ref set is also scattered, and it's not showing 9 GB.** The brief's own measurement at line 131 shows `collect_relation_member_closure` is a NEW phase (not present pre-refactor), indicating the tags-filter path changed. Before the refactor, tags-filter had a single PASS1 doing match+closure+way-deps together. Now it has PASS1 → CLOSURE → WAYDEPS as separate phases.

Look at the pre-refactor tags-filter PASS1 peak: 1.87 GB anon. HEAD PASS1 peak: **66 MB**. The 1.87 GB moved somewhere. It's now sitting in the persistent `matched_node_ids`, `included_way_ids`, `way_dep_node_ids` etc. during CLOSURE and WAYDEPS phases (1.86-1.89 GB persistent). The per-worker contribution from WAYDEPS is tiny on top of this because **tags-filter's WAYDEPS only scans ways in `included_way_ids`** — a small selective set for `highway=primary`. The refs it collects span a small chunk range. Per-worker IdSetDense is maybe ~50-200 MB, lost under the 1.86 GB persistent noise.

**Extract-smart's PASS2 scans `extra_way_ids` which is MUCH larger than tags-filter's `included_way_ids` for a relation-heavy region.** The 10.7 GB peak reflects 6 workers each holding an IdSetDense populated from potentially millions of unique node IDs across the full Europe ID range.

**How to confirm before changing code:** instrument `extract.rs:2821` to count `(extra_way_ids_ref.get(wid) && !matched_way_ids_ref.get(wid))` hits per worker. If it's in the millions, the chunk-spread theory is confirmed for extract-smart specifically. A simpler check: add `eprintln!("extra_way_ids.len()={}", extra_way_ids.chunk_count())` just before the parallel_classify_accumulate call at 2813. If it's >100K, the theory holds.

### Q2: Why is the chunk-spread model wrong for tags-filter?

**The model is right about what CAN happen; it's wrong about what tags-filter's workload actually does.**

The chunk-spread prediction (9 GB per-worker IdSetDense) assumes each worker touches the full ID range. In tags-filter's WAYDEPS:

1. `included_way_ids` is a tag-selective subset (e.g., `highway=primary` = ~500K ways at Europe vs ~454M total ways). Selectivity: ~0.1%.
2. Way refs from matched ways span a narrow chunk range because tag-based selectivity correlates with geography (primary highways cluster along road networks, not uniformly distributed).
3. With 256-bucket work-stealing distribution, each worker sees a random sample of way blobs. Each way blob contributes refs to maybe 5-20 chunks. After processing ~8K way blobs / 6 workers = ~1.3K blobs/worker, each worker's IdSetDense has maybe 50-200 chunks allocated.
4. 200 chunks × 4 MB = 800 MB per worker × 6 = 4.8 GB theoretical, but because the filter is selective and many blobs contribute **zero** matching ways (no included way_ids in them), the actual touched chunk count is smaller.
5. The CLOSURE/WAYDEPS phases happen sequentially, so the peak is dominated by the already-resident persistent state (~1.86 GB) plus maybe 200 MB of per-worker scratch. Total ~2 GB, matching the measurement.

**For extract-smart, `extra_way_ids` is relation-driven, not tag-driven.** Relations in OSM are geographically huge (coastlines span continents, admin boundaries trace country perimeters). Extra ways pulled from a regional bbox can span the full continent's ID range. Every way blob in the entire Europe PBF likely contributes matching refs. Each worker's IdSetDense fills out a significant fraction of the ~500 chunk range. 6 workers × ~1.5 GB = 9 GB, matching the measurement.

**So the tags-filter and extract-smart call sites are only structurally identical at the code level, not at the workload level.** The chunk-spread model is correct for the worst case (relation-driven wide-ID extra_way_ids) and pessimistic for the selective case (tag-driven narrow-ID included_way_ids).

**The recommendation is:** convert `extract.rs:2813` to `parallel_classify_phase<(), Vec<i64>>` per-blob send. Leave `tags_filter.rs:1000` on accumulate. Update `notes/columnar-integration.md` to note that per-worker IdSetDense accumulate is safe ONLY when the filter keeps `included_way_ids` tag-bounded, not relation-bounded.

But I would also add a defensive guard: if `extra_way_ids.chunk_count()` (or similar heuristic for "how spread-out is this set") exceeds a threshold, fall back to per-blob send. This future-proofs against new callers hitting the same trap.

### Q3: The shared +22-24% wall regression

This is the harder question and the evidence points away from memory. Tags-filter has flat memory and still regressed 24%. So there's a shared, non-memory cause.

**Candidates from the brief, ranked by likelihood:**

1. **Reduced effective parallelism from per-worker accumulate's merge barrier** (the brief's candidate #3). This is the most likely cause.

Compare the two helpers:
- `parallel_classify_phase`: workers send `R` per blob. Merge runs on the main thread **concurrently** with ongoing worker processing. As soon as a worker produces a result, the main thread merges it into shared state. When the last worker finishes its last blob, only ONE `merge(R)` call is pending.
- `parallel_classify_accumulate`: workers send `S` **only at completion**. Merge runs on the main thread **sequentially** after ALL workers finish. The last worker to finish gates the merge.

This introduces a **critical-path serialization**: the slowest worker's completion time blocks the merge. With per-blob send, faster workers' early results are merged while slower workers catch up. With per-worker accumulate, the merge phase is purely sequential after parallel completion.

The merge cost itself is also different:
- Per-blob: `merge(R)` called ~8K times (once per blob), each cheap (~100 μs for adding Vec contents to IdSetDense)
- Per-worker: `merge(S)` called ~6 times, each processing ~1.3K blobs worth of data in ~200 ms — BUT these run sequentially, not overlapped with worker CPU time

At Europe scale with 30 seconds of worker time, the lost overlap is 100-500 ms per merge call × 6 workers = 1-3 seconds. That's not enough to explain 20-25 seconds of regression.

**But there's a second effect**: per-worker `IdSetDense::merge` (see `id_set_dense.rs`) is an OR of chunks between two IdSetDense instances. With ~1.5 GB per worker and 6 workers, the sequential merge phase is bitwise-ORing 9 GB of data through the main thread. At ~10 GB/s memory bandwidth, that's ~1 second of pure merge time. Not 20.

2. **Schedule receiver mutex contention** (brief's candidate #4). Each blob's descriptor pickup requires `desc_rx.lock()`. With per-worker accumulate, each worker does the **same number of pickups** as per-blob send (one per blob processed). So contention shouldn't be different. Unlikely cause.

3. **Worker init overhead from `IdSetDense::new()`** per worker. Each IdSetDense allocation is O(1) (just an empty chunks Vec). But as workers populate chunks, each new chunk is a 4 MB allocation. For 6 workers × 200+ chunks each = 1200+ allocations of 4 MB. That's ~5 GB of allocator throughput per call. glibc's large-allocation path (mmap for ≥128 KB) is slower than small-allocation fast path — each 4 MB allocation is an mmap syscall. 1200+ mmap/munmap pairs take ~100 μs each = ~120 ms of syscall time. Not 20 seconds.

4. **Page cache pressure from the persistent state during CLOSURE/WAYDEPS phases**. Tags-filter holds ~1.86 GB of IdSetDense across multiple phases. The per-worker accumulate adds another ~1 GB transient. Total working set ~3 GB, which fits comfortably in Europe's 16 GB available page cache. Not plausibly causing thrashing.

5. **Hot-path allocator churn from the columnar decode work** that shipped with the refactor (see `notes/columnar-integration.md`). Every blob now decodes to `DenseNodeColumns` scratch even in paths that don't need it. This affects both extract and tags-filter. **Worth checking.**

6. **Measurement noise** (brief's candidate #6). A 24% regression on a single `--bench 1` run is not rigorous. The pre-refactor and post-refactor measurements are from different UUIDs on different days. If plantasjen had 5% variance from day to day and the real regression is 10-15%, it might be entirely explained by a different root cause.

**What I'd do first: run `--bench 3` on both HEAD and `fc17b51`/`75ad21d` under the same conditions, same day.** This eliminates day-to-day variance. If the 24% holds at --bench 3, it's real. If it drops to 10-15%, the cause is narrower than the brief assumes.

**Then, if it's real**, profile with hotpath:

```
brokkr hotpath extract-smart --dataset europe
```

and compare the parallel_classify callees between HEAD and fc17b51. Look for time attributed to `IdSetDense::merge`, `chunks.resize_with`, allocator calls, or columnar decode. The hotpath data will identify the +20-25s component.

**A narrower test**: on HEAD, temporarily convert `extract.rs:2813` to `parallel_classify_phase<(), Vec<i64>>` (the recommended fix) and re-benchmark. If extract-smart wall drops by ~20 seconds AND tags-filter stays at 130s, then the shared cause is not from `parallel_classify_accumulate` semantics — it's from something else in the refactor affecting tags-filter independently. If both drop proportionally, the shared cause IS in the accumulate path.

**I suspect the answer is: extract-smart wall drops to ~215s (memory fix + overlap restoration), tags-filter wall stays at 125-130s.** The tags-filter regression is likely from the phase split (PASS1 → CLOSURE → WAYDEPS) adding schedule-build overhead and extra blob reads. The WAYDEPS phase re-reads way blobs that PASS1 already read. Pre-refactor did this in one pass; HEAD does it in two. At ~20K way blobs × ~200 ms re-read cost = 4 seconds, plus CLOSURE's additional scan = another few seconds. Call it ~8-10 seconds of extra wall from the phase split alone.

The remaining ~15 seconds of tags-filter regression could be noise, columnar scratch overhead, or the CLOSURE/WAYDEPS merge-path differences. Need profiling to nail it down.

### Overall recommendation

**Act on Q1 first.** Convert `extract.rs:2813` to `parallel_classify_phase<(), Vec<i64>>` per-blob send — it's self-contained, ~50 LoC, and the 10.72 GB peak is a confirmed planet blocker regardless of Q3's resolution. This is the fix the design review already recommended; the regression confirms the recommendation was correct.

**Don't act on Q2.** Leave `tags_filter.rs:1000` on accumulate. It's working. Add a comment documenting why (selective included_way_ids, not relation-driven).

**Investigate Q3 before acting.** The tags-filter wall regression with flat memory is diagnostically important. It tells you something changed that affects both paths independent of the memory issue. Profiling with `--bench 3` and hotpath is the cheap next step. Don't refactor anything else until Q3 has a root cause. Premature fixes here risk chasing noise.

**Update `notes/columnar-integration.md`** with the measured chunk-spread reality: per-worker IdSetDense is safe when the filter produces a **narrow, geographically-clustered** way ID set (tag-selective filters like tags-filter). It's unsafe when the filter produces a **wide, relation-driven** way ID set (smart-extract relation member expansion). The distinction isn't about the helper's mechanics — it's about the workload's chunk-touch distribution.

### --- codex ---

**Findings**

1. The note's strongest claim is well-supported: `extract.rs:2813` is the most credible cause of the extract-smart memory blowup. That call site uses `parallel_classify_accumulate` with `S = IdSetDense`, and it is the only smart-extract phase still accumulating an unbounded node-ID set per worker. The measured jump is concentrated exactly in `EXTRACT_PASS2`, and the code shape matches the disputed case from `notes/columnar-integration.md`.

2. The tags-filter result does **not** invalidate the extract diagnosis. `tags_filter.rs:1000` is structurally similar, but not workload-equivalent. It only accumulates refs for ways already in `included_way_ids`, while smart extract pass 2 scans `extra_way_ids` induced by relation expansion. Those sets can have very different density and ID-range spread. `IdSetDense` memory is driven by touched chunk spread, not by output count, so a selective tags-filter workload can stay cheap while smart extract still explodes.

3. The note overstates the evidence for a single shared `+22-24%` wall regression cause. The numbers show two regressions, but not one proven shared mechanism.
- Extract: regression is concentrated in pass 2 and pass 3, with pass 2 showing the large memory spike.
- Tags-filter: pass 1 regressed heavily even though it already uses `parallel_classify_phase`, not accumulate.
That means "accumulate itself caused both regressions" is not yet demonstrated. There may be a shared cause, but the note does not prove one.

4. The current `parallel_classify_accumulate` implementation really does hold each worker's full `S` until worker completion; there is no hidden incremental merge. In `mod.rs`, each worker sends exactly one `S` at the end. So if a path's per-worker `IdSetDense` truly grows wide, the memory model is exactly as bad as the design review warned. The low tags-filter peak therefore points to narrower effective chunk spread there, not to a surprise implementation detail.

5. The note's own data already argues for a surgical fix first. The extract-smart regression is sharp, localized, and planet-blocking. The tags-filter path is slower but still memory-flat at Europe scale. Those are different severities. There is enough evidence to flip `extract.rs:2813` back to per-blob send immediately, even if you leave `tags_filter.rs:1000` alone for now.

**Assessment**

**Q1: Is `extract.rs:2813` the actual cause?**
Probably yes, with high confidence.

Why:
- The regression lands exactly in `EXTRACT_PASS2`.
- `extract.rs:2813` is the only smart-extract pass using worker-accumulated `IdSetDense` over way refs.
- That state type is the known danger case from the prior design review.
- The magnitude fits the model: extra ~6.6 GB over baseline is entirely plausible for several worker-local wide-spread `IdSetDense`s plus the merged destination.

What I would check first if you want hard confirmation before changing code:
- Add a one-line sidecar marker around just the `parallel_classify_accumulate` call in smart pass 2 if not already isolated tightly enough.
- Add temporary worker-side instrumentation counting `node_ids.chunks.iter().filter(|c| c.is_some()).count()` at thread end.
If you see each worker touching hundreds of chunks, the case is closed without needing a flamegraph.

I would **not** start with perf/flamegraph. This is memory-shape debugging, not CPU attribution.

**Q2: Why is tags-filter flat?**
Because the chunk-spread model was a worst-case model, and this workload is not hitting it.

The most likely explanation is selectivity:
- `collect_way_node_dependencies` only includes refs for ways already selected by prior filtering.
- Those ways are likely much sparser and more clustered than the smart-extract extra-way set.
- `IdSetDense` allocates per touched chunk. If each worker only touches a relatively narrow subset of chunks, worker-local memory stays modest.

The note's "they are structurally identical" framing is too strong. They are only identical at the API level. They are not equivalent in:
- match density
- way-ID coverage
- implied node-ID spread

So my conclusion is:
- tags-filter's low memory is probably real
- it does **not** exonerate smart extract
- it does mean the original 9 GB model was too pessimistic for selective workloads

**Q3: What explains the wall regressions?**
The note does not isolate this yet.

Most plausible explanations, in order:
1. Different changes are driving the two regressions.
2. Lost producer/consumer overlap in accumulate paths is part of it.
3. Single-run noise is amplifying the apparent similarity.

Important code-level point:
- `parallel_classify_phase` merges results continuously as workers produce them.
- `parallel_classify_accumulate` does all merging only after each worker completes.

That can absolutely hurt wall time even when memory is fine, because it removes overlap between worker decode/classify and consumer merge. But it cannot explain tags-filter pass 1, which already uses `_phase`. So it is not a full explanation for the whole `+24%`.

Fastest way to isolate:
1. Re-run with `--bench 3` first. `24%` is probably real, but one sample is too weak for root-cause work.
2. A/B the two suspect call sites independently on current HEAD:
- flip only `extract.rs:2813`
- flip only `tags_filter.rs:1000`
3. Compare phase-level wall and anon with sidecar.
That will answer more than a flamegraph right now.

**Recommended plan**

1. Treat the extract-smart issue as confirmed enough to fix now.
- Convert `extract.rs:2813` to `parallel_classify_phase` with per-blob `Vec<i64>`.
- This is low-risk and directly addresses the planet blocker.

2. Do **not** change `tags_filter.rs:1000` yet.
- Its memory behavior at Europe looks acceptable.
- The evidence for changing it is much weaker than for extract.

3. Investigate the wall regression separately, not as a blocker for the extract fix.
- First reduce noise with `--bench 3`.
- Then A/B the two call sites on current HEAD.
- Only profile deeper if the regression survives that simpler isolation.

**Bottom line**

The note correctly identifies one urgent issue and one open issue:
- Urgent and actionable: smart extract pass 2 accumulation is almost certainly the memory regression and should be reverted to per-blob send.
- Open and not yet isolated: the cross-command wall regression is real-looking but not yet attributable to a single shared cause.

That is enough to move forward surgically without waiting for a perfect theory.

---

## === perf ===

### --- claude ---

`error: claude exited with error:` (no further detail)

### --- codex ---

**Findings**

- The note presents two separate regressions, not one:
  - `extract --strategy smart` has a real memory regression and a wall-time regression.
  - `tags-filter --two-pass` has a wall-time regression without a corresponding memory regression.
  That strongly argues against a single "more memory => slower" explanation.

- The strongest candidate for the `extract-smart` memory blowup is still `extract.rs:2813`, exactly as the note says. The measured jump is localized to PASS2, and that call site is the one disputed unbounded `IdSetDense` accumulate site. Nothing in the note points to another equally plausible memory source.

- The note also makes a good case that the original chunk-spread model was too coarse. `tags_filter.rs:1000` and `extract.rs:2813` are superficially similar, but the workloads are not:
  - `extract` smart PASS2 can touch a much broader set of ways and referenced nodes.
  - `tags-filter` way-deps are gated by an already-selected subset of ways.
  So "same accumulator type" does not imply "same chunk spread" in practice.

- The wall-time regression is likely caused by the execution model change itself, not by `IdSetDense` memory.
  The most plausible shared causes from the note are:
  - loss of overlap between worker production and consumer merge
  - more end-of-phase reduction / less streaming behavior
  - extra allocator / merge work in accumulate mode
  The note's own data rules out page-fault-driven slowdown as the whole story, because tags-filter regressed with flat memory and no fault spike.

- The current API split in `mod.rs` is the right architecture:
  - `parallel_classify_phase<S, R>` for dense/hot paths
  - `parallel_classify_accumulate<S>` only for truly bounded sparse paths
  The problem is call-site selection, not the API shape.

**My read on the three questions**

1. `extract.rs:2813` is very likely the cause of the smart-extract memory regression.
Not proven with absolute certainty, but it is the highest-confidence explanation by far. The regression is phase-localized, the site matches the disputed pattern, and the magnitude matches "worker-local wide-spread `IdSetDense`" much better than anything else in the note.

2. The chunk-spread model is not "wrong," but it is worst-case.
It appears too pessimistic for selective workloads like `tags-filter` way deps. It may still be accurate enough for smart extract PASS2. So I would not generalize from tags-filter to conclude the model failed entirely; I would conclude workload selectivity matters much more than the original review allowed for.

3. The shared wall regression is probably from accumulate-mode structure, not from memory.
My best hypothesis is:
- per-worker accumulation removes streaming overlap
- the final merge/reduction becomes more serialized
- maybe small allocator overhead contributes
I would not chase mutex contention first; the note itself already makes that unlikely.

**What I would do next**

1. Change only `extract.rs:2813` back to per-blob send first.
That is the cleanest, lowest-risk fix for the confirmed planet blocker.

2. Leave `tags_filter.rs:1000` alone for now.
Its memory profile looks acceptable in the data you have. Do not "fix" it speculatively.

3. Re-measure extract-smart after that one surgical change.
That should answer both:
- whether the 10.7 GB peak was really that call site
- how much of the wall regression was tied to it

4. Only after that, decide whether the remaining +24% wall regression on tags-filter needs a separate investigation.
If yes, profile accumulate vs per-blob send at the helper level rather than reasoning from sidecar alone.

**Bottom line**

The note supports a narrow conclusion, not a broad rollback:

- revert `extract.rs:2813`
- keep `tags_filter.rs:1000` for now
- treat the wall regression as a second issue, likely caused by accumulate-mode scheduling/overlap rather than memory

That is the highest-confidence next move.

---

## === arch ===

### --- claude ---

`error: claude exited with error:` (no further detail)

### --- codex ---

**Findings**

1. The brief is directionally right about the memory bug candidate in smart extract: `extract.rs:2813` is the only current call site in the reviewed set that is both:
- using `parallel_classify_accumulate()`, and
- accumulating an unbounded `IdSetDense` over node refs from way scans.

That makes it the strongest explanation for the extract-smart PASS2 anon blowup.

2. The note overstates the structural similarity between smart extract PASS2 and tags-filter way deps. `tags_filter.rs:1000` does have the same accumulator type, but the selection predicate is very different: it only sets refs for ways already present in `included_way_ids` and optionally excludes `skip_way_ids` at `tags_filter.rs:1006`. Smart extract PASS2 at `extract.rs:2817` scans all way blobs and collects refs for `extra_way_ids \ matched_way_ids`, which can be much less selective and much more geographically dispersed.

3. The wall-time section of the brief is not yet evidence-tight. The strongest red flag is internal inconsistency: the note attributes the tags-filter PASS1 memory change to accumulate, but current PASS1 is still using `parallel_classify_phase()`, not accumulate. So the shared `+22–24%` wall regression cannot be explained purely by the accumulate change from the information in this note alone.

4. The helper split in `mod.rs` is architecturally sound. The problem is call-site selection, not the abstractions:
- `parallel_classify_phase()`: per-blob result, bounded live memory, merge overlaps with ongoing worker decode.
- `parallel_classify_accumulate()`: per-worker result, bounded only if `S` is truly sparse in address-space spread, and merge happens only after workers finish.

5. The note is correct that `IdSetDense` memory depends on chunk spread, not count. That is explicit in `id_set_dense.rs`: `set()` allocates 4 MB chunks on demand based on chunk id, so a relatively small but widely scattered node-ref set can still be expensive.

**Answers**

**Q1. Is `extract.rs:2813` the likely cause of the smart-extract memory regression?**

Yes, it is the most likely cause.

Why:
- It is exactly the unsafe shape the helper doc warns against at `mod.rs:556-559`.
- It accumulates worker-local `IdSetDense` over node refs, which uses chunk-addressed allocation at `id_set_dense.rs:42-48`.
- The measured regression is concentrated in smart PASS2, which is exactly where that call site lives at `extract.rs:2801-2833`.

I would treat this as the default hypothesis unless a targeted A/B disproves it.

What I would look at first:
- Do the smallest isolating change: flip only `extract.rs:2813` from `parallel_classify_accumulate` to `parallel_classify_phase`.
- Re-run the same Europe smart benchmark once.
- If PASS2 anon collapses, the causal question is answered more directly than with profiler archaeology.

I would not start with flamegraphs. This is primarily a memory-topology question, and the code already points to the likely culprit.

**Q2. Why does tags-filter not show the same blowup?**

Because the workloads are not equivalent enough for the chunk-spread model to transfer directly.

Most plausible explanation:
- `tags-filter` way deps only touches ways in `included_way_ids` at `tags_filter.rs:1006-1015`, which is likely a much smaller and more clustered subset.
- smart extract PASS2 touches `extra_way_ids` at `extract.rs:2821`, which can be large and globally dispersed because it comes from smart relation expansion.

That means:
- tags-filter may allocate far fewer `IdSetDense` chunks per worker,
- while smart PASS2 may approach worst-case spread.

Two secondary effects probably also help tags-filter:
- its persistent `IdSetDense` state already dominates the RSS curve, masking some worker-local growth,
- and `parallel_classify_accumulate()` merges worker states as workers finish at `mod.rs:631-632`, so peak simultaneous post-worker copies is lower than a naive `N × full-size` model.

My conclusion:
- the tags-filter measurement does not invalidate the extract-smart diagnosis,
- it just shows the original "6 workers × full planet spread each" model was too pessimistic as a universal model.

**Q3. What explains the shared wall regression?**

The note does not yet isolate this. There are two viable causes, and the current evidence mixes them.

Most plausible helper-level cause:
- `parallel_classify_phase()` streams results at `mod.rs:544-546`, so merge work overlaps with ongoing worker decode/classify.
- `parallel_classify_accumulate()` delays all merge work until worker completion at `mod.rs:622-624`, which removes that overlap.

That can hurt wall time even when RSS stays flat.

But there is a major confounder:
- the note's own tags-filter PASS1 regression cannot be blamed on accumulate, because PASS1 currently uses `parallel_classify_phase` at `tags_filter.rs:596`.

So the current `+22–24%` wall story is not clean enough to assign one shared cause. It may be:
- lost overlap in the accumulate paths,
- plus unrelated changes between the older baseline commits and current HEAD,
- plus normal single-run noise.

The fastest way to isolate it is not broader profiling. It is:
1. Hold HEAD fixed.
2. Toggle one call site at a time.
3. Re-measure.

**What I would do next**

1. Change only `extract.rs:2813` to `parallel_classify_phase`.
2. Re-run Europe smart once.
3. If PASS2 anon drops sharply, keep that fix.
4. Leave `tags_filter.rs:1000` alone for now unless you see a planet-scale RSS problem there.
5. Do not treat the current wall regression note as resolved. It needs a same-commit A/B, because the cross-commit comparison is too confounded.

**Bottom line**

The note is strongest on one point: `extract.rs:2813` is the right first thing to fix. It is weaker on the "shared wall regression" thesis, and it slightly overstates the equivalence between smart extract and tags-filter. The safest interpretation is:

- smart extract PASS2 is very likely a real `parallel_classify_accumulate(IdSetDense)` misuse,
- tags-filter is not sufficient evidence that the model is wrong,
- and the wall regression needs a cleaner experiment before drawing architecture conclusions.
