# ALTW External-Join: Structural Opportunities

Synthesis of six independent reviews of the ALTW (`add-locations-to-ways --index-type external`) pipeline. Five original reviewers plus a sixth code-only reviewer — later clarified after explicit questions about internal API replacement and a hard 30 GB-RAM planet host — all land on the same framing: ALTW today behaves like a reorder pipeline, not a saturated engine. It pays real wall time to destroy blob ownership, externally permute coordinates through rank-sharded and slot-bucketed intermediates, then reconstruct blob order. The disciplined four-stage structure survives because each handoff is a filesystem round-trip; the cost shows up as long idle moments at stage boundaries.

Convergence across six reviewers: the **stage 2 → stage 3 → stage 4 disk-seam chain** is still the dominant theme — every reviewer attacks at least one of those seams. **Stage 1 decompress duplication** appears in five reports (R2 #3, R3 #2, R4 A1, R5 #2, R6 #1). **Epoch-spill promotion** of the stage 2→3 seam still converges only in two reports (R2 #1, R3 #1), with R4 A3 attacking the same seam via a different mechanism; the sixth reviewer did not assess it because the existing `stage23_epoch.rs` prototype sat outside the mainline code read. New convergence strengthened by R6: **routing-table removal of `coord_payloads` consolidation** (R4 A2, R6 #2), **upstream-cat BlobHeader extension for control metadata** (R4 B5, R5, R6, with explicit disagreement on scope), and a brand-new item from the R6 follow-up: **stage-2 de-ranking** — delete per-node `rank_if_set()` by using blob-local rank counters derived from `NodeBlobInfo`. This document consolidates eleven distinct opportunities and everything the six reviewers flagged as *not* worth pursuing.

---

## Context: already shipped on current `main`

Do not re-propose these — they are in tree and are reflected in the baseline measured below:

- `coords_by_rank` removal: stage 2 decodes node blobs directly via `NodeBlobInfo`
- Stage-3 direct scatter from raw `ResolvedEntry` bytes (no `Vec<ResolvedEntry>` materialization)
- Parallel finalize tail in [coord_payloads.rs](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs) — per-blob pread+pwrite work-stealing across `available_parallelism` threads
- Stage-4 per-way refcount sidecar consumption in the way reframe path
- Stage-4 raw passthrough for relation blobs (always) and node blobs when `keep_untagged_nodes` is set
- `PerWayRcs` lazy per-blob decode via blob-offset sidecar
- `IdSetDense::rank_if_set()` fused get+rank in stage 2; the remaining opportunity is deleting per-node rank queries entirely, not re-proposing separate `get()+rank()` lookups
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
- Read rank shards → counting-sort per rank bucket → `pread + decompress` node blobs → `extract_node_tuples` and call `rank_if_set()` on each tuple to populate `coord_slice` ([stage2.rs:382](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:382), [:460](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:460)) → resolve `(slot_pos, lat, lon)` → write to shared slot buckets via per-bucket `Mutex<BufWriter>`
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
- **2-piece straddler invariant.** A blob's slot range spans at most two adjacent slot buckets. `slot_bucket_count` is chosen so every bucket width ≥ `max_blob_slots`. Constrains #6 (blob-group rewrite) and any layout change to slot buckets.
- **Zero-coord sentinel.** Stage 2's `coord_slice` uses `(lat==0, lon==0)` as the unresolved sentinel; the slice is fully zeroed at the start of each rank bucket. Any redesign that removes zeroing (e.g. #5's per-blob accumulators skipping empty slots or #11's explicit-presence bitmap) must replace the sentinel with an explicit presence signal.
- **Per-way refcount ordering.** The stage-1 per-way refcount sidecar is written in PBF blob order and consumed in PBF blob order by stage-4 reframe. Any stage-1 reshape preserves this ordering.
- **Straddler state machine.** Stage 3's merge is an exhaustive `None → Left|Right → Both`; duplicate or third halves error. Do not weaken to `Option<(Vec<u8>, Vec<u8>)>`. Affects #2 (the streaming coordinator must maintain this).
- **Blob-local rank monotonicity.** For sorted PBFs, `extract_node_tuples()` yields node tuples in ascending ID order, and referenced nodes inside a blob occupy the contiguous rank interval `[ref_rank_start, ref_rank_end)`. Affects #4 — `get()+counter` is only correct if every `get(id)==true` consumes exactly one rank from that interval.
- **`build_rank_index()` before any `rank_if_set` / `rank` / `count_below()`, and keep it until the last rank consumer is gone.** `IdSetDense` requires the rank index built after all `set_atomic` calls. Affects #3 and #4 — the scratch-spool variant must finish populating `IdSetDense` during pass A before `build_rank_index()`, and current-stage-1 pass B plus `build_node_blob_mapping()` must finish before rank metadata can be dropped. If pass B disappears, the stage-1 boundary becomes the drop point.

---

## Ranked opportunities

### #1 — Promote epoch-spill to default; delete the disk slot-bucket path

**Convergence: R2 #1, R3 #1.** R4 A3 attacks the same stage 2→3 seam with a different mechanism (per-bucket `Vec<Vec<u8>>` append + per-slot-bucket completion counters instead of `scatter_buf`+epochs); see "Mechanism alternative" below. The code for the epoch-spill mechanism already exists in [src/commands/altw/stage23_epoch.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage23_epoch.rs) as an env-var-gated prototype. This is delete + promote, not greenfield. R6 did not evaluate it because the prototype was not part of the mainline code read.

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

**Mechanism alternative (R4 A3).** Instead of epoch-bounded `scatter_buf` resolution, R4 proposes per-bucket append-only `Vec<Vec<u8>>` segment lists — one inner `Vec` per rank-bucket worker contribution per slot bucket. Each stage 2 worker, after finishing its rank bucket, atomically increments a per-slot-bucket "contributors_done" counter; when the counter hits `num_rank_workers`, the slot bucket is complete and stage 3 workers can drain it from a queue of "complete slot buckets." Memory budget at planet: ~700 MB per bucket peak, ~4 GB resident with 6 stage-3 workers each holding one in-flight bucket; bounded by stage-3 throughput on the producer side (use `mem::take` on drain). The mechanisms differ in two ways: (a) epoch-spill is batch-scheduled (work proceeds in epoch waves), R4 A3 is streaming (work proceeds slot-bucket-by-slot-bucket as completion fires); (b) epoch-spill keeps random-access `scatter_buf` semantics so encoding can read in any order, R4 A3 has stage 3 do the scatter into its own `scatter_buf`-equivalent after draining (one extra memcpy). Worth prototyping as a comparison once #1's epoch path is the baseline; the completion-counter pattern is also a building block for #2's streaming coordinator.

---

### #2 — Stream stage 3 → stage 4; eliminate the `coord_payloads` file

**Convergence: R1 #1, R2 #2, R5 #1.** The biggest remaining double-digit wall opportunity, but a real architectural rewrite of the stage 3/4 boundary. Lands cleaner after #1. R5 frames it more sharply: "stage 3 should disappear as a standalone phase" — `SharedSlotBuckets`, `stage3_slot_reorder`, `finalize_coord_payloads`, `CoordPayloadsReader`, and most straddler machinery should go away. R6 independently identifies the same seam and argues stage 4 should start as soon as blob payloads are resolvable, but its cleanest low-risk mechanism maps more directly to #8. See also #8 for a much smaller-scope alternative that eliminates only the consolidate copy without touching the stage 3/4 boundary.

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
- The explicit 30 GB planet-host constraint tightens the design: this is a bounded-queue cascade, not an unbounded readiness map. Straddler staging, ready-payload buffering, and writer handoff all need fixed depths with prompt free-after-consume behavior

**Writer-ceiling diagnostic.** A shelved probe on stage-4 wire-format DenseNodes filtering (`4910fd9`) delivered a real stage-4-local CPU win that did not reach wall: Europe `s4_nonway_assemble_ms` 78.5 s → 36.9 s (−53%), yet `EXTJOIN_STAGE4` went 122.7 s → 127.6 s because `s4_send_ms` cumulative grew 561 s → 672 s — freed decoder CPU refilled the writer queue. Under `zstd:1` the phase moved only −1.3% and total wall still regressed (5m40s → 5m48s). **Streaming 3 → 4 will hit the same ceiling on any workload where `PbfWriter` compression is the true limit.** The keep-decision gate for #2 therefore evaluates under both default `zlib:6` and `zstd:1` (or `compression:none` for the internal pipeline), because a stage-boundary win can be real and still invisible on wall under a writer-bound output mode.

**Composition note.** #2 is viable on the **current slot-bucket representation** as a legitimate smaller first cut before #6 (the full blob-group rewrite). After #1 lands, the finalize phase is already absorbed into per-epoch emits, which makes #2 cleaner because the boundary it attacks is already softer.

**Conviction: high on payoff, medium on ease. Scope: large.** Not a try-and-revert-in-a-day change — needs careful design, phased rollout, extensive benchmarking.

---

### #3 — Fuse stage 1A + 1B via a node-ID scratch spool

**Convergence: R2 #3, R3 #2, R5 #2, R6 #1/follow-up.** Independent of #1 and #2; stacks cleanly. Subsumes R1's medium-value "single-ingest way-ref spool" note targeting [stage1.rs:327](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:327) and [:421](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:421). R4 A1 attacks the same bottleneck with a more aggressive mechanism — see "Variant" below. R6's follow-up, after the explicit 30 GB RAM constraint, sharpens which sub-variants are actually viable.

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

**Variant — R4 A1: node-ID-partitioned single pass (no Pass B at all).** Instead of caching node IDs to scratch and replaying them in Pass B, change the partition key for the downstream shards from *rank* to *node-ID high bits*. Pass A then becomes the only pass:

- Each worker decompresses each way blob once, calls `set_atomic` on `IdSetDense` as today, **and** simultaneously emits `(node_id: u64, slot_pos: u64)` records (16 bytes) into 256 ID-bucketed shard files (partition by `node_id >> shift`).
- `slot_pos` is `slot_start[blob_seq] + i`; workers either compute their own `slot_start` from a back-channel keyed by `blob_seq`, or buffer per-blob `Vec`s and ship them through a small bucketing pool that assigns `slot_start`s and dispatches (R4's preferred shape — keeps worker code simple).
- `IdSetDense::build_rank_index()` and `build_node_blob_mapping` run after Pass A finishes, just like today, but with no Pass B between them.
- Stage 2 changes shape: load each ID bucket, sort by ID (radix or counting sort on `(node_id - bucket_id_low) as u32`), then proceed. The current per-bucket counting-sort by `local_rank` in `prepare_bucket` is replaced.

**Tradeoffs vs. the scratch-spool variant.**
- A1 eliminates Pass B entirely (no rebuild from scratch, no zlib at all in pass B). Scratch-spool keeps Pass B but serves it from compact scratch rather than re-decompression.
- A1 grows shard records 12 → 16 bytes (+33%): planet shard volume rises ~175 GB → ~234 GB. At multi-GB/s NVMe this extra ~60 GB is sub-minute, while the saved decompression at planet is several minutes of CPU. Net positive.
- A1 changes the downstream sort from rank-counting to ID-sorting. ID density in OSM is uneven (deletion churn + historical ID-space allocations). Mitigation: existing work-stealing dispatch handles bucket-size variance; the worst real-world skew is maybe 2–3× from uniform.
- A1 is a larger change to stage 2's loader (sort-by-ID replacing counting-sort-by-rank); scratch-spool is a more localized change inside `stage1.rs` only.

**30 GB host constraint.** The sixth review's follow-up rules out all-RAM "hold per-blob Vecs until pass A finishes, then rank-sweep" forms (~52 GB at planet). The existing scratch-spool-to-disk variant remains viable, but now carries a clearer downside: extra scratch write+read volume may eat into the decompression win. That shifts the aggressive ID-bucketed form upward once #4 lands, because the removed rank work no longer just sloshes into stage 2.

**Recommendation between variants.** Under the 30 GB host constraint, discard all-RAM buffer/sweep forms. If appetite is limited and you want the smallest localized diff, scratch-spool is still the conservative benchmark (R5's "exact ordering bugs, and if you use BlobHeader piggybacking you must stay compact" warning applies — flat i64 is the simplest, delta-varint the better production form). If #4 lands or stage 2's `prepare_bucket` path is being reworked anyway, the ID-bucketed form deserves equal billing — its main downstream objection weakens substantially once per-node `rank_if_set()` is gone.

---

### #4 — Remove stage-2 per-node rank queries; assign ranks by blob-local counters

**Convergence: R6 follow-up only, but grounded in current code and historical stage-2 profiling.** This is not an `IdSetDense` rewrite. Keep the bitmap / `set_atomic()` / `get()` path; stop using rank metadata in the stage-2 hot loop.

**Bottleneck.** Stage 2 calls `IdSetDense::rank_if_set(id)` for every extracted node tuple at [stage2.rs:460](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:460). At planet scale that is billions of cache-unfriendly prefix-walk queries, and it is why `IdSetDense` rank metadata must stay resident through stage 2.

**Why the structure causes it.** `NodeBlobInfo` already tells stage 2 the referenced-rank interval `[ref_rank_start, ref_rank_end)` for each node blob. Node blobs are ID-sorted, and `extract_node_tuples()` emits tuples in ID order. Yet stage 2 throws that monotonicity away and re-derives the global rank from scratch for each referenced node.

**Redesign.** For each decoded node blob:
- initialize `next_rank = blob.ref_rank_start`
- iterate tuples in decoded ID order
- use `node_id_set.get(id)` as the membership test
- when membership hits, treat the tuple as rank `next_rank`, write it into `coord_slice` if the rank falls inside the current bucket, then increment `next_rank`
- after finishing the blob, assert `next_rank == blob.ref_rank_end` in debug / validation builds

This deletes `rank_if_set()` from stage 2 entirely. `build_node_blob_mapping()` still needs rank metadata up front, but once stage 1's remaining `rank()` / `count_below()` consumers are finished, `IdSetDense` can expose a `drop_rank_index()` helper and carry only the bitmap into stage 2. On today's architecture the safe drop point is after pass B + `build_node_blob_mapping()`; if #3's single-pass forms land, the stage-1 boundary becomes the drop point.

**Payoff.**
- Removes billions of `rank_if_set()` calls from the stage-2 hot loop
- Likely improves both wall and cache behavior because membership becomes an O(1) bit test instead of chunk-prefix + block-prefix + residual word scans
- Frees the rank index metadata earlier (~100 MB at planet-scale by reviewer estimate; exact number depends on allocated chunks) and removes its cache pollution from stage 2
- Historical anchor: `06f2a30`'s "fused `rank_if_set` + parse-free bucket prep" moved stage 2 from 181 s → 140 s in an earlier pipeline shape, so deleting rank queries entirely is plausible enough to benchmark immediately

**Risks.**
- Correctness depends on the blob-local rank monotonicity invariant: decoded node tuples must be nondecreasing in ID and the referenced nodes inside a blob must occupy exactly `[ref_rank_start, ref_rank_end)`
- Boundary blobs still straddle adjacent rank buckets on the current architecture, so each bucket worker that touches the blob must replay the same local counter logic consistently
- The reviewer-level 30–60 s planet estimate is plausible but not measured on current `main`; treat it as a hypothesis, not ground truth

**Conviction: medium-high. Scope: small-to-moderate.** Under the explicit 30 GB planet-host constraint, this becomes a first-tier contained experiment rather than an M-series cleanup.

**Relationship to #3.** #4 stands alone, but it also changes the tradeoff inside #3. Once stage 2 stops doing per-node `rank_if_set()`, the main objection to ID-bucketed stage-1 emission weakens: the rank work no longer merely migrates downstream.

---

### #5 — Direct-to-`coord_payloads` via per-blob accumulators (skip `scatter_buf`)

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

**Relationship to #6.** Both #5 and #6 re-key downstream around blobs. #5 stays inside the fused epoch path (resolve → per-blob accumulator → encode). #6 re-keys the entire pipeline including stage 1 emission.

**Conviction: medium-high. Scope: substantial rewrite of the fused path.**

---

### #6 — Blob-group downstream rewrite: re-key around way blobs, not global slot buckets

**Convergence: R1 #2, R5 #1.** R5 explicitly endorses this framing ("re-key the downstream path around way blobs/blob groups and stream directly into stage 4") and names the artifacts to delete: `SharedSlotBuckets`, `stage3_slot_reorder`, `finalize_coord_payloads`, `CoordPayloadsReader`, and most straddler machinery. The structurally cleanest answer, at the cost of rewriting stages 1–4.

**Bottleneck.** Stage 1 emits global `slot_pos` records; stage 2 routes every resolved coordinate into shared global slot buckets; stage 3 rebuilds dense bucket-local slot images and then classifies blob/bucket intersections and straddlers ([stage3.rs:292](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:292), [:386](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:386)). The entire `slot_bucket_count` and 2-piece straddler apparatus at [mod.rs:238](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:238) exists only to survive this key choice.

**Why the structure causes it.** Blob ownership is thrown away at [stage1.rs:451](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:451) and only reconstructed downstream. Every subsequent stage rebuilds it, in a different ownership domain.

**Redesign.** Change the downstream key from `slot_pos` to `way_blob_idx + blob_local_slot` (or a blob-group-local equivalent). Partition contiguous way blobs into bounded blob groups. Stage 2 emits resolved records to blob-group files. Stage 3 scatters and encodes directly within those blob-aligned groups. This deletes blob/bucket classification, straddler staging, and most of finalize **by construction**.

**Payoff.** The cleanest way to stop rebuilding the same coordinate stream in three ownership domains. Also makes #2 (streaming 3→4) much cleaner — payloads are produced already in blob-aligned order, and straddlers vanish.

**Risks.** Real rewrite of stages 1–4. The fundamental rank-order vs blob-order mismatch does not go away; a bad blob-group design can preserve most of the scatter cost while adding new bookkeeping.

**Conviction: medium** (high structural payoff, high implementation risk). **Scope: very large.**

---

### #7 — Single-decode node path

**Convergence: R1 #3, R5 #3.** Hardest item here. The old optimization plan explicitly deferred this: stage 2 is rank-bucket ordered while stage 4 is file-ordered and consumer/writer-bound; fusing is architecturally awkward. Measured evidence: planet `s2_node_decompress_ms = 192356` cumulative, and stage 4 processes all 32835/32835 node blobs again. R5 affirms but adds the same risk caveat: easiest big rewrite to get wrong — can reduce decode cost without moving wall if the writer stays dominant.

**Bottleneck.** Stage 2 decodes node blobs to populate `coord_slice` at [stage2.rs:382](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:382). Stage 4 decodes the kept node blobs **again** on the non-way passthrough path at [stage4.rs:439](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:439).

**Why the structure causes it.** Stage 2 is rank-bucket-owned work; stage 4 is file-order output work. Node blobs are treated as stage-local inputs instead of source-owned work units.

**Redesign.** Move to a node-blob-owned executor (or node-stripe executor) that decodes each kept node blob once, fans its tuples into the way-join path, and directly emits the filtered node output side. This almost certainly means rewriting the stage-2 scheduler, not patching it.

**Payoff.** Attacks duplicated input decode on the largest planet-side phase and removes one more full stage-local ownership handoff.

**Risks.** Easiest item here to get wrong. It is easy to trade a duplicate decode for worse buffering or a weaker stage-2 join.

**Conviction: medium-low. Scope: very large** — scheduler rewrite.

---

### #8 — Routing table over worker tmp fds; eliminate finalize's consolidate copy

**Convergence: R4 A2, R6 #2 (unchanged after the 30 GB follow-up).** A much smaller-scope variant of #2. Stages 1–3 unchanged; only finalize and stage 4 change. R4 explicitly recommends this as the first cut for blast-radius reasons, and R6 independently rediscovers it from code alone.

**Bottleneck.** Stage 3 produces per-worker temp files (`payloads-W{i}`); `finalize_coord_payloads` then reads ~55 GB from worker tmps and `pwrite`s ~55 GB into a consolidated `coord_payloads` file ([coord_payloads.rs:255](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:255)); stage 4 preads the same ~55 GB from that consolidated file ([stage4.rs:376](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:376)). Planet: ~110 GB of disk traffic to ferry already-existing bytes from N files into 1 file and back out.

**Why the structure causes it.** `CoordPayloadsReader::pread_blob_payload(blob_idx)` requires a contiguous random-access file with an upfront offsets table ([coord_payloads.rs:16](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:16), [:686](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:686)). The consolidated file's only job is to make the bytes addressable from one fd by `(blob_idx → offset, len)`.

**Redesign.** Replace `CoordPayloadsReader` with a `BlobLocation` router holding:
- `Vec<Arc<File>>` — one entry per worker tmp file, opened once during finalize
- `Vec<BlobLocation>` indexed by `blob_idx`, where `BlobLocation` is either:
  - `Worker { worker_id, byte_offset, byte_length }`
  - `Straddler(Vec<u8>)`
  - a zero-ref sentinel

Building the routing table is a metadata pass over the existing per-worker manifests plus the existing straddler staging. R6's cleaner variant keeps the fully encoded straddler payloads in RAM instead of appending them to a new file — there are only a few hundred of them, so the total resident size is tens of MB, not GB. Stage 4 looks up the blob location and either `pread`s from the correct worker tmp fd or consumes the in-RAM straddler bytes directly.

**Payoff.**
- Eliminates ~110 GB of disk traffic at planet (55 GB write + 55 GB read of the consolidated artifact)
- Finalize today is ~tens of seconds of pwrite-bound work; stage 4's `coord_payloads` preads compete with input PBF preads on the same disk
- **Estimates: 30–60s planet, comparable Europe fraction**

**Risks.**
- N tmp files (≤ 6 workers) → no fd pressure issue. Random-pread latency per blob unchanged; reads spread across more files.
- RAM-held straddlers are small enough to be a non-issue, but their lifetime now spans finalize → stage 4 instead of being flushed to disk
- After #1 (epoch-spill promoted), finalize already merges into per-epoch emits but worker tmps still get written and consolidated; #8 still applies and stacks cleanly.
- Subsumed by #2 (full streaming). If #2 lands first, #8 is moot.

**Conviction: high. Scope: small.** Smallest blast radius of any opportunity in this list.

---

### #9 — Pull relation-member collection forward into stage 1

**Convergence: R4 B1, R5 medium.** Two reviewers independently flag the extra full-PBF pass as wasted serial time wedged between stage 3 and stage 4.

**Bottleneck.** `external_join` runs `collect_relation_member_node_ids` as a serial pass after finalize ([mod.rs:400](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:400)) for the filtered case (the default). `BlobReader::next()` reads every blob payload even when the consumer later skips non-relations ([read/blob.rs:813](/home/folk/Programs/pbfhogg/src/read/blob.rs:813)) — so today this scan reads and decompresses way + node blobs purely to skip them.

**Why the structure causes it.** The pass exists because stage 4 needs to know which untagged nodes are referenced by relations and must be kept. It is currently scheduled after finalize as a separate phase, even though it shares no state with stages 1–3.

**Redesign.** Two layers:
1. **Pread relation blobs only.** `blob_meta` already knows where relation blobs live. Skip `BlobReader`'s general scan and use the metadata to pread only relation blob payloads. Eliminates wasted decompression of way/node blobs. ([add_locations_to_ways.rs:955](/home/folk/Programs/pbfhogg/src/commands/add_locations_to_ways.rs:955), [read/blob.rs:813](/home/folk/Programs/pbfhogg/src/read/blob.rs:813))
2. **Fold into stage 1 workers (or run concurrently with stage 1).** Stage 1 already has parallel workers preading the input PBF via `Arc<File>`. Add relation-blob handling keyed off `meta.kind == Relation`, either to the same worker pool or to a parallel set sharing the same `Arc<File>`. R5 emphasizes this should start much earlier than today's post-finalize position.

**Payoff.**
- Eliminates a serial full-PBF scan that currently sits between stage 3 and stage 4
- Removes wasted decompression of non-relation blobs (today's `BlobReader::next()` decompresses everything before the kind filter)
- **Estimates: 5–15s planet depending on how much overlap is achieved**

**Risks.**
- Trivial implementation; correctness gate is straightforward (compare collected node-ID set to current implementation, byte-equal)
- If folded into stage 1 workers, contention on the shared `Arc<File>` is bounded by NVMe queue depth
- Output-side ordering invariants don't apply (the collected node-ID set has no order requirement)

**Conviction: high. Scope: small.**

---

### #10 — Upstream-cat BlobHeader extension for ALTW control metadata

**Convergence: R4 B5, R5 medium, R6 #4/follow-up — with explicit disagreement on scope.** All three reviewers propose using PBF `BlobHeader` unknown-field extensions (the spec invites this) to carry ALTW-relevant per-blob metadata produced by `pbfhogg cat`. The disagreement is now narrower: the conservative refcount-only variant is supported by R5 and R6, while the aggressive "stuff full node-ref lists into headers" form remains R4-only and still runs into the 64 KiB cap.

**Conditional applicability.** Only relevant if the production pipeline always feeds ALTW from `pbfhogg cat` output. If ALTW must work on raw Geofabrik/planet PBFs without the prior cat step, this is moot.

**Bottleneck.** Stage 1 pass A decompresses every way blob to extract node-ID lists ([stage1.rs:71](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:71)). The per-way refcount sidecar at [mod.rs:189](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:189) and per-way-refcounts scratch at [mod.rs:323](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:323) are entirely derived from the same way blob contents.

**Practical constraints.** Header size is hard-capped at 64 KiB ([read/blob.rs:346](/home/folk/Programs/pbfhogg/src/read/blob.rs:346)); current `BlobHeader` encode/decode only handles fields 1–4 ([write/writer.rs:1247](/home/folk/Programs/pbfhogg/src/write/writer.rs:1247)). Both writer and reader need extending.

**Two variants — both reviewers proposed, with opposing scope choices.**

- **Conservative (R5/R6):** embed per-way refcount + per-blob total refs only. Eliminates `ref_count_sidecar` / per-way-refcounts scratch. R5 is explicit: "I would not try to stuff full ref lists or payloads into BlobHeaders." At ~8000 ways per blob × ~2 bytes/varint refcount ≈ ~16 KB/blob — fits comfortably in the 64 KiB cap.
- **Aggressive (R4 B5):** embed per-way node-ID lists (delta-varint, the same shape Pass A would scan out). With this, ALTW's stage 1 reads only blob headers — no decompression of way blob payloads at all. Eliminates the stage-1 CPU-bound decompression entirely, even with #3. **But:** at ~8000 ways/blob × ~10 refs/way average × 2–3 bytes/delta-varint ≈ ~240 KB/blob — well over the 64 KiB header cap. Naive form does not fit. Would need either smaller blob groups (more blobs, more headers, less data per header) or a side-table addressed by blob position rather than header-embedded.

**Payoff.**
- Conservative: removes scratch creation cost for refcount sidecars (small fraction of stage 1 wall — measured in the existing ref_count_sidecar code path)
- Aggressive: removes the entire stage 1 way-blob decompression (CPU-bound, ~50% of stage 1 wall) — but only if the size cap can be worked around

**Risks.**
- Couples ALTW to `pbfhogg cat`'s output schema. Other consumers treat the extension as opaque (which the PBF spec prescribes), but the convention becomes a private contract.
- 64 KiB header cap rules out the aggressive variant in its naive form; either a different framing or smaller blob groups required.
- Cat itself becomes the natural producer; downstream consumers of ALTW output cannot benefit from this without their own changes.
- R6 follow-up notes this is RAM-neutral-to-slightly-negative on its own. The value is structural cleanup and platform leverage, not resident-set reduction.

**Conviction: medium (conservative variant), low (aggressive variant). Scope: moderate** — requires changes to both `pbfhogg cat` (writer side, header encoding) and ALTW (reader side, header decoding). R4 rated this as "second-best long-term direction if the production pipeline always feeds ALTW from `pbfhogg cat`."

---

### #11 — Replace zero-filled stage-2 coord slices with an explicit presence bitmap

**Convergence: R6 M1 only.** Smaller than the structural items above and potentially obsoleted by #5 or #6, but worth recording because the current `coord_slice[..slice_bytes].fill(0)` at [stage2.rs:397](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:397) is correctness-driven, not incidental.

**Bottleneck.** Each stage-2 worker reuses a large `coord_slice` across rank buckets and fully zero-fills the active prefix for each bucket. The cumulative memset cost is already measured as `s2_coord_zero_ms` / `s2_coord_zero_ns`.

**Why the structure causes it.** `coord_slice` uses `(lat==0, lon==0)` as the unresolved sentinel. Because the buffer is reused, stale bytes from a previous bucket would silently look like resolved coordinates unless the whole active range is cleared first.

**Redesign.** Keep the coordinate payload bytes as today, but replace the sentinel with an explicit presence signal:
- `coord_slice` stores raw `(lat, lon)` bytes for touched ranks
- a parallel bitmap / bitset marks which local ranks were actually resolved
- the resolve loop checks the bitmap instead of `(lat, lon) != (0, 0)`

This can zero only the bitmap (or use a generation-tag trick) instead of the full 8-byte-per-rank coord slice.

**Payoff.**
- Deletes repeated zero-fill of the reused stage-2 coord slice
- Makes the missing-coordinate signal explicit instead of overloading a coordinate value
- Pairs naturally with #4 if stage 2's fill loop is already being edited

**Risks.**
- Adds another side structure per worker, though the bitmap is much smaller than the coord slice
- Moot if #5 or #6 delete dense bucket-local coord slices entirely
- Measured impact is likely modest relative to the seam deletions above

**Conviction: medium-low. Scope: small.** Distinct from earlier rejected stage-2 hot-loop micro-passes because it removes a correctness-driven memset rather than reshuffling bookkeeping inside the same loop.

---

## Probably not worth pursuing

Consolidated from all six reports:

- **More rank-bucket-count experiments.** Measured at 256 / 384 / 512 on Japan: stage 2+3+finalize slice went +6.5% then +13.8%; `s2_open_calls` scaled 5632 → 8448 → 11264; `s2_node_straddler_blobs` 510 → 766 → 1022; `s3_integrated_straddler_count` 255 → 383 → 511. More buckets grow reopens and straddlers faster than they improve cache fit. Keep `NUM_BUCKETS = 256`. R5 corroborates: not a first-order optimization.
- **Another stage-1B shard-shape experiment on the existing emission shape.** The grouped-by-local-rank variant regressed `EXTJOIN_STAGE1 +31.9%` on Japan with scratch +25%; the per-blob bucket-staging variant regressed Europe stage 1 +30% because the `BufWriter` layer was already amortizing syscall cost and the staging layer added memcpy + 256-way cache thrash. Excludes #3 — the scratch-spool fusion is a different mechanism (replaces pass B's zlib path entirely, does not reshape the emission). **R4 B2 proposes a third, untested variant:** consolidate the per-worker fanout (1500 files = `num_workers × NUM_BUCKETS` at planet, ~400 MB of `BufWriter` buffer memory) down to 256 shared per-bucket writers with batched per-worker flush (e.g. 64 KB chunks under per-bucket lock). Distinct from both regressed variants — fewer files + less buffer memory rather than reshaping emission. Worth measuring as a contained experiment if #1+#8 don't subsume the rank-shard intermediate, but R4 itself notes "the contention concern goes away if A1 + A3 are done (records flow through memory, not files)" — so this is fallback territory only.
- **Another stage-2 hot-loop micro pass on the current `rank_if_set` shape.** Measured batching (`237cb2e`) reshuffled subcounter attribution — `s2_coord_fill_ms` −16%, `s2_node_extract_ns` down, `s2_node_rank_ns` up correspondingly — without moving `EXTJOIN_STAGE2` wall. Distinct from #4 and #11, which delete or replace whole classes of stage-2 work rather than rearranging the current loop.
- **Stage-4 non-way wire filtering as the main bet.** Shelved — real CPU win (`s4_nonway_assemble_ms` −53% Europe) but freed decoder CPU refilled the writer queue; wall regressed under both `zlib:6` and `zstd:1`. See "Writer-ceiling diagnostic" under #2. R5 corroborates from new evidence ([stage4.rs:258](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:258), [stage4.rs:676](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:676)) — the writer-ceiling is visible in code, not just in measurements.
- **Compressing or varint-encoding rank records further.** The 12-byte record (down from 16) is already optimized. In a stage that is not I/O-bound, more encode/decode CPU buys marginal I/O savings. (Note: R4 A1 deliberately accepts 12 → 16 bytes to enable single-pass stage 1 — a different tradeoff in a different context.)
- **Stage-4 `coord_payloads` pread micro-optimizations** — `madvise` tuning, `mmap` variants, batching ~57K preads. Reads are sequential (blobs in order) and OS readahead handles them; the optimization history shows per-blob work is at the NVMe floor. Stage 4's ~259s is dominated by input PBF read + output PBF write + rayon compression; `coord_payloads` reads are a small fraction.
- **Reducing stage-2 node-blob straddler re-reads.** At planet scale with 256 rank buckets and ~400K node blobs, ~255 straddler re-decompressions total — roughly 100 MB of extra decompress. Negligible. R5 reframes with a related but distinct concern: atomic bucket stealing at [stage2.rs:356](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:356) throws away locality (workers may end up processing non-contiguous buckets), and a contiguous bucket assignment or tiny boundary-blob cache is "a real, contained win." But R5 itself concludes "it will not compete with deleting the slot-bucket path" — defer until after #1/#8 land, since the slot-bucket layer may go away first.
- **`io_uring` for scattered writes.** Stage 3's write pattern is large sequential writes (one per bucket). `io_uring` helps most with many small concurrent I/Os — not applicable here.
- **Overlapping stages 1 and 4** (pipe decompressed way blobs from stage 1 through to stage 4). Requires running stages 2/3 concurrently with way-blob transit — a fundamentally different pipeline architecture. Win: one fewer PBF read of way blobs. Complexity: enormous. Not justified pre-1.0.
- **Generic `PbfWriter` / writer refactoring as the primary ALTW answer.** The writer's rayon-based compression pipeline is already parallel and well-tuned; stage 4's consumer is not the bottleneck (passthrough blobs skip compression entirely; way reframe is fast). Writer work is relevant but not ALTW-local. R5 corroborates: "Generic writer/API cleanup first" is on R5's "Not Next" list. R6 follow-up adds a narrower post-#2/post-#6 idea — have stage-4 workers frame/compress way blobs directly (e.g. `frame_blob_pipelined()` → `write_raw_owned()`) once compression becomes the actual ceiling — but that is explicitly a later refinement, not a current-architecture next step.
- **Wholesale internal-API replacement as a prerequisite.** R6 follow-up explicitly backs away from this. The useful `IdSetDense` work is the ALTW-local usage change in #4, not a new set structure; the useful writer work is the later way-path framing specialization above, not a writer rewrite first.
- **Telemetry/counter cleanup as a standalone optimization program.** R6's suggestions to gate per-group reframe atomics, remove the `s4_channel_high_water` CAS path, collapse duplicate ms/ns timers, and tidy small `IdSetDense` rank-prefix allocation details may improve profile readability, but they are not first-order structural opportunities. Take them opportunistically when the surrounding code is already open.
- **Lifting the hard `.min(6)` worker caps** at [mod.rs:328](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:328), [stage2.rs:234](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:234), [stage3.rs:125](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:125). R5 flags these as obvious anti-saturation choices on wide hosts but explicitly says "I would not treat it as a first-order optimization on the current architecture." The structural rewrites (#1, #2, #6) may change the parallelism model entirely; revisiting the caps before then would be tuning a moving target.

---

## Recommendation

**Sequence (revised after the sixth-review follow-up and the explicit 30 GB-RAM planet constraint).**

1. **#1 first — promote epoch-spill.** Still the strongest structural seam deletion with real payoff, and the code already exists in `stage23_epoch.rs`. Delete + promote, not a write. Clean keep/revert candidate. R6 did not evaluate it because the prototype sat outside the mainline code read.
2. **Prototype #4 immediately before or after #1.** Small, local, RAM-positive, and the cleanest contained benchmark under the 30 GB host constraint. Add blob-local rank-monotonicity asserts and measure it directly.
3. **Then #8 — routing table / `BlobLocation` over worker tmp fds.** Smallest blast radius among the remaining seam deletions. Eliminates ~110 GB of disk traffic at planet (the finalize consolidate copy + stage-4 payload preads) without architectural change. R6 independently rediscovers it and tightens the implementation by keeping straddlers in RAM.
4. **#9 in parallel** — pull relation-member collection forward into stage 1. Independent of #1/#2/#3/#4/#8, small scope, can land any time. Two-review convergence remains enough for a clear keep candidate.
5. **Then #3 — fuse stage 1A + 1B.** Under the 30 GB host constraint, discard all-RAM buffer/sweep forms. Re-evaluate scratch-spool vs ID-bucketed emission after #4; the latter becomes more attractive once per-node rank queries are gone.
6. **Then #2 — stream stage 3 → stage 4.** Largest remaining payoff, but the biggest rewrite; wants #1 landed first and bounded backpressure discipline from the start. Subsumes #8 — if #2 is intended within the same release, consider skipping #8 and going directly to #2. If horizons are uncertain, take the #8 win first.
7. **Then #5, #6, #7, #11** as appetite allows. #5 is the natural continuation of #1's fused path; #6 subsumes #5 at whole-pipeline scope (and R5 #1 + R1 #2 both land here); #7 is the hardest and most speculative; #11 only matters if dense stage-2 coord slices survive.
8. **#10 separately, conditional.** Conservative refcounts-only BlobHeader metadata is now supported by three reviewers, but it still depends on codifying the `cat` output contract. Aggressive full-ref-list forms remain blocked on the 64 KiB header cap.

### Benchmark plan for #4 (stage-2 de-ranking)

1. Replace per-tuple `rank_if_set(id)` in stage 2 with `get(id)` + blob-local `next_rank`, seeded from `NodeBlobInfo.ref_rank_start`.
2. Add debug/validation assertions: tuple IDs within each decoded node blob are monotonic; the number of membership hits equals `blob.ref_count()`; and `next_rank` finishes at `blob.ref_rank_end`.
3. Add an explicit `drop_rank_index()` helper to `IdSetDense` and call it once pass B + `build_node_blob_mapping()` are complete. On the current architecture, stage 2 should carry only the bitmap, not the rank prefixes.
4. Gate with semantic Denmark parity first, then benchmark Europe with `brokkr add-locations-to-ways --dataset europe --index-type external --bench 3` against current `main`.
5. Key metrics: total wall, `EXTJOIN_STAGE2`, `s2_node_rank_ns` disappearance, peak RSS / anon, page-fault deltas, and whether `s2_coord_fill_ms` or downstream phases move unexpectedly.
6. **Keep if** Europe total wall improves clearly or stage 2 drops materially with no RSS increase. Planet confirmation matters here because the benefit should scale with tuple count much more strongly than on Europe.

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
- **ScratchDir for all temp files.** `scratch.file_path(name)` or `scratch.bucket_path(kind, idx)`. Lifetime-tied cleanup on drop. Applies to #3's node-ID scratch and #5's per-blob spill.
- **`#[hotpath::measure]` on functions > 1 ms wall** so they show in `--hotpath` profiles.
- **Worker count convention.** `available_parallelism() - 2 max 1 min 4`, often `.min(6)`. The `-2` reserves cores for the consumer + writer threads. Any tuning that changes this must justify why.
- **Counter naming.** `s<stage><phase>_<thing>_ms` / `_bytes` / `_calls`. Stage-scoped prefix keeps grep/history readable.
- **Prototype discipline.** Prefer full coherent branch rewrites with keep/revert benchmarking over env-var-gated probes. If a temporary fallback is unavoidable during rollout, keep it short-lived and delete it as soon as the decision is made. The old plan showed that narrow env-var probes created codebase pollution and often failed to answer the real structural question.
- **When deleting `rank_if_set()` via #4, assert the invariant.** Add debug/validation checks for monotonic node IDs and final `next_rank == ref_rank_end`; do not rely on comments alone for blob-local rank correctness.

---

## Historical probe record

See [`altw-external-optimization-plan.md`](altw-external-optimization-plan.md) — the stripped historical record of probes attempted before the structural re-plan. Useful when a proposal looks like an old probe: the UUIDs, measured outcomes, and reasons for shelving are recorded there so future work can distinguish between *the idea was wrong* and *the probe was too timid*.
