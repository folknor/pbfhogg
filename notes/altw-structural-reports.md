## Report 1

ALTW today behaves like a reorder pipeline, not a saturated engine: way blobs are exploded into rank-sharded `slot_pos` records in [src/commands/altw/stage1.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:340), regrouped by referenced-node rank and fanned back out into
slot buckets in [src/commands/altw/stage2.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:365), rebuilt into dense slot images and sliced back into blob payloads in [src/commands/altw/stage3.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:234), then reread again for way assembly in [src/commands/altw/stage4.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:376).
The code is disciplined, but it still pays real wall time to destroy and then reconstruct blob ownership.

### Ranked Opportunities

#### 1. Delete the stage3 -> finalize -> stage4 barrier and make payload handoff streaming.

Bottleneck: the current code finishes stage 3, then does a full finalize/copy pass in [src/commands/altw/coord_payloads.rs](/home/folk/Programs/pbfhogg/src/commands/altw/coord_payloads.rs:255), then opens a second reader and preads each payload again in [src/commands/altw/stage4.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:376).
In `mod.rs`, that is a hard serialized seam from lines [src/commands/altw/mod.rs](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:340) through [src/commands/altw/mod.rs](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:425).

Why the structure causes it: stage 3 workers own temporary payload fragments, not blob-order emission. So ALTW stops, reconstructs blob order, writes another artifact, then stage 4 re-reads it.

Stronger redesign: stage 3 workers should emit ready `blob_idx -> payload_bytes` items to a blob-order coordinator. That coordinator should merge straddlers, reorder by `blob_idx`, and either append directly to a final
blob-ordered payload stream that stage 4 can consume immediately, or feed stage 4 directly through a bounded queue. No worker tmp manifests. No finalize copy. No second payload pread.

Why high-payoff: this directly attacks the biggest remaining seam. On the current baseline in [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:37), Europe still spends 37.2s in stage 3, 17.8s in finalize, and 121.1s in stage 4;
planet spends 100.2s, 46.4s, and 231.6s. Turning that from serial to overlapped is one of the few remaining double-digit wall opportunities.

Risks: you need real backpressure and bounded reorder state. If stage 4 or the writer is the true limiter, this can just move waiting around.

#### 2. Re-key the downstream half around way blobs, not global slot buckets.

Bottleneck: stage 1 emits global `slot_pos` records, stage 2 routes every resolved coordinate into shared slot buckets, stage 3 rebuilds dense bucket-local slot images and then classifies blob/bucket intersections plus
straddlers in [src/commands/altw/stage3.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:292) and [src/commands/altw/stage3.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage3.rs:386).

Why the structure causes it: blob ownership is thrown away in [src/commands/altw/stage1.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:451) and only reconstructed later. The whole `slot_bucket_count` and 2-piece straddler machinery in [src/commands/altw/mod.rs](/home/folk/Programs/pbfhogg/src/commands/altw/mod.rs:238)
exists to survive that choice.

Stronger redesign: change the downstream key from `slot_pos` to `way_blob_idx + blob_local_slot` or a blob-group-local equivalent. Partition contiguous way blobs into bounded blob groups, have stage 2 emit resolved records
to blob-group files, and have stage 3 scatter and encode directly within those blob-aligned groups. That deletes blob/bucket classification, straddler staging, and most of finalize by construction.

Why high-payoff: this is the cleanest way to stop rebuilding the same coordinate stream in three ownership domains. It also makes opportunity 1 much cleaner.

Risks: it is a real rewrite of stages 1-4. The fundamental rank-order vs blob-order mismatch still exists, so a bad blob-group design can preserve most of the scatter cost while adding new bookkeeping.

#### 3. Decode each kept node blob once and use that decode for both the join and final node output.

Bottleneck: stage 2 decodes node blobs to populate `coord_slice` in [src/commands/altw/stage2.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage2.rs:382), then stage 4 decodes the kept node blobs again on the non-way path in [src/commands/altw/stage4.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage4.rs:439). The note already
calls this out as a deferred structural issue [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:892).

Why the structure causes it: stage 2 is rank-bucket-owned work; stage 4 is file-order output work. Node blobs are treated as stage-local inputs instead of source-owned work units.

