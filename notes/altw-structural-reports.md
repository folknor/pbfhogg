# ALTW External-Join: Structural Opportunities

Synthesis of three independent reviews of the ALTW (`add-locations-to-ways --index-type external`) pipeline. All three reviewers land on the same framing: ALTW today behaves like a reorder pipeline, not a saturated engine. It pays real wall time to destroy blob ownership, externally permute coordinates through rank-sharded and slot-bucketed intermediates, then reconstruct blob order. The disciplined four-stage structure survives because each handoff is a filesystem round-trip; the cost shows up as long idle moments at stage boundaries.

Convergence: two reviewers rank **epoch-spill promotion** as the #1 lever; the **stage 3 → stage 4 seam** and the **stage 1 decompress duplication** each appear in two reports. This document consolidates all six distinct opportunities and everything the three reviewers flagged as *not* worth pursuing.

---

## Context: already shipped on current `main`

Do not re-propose these — they are in tree and are reflected in the baseline measured below:

- `coords_by_rank` removal: stage 2 decodes node blobs directly via `NodeBlobInfo`
- Stage-3 direct scatter from raw `ResolvedEntry` bytes (no `Vec<ResolvedEntry>` materialization)
- Parallel finalize tail in [coord_payloads.rs](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs) — per-blob pread+pwrite work-stealing across `available_parallelism` threads
- Stage-4 per-way refcount sidecar consumption in the way reframe path
- Stage-4 raw passthrough for relation blobs (always) and node blobs when `keep_untagged_nodes` is set
- `PerWayRcs` lazy per-blob decode via blob-offset sidecar
- Slot-bucket `ResolvedEntry` record shrunk 16 → 12 bytes (`fcd4fa2`) — −25% stage 2+3 scratch
- Shared header-scan sidecar replacing three separate header-only passes (`f864b64f`) — saved ~56 s Europe wall

---

## How the pipeline works today

A four-stage serial chain with three disk-materialized intermediates and no stage overlap. The serialized seam spans [mod.rs:340](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:340)–[mod.rs:425](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:425); the `slot_bucket_count` and 2-piece straddler machinery lives at [mod.rs:238](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:238).

**Stage 1 — way scan (two sub-passes).** [stage1.rs:340](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:340)
- 1A: decompress every way blob → build `IdSetDense`, write ref-count sidecars
- 1B: re-decompress the same way blobs → emit rank-bucketed `(local_rank, slot_pos)` records ([stage1.rs:327](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:327), [:421](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:421))
- 1B cannot start until `IdSetDense::build_rank_index()` at [stage1.rs:247](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:247) completes
- Blob ownership is discarded in [stage1.rs:451](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:451)
- → **~80 GB of rank shard files** (256 × W per-worker files)

**Stage 2 — node join.** [stage2.rs:365](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:365)
- Read rank shards → counting-sort per rank bucket → `pread + decompress` node blobs → populate `coord_slice` ([stage2.rs:382](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:382)) → resolve `(slot_pos, lat, lon)` → write to shared slot buckets via per-bucket `Mutex<BufWriter>`
- → **~112 GB of slot bucket files** (R3 on-disk accounting; R2 gives ~200 GB of raw `ResolvedEntry` records across 256 files, and ~150 GB for the current spill volume)

**Stage 3 — slot reorder.** [stage3.rs:234](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:234)
- Read slot buckets → scatter into a dense bucket-local buffer → classify blob/bucket intersections plus straddlers ([stage3.rs:292](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:292), [:386](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:386)) → delta-varint encode per-blob `coord_payloads` → finalize/copy pass in [coord_payloads.rs:255](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:255)
- → **~55 GB `coord_payloads` file**

**Stage 4 — assembly.** [stage4.rs:376](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:376)
- Open a second reader, pread each payload from `coord_payloads` again
- Re-read the full input PBF → decompress way blobs → wire-format reframe using payloads → passthrough node/relation blobs → write enriched PBF
- Also **re-decodes the kept node blobs** on the non-way path at [stage4.rs:439](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:439) (decoded already in stage 2)

