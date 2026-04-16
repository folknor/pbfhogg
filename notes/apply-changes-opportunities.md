# `apply-changes --locations-on-ways` — optimization plan

Target: `pbfhogg apply-changes --locations-on-ways` on planet with a daily OSC. Current: 12m42s (762 s) wall, 1.8 GB peak RSS. Production uses `--compression none` (no zlib encode in the output path).

## Thesis

Unlike ALTW, the geocode builder, and check-refs, apply-changes is **already mostly well-shaped**. There is no single structural mistake to point at. The merge pipeline is:

- single sequential pass over the base PBF
- parallel classify (rayon `par_iter`)
- pipelined writer with bounded channels
- `Arc<NodeLocationIndex>` to avoid per-batch location-index cloning
- per-rayon-task `PrimitiveBlock` drop after rewrite, for early memory release
- coalesced passthrough writes (consecutive raw frames flush as a single `write_raw_owned` move)
- raw-bytes pre-seeded string table path for base element rewrite (no re-parse, no re-intern)

The 12m42s is the real cost of rewriting 70–90 % of a planet's blobs with locations preserved, not an artefact of a wrong shape.

The wins are **two incremental parallelizations** of the remaining single-threaded stretches: `NodeLocationIndex::prefill_from_base`, and the reader thread. Plus one default-change that is moot in production but worth knowing exists.

No internal API rewrites. `IdSetDense` is not used here (location index is a sparse HashMap keyed by node ID, which is right for the sparse-lookup pattern). `PbfWriter` is used correctly. `parallel_classify_accumulate` and `pass1_parallel_scan` are the patterns to reuse.

Target after this plan: **~6–9 min at planet under `--compression none`**, RSS unchanged (~1.8–2.2 GB).

## Yardstick

| Command | Wall | Peak RSS | Notes |
|---|---:|---:|---|
| `apply-changes --locations-on-ways` (current, `--compression none`) | 12m42s | 1.8 GB | ~80 % blob rewrite ratio on daily OSC |

Inferred per-phase breakdown for a daily planet OSC under `--compression none`:

| Phase | Est. wall | Parallelized? | Dominant cost |
|---|---:|---|---|
| OSC parse + `DiffRanges` build | ~10–30 s | no | small input |
| `NodeLocationIndex::prefill_from_base` | **~30–100 s** | **no (sequential)** | single-threaded node-blob decompress |
| Reader thread (sequential pread of 87 GB) | **~50–150 s** | **no (single thread)** | `FileReader` + blob-header parse |
| Phase 1 classify (~80 % blobs decompress) | ~100–250 s | yes (rayon) | zlib decompress |
| Phase 3 rewrite (~80 % blobs re-encode) | ~50–150 s | yes (rayon) | `BlockBuilder` + tag work |
| Phase 4 output (no compression) | ~30–60 s | yes (writer thread) | NVMe write throughput |
| Writer flush | ~5 s | — | trivial |
| **Sum** | ~275–745 s | | 762 s lands at the high end |

Two single-threaded stretches (~80–250 s combined) and input decompression (~100–250 s) are the main targets.

## Current architecture (reference)