Stronger redesign: move to a node-blob-owned executor or node-stripe executor that decodes a node blob once, fans its tuples into the way-join path, and also emits the filtered node output side directly. That probably
means rewriting the stage-2 scheduler, not patching it.

Why high-payoff: this attacks duplicated input decode on the largest planet-side phase and can remove one more full stage-local ownership handoff.

Risks: hardest item here. It is easy to trade duplicate decode for worse buffering or a weaker stage-2 join.

### Medium-Value Local Changes

- If you want a smaller first cut before item 2, do item 1 on the current slot-bucket representation. I think that is a legitimate keep/revert candidate.
- A single-ingest way-ref spool that removes stage 1 pass B’s second PBF decode in [src/commands/altw/stage1.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:327) and [src/commands/altw/stage1.rs](/home/folk/Programs/pbfhogg/src/commands/altw/stage1.rs:421) is plausible, but I would not treat it as a first-order wall lever
  unless it comes bundled with the downstream key rewrite.

### Probably Not Worth Pursuing

- More rank-bucket-count experiments. The repo note already shows the failure mode was structural: more opens, more straddlers, worse stage 2+3+finalize [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:147).
- Another stage-1B shard-shape experiment. The grouped and batched variants already regressed or were flat [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:126) and [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:602).
- Another stage-2 hot-loop micro pass. The measured batch already reshuffled counters without moving wall.
- Stage-4 non-way wire filtering as the main bet. It was a real CPU win and still the wrong wall lever on Europe [notes/altw-external-optimization-plan.md](/home/folk/Programs/pbfhogg/notes/altw-external-optimization-plan.md:498).
- Generic writer work as the primary ALTW answer. Relevant, but not ALTW-local.

### Recommendation

Top 3 opportunities: 1) streaming stage3 -> stage4 handoff, 2) blob-group downstream rewrite, 3) single-decode node path.

I would do 1) first. It is the clearest test of your stated thesis: whether ALTW is still losing end-to-end throughput at stage seams. It is also a full change, not a probe.

Keep/revert plan for 1):

1. Implement the full coordinator path on a branch with no env-var default path.
2. Denmark: correctness only. Use semantic parity, not MD5-only parity.
3. Europe: this is the real gate, not Japan. Run `--bench 3` against current main and the branch.
4. Keep metrics: total wall, old downstream slice equivalence, `s4_send_ms`, eliminated `s3_integrated_finalize_*`, eliminated `s4_coord_payload_pread_ms`, new payload reorder-depth/high-water, scratch bytes, peak RSS.
5. Keep only if Europe total wall improves clearly, or the old `stage3 + finalize + stage4` slice drops materially with no RSS/scratch blow-up. For this size of rewrite, I would want roughly `>=5%` Europe wall or a very
   obvious downstream-phase win.
6. If Europe wins, run one planet confirmation. If Europe is flat or worse, revert cleanly.

## Report 2

### Ranked Opportunities

#### #1: Fuse stages 2+3 by making epoch-spill the default path (HIGH CONVICTION)

Bottleneck. The slot-bucket intermediate between stages 2 and 3 is the single largest disk materialization in the pipeline. Stage 2 writes ~200 GB of `(slot_pos, lat, lon)` entries to 256 files. Stage 3 reads them back, scatters them into dense buffers, then encodes `coord_payloads`. This write-read cycle exists solely because rank order ≠ slot order — stage 2 produces entries in rank order, stage 3 needs them in slot order.

Why the current structure causes it. Stages 2 and 3 are separate `thread::scope` blocks with a filesystem intermediate. Stage 2 workers write `ResolvedEntry` records to shared `SlotBuckets` via per-bucket `Mutex<BufWriter>`. Stage 3 workers re-read those files, scatter into a dense `Vec<u8>`, then encode. The structural seam is the filesystem round-trip.

The stronger design. The epoch-spill path (`stage23_epoch.rs`) already implements the right idea: during stage 2 processing, entries destined for the "active epoch's" slot buckets scatter directly into in-memory `Mutex<Box<[u8]>>` buffers. After each epoch's producer pass, a separate emit pass encodes `coord_payloads` from those buffers. Entries for other epochs spill to disk.

The change: make epoch-spill the default, auto-tune epoch count based on available memory, eliminate the disk-backed `SlotBuckets` path entirely.

Concretely:

- Remove `parse_epoch_env()` and the env-var gate
- Compute epoch count as `max(1, total_slots * 8 / target_memory)` where `target_memory` is ~40-50% of available RAM (use `sysinfo` or `/proc/meminfo`)
- At `E=1` (small datasets where `total_slots * 8` fits in RAM): zero slot-bucket disk I/O — the entire stage 2→3 handoff is in-memory
- At `E=4` (planet, ~30 GB RAM): 25% of entries handled in-memory, 75% spill. Spill I/O: ~112 GB vs current 150 GB (net saving ~38 GB disk I/O)
- Eliminate the separate stage 3 phase — the emit loop runs immediately after each epoch's scatter
- For Europe (~40 GB scatter): `E=2` would give ~20 GB in memory, comfortably fitting 30 GB RAM with 5.9 GB peak anon from other state

What makes this plausibly high-payoff.

- Europe/planet crossover: at Europe scale with `E=1` or `E=2`, the entire slot-bucket intermediate vanishes. Current stage 3 is 42.5s (Europe) — most of that goes away.
- Planet: I/O reduction is ~38 GB. At ~2 GB/s NVMe, that's ~19s. Plus the elimination of stage-3 file-open/read/close overhead.
- The finalize step (68s planet) should see reduced overhead because it's no longer a separate phase boundary — it's the tail of the last epoch's emit.
- Conservative estimate: 30–60s planet, 20–40s Europe.

Risks.

- Memory pressure: if epoch count is set too low, scatter buffers + `IdSetDense` + other stage-2 state may exceed physical memory. Need conservative auto-tuning.
- The epoch path has had limited production testing (env-var gated prototype). Would need full `brokkr verify` on Denmark + Europe.
- Spill file I/O for epochs > 0 has worse spatial locality than the current slot-bucket approach (entries arrive interleaved across epochs).

Implementation scope. Moderate. The epoch code exists in `stage23_epoch.rs`. Main work: remove the env-var gate, add auto-tuning, run it through the full benchmark/verify cycle. This is a "make the full change, benchmark, keep/revert" candidate.

---

#### #2: Overlap stage 3 coord_payloads emission with stage 4 assembly (HIGH PAYOFF, HIGH COMPLEXITY)

Bottleneck. Stages 2+3+finalize (388s planet) and stage 4 (259s planet) run sequentially. Stage 4 doesn't touch any shared state with stage 3 — it only consumes the `coord_payloads` file (read-only) and re-reads the input PBF (read-only). These two workloads are naturally independent once a blob's payload is available.

Why the current structure causes it. Stage 4 opens the `coord_payloads` file after finalize completes and uses `CoordPayloadsReader::pread_blob_payload()` indexed by the file's header. The header requires all blob offsets to be known upfront, which requires all payloads to be written first. This forces full serialization.

The stronger design. Replace the `coord_payloads` file with a streaming handoff:

1. Stage 3 workers produce encoded payloads per way blob. For `FullyContained` blobs, the payload is immediately ready. For straddlers, both halves are needed.
2. A shared `Vec<OnceCell<Vec<u8>>>` (or `Vec<Mutex<Option<Vec<u8>>>>`) indexed by `blob_idx` serves as the handoff buffer. Stage 3 deposits payloads as they become available.
3. Stage 4 starts concurrently with stage 3. Its way-blob workers check the handoff buffer; if the payload isn't ready yet, they block (condvar or spin) until stage 3 deposits it.
4. For node/relation blobs, stage 4 proceeds immediately (no `coord_payloads` dependency).
5. The `coord_payloads` file is eliminated entirely. 55 GB write + 55 GB read = 110 GB of disk I/O removed at planet.

The key insight: blob payloads are produced in roughly increasing blob-index order (because `way_slot_starts` is monotonic and slot buckets are processed in order). So stage 4, which processes blobs in PBF order (also roughly blob-index order), would rarely block.

What makes this plausibly high-payoff.

- If stage 3 and stage 4 fully overlapped: wall time ≈ `max(176, 259)` ≈ `259s` instead of `176 + 259 = 435s`. That's ~176s saved at planet — a ~18% total improvement.
- In practice, partial overlap is realistic. Stage 4's way-blob processing starts after stage 3 has processed a few slot buckets (which contain the lowest-indexed way blobs). The overlap would be substantial.
- Conservative estimate: 100–150s at planet, 40–60s at Europe.

Risks.