**Planet-scale totals.**
- Scratch: ~80 + ~112 + ~55 = **~247 GB written, ~247 GB read back** (R3 accounting)
- Input PBF read ~3×: ways twice (1A, 1B), nodes in stage 2, everything in stage 4
- Fully serialized — the machine idles at every stage boundary while setup/teardown runs

**Measured baselines on current `main`** (UUIDs stored in `.brokkr/results.db`):

| Dataset | UUID | Wall | Stage 1 | Stage 2 | Stage 3 | Finalize | Stage 4 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Europe | `ffdf5f69` | 375.9 s | 71.0 s | 97.0 s | 37.2 s | 17.8 s | 121.1 s |
| Planet | `4f059b67` | 867.7 s | 148.5 s | 266.6 s | 100.2 s | 46.4 s | 231.6 s |

Reviewer estimates occasionally quote ~68 s finalize and ~259 s stage 4 at planet — a different run or reviewer-level approximation. Treat the table as ground truth. Europe is stage-4-led; planet is stage-2-led with stage 4 second.

---

## Correctness invariants

Any rewrite preserves these or explicitly replaces them:

- **Sorted + indexed PBF precondition.** `external_join` requires `Sort.Type_then_ID` headers and indexdata. Enforced at entry; do not relax.
- **2-piece straddler invariant.** A blob's slot range spans at most two adjacent slot buckets. `slot_bucket_count` is chosen so every bucket width ≥ `max_blob_slots`. Constrains #5 (blob-group rewrite) and any layout change to slot buckets.
- **Zero-coord sentinel.** Stage 2's `coord_slice` uses `(lat==0, lon==0)` as the unresolved sentinel; the slice is fully zeroed at the start of each rank bucket. Any redesign that removes zeroing (e.g. #4's per-blob accumulators skipping empty slots) must replace the sentinel with an explicit presence signal.
- **Per-way refcount ordering.** The stage-1 per-way refcount sidecar is written in PBF blob order and consumed in PBF blob order by stage-4 reframe. Any stage-1 reshape preserves this ordering.
- **Straddler state machine.** Stage 3's merge is an exhaustive `None → Left|Right → Both`; duplicate or third halves error. Do not weaken to `Option<(Vec<u8>, Vec<u8>)>`. Affects #2 (the streaming coordinator must maintain this).
- **`build_rank_index()` before any `rank_if_set` / `rank`.** `IdSetDense` requires the rank index built after all `set_atomic` calls. Affects #3 — the scratch-spool variant must finish populating `IdSetDense` during pass A before `build_rank_index()`, and pass B's rank lookups must see the completed index.

---

## Ranked opportunities

### #1 — Promote epoch-spill to default; delete the disk slot-bucket path

**Convergence: R2 #1, R3 #1.** The code already exists in [src/commands/altw/stage23_epoch.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage23_epoch.rs) as an env-var-gated prototype. This is delete + promote, not greenfield.

**Bottleneck.** The stage 2 → stage 3 handoff materializes the largest intermediate in the pipeline — ~112 GB of slot bucket files, which stage 3 then reads cold, scatters, and encodes.

**Why the structure causes it.** Stage 2 produces resolved entries in rank-bucket (node-ID) order; stage 3 needs them in slot-pos (way-PBF) order. The slot bucket files are the external radix step that bridges those orderings. Stages 2 and 3 are separate `thread::scope` blocks connected only by the filesystem, so **100% of entries transit disk** — even entries that could be processed immediately in memory.

**Redesign.** The epoch-spill path already fuses stages 2+3. Epoch 0 resolves entries and scatters them directly into in-memory `Mutex<Box<[u8]>>` `scatter_buf`s (zero disk). Entries for epochs > 0 spill to disk and drain on later passes. After each epoch's producer pass, an emit pass encodes `coord_payloads` from the in-memory buffers while they are still L3-hot. Stage 3 ceases to exist as a separate phase — finalize becomes the tail of the last epoch's emit.