Entry: `merge()` at [rewrite.rs:702](../src/commands/merge/rewrite.rs#L702). The public command name is `apply-changes`; the internal module is called `merge`.

**Setup phase**:

1. Parse OSC → `CompactDiffOverlay` ([rewrite.rs:719](../src/commands/merge/rewrite.rs#L719)).
2. Build `DiffRanges` — sorted upsert + delete ID vecs per type ([rewrite.rs:746](../src/commands/merge/rewrite.rs#L746)).
3. If `--locations-on-ways`: build `NodeLocationIndex` ([rewrite.rs:756](../src/commands/merge/rewrite.rs#L756)):
   - `NodeLocationIndex::build_from_diff` collects all node IDs referenced by OSC ways, seeds coords from OSC nodes, leaves the rest in `needed_set` ([node_locations.rs:31](../src/commands/merge/node_locations.rs#L31)).
   - `NodeLocationIndex::prefill_from_base` walks the base PBF sequentially, decompressing node blobs whose ID range overlaps needed IDs, extracting tuples, filling `locations` ([node_locations.rs:94](../src/commands/merge/node_locations.rs#L94)).
4. Read header, create pipelined writer.
5. Spawn reader thread: single-threaded sequential scan via `FileReader`, producing `RawBlobFrame`s on a 128-deep `sync_channel` ([rewrite.rs:297](../src/commands/merge/rewrite.rs#L297)).

**Main batch loop** ([rewrite.rs:814](../src/commands/merge/rewrite.rs#L814)):

For each byte-budgeted batch of raw frames (from `collect_batch`):

- **Phase 1 (parallel classify)**: `classify_only` per frame via rayon. Returns `Passthrough`, `FalsePositive`, or `NeedsRewrite(PrimitiveBlock, BlobIndex)` ([rewrite.rs:823](../src/commands/merge/rewrite.rs#L823)).
- **Phase 2 (sequential inline assignment)**: for each `NeedsRewrite` slot, binary-search the sorted upsert vec for IDs landing in the blob's OSM range. O(log n) per blob ([rewrite.rs:840](../src/commands/merge/rewrite.rs#L840)).
- **Phase 3 (parallel rewrite)**: `rayon::spawn` per `RewriteJob`, each emitting to an `mpsc::sync_channel` sized to `num_threads.min(rewrite_count)` ([rewrite.rs:882](../src/commands/merge/rewrite.rs#L882)). Jobs own their `PrimitiveBlock` and drop it after completion.
- **Phase 4 (streaming output)**: main thread processes slots in file order; passthrough slots flow into a coalescing buffer (`write_raw_owned`); rewrite slots block waiting for their job's result ([rewrite.rs:917](../src/commands/merge/rewrite.rs#L917)).

**Teardown**: flush remaining upserts per type, writer flush.

## Opportunities, ranked

### #1 — (Moot in production, listed for completeness) Default to zlib:1 like renumber does

[renumber_external.rs:118–126](../src/commands/renumber_external.rs#L118) overrides the compression default when the caller didn't specify:

```rust
let effective_compression = if compression == Compression::default() {
    Compression::Zlib(1)
} else {
    compression
};
```

Renumber's in-place rationale: zlib:6 adds ~22 s of backpressure at planet for ~15 % smaller output, which is a bad trade for transient outputs in a pipeline that rewrites again downstream.

**Production impact: zero.** Production runs with `--compression none`. The compression phase is already fast.

**Off-production impact**: any non-production caller that invokes `apply-changes` without a `--compression` flag currently pays zlib:6 unnecessarily. A zlib:1 default would cut that path's wall by ~150–300 s at planet.

**Keep on the list but de-prioritize.** Not on the critical path for the production pipeline. Land it opportunistically if touching the command for any other reason.

### #2 — Parallelize `NodeLocationIndex::prefill_from_base`

[node_locations.rs:112–144](../src/commands/merge/node_locations.rs#L112) is a straight sequential loop over node blobs:

```rust
for blob_result in &mut reader {
    // overlap check via needed_sorted binary search
    blob.decompress_into(&mut buf)?;
    extract_node_tuples(&buf, &mut tuples, &mut group_starts);
    for t in &tuples {
        if self.needed_set.remove(&t.id) {
            self.locations.insert(t.id, (t.lat, t.lon));
            nodes_found += 1;
        }
    }
    if self.all_found() { break; }
}
```

`overlaps_needed` ([node_locations.rs:73](../src/commands/merge/node_locations.rs#L73)) is effective at skipping blobs that contain zero needed IDs. But every overlapping blob is decompressed on the main thread, serially, before the main pipeline even starts. For a daily diff touching ~10 M referenced nodes spread across the node ID space, probably 30–50 % of node blobs overlap, giving ~20–30 GB of compressed node data to decompress. At ~500 MB/s single-threaded, 40–60 s. On 6 cores: 10–15 s.

The shape matches [`parallel_classify_accumulate`](../src/commands/mod.rs#L571) exactly — it's the same pattern the geocode builder uses in Pass 1.5 for a dense-decode accumulator ([geocode_index/builder.rs:498](../src/geocode_index/builder.rs#L498)). Reuse it:

- Build a node-only schedule via [`build_classify_schedule`](../src/commands/mod.rs#L429) with `kind_filter = Some(ElemKind::Node)`. Apply the `overlaps_needed` filter at schedule-construction time (header-only blob walk, cheap). The filtered schedule contains only blobs worth decompressing.
- `parallel_classify_accumulate` with per-worker state `S = FxHashMap<i64, (i32, i32)>`. Workers do `pread → decompress → extract_node_tuples → if needed_set.contains(id) { local.insert(id, (lat, lon)) }`.
- Merge: drain each per-worker map into `self.locations`. HashMap insert is last-write-wins; all coords for a given ID are identical, so the merge is straightforward.

**Two nuances**:

- The current code uses `needed_set.remove(&t.id)` to (a) avoid double-insertion and (b) support early-exit via `all_found()`. In parallel land, workers read `needed_set` (shared immutable after build; swap `remove` for `contains`) and insert unconditionally. Early-exit is less useful once blobs are all in flight; drop the `all_found()` check or gate it on an atomic counter polled every N tuples.
- Per-worker map size at peak: ~2–5 MB for a daily diff (10M / 6 workers × ~50 bytes/entry). Merge is a single linear drain. No backpressure.

**Expected win**: ~30–60 s at planet.

**Risk**: low. Pattern is already used in the codebase. Correctness is straightforward (merge is commutative + idempotent for sparse location lookups).

### #3 — Replace the sequential reader thread with parallel pread schedule

[rewrite.rs:297–327](../src/commands/merge/rewrite.rs#L297): `spawn_reader_thread` runs one thread that opens a `FileReader` and streams `RawBlobFrame`s through a 128-deep `sync_channel`. That thread is the only reader. The batch loop decouples reader from workers but does not parallelize the read itself.

At sequential BufReader + blob-header-parse overhead, realistic throughput is ~500 MB/s – 1 GB/s. 87 GB is 90–180 s. Parallel `pread` on NVMe reaches 3–5 GB/s, dropping to 17–30 s.

**Refactor**: replace the reader thread with the same work-stealing pread schedule pattern used in [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615), ALTW's `stage2d_worker`, and geocode's proposed Phase 2a/2b:

- Header-only schedule scan up front, producing `(seq, frame_offset, data_offset, data_size, blob_type, indexdata_hint, tagdata_hint)` tuples. One sequential BufReader pass over the whole PBF, skipping blob bodies — fast (~3–10 s at planet).
- Collapse "reader thread → frame channel → classify workers" into one stage: each worker preads + classifies in the same loop and emits `ClassifyResult` downstream.
- Retain the existing batch structure by having the consumer side pull `ClassifyResult`s in seq order (reorder buffer) rather than pulling raw frames.

**Two wrinkles**:

- **`copy_file_range` path** ([rewrite.rs:790–795](../src/commands/merge/rewrite.rs#L790)) needs `frame.file_offset`. That survives cleanly — the schedule entry has both `frame_offset` (for raw passthrough) and `data_offset` (for pread of the compressed body). Include both in the tuple.
- **Raw-frame ownership** for the zero-copy passthrough move (`write_raw_owned(std::mem::take(&mut frame.frame_bytes))` at [rewrite.rs:938](../src/commands/merge/rewrite.rs#L938)). Workers already own their pread buffer; move it out the same way. The concept of `RawBlobFrame` survives; the difference is *when* the frame bytes are read (worker pread) versus *who* read them (reader thread today).
- **Reader-thread backpressure semantics.** The current `sync_channel(128)` gives 128 blobs of read-ahead. Parallel pread gives `num_workers × per-worker-batch` blobs of concurrent in-flight reads, which is similar or slightly higher. Page cache pressure is the same (reading the same bytes). No new RSS concern.

**Expected win**: ~50–100 s at planet on NVMe. Smaller on spinning disk.

**Risk**: medium. Largest of the three changes. Touches the main loop structure, not just a helper. Preserve the reorder-buffer + batch-boundary logic carefully.

## Overall expected savings

Under `--compression none` at planet:

- #2 alone: ~30–60 s saved. New wall ~11 min.
- #2 + #3: ~80–160 s saved. New wall **~9–10 min**.
- #2 + #3 + #1 (if any caller hits the zlib:6 default): #1 doesn't help production. Off-production callers see an additional ~150–300 s on their paths.

Primary target: **~9–10 min at planet in production**, from 12m42s.

## What to leave alone

- **The classify phase.** Already rayon-parallel with correct fast-paths (indexdata-based passthrough without decompress at [classify.rs:139](../src/commands/merge/classify.rs#L139), range-overlap secondary check, precise block-overlap check). No restructure needed.
- **The rewrite hot path** (`rewrite_block_parallel`, `emit_create_local`, `write_base_*_local` family). Already uses `pre_seed_string_table` to avoid re-interning, `add_way_raw_bytes_with_locations` to forward raw fields 9/10 byte-for-byte, and `add_relation_raw_bytes` to skip re-parsing members. This path is tightly written.
- **The pipelined writer** (`PbfWriter` + rayon compression + 64 permits). Under `--compression none` the rayon tasks become near-passthrough, but the structure is still correct and sized.
- **The coalescing passthrough buffer** ([rewrite.rs:812](../src/commands/merge/rewrite.rs#L812)). Collapses consecutive raw-frame writes into single sends. Correct at 70–90 % rewrite ratio (small fraction of bytes, but the collapse still matters for channel send overhead).
- **`NodeLocationIndex.locations` as `FxHashMap<i64, (i32, i32)>`.** Lookups are only for OSC ways (few million at daily scale), not base-way refs. Base ways forward their existing fields 9/10 via `write_base_way_local_with_locations`. HashMap at ~240 MB for 10 M entries is the right shape for sparse lookup.
- **`DiffRanges` sorted vecs + `partition_point`.** Already the right shape for range-overlap and inline upsert assignment.
- **`CompactDiffOverlay` / OSC parse.** Single-threaded but small (100–500 MB input, ~1–5 s); not on the critical path.
- **`UpsertCursors` + gap-create / trailing-create logic.** Complex but correct; sequential constraints are fundamental to preserving OSM ID order across passthrough boundaries.
- **Per-rayon-task `PrimitiveBlock` drop** ([rewrite.rs:905](../src/commands/merge/rewrite.rs#L905)). Already frees memory eagerly. No change.
- **`#[cfg(feature = "hotpath")]` phase timers.** Existing measurement scaffolding. Flip them on for the first post-#2 / post-#3 measurement runs.

## Plan of attack

1. **Enable `#[cfg(feature = "hotpath")]` per-phase timers unconditionally** (or at least for the first measurement pass). The inferred breakdown in this doc is a model; measure to ground-truth it before committing to the order of #2 and #3.
2. **Land #2 first** — parallel `prefill_from_base` via `parallel_classify_accumulate`. Smaller refactor, lower risk. Cross-validate output byte-for-byte on Denmark and Europe. Re-measure planet to confirm ~30–60 s save.
3. **Land #3** — parallel pread schedule replacing the reader thread. Preserve `copy_file_range` semantics and raw-frame move-ownership. Cross-validate byte-for-byte. Re-measure.
4. **#1 (zlib:1 default)** is opportunistic — tack it on when touching the command again, or skip if production is the only caller.

Cross-validation: `brokkr verify apply-changes --dataset denmark` if it exists (check `.review.toml` / brokkr for available verify targets). Otherwise: identical output PBF byte-for-byte after the primary merge batch; tail creates that get out-of-order under the existing implementation would be the same out-of-order set. Element-level diff (decompress, compare per-blob element lists sorted by ID) is the fallback.

## Memory budget (planet, post-#2 + #3)

| Component | Size |
|---|---:|
| `CompactDiffOverlay` (daily OSC) | ~500 MB – 1 GB |
| `NodeLocationIndex.locations` | ~200–500 MB |
| `DiffRanges` sorted vecs | ~50–100 MB |
| Per-worker pread + decompress buffers × ~6 | ~200–400 MB |
| Per-worker prefill `FxHashMap` (transient, phase #2 only) | ~50–300 MB |
| Writer pipeline + reorder buffer | ~200–500 MB |
| **Total** | **~1.8–2.5 GB** |

Unchanged from current 1.8 GB, or slightly higher during phase #2 merge. Host budget: irrelevant under 30 GB ceiling.

**Sizing robustness note.** None of the structures above scale with `unique_referenced_nodes` the way the failed [altw-as-renumber](altw-as-renumber.md) `coord_table` did. `NodeLocationIndex` scales with the OSC's own node-ref set (daily-diff-sized, bounded), not with the base PBF's population. No structure here depends on an estimate of the planet-scale referenced-node count. That is why this plan's recommendations survive the 2026-04-16 ALTW reshape failure unchanged.

## Correctness invariants

- **OSM ID ordering.** The main batch loop emits passthrough blobs in file order, rewrite blobs in file order (via the reorder buffer on the rayon mpsc channel), and gap creates before their matching blob's `min_id`. Any parallelization of reader or prefill must preserve file-order output. Phase #3's refactor must keep the reorder buffer intact.
- **`LocationsOnWays` preservation on base ways.** `write_base_way_local_with_locations` forwards raw `lat_data()` + `lon_data()` verbatim. Do not touch this path. Under `--locations-on-ways`, every base way must produce fields 9/10 in the output; the existing logic does this by calling the `_with_locations` variant whenever `loc_map.is_some()`.
- **Zero-coord fallback for missing node refs in OSC ways.** [rewrite.rs:67–70](../src/commands/merge/rewrite.rs#L67): `match locs.get(&node_id) { Some(&loc) => ..., None => locations.push((0, 0)) }`. Preserved under parallel prefill — the merged locations map has the same entries the sequential version would produce.
- **Straight `needed_set.contains` replaces `remove` in parallel prefill.** `contains` is cheaper than `remove`, and parallel workers cannot safely mutate a shared `FxHashSet`. Merge-at-end dedup covers the uniqueness semantic (a node hit by multiple workers will just insert the same `(lat, lon)` twice; last write wins, both values are identical).
- **Early-exit via `all_found()`.** Currently lets the sequential pass stop once all needed IDs are resolved. Under parallel prefill, all workers will have already claimed blobs from the schedule by the time the last needed ID is found. Either drop the early-exit (workers complete their claimed blobs; filters at schedule-construction time have already pruned most non-overlapping blobs) or add an atomic "remaining-needed" counter polled every N tuples. Probably not worth the complexity — the `overlaps_needed` filter already prunes aggressively.
- **`copy_file_range` path** on passthrough blobs ([rewrite.rs:960–970](../src/commands/merge/rewrite.rs#L960)). Under #3 the file offset must still be correct in the replacement schedule. The existing `frame.file_offset` field corresponds to `frame_offset` in the header-only scan — preserve this.
- **Reader-thread graceful shutdown.** The current reader joins at [rewrite.rs:1063](../src/commands/merge/rewrite.rs#L1063). Under #3 there is no separate reader thread to join; the schedule is consumed by the workers themselves, and shutdown is when all workers exit their claim loop.

## Open questions

- **Actual current phase breakdown.** The numbers in this doc are inferred. First step (measurement) either confirms the ordering of #2 and #3 or flips it. If the reader thread is not actually I/O-bound at production NVMe speeds, #3's payoff shrinks.
- **Does `overlaps_needed` prune as aggressively under a daily diff as estimated?** The 30–50 % overlap estimate is heuristic. If the actual overlap ratio is 70–80 %, prefill is genuinely most of a minute of serial work and #2 matters more. If it's 10–20 %, the serial cost is already small and #2 is marginal.
- **Does `--compression none` leave phase 4 measurably free?** Under zstd or zlib, the writer pipeline's rayon compression tasks can dominate. Under `none`, they're near-passthrough. Worth confirming that phase 4 is not still a bottleneck under production settings (e.g. due to output file writes being synchronous to disk rather than to page cache).
- **Does the prefill pre-pass RSS behave under parallel decompress?** Sequential prefill reuses one decompress buffer; parallel prefill needs one per worker (~16–32 MB × 6 = ~100–200 MB transient). Fine under 30 GB, but document the per-worker overhead for completeness.
- **Interaction with `--io-uring`.** Current `spawn_reader_thread` uses `FileReader` (BufReader + File). Under #3's pread-schedule model, workers use `pread` directly; `--io-uring` would need to be plumbed into the worker's read path rather than the reader thread's. Check whether the existing io_uring integration is on the reader side or the writer side; if reader, #3 needs to preserve it.