- Architectural complexity is significant. Concurrent stage 3 + stage 4 sharing the `coord_payloads` data requires careful synchronization.
- Memory pressure: at planet, 55 GB of `coord_payloads` can't all be in memory. The handoff buffer would need to be bounded — once stage 4 consumes a blob's payload, that memory should be freed.
- Straddler completion ordering: a straddler blob's payload isn't ready until both halves arrive (from two different slot buckets). If the second half's bucket is processed late, stage 4 could block for a long time on that blob.
- The write-path (`PbfWriter`) for stage 4 uses rayon for compression. Concurrent stage 3 workers + stage 4 workers + rayon compression threads = high thread contention.

Implementation scope. Large. This is a full architectural rewrite of the stage 3→4 boundary. Not a "try and revert in a day" change. Would need careful design, phased rollout, and extensive benchmarking.

---

#### #3: Single-pass stage 1 via deferred ranking (MEDIUM CONVICTION)

Bottleneck. Stage 1 reads all way blobs twice. Pass A decompresses and scans every way blob to build the `IdSetDense`. Pass B decompresses and scans the same blobs again to compute ranks and emit rank-bucketed records. The decompression + protobuf parse is duplicated across ~57K way blobs at planet.

Why the current structure causes it. Pass B depends on the rank index, which is built after pass A completes. The `IdSetDense::build_rank_index()` call at line 247 of `stage1.rs` happens after all pass A workers join. Only then can `rank()` be called. So pass B can't start until pass A is done and the rank index is built.

The stronger design. During pass A, each worker writes per-blob node-ID lists to a temporary file alongside its normal work (populating the local `IdSetDense` and writing sidecars). After pass A, build the rank index. Then pass B re-reads the node-ID files (flat `i64` arrays) instead of re-decompressing the PBF.

Reading flat `i64` arrays is dramatically cheaper than PBF `pread` + zlib decompress + protobuf parse:

- PBF: `pread` (kernel copy) → zlib decompress (CPU-bound) → protobuf decode (branch-heavy)
- Flat file: `pread` (kernel copy) → directly usable

At planet scale, pass B's `s1b_decompress_ms + s1b_scan_ms` represent substantial CPU time. The trade is writing ~37.5 GB of flat node-ID files during pass A (minimal overhead since pass A already has the IDs in `blob_node_ids`), then reading them in pass B instead of re-decompressing.

What makes this plausibly high-payoff.

- Eliminates all decompression and protobuf work in pass B. At planet: `s1b_decompress_ms + s1b_scan_ms` cumulative across workers is significant (extrapolating from Europe: ~45s cumulative, ~8s wall)
- The PBF data may or may not be in page cache by pass B. On 30 GB RAM with 87 GB input PBF, pass A evicts its own data. Re-reading from PBF in pass B triggers real NVMe reads. Flat node-ID files, being smaller and sequentially written, would have better cache residency.
- Conservative estimate: 15–30s at planet.

Risks.

- Adds ~37.5 GB of temporary disk writes during pass A. On NVMe this is ~18s. If pass B's PBF data is still cached, the net effect could be negative.
- Memory accounting: pass A workers would need slightly more memory per blob for the write buffer.
- Correctness: the order of node IDs in the flat file must exactly match the scan order for pass B's `slot_pos` computation.

Implementation scope. Moderate. Localized to `stage1.rs`. The pass A worker loop already has `blob_node_ids`; just add a `BufWriter` per worker that dumps them. Pass B's `pread+decompress+scan` loop gets replaced with a flat-file read.

---

### Things probably NOT worth pursuing

- Changing the number of rank buckets (256). The counting sort is O(n) regardless. Fewer buckets = larger per-bucket memory but fewer node-blob re-decompressions. The current 256 is well-tuned for the planet-scale memory envelope.
- Stage 4 coord-access micro-optimizations. The optimization history exhaustively demonstrates that per-blob `pread`, `madvise` tuning, `mmap` variants, and inner-loop work are all at the NVMe floor. The ~259s is dominated by input PBF read + output PBF write + rayon compression. `coord_payloads` reads are a small fraction.
- Compressing rank records further. The 12-byte rank records (down from 16) were already optimized. Delta-encoding or varint would add CPU cost for marginal I/O savings, in a stage that's not I/O-bound.
- Generic writer refactoring. The wire-format way reframe in stage 4 is already the right approach. Abstracting it further wouldn't improve ALTW throughput.
- `io_uring` for scattered writes. Stage 3's write pattern is large sequential writes (one per bucket). `io_uring` helps most with many small concurrent I/O operations. Not applicable here.