Concrete changes:
- Remove `parse_epoch_env()` and the env-var gate
- Auto-tune `num_epochs = max(1, total_slots * 8 / target_memory)` where `target_memory` ≈ 40–50% of available RAM (`sysinfo` or `/proc/meminfo`); or hardcode `num_epochs = 4` as a simpler initial default
- Delete the disk-path `else` branch in [mod.rs](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs) along with `SlotBuckets`, `SharedSlotBuckets`, and `stage2_node_join` from `stage2.rs`. Keep `prepare_bucket` and `LoaderScratch` — they are shared with the epoch path

**Payoff.**
- **E=1** (datasets where `total_slots * 8` fits in RAM): the entire slot-bucket intermediate vanishes; zero slot-bucket disk I/O
- **E=2** for Europe (~40 GB scatter → ~20 GB in memory; fits 30 GB RAM with 5.9 GB peak anon from other state): the ~42.5s Europe stage-3 wall mostly disappears
- **E=4** for planet (~30 GB RAM): ~25% of entries never touch disk; spill is ~84 GB (R3) or ~112 GB (R2) vs current 150 GB — **net saving ~38 GB of disk I/O**, ≈19s at ~2 GB/s NVMe, plus eliminated stage-3 open/read/close overhead and eliminated finalize as a separate phase (absorbed into the final epoch's emit; current finalize ≈ 68s at planet)
- Epoch-0 scatter is per-bucket, which eliminates the most-contended cross-bucket slot-bucket mutexes
- **Estimated wall savings: 30–60s planet, 20–40s Europe**

**Risks.**
- Peak memory rises ~6.8 GB at E=4 for epoch-0 `scatter_buf`s (vs < 1 GB for the disk path). Acceptable on any machine that can run ALTW planet — which already needs ~8.7 GB for `IdSetDense` — but conservative auto-tuning matters
- The epoch path has had limited production testing; a full `brokkr verify` on Denmark and Europe is required
- Spill for epochs > 0 has worse spatial locality than the current slot-bucket layout, because entries arrive interleaved across epochs

**Conviction: high.** Delete + promote, not new architecture. **Scope: moderate.**

---

### #2 — Stream stage 3 → stage 4; eliminate the `coord_payloads` file

**Convergence: R1 #1, R2 #2.** The biggest remaining double-digit wall opportunity, but a real architectural rewrite of the stage 3/4 boundary. Lands cleaner after #1.

**Bottleneck.** Stage 3 finishes, then a finalize/copy pass runs in [coord_payloads.rs:255](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:255), then stage 4 opens a second reader and preads each payload again at [stage4.rs:376](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:376). Stages 2+3+finalize (~388s planet) and stage 4 (~259s planet) are fully sequential — yet stage 4 touches no shared state with stage 3 beyond the read-only `coord_payloads` file and the read-only input PBF. They are naturally independent once any given blob's payload is available.

**Why the structure causes it.** Stage 3 workers own temporary payload *fragments*, not blob-order emission — so ALTW must stop, reconstruct blob order, write another artifact, then stage 4 re-reads it. `CoordPayloadsReader::pread_blob_payload()` requires all blob offsets to be known upfront (from the file's header), which forces the full serialization.

**Redesign.** Replace the `coord_payloads` file with a streaming handoff:
1. Stage 3 workers emit ready `blob_idx → payload_bytes` items to a blob-order coordinator. The coordinator merges straddler halves, reorders by `blob_idx`, and either appends to a final blob-ordered payload stream or feeds stage 4 directly through a bounded queue. **No worker tmp manifests. No finalize copy. No second payload pread.**
2. Handoff buffer: shared `Vec<OnceCell<Vec<u8>>>` (or `Vec<Mutex<Option<Vec<u8>>>>`) indexed by `blob_idx`. Stage 3 deposits payloads as they become available. Stage 4's way-blob workers read the buffer and block (condvar or spin) if a payload is not yet ready. Node/relation blobs proceed immediately.
3. Key insight: payloads are produced in roughly increasing `blob_idx` order (because `way_slot_starts` is monotonic and slot buckets are processed in order), and stage 4 processes blobs in PBF order (also roughly blob-index order). Blocking on late straddlers should be rare.

**Payoff.**
- Eliminates the 55 GB write + 55 GB read of `coord_payloads` — **~110 GB of planet I/O removed**
- Full overlap upper bound: wall ≈ max(stage-3-side, stage-4) instead of sum. R2 gives a rough planet figure of `max(176, 259) ≈ 259s` vs sequential `176 + 259 = 435s`, i.e. ~176s saved at planet (~18% total)
- **Conservative estimates: 100–150s planet, 40–60s Europe**

**Risks.**
- Real backpressure and bounded reorder state required. If stage 4 or the writer is the true limiter, this just relocates idle time
- Memory pressure: 55 GB of `coord_payloads` cannot all live in RAM. The handoff buffer must be bounded — once stage 4 consumes a blob's payload, that memory is freed
- Straddler completion ordering: a straddler isn't ready until both halves arrive from two different slot buckets; if the second half is produced late, stage 4 blocks on that blob
- Thread contention: concurrent stage 3 workers + stage 4 workers + `PbfWriter`'s rayon compression threads all running together

**Writer-ceiling diagnostic.** A shelved probe on stage-4 wire-format DenseNodes filtering (`4910fd9`) delivered a real stage-4-local CPU win that did not reach wall: Europe `s4_nonway_assemble_ms` 78.5 s → 36.9 s (−53%), yet `EXTJOIN_STAGE4` went 122.7 s → 127.6 s because `s4_send_ms` cumulative grew 561 s → 672 s — freed decoder CPU refilled the writer queue. Under `zstd:1` the phase moved only −1.3% and total wall still regressed (5m40s → 5m48s). **Streaming 3 → 4 will hit the same ceiling on any workload where `PbfWriter` compression is the true limit.** The keep-decision gate for #2 therefore evaluates under both default `zlib:6` and `zstd:1` (or `compression:none` for the internal pipeline), because a stage-boundary win can be real and still invisible on wall under a writer-bound output mode.

**Composition note.** #2 is viable on the **current slot-bucket representation** as a legitimate smaller first cut before #5 (the full blob-group rewrite). After #1 lands, the finalize phase is already absorbed into per-epoch emits, which makes #2 cleaner because the boundary it attacks is already softer.

**Conviction: high on payoff, medium on ease. Scope: large.** Not a try-and-revert-in-a-day change — needs careful design, phased rollout, extensive benchmarking.

---

### #3 — Fuse stage 1A + 1B via a node-ID scratch spool

**Convergence: R2 #3, R3 #2.** Independent of #1 and #2; stacks cleanly. Subsumes R1's medium-value "single-ingest way-ref spool" note targeting [stage1.rs:327](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:327) and [:421](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:421).

**Bottleneck.** Stage 1 decompresses and scans every way blob twice — ~57K blobs, ~37 GB compressed at planet. Zlib decompression is pure CPU, accounting for roughly 50% of stage 1 wall time, and it executes twice.

**Why the structure causes it.** Pass B depends on the rank index, and `IdSetDense::build_rank_index()` at [stage1.rs:247](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:247) only runs after all pass-A workers join. The decompression itself is identical in both passes; only the callback differs (1A inserts into `IdSetDense`; 1B looks up ranks + emits records).

**Redesign.** During pass A, each worker already has `blob_node_ids`. Dump those to per-worker scratch files alongside the normal work. Once pass A joins and the rank index is built, pass B reads back the scratch files instead of re-decompressing the PBF. Two viable encodings:
- **Flat `i64` arrays** (R2): one `BufWriter` per worker; `pread` → directly usable, no zlib, no protobuf. Planet scratch ≈ 37.5 GB
- **Delta-varint per blob** (R3): node IDs within a way blob are correlated; planet scratch ≈ 15–20 GB; pass B does sequential read + simple varint decode

The cost model flips from `pread + zlib (CPU) + protobuf (branch-heavy)` to `pread → directly usable` (flat) or `pread → varint` (compact).

**Payoff.**
- Eliminates one full zlib decompression + protobuf parse of all way blobs — CPU-bound and not cacheable
- On planet (30 GB RAM, 87 GB input PBF), pass A evicts its own data from page cache, so pass B re-reads from NVMe. Flat/compact scratch is smaller and sequentially written → better cache residency
- **Estimates: 15–30s planet (R2); 20–30% of stage 1 wall (R3); Europe extrapolates to ~45s cumulative across workers, ~8s wall**

**Risks.**
- Adds ~20–37.5 GB to the scratch budget (marginal against the current ~247 GB). NVMe write overhead ≈ 18s at planet for the flat variant
- If pass B's PBF data is still in page cache (unlikely at planet scale, possible on smaller datasets), the net effect could be negative
- Correctness: node ID order in scratch must exactly match the scan order pass B expects for `slot_pos` computation. Bit-exact validation required
- Varint encode/decode adds CPU — but far less than zlib

**Conviction: high.** The duplication is measured, not speculative. **Scope: moderate** — localized to `stage1.rs`.

---

### #4 — Direct-to-`coord_payloads` via per-blob accumulators (skip `scatter_buf`)

**R3 #3.** Builds on #1 (the fused epoch path). A coherent rewrite of the fused resolve/encode inner loop.

**Bottleneck.** Even in the epoch-spill path, each epoch does: scatter resolved entries into a dense `scatter_buf` → classify blobs → slice per blob → delta-varint encode → write worker tmp → finalize copies to `coord_payloads`. `scatter_buf` touches every byte of the epoch's bucket range, including empty slots (zeroed = missing coord); encoding then re-reads the same bytes.

**Why the structure causes it.** `scatter_buf` provides O(1) random access by `slot_pos`, which `coord_payloads` encoding needs. But it is write-once-read-once with poor locality — stage 2 writes in rank-bucket order (scattered across the buffer) while encoding reads in blob order (sequential within a blob but different from the write order). The dense layout also pays memset cost for empty slots.

**Redesign.** Skip `scatter_buf`. Each resolved entry already knows its `slot_pos`; derive `blob_idx` via binary search in `way_slot_starts` (~16 comparisons for ~57K blobs) and local offset `= slot_pos - way_slot_starts[blob_idx]`. Route directly to per-blob accumulators:
- `Vec<(u16 local_offset, i32 lat, i32 lon)>` — 10 bytes per entry vs 8 bytes in `scatter_buf`, but **only non-zero entries**; no zero-fill for missing
- Planet: ~10B resolved entries × 10 bytes ≈ 100 GB total — same ballpark as `scatter_buf`, but no dense allocation, no memset, no second read pass
- Epoch 0: accumulators live in memory. Epochs > 0: spill to per-blob (or per-blob-group) files
- Encode reads each accumulator, sorts by local offset (trivial at ~175 entries/blob average — entries arrive out-of-order across rank buckets), delta-varint encodes in one shot

**Payoff.**
- Eliminates `scatter_buf` allocation and zero-fill (~6.8 GB memset at E=4)
- Eliminates the `scatter_buf` write → read round-trip (access patterns differ, cache is cold)
- Eliminates the `classify_blobs_in_bucket` + `emit_integrated_intersections` machinery — slot-bucket boundaries become irrelevant, blobs are the natural unit
- Per-blob accumulators are the right granularity for the final output format

**Risks.**
- Per-blob sort is trivial (~175 entries/blob average)
- Binary search per resolved entry — cheap vs the current dense random-scatter store, but still a per-entry CPU cost
- Significant rewrite of the stage-2 resolve loop and stage-3 replacement
- Cache-miss savings are real but quantification requires measurement

**Relationship to #5.** Both #4 and #5 re-key downstream around blobs. #4 stays inside the fused epoch path (resolve → per-blob accumulator → encode). #5 re-keys the entire pipeline including stage 1 emission.

**Conviction: medium-high. Scope: substantial rewrite of the fused path.**

---

### #5 — Blob-group downstream rewrite: re-key around way blobs, not global slot buckets

**R1 #2.** The structurally cleanest answer, at the cost of rewriting stages 1–4.

**Bottleneck.** Stage 1 emits global `slot_pos` records; stage 2 routes every resolved coordinate into shared global slot buckets; stage 3 rebuilds dense bucket-local slot images and then classifies blob/bucket intersections and straddlers ([stage3.rs:292](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:292), [:386](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:386)). The entire `slot_bucket_count` and 2-piece straddler apparatus at [mod.rs:238](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:238) exists only to survive this key choice.

**Why the structure causes it.** Blob ownership is thrown away at [stage1.rs:451](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:451) and only reconstructed downstream. Every subsequent stage rebuilds it, in a different ownership domain.

**Redesign.** Change the downstream key from `slot_pos` to `way_blob_idx + blob_local_slot` (or a blob-group-local equivalent). Partition contiguous way blobs into bounded blob groups. Stage 2 emits resolved records to blob-group files. Stage 3 scatters and encodes directly within those blob-aligned groups. This deletes blob/bucket classification, straddler staging, and most of finalize **by construction**.

**Payoff.** The cleanest way to stop rebuilding the same coordinate stream in three ownership domains. Also makes #2 (streaming 3→4) much cleaner — payloads are produced already in blob-aligned order, and straddlers vanish.

**Risks.** Real rewrite of stages 1–4. The fundamental rank-order vs blob-order mismatch does not go away; a bad blob-group design can preserve most of the scatter cost while adding new bookkeeping.

**Conviction: medium** (high structural payoff, high implementation risk). **Scope: very large.**

---

### #6 — Single-decode node path

**R1 #3.** Hardest item here. The old optimization plan explicitly deferred this: stage 2 is rank-bucket ordered while stage 4 is file-ordered and consumer/writer-bound; fusing is architecturally awkward. Measured evidence: planet `s2_node_decompress_ms = 192356` cumulative, and stage 4 processes all 32835/32835 node blobs again.

**Bottleneck.** Stage 2 decodes node blobs to populate `coord_slice` at [stage2.rs:382](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:382). Stage 4 decodes the kept node blobs **again** on the non-way passthrough path at [stage4.rs:439](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:439).

**Why the structure causes it.** Stage 2 is rank-bucket-owned work; stage 4 is file-order output work. Node blobs are treated as stage-local inputs instead of source-owned work units.

**Redesign.** Move to a node-blob-owned executor (or node-stripe executor) that decodes each kept node blob once, fans its tuples into the way-join path, and directly emits the filtered node output side. This almost certainly means rewriting the stage-2 scheduler, not patching it.

**Payoff.** Attacks duplicated input decode on the largest planet-side phase and removes one more full stage-local ownership handoff.

**Risks.** Easiest item here to get wrong. It is easy to trade a duplicate decode for worse buffering or a weaker stage-2 join.

**Conviction: medium-low. Scope: very large** — scheduler rewrite.

---

## Probably not worth pursuing

Consolidated from all three reports:

- **More rank-bucket-count experiments.** Measured at 256 / 384 / 512 on Japan: stage 2+3+finalize slice went +6.5% then +13.8%; `s2_open_calls` scaled 5632 → 8448 → 11264; `s2_node_straddler_blobs` 510 → 766 → 1022; `s3_integrated_straddler_count` 255 → 383 → 511. More buckets grow reopens and straddlers faster than they improve cache fit. Keep `NUM_BUCKETS = 256`.
- **Another stage-1B shard-shape experiment on the existing emission shape.** The grouped-by-local-rank variant regressed `EXTJOIN_STAGE1 +31.9%` on Japan with scratch +25%; the per-blob bucket-staging variant regressed Europe stage 1 +30% because the `BufWriter` layer was already amortizing syscall cost and the staging layer added memcpy + 256-way cache thrash. Excludes #3 — the scratch-spool fusion is a different mechanism (replaces pass B's zlib path entirely, does not reshape the emission).
- **Another stage-2 hot-loop micro pass.** Measured batching (`237cb2e`) reshuffled subcounter attribution — `s2_coord_fill_ms` −16%, `s2_node_extract_ns` down, `s2_node_rank_ns` up correspondingly — without moving `EXTJOIN_STAGE2` wall.
- **Stage-4 non-way wire filtering as the main bet.** Shelved — real CPU win (`s4_nonway_assemble_ms` −53% Europe) but freed decoder CPU refilled the writer queue; wall regressed under both `zlib:6` and `zstd:1`. See "Writer-ceiling diagnostic" under #2.
- **Compressing or varint-encoding rank records further.** The 12-byte record (down from 16) is already optimized. In a stage that is not I/O-bound, more encode/decode CPU buys marginal I/O savings.
- **Stage-4 `coord_payloads` pread micro-optimizations** — `madvise` tuning, `mmap` variants, batching ~57K preads. Reads are sequential (blobs in order) and OS readahead handles them; the optimization history shows per-blob work is at the NVMe floor. Stage 4's ~259s is dominated by input PBF read + output PBF write + rayon compression; `coord_payloads` reads are a small fraction.
- **Reducing stage-2 node-blob straddler re-reads.** At planet scale with 256 rank buckets and ~400K node blobs, ~255 straddler re-decompressions total — roughly 100 MB of extra decompress. Negligible.
- **`io_uring` for scattered writes.** Stage 3's write pattern is large sequential writes (one per bucket). `io_uring` helps most with many small concurrent I/Os — not applicable here.
- **Overlapping stages 1 and 4** (pipe decompressed way blobs from stage 1 through to stage 4). Requires running stages 2/3 concurrently with way-blob transit — a fundamentally different pipeline architecture. Win: one fewer PBF read of way blobs. Complexity: enormous. Not justified pre-1.0.
- **Generic `PbfWriter` / writer refactoring as the primary ALTW answer.** The writer's rayon-based compression pipeline is already parallel and well-tuned; stage 4's consumer is not the bottleneck (passthrough blobs skip compression entirely; way reframe is fast). Writer work is relevant but not ALTW-local.

---

## Recommendation

**Sequence.**

1. **#1 first — promote epoch-spill.** Lowest-risk architectural change with real payoff. Code already exists in `stage23_epoch.rs`. Delete + promote, not a write. Clean keep/revert candidate. Creates the foundation for #2 (softens the stage 3/4 boundary) and #4 (refines the same fused path).
2. **Then #3 — fuse stage 1A + 1B.** Independent of #1/#2, stacks cleanly, moderate scope.
3. **Then #2 — stream stage 3 → stage 4.** Largest remaining payoff, but the biggest rewrite; needs #1 landed first to be tractable.
4. **Then #4, #5, #6** as appetite allows. #4 is a natural continuation of #1's fused path; #5 subsumes #4 at whole-pipeline scope; #6 is the hardest and most speculative.

### Benchmark plan for #1 (epoch-spill default)

1. Remove `parse_epoch_env()`. Auto-compute `num_epochs` from `/proc/meminfo`, or hardcode `num_epochs = 4` initially. Delete `SlotBuckets`, `SharedSlotBuckets`, `stage2_node_join`, and the disk-path `else` branch in `mod.rs`. Keep `prepare_bucket` and `LoaderScratch`.
2. Correctness gate: semantic Denmark parity, not MD5-only parity. Use direct output comparison / semantic diff as the primary gate; `brokkr verify add-locations-to-ways --dataset denmark` is optional extra signal only, because ALTW has accepted deviations from `osmium` and the verify harness is expensive.
3. Europe is the real gate (not Japan): `brokkr add-locations-to-ways --dataset europe --index-type external --bench 3` against current main via `brokkr results --compare`.
4. Key metrics: total wall, peak RSS, scratch disk usage, per-stage marker durations, old downstream slice equivalence, new `s4_send_ms`, eliminated `s3_integrated_finalize_*`, eliminated `s4_coord_payload_pread_ms`, new payload reorder-depth/high-water.
5. **Keep if** Europe total wall improves clearly — thresholds from the three reviewers: ≥5% Europe wall (R1), ≥10s wall (R2), improves-or-neutral with peak RSS ≤ ~10 GB (R3). If flat or worse, check whether auto-tuned epoch count is suboptimal — try manual E=2 for Europe as a diagnostic before reverting. If structurally broken, revert cleanly with diagnostic data.
6. If Europe wins, run one planet confirmation.

### Benchmark plan for #2 (streaming stage 3 → stage 4)

Same shape, scaled for a bigger rewrite. Implement the full coordinator path on a branch with no env-var default. Denmark semantic correctness/parity first. Europe `--bench 3`. Keep only if Europe total wall improves clearly, or the old `stage3 + finalize + stage4` slice drops materially with no RSS/scratch blow-up — roughly **≥5% Europe wall** for a rewrite of this size. Planet confirmation if Europe wins. Revert cleanly if flat or worse. Evaluate under `zstd:1` (or `compression:none`) as well — see writer-ceiling diagnostic.

---

## Implementation conventions

Apply when implementing any of the opportunities above:

- **Ns accumulators for per-item timing.** `AtomicU64` holding nanoseconds, `ns_to_ms` helper at emit time. Reference: `WayReframeCounters` in `stage4.rs`. Do not accumulate `as_millis()` per item — sub-ms work truncates.
- **Reorder-buffer for parallel producer → serialized consumer.** `crate::reorder_buffer::ReorderBuffer::with_capacity(N)`; push with `(seq, value)`, `pop_ready()` drains in order. Already used by stage 1 pass A, stage 3, stage 4. Reuse for #2's streaming coordinator — do not reinvent.
- **ScratchDir for all temp files.** `scratch.file_path(name)` or `scratch.bucket_path(kind, idx)`. Lifetime-tied cleanup on drop. Applies to #3's node-ID scratch and #4's per-blob spill.
- **`#[hotpath::measure]` on functions > 1 ms wall** so they show in `--hotpath` profiles.
- **Worker count convention.** `available_parallelism() - 2 max 1 min 4`, often `.min(6)`. The `-2` reserves cores for the consumer + writer threads. Any tuning that changes this must justify why.
- **Counter naming.** `s<stage><phase>_<thing>_ms` / `_bytes` / `_calls`. Stage-scoped prefix keeps grep/history readable.
- **Prototype discipline.** Prefer full coherent branch rewrites with keep/revert benchmarking over env-var-gated probes. If a temporary fallback is unavoidable during rollout, keep it short-lived and delete it as soon as the decision is made. The old plan showed that narrow env-var probes created codebase pollution and often failed to answer the real structural question.

---

## Historical probe record

See [`altw-external-optimization-plan.md`](altw-external-optimization-plan.md) — the stripped historical record of probes attempted before the structural re-plan. Useful when a proposal looks like an old probe: the UUIDs, measured outcomes, and reasons for shelving are recorded there so future work can distinguish between *the idea was wrong* and *the probe was too timid*.