---

### Recommendation

Top 3:

1. Epoch-spill as default (fuse stages 2+3)
2. Pipeline stage 3 + stage 4 (overlap `coord_payloads` emission with assembly)
3. Single-pass stage 1 via deferred ranking

Do first: #1, epoch-spill as default. Reasons:

- The code already exists in `stage23_epoch.rs` — it's a prototype that needs hardening, not a greenfield implementation
- It's the lowest-risk architectural change with real payoff
- It creates the foundation for #2: once stage 3 is fused into stage 2, the stage boundary that blocks #2 is softer
- It's a clean keep/revert candidate

Benchmark plan for #1:

1. Make the change: remove `parse_epoch_env()`, auto-compute epoch count from `/proc/meminfo`, delete the disk-backed `SlotBuckets` path and all code guarded by `epoch_count_opt.is_none()`
2. Verify correctness: `brokkr verify add-locations-to-ways --dataset denmark` must produce identical output
3. Benchmark Europe: `brokkr add-locations-to-ways --dataset europe --index-type external --bench`
4. Compare against the most recent baseline (`768d3d4e` or equivalent) using `brokkr results --compare`
5. If Europe shows `≥10s` wall improvement with no correctness regression: keep
6. If flat or regression: check whether auto-tuned epoch count is suboptimal, try manual `E=2` for Europe as a diagnostic
7. If structurally broken: revert, file the diagnostic data

## Report 3

### How it works today

The pipeline is a 4-stage serial chain with two large disk-materialized intermediates:

Stage 1  (way scan, 2 sub-passes)

1A: decompress all way blobs → build `IdSetDense`, write ref-count sidecars
1B: re-decompress all way blobs → emit rank-bucketed `(local_rank, slot_pos)` records
→ ~80 GB rank shard files (256 × W per-worker files)

Stage 2  (node join)

Read rank shards → counting-sort per rank bucket → `pread+decompress` node blobs
→ resolve `(slot_pos, lat, lon)` → write to shared slot bucket files
→ ~112 GB slot bucket files

Stage 3  (slot reorder)

Read slot bucket files → scatter into dense buffer → classify blobs
→ encode per-blob delta-varint `coord_payloads` → finalize
→ ~55 GB `coord_payloads` file

Stage 4  (assembly)

Re-read full PBF → decompress way blobs → wire-format reframe with `coord_payloads`
→ passthrough node/relation blobs → write enriched PBF

Total scratch I/O at planet scale: ~80 GB (rank shards) + ~112 GB (slot buckets) + ~55 GB (`coord_payloads`) = ~247 GB written, ~247 GB read back. Plus ~3 PBF reads of the ~80 GB input (stage 1 reads ways twice, stage 2
reads nodes, stage 4 reads everything).

Stages are fully serialized — no overlap between 1→2, 2→3, or 3→4. The machine goes idle at every stage boundary while setup/teardown runs.

---

### Opportunity #1: Promote epoch-spill to default, delete the disk path

Bottleneck: The stage 2 → stage 3 handoff materializes 112 GB of slot bucket files. Stage 3 reads them cold from disk, scatters into memory, encodes, and writes `coord_payloads`. This is the single largest intermediate
materialization in the pipeline.

Why the current structure causes it: Stage 2 produces resolved entries in rank-bucket order (node-ID order). Stage 3 needs them in slot-pos order (way-PBF order). The slot bucket files serve as the external radix step for
this permutation. But 100% of entries go through disk — even entries that could be processed immediately.

What the stronger design looks like: The epoch-spill path (`stage23_epoch.rs`) already subsumes stages 2+3 into a fused pass. Epoch 0 resolves entries and scatters them directly into in-memory `scatter_buf`s (zero disk).
Entries for epochs >0 spill to disk and drain later. At `E=4`, ~25% of entries never touch disk at all, and the epoch-0 emit runs while scatter data is cache-hot.

The change is: remove the env-var gate, make the epoch path the only path, delete `SlotBuckets`/`SharedSlotBuckets` and the disk-path branch in `mod.rs`.

What makes it high-payoff:

- Eliminates 112 GB slot bucket write + 112 GB slot bucket read
- Epoch-0 entries (25% at `E=4`) are never serialized — pure memory scatter
- Emit runs while scatter data is L3-hot instead of cold from disk
- Spill is still 84 GB (75% of 112 GB), but it's write-once/read-once sequential — same I/O pattern, 25% less volume
- The epoch-0 path eliminates the most-contended slot bucket mutexes (epoch-0 scatters per-bucket, not cross-bucket)

Risks:

- Peak memory: ~6.8 GB at `E=4` (epoch-0 `scatter_buf`s) vs <1 GB for disk path. Acceptable on any machine that can run ALTW planet (which requires ~8.7 GB for the `IdSetDense`). Could default `E=4` and let users tune.
- Epoch path has had limited production testing (env-var gated prototype). Would need full `brokkr verify` on Denmark + Europe.

Conviction: High. The code exists. It's a delete + promote, not a write.

---

### Opportunity #2: Fuse stage 1A + 1B into a single way decompression pass

Bottleneck: Way blobs are decompressed and scanned twice. 1A decompresses to build `IdSetDense`. 1B decompresses the same blobs again to emit rank records (because rank computation requires the complete `IdSetDense` from
1A). At planet scale, way blobs are ~37 GB compressed. Zlib decompression is pure CPU — this is ~50% of stage 1 wall time, executed twice.

Why the current structure causes it: The dependency is real: 1B needs the rank index, which needs all of 1A to complete. But the decompression is the same in both passes — only the callback differs (1A: insert node IDs
into `IdSetDense`, 1B: look up rank and emit records).

What the stronger design looks like: During 1A, after `scan_way_refs`, capture each blob's extracted node ref list compactly into a per-blob scratch record (delta-varint encoded refs — much smaller than the full protobuf
blob). Store these in a single scratch file. When 1A completes and the rank index is built, 1B reads from the compact scratch file instead of re-decompressing the PBF.

At planet: ~10B node refs. Delta-varint encoded per blob (node IDs within a way blob are correlated), estimate ~15-20 GB scratch. Net effect: trade 37 GB PBF re-read + full zlib decompression + protobuf parse for 20 GB
sequential scratch read + simple varint decode.

Alternative (even more aggressive): during 1A, accumulate per-blob `(node_id, intra_blob_position)` pairs in a memory-mapped scratch structure and stream 1B's output as a continuation of 1A. But the sequential-scratch
approach is simpler and still a large win.

What makes it high-payoff:

- Eliminates one full zlib decompression of all way blobs (CPU-bound, not cacheable)
- On NVMe, the scratch file I/O is nearly free compared to decompression CPU
- Stage 1 is currently 2× the serial work it needs to be for its actual output
- Wall time reduction is approximately `(1B_decompress_ms / num_workers)`, likely 20-30% of stage 1

Risks:

- Adds ~20 GB to scratch disk budget (currently ~247 GB, so marginal)
- The varint encode/decode adds some CPU — but far less than zlib decompress
- Design requires choosing a compact per-blob format and validating bit-exactness

Conviction: High. The decompress duplication is measured, not speculative.

---

### Opportunity #3: Direct-to-coord_payloads encoding in the fused epoch path (skip scatter_buf intermediation)

Bottleneck: Even in the epoch-spill path, each epoch does: scatter resolved entries into a dense `scatter_buf` (per-bucket, slot-pos indexed) → classify blobs → slice `scatter_buf` per blob → delta-varint encode → write to
worker tmp → finalize copies to `coord_payloads`. The scatter step touches every byte of the epoch's bucket range even when many slots are empty (zeroed = missing coord). Then encoding re-reads the same bytes.

Why the current structure causes it: The `scatter_buf` exists because `coord_payloads` encoding needs per-blob coords in slot-pos order. The dense buffer provides O(1) random access by `slot_pos`. But it's write-once-read-once
with poor locality: stage 2 writes entries in rank-bucket order (scattered across the buffer), then encoding reads in blob order (sequential but different from write order).

What the stronger design looks like: Skip the `scatter_buf` entirely. During stage 2's resolve loop, instead of writing `ResolvedEntry` to a slot bucket or `scatter_buf`, write `(blob_local_offset, lat, lon)` directly to a
per-blob accumulator. Each resolved entry knows its `slot_pos`, from which we derive `blob_idx` (binary search in `way_slot_starts`) and the offset within that blob (`slot_pos - way_slot_starts[blob_idx]`).

Per-blob accumulators could be small `Vec<(u16_local_offset, i32_lat, i32_lon)>` — 10 bytes per entry instead of 8 bytes in the dense `scatter_buf`, but only for non-zero entries (skip missing coords entirely). At planet scale
with ~10B resolved entries, that's ~100 GB total... same ballpark. But the crucial difference: no dense buffer allocation, no zero-fill of empty slots, and no second read pass. Entries arrive and are routed directly to
where they'll be consumed.

For the epoch variant: epoch 0 routes directly to per-blob accumulators in memory. Epochs >0 spill to per-blob (or per-blob-group) files. The encode step reads per-blob accumulators, sorts by local offset (they arrive out
of order from different rank buckets), and delta-varint encodes in one shot.

What makes it high-payoff:

- Eliminates `scatter_buf` allocation + zero-fill (6.8 GB memset at `E=4`)
- Eliminates the `scatter_buf` write → read round-trip (different access patterns = poor cache)
- Eliminates the `classify_blobs_in_bucket + emit_integrated_intersections` machinery (slot-bucket boundaries become irrelevant; blobs ARE the natural unit)
- Per-blob accumulators are the right granularity for the final output format

Risks:

- Requires a sort per blob (entries arrive from different rank buckets in rank order, need slot-pos order). At ~175 entries/blob average, this is trivial.
- Binary search per resolved entry (`slot_pos → blob_idx`) adds CPU. But with ~57K blobs, binary search is 16 comparisons — cheap vs. the current dense-buffer random scatter.
- Significant rewrite of stage 2 resolve loop + stage 3 replacement. This is a "full coherent rewrite" of the fused path.

Conviction: Medium-high. The `scatter_buf` intermediation is real overhead, but quantifying the cache miss cost requires measurement. The rewrite is substantial.

---

### Things probably not worth pursuing

- Reducing node blob straddler re-reads in stage 2. At planet scale with 256 rank buckets and ~400K node blobs, ~255 straddler re-decompressions total. Cost: ~100 MB of extra decompress. Negligible.
- Changing the `PbfWriter` architecture for ALTW. The writer's rayon-based compression pipeline is already parallel and well-tuned. Stage 4's consumer thread is not the bottleneck (passthrough blobs skip compression entirely;
  way reframe is fast). Writer changes would be generic improvements, not ALTW-specific wins.
- Reducing stage 4 `coord_payloads` preads. ~57K preads is a lot of syscalls, but each is sequential (the consumer reads blobs in order) and the OS readahead handles this well. Could batch with a single `mmap`, but the win is
  small.
- Attempting to overlap stages 1 and 4 (pipe decompressed way blobs from stage 1 through to stage 4). This would require running stages 2/3 concurrently with way blob transit — a fundamentally different pipeline
  architecture. The win is one fewer PBF read of way blobs, but the complexity is enormous. Not justified pre-1.0.

---

### Recommendation

Do #1 first. Promote epoch-spill to default, benchmark, keep/revert.

Benchmark plan:

1. Remove the `parse_epoch_env()` gate. Hardcode `num_epochs = 4` (or make it a computed default based on available memory).
2. Delete the disk-path branch in `mod.rs` (the `else` block with `SlotBuckets::create → stage2_node_join → stage3_slot_reorder`).
3. Delete `SlotBuckets`/`SharedSlotBuckets` from `stage2.rs` and the `stage2_node_join` function. Keep `prepare_bucket` and `LoaderScratch` (shared with epoch path).
4. `brokkr add-locations-to-ways --dataset europe --index-type external --bench` — compare against the most recent disk-path result. Key metrics: wall time, peak RSS, scratch disk usage, per-stage marker durations.
5. `brokkr verify add-locations-to-ways --dataset denmark` — correctness gate.
6. If wall time improves or is neutral and peak RSS stays under ~10 GB: keep. If wall time regresses >5%: investigate epoch count tuning before reverting.

After #1 ships, do #2 (fuse 1A+1B). It's an independent change targeting a different stage, so it stacks cleanly. Then #3 if there's appetite for a deeper rewrite of the fused resolve/encode path.
