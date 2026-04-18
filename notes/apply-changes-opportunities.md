# `apply-changes --locations-on-ways` - optimization plan

Target: `pbfhogg apply-changes --locations-on-ways` on planet with a daily OSC. Current: 12m33s (753 s, zlib) / 8m52s (532 s, none) wall, 1.8 GB peak RSS (commit `7e9c2e9`, 2026-04-17). Production uses `--compression none` (no zlib encode in the output path).

## Thesis

Unlike ALTW, the geocode builder, and check-refs, apply-changes is **already mostly well-shaped**. There is no single structural mistake to point at. The merge pipeline is:

- single sequential pass over the base PBF
- parallel classify (rayon `par_iter`)
- pipelined writer with bounded channels
- `Arc<NodeLocationIndex>` to avoid per-batch location-index cloning
- per-rayon-task `PrimitiveBlock` drop after rewrite, for early memory release
- coalesced passthrough writes (consecutive raw frames flush as a single `write_raw_owned` move)
- raw-bytes pre-seeded string table path for base element rewrite (no re-parse, no re-intern)

The 12m33s zlib / 8m52s none is the real cost of rewriting 70-90 % of a planet's blobs with locations preserved, not an artefact of a wrong shape.

The wins are **two incremental parallelizations** of the remaining single-threaded stretches: `NodeLocationIndex::prefill_from_base`, and the reader thread. Plus one default-change that is moot in production but worth knowing exists.

No internal API rewrites. `IdSetDense` is not used here (location index is a sparse HashMap keyed by node ID, which is right for the sparse-lookup pattern). `PbfWriter` is used correctly. `parallel_classify_accumulate` and `pass1_parallel_scan` are the patterns to reuse.

Target after this plan: **~6-9 min at planet under `--compression none`**, RSS unchanged (~1.8-2.2 GB).

## Yardstick

| Command | Wall | Peak RSS | Notes |
|---|---:|---:|---|
| `apply-changes --locations-on-ways` (current, `--compression none`) | 8m52s | 1.8 GB | ~80 % blob rewrite ratio on daily OSC |
| `apply-changes --locations-on-ways` (current, `--compression zlib`) | 12m33s | 1.8 GB | as above with zlib encode (osmium-interop default) |

## Measured: Europe altw + `--locations-on-ways --compression none`

Commit `b4f45ff`, 2026-04-18, plantasjen. UUID `f0af4170`, `--bench 1`.
Europe altw (38 GB), OSC seq 4715 (planet-scale daily diff applied to a
regional extract - see the missing-node note below).
Total wall: **46.1 s**.

### Per-phase wall (from always-on sidecar counters)

| Phase | Wall (ms) | % of wall | Parallelised? | Counter |
|---|---:|---:|---|---|
| OSC parse | 1 860 | 4.0 % | no | `merge_osc_parse_ms` |
| `DiffRanges::from_diff` | 59 | 0.1 % | no | `merge_diffranges_ms` |
| `NodeLocationIndex::prefill_from_base` | **5 819** | **12.6 %** | no (sequential) | `merge_prefill_ms` |
| Header read + writer setup | ~0 | - | no | `merge_header_read_ms`, `merge_writer_setup_ms` |
| Phase 1 classify (cumulative) | **19 767** | **42.9 %** | yes (rayon) | `merge_classify_total_ms` |
| Phase 2 inline assignment (cumulative) | 54 | 0.1 % | no (sequential) | `merge_phase2_inline_total_ms` |
| Phase 3 rewrite spawn (cumulative) | 145 | 0.3 % | - | `merge_rewrite_spawn_total_ms` |
| Phase 4 rewrite recv + dispatch (cumulative) | **13 651** | **29.6 %** | main thread blocks on rayon | `merge_rewrite_recv_total_ms` |
| Phase 4 output write (cumulative) | 110 | 0.2 % | - | `merge_output_write_total_ms` |
| Passthrough write (cumulative) | 14 | 0.0 % | - | `merge_passthrough_write_total_ms` |
| Trailing creates | 1 | 0.0 % | no | `merge_trailing_creates_ms` |
| Final writer flush | 1 354 | 2.9 % | - | `merge_final_flush_ms` |
| **Sum of phases** | **~42 800** | **~93 %** | | |

Unaccounted ~3.3 s (~7 %) is thread startup/join, OSC file I/O before
the parse marker, and stderr printing. Not a target.

### Stall attribution (from WAIT_* spans + atomic accumulators)

| Stall | Total | % of wall | Meaning |
|---|---:|---:|---|
| `merge_rewrite_recv_wait_us` | 13.50 s | 29.3 % | main thread blocked on rayon rewrite results |
| `merge_reader_send_wait_us` | 1.47 s | 3.2 % | reader thread blocked on full frame channel |
| `merge_consumer_recv_wait_us` | 1.44 s | 3.1 % | `collect_batch` blocked on empty frame channel |
| `merge_writer_call_us` | 1.14 s | 2.5 % | time spent in `writer.write_*` calls |

Writer-internal stalls (from the writer's own counters):

| Counter | Total | Comment |
|---|---:|---|
| `writer_recv_wait_ns` | 19.6 s | reorder-buffer thread waiting for inputs - writer isn't the bottleneck |
| `writer_compress_ns` | 8.6 s | cumulative framing/encode (note: `none` compression still frames wire bytes) |
| `writer_write_ns` | 15.2 s | actual disk writes |
| `writer_flush_ns` | 1.35 s | matches `merge_final_flush_ms` |

### Shape counters

| Counter | Value |
|---|---:|
| `merge_batches_total` | 14 744 |
| `merge_reader_frames_sent` | 514 664 |
| `merge_reader_blocked_sends` | 2 533 (0.49 %) |
| `merge_blobs_passthrough` | 473 706 |
| `merge_blobs_rewritten` | 40 958 |
| `merge_blobs_index_hit` | 451 231 |
| `merge_bytes_passthrough` | 20.8 GB |
| `merge_bytes_rewritten` | 26.4 GB (56 % rewrite ratio) |

### Prefill shape

| Counter | Value |
|---|---:|
| `merge_loc_needed` | 3 576 070 |
| `merge_loc_from_diff` | 731 618 |
| `merge_loc_from_base` | 37 585 |
| `merge_loc_missing` | **2 806 867 (78 %)** |
| `merge_prefill_blobs_scanned` | 95 071 |
| `merge_prefill_blobs_skipped_range` | 361 872 (79 % skip rate) |
| `merge_prefill_early_exit` | 0 |

**Missing-node caveat.** Europe's OSC (seq 4715) is a full-planet daily
diff, not a regional one. Most referenced nodes are outside Europe's
extract bbox and won't be in the base PBF. The 78 % missing rate is
expected for this dataset combination, not a bug. It does mean prefill
is doing less useful work than it would on a planet run, where the
OSC and the base cover the same extent.

### What this changes vs the inferred breakdown

Plan-level intuition held for the order of magnitude (prefill slow,
reader single-threaded), but the proportions shifted:

- **Classify is the dominant phase, not prefill or rewrite-encode.**
  19.8 s / 43 % of wall. The plan's "leave alone" recommendation
  deserves revisiting. Candidate: reduce per-blob decompress cost
  (pipelined decompress reuses buffers), or skip more blobs via
  better blob-index fast paths.
- **Main thread spends 30 % of wall blocked on rayon rewrite
  results.** `merge_rewrite_recv_wait_us` and
  `merge_rewrite_recv_total_ms` both land at ~13.5 s. The plan framed
  rewrite-encode as a rayon-parallel phase to leave alone; actually
  its wall-clock contribution from the main thread's point of view is
  second only to classify. Faster rewrites, or more rewrite workers,
  are worth considering.
- **Prefill is 5.8 s on Europe, not the 30-100 s predicted for
  planet.** Scaling by file size (87/38 = 2.3×) predicts ~13 s on
  planet; scaling by node count (10.4 B / 3.7 B = 2.8×) predicts ~16 s.
  Plan #2 estimate was 30-60 s. Either the estimate was generous or
  the Europe OSC touches fewer nodes proportionally. Either way, plan
  #2's ceiling is probably ~10-20 s saved at planet, not 30-60 s.
- **Reader thread stalls only 3 % of wall.** The plan estimated
  reader wall at 50-150 s (planet). On Europe only 1.47 s stalled; if
  the whole reader walked 46 s at ~830 MB/s it would take ~45 s, but
  the reader runs concurrently with the consumer, so its wall doesn't
  add. Plan #3's payoff on `--compression none` looks modest; under
  zlib-output (where classify + rewrite + writer get slower) the
  reader still isn't the critical path.

### Revised ranked targets (post-measurement)

1. **Classify** (19.8 s @ Europe, 43 % wall) - newly elevated. Was "leave alone" in the inferred plan; measurement says it's the biggest bucket.
2. **Rewrite throughput** (13.65 s @ Europe, 30 % wall) - consumer blocked on rayon results. Also not originally on the plan's ranked list.
3. **Prefill** (5.8 s, 13 %) - plan #2 target; confirmed sub-optimal but smaller ceiling than estimated.
4. **Reader parallelisation** (1.47 s stall, 3 %) - plan #3 target; payoff small on `--compression none` at Europe scale.
5. **zlib:1 default** - plan #1. Moot in production (uses `none`). Leave for opportunistic pickup.

Items #1 and #2 weren't on the ranked opportunities list in the
inferred plan. Before committing to implementation order, the next
measurement should separate classify's work into its three fast
paths (indexdata hit / scan-only / precise decompress) to see whether
the 19.8 s is dominated by decompress of the 8 % of blobs that get
rewritten, or by the scan-only path across the 92 % that pass through.

## Hotpath: per-function cumulative CPU (Europe altw, 2026-04-18)

Two runs at commit `b4f45ff`, `--hotpath` mode. Totals are cumulative
wall across all calls; `% Total` is cumulative vs primary-thread wall,
so values >100 % mean the function saw parallel execution across
multiple cores.

### `--compression none` (UUID `e583036e`, 60.5 s wall)

| Function | Calls | Avg | Total | % wall |
|---|---:|---:|---:|---:|
| `merge::classify::classify_only` | 514,636 | 192 µs | **98.7 s** | 163 % |
| `merge::rewrite::rewrite_block_parallel` | 40,921 | 1.65 ms | **67.4 s** | 111 % |
| `pbfhogg::commands::read_raw_frame` | 514,667 | 96 µs | 49.3 s | 82 % |
| `write::writer::frame_blob_into` | 61,670 | 220 µs | 13.6 s | 22 % |
| `write::block_builder::take_owned` | 61,675 | 151 µs | 9.3 s | 15 % |
| `merge::rewrite::collect_batch` | 15,312 | 545 µs | 8.4 s | 14 % |
| `read::block::new` | 63,406 | 96 µs | 6.1 s | 10 % |
| `merge::node_locations::prefill_from_base` | 1 | **5.94 s** | 5.94 s | **10 %** |

### `--compression zlib:6` (UUID `ff3b07aa`, 56.7 s wall - different cache state)

| Function | Calls | Avg | Total | % wall |
|---|---:|---:|---:|---:|
| `write::writer::frame_blob_into` | 61,686 | 10.1 ms | **625 s** | 1104 % |
| `merge::classify::classify_only` | 514,651 | 225 µs | 115.7 s | 204 % |
| `merge::rewrite::rewrite_block_parallel` | 40,942 | 2.06 ms | 84.4 s | 149 % |
| `pbfhogg::commands::read_raw_frame` | 514,667 | 83 µs | 42.8 s | 75 % |
| `write::block_builder::take_owned` | 61,695 | 191 µs | 11.8 s | 21 % |
| `read::block::new` | 63,420 | 115 µs | 7.3 s | 13 % |
| `merge::node_locations::prefill_from_base` | 1 | **5.85 s** | 5.85 s | 10 % |
| `read::blob::decompress_into` | 95,071 | 43 µs | 4.1 s | 7 % |

Delta from none → zlib:

- `frame_blob_into`: **13.6 s → 625 s** (+611 s CPU). Pure zlib encode cost, parallel.
- `classify_only`: 99 s → 116 s (+17 s CPU). **Classify doesn't encode anything** - the delta is core contention, see the "zlib vs none" section.
- `rewrite_block_parallel`: 67 s → 84 s (+17 s CPU). Same mechanism.
- `prefill_from_base`: unchanged at ~5.9 s. Runs before the main pipeline, no contention.

## Alloc: per-function cumulative bytes (Europe altw, 2026-04-18)

UUID `4d4d9954`, commit `b4f45ff`, `--alloc` mode (`hotpath-alloc`
feature), default `--compression` (zlib:6). 60.8 s wall. Total
allocations ~291 GB across the run; peak RSS 1.1 GB - the allocator
turns bytes over fast, doesn't retain.

| Function | Calls | Avg | Total | % total |
|---|---:|---:|---:|---:|
| `merge::rewrite::rewrite_block_parallel` | 40,936 | 2.0 MB | **80.7 GB** | 27.7 % |
| `read::wire::parse_and_inline_with_scratch` | 63,423 | 860 KB | **52.0 GB** | 17.9 % |
| `merge::classify::classify_only` | 514,654 | 83.5 KB | 41.0 GB | 14.1 % |
| `commands::read_raw_frame` | 514,667 | 74.1 KB | 36.4 GB | 12.5 % |
| `write::block_builder::take_owned` | 61,690 | 563 KB | 33.1 GB | 11.4 % |
| `read::block::new` | 63,423 | 411 KB | 24.9 GB | 8.6 % |
| `write::writer::frame_blob_into` | 61,683 | 281 KB | 16.5 GB | 5.7 % |
| `merge::node_locations::prefill_from_base` | 1 | **2.9 GB** | 2.9 GB | 1.0 % |
| `read::blob::parse` | 971,613 | 901 B | 835 MB | 0.3 % |

Signals:

- **`parse_and_inline_with_scratch` at 860 KB per call** is the most
  surprising row - the "with_scratch" variant suggests scratch reuse
  already happened, but 52 GB cumulative shows the scratch doesn't
  cover every per-call vec. Worth auditing which vecs inside the
  function are still fresh per call.
- **`rewrite_block_parallel` at 2 MB per call × 41 k calls = 80.7 GB**
  is the largest single-callsite bucket. Per-call BlockBuilder (`task_bb`
  at rewrite.rs:958), output `Vec<OwnedBlock>`, stats - all per-task
  greenfield. This is the biggest arena / scratch pool target.
- **`classify_only` at 83 KB per call × 514 k calls = 41 GB** adds up
  from the per-call decompress buffer + wire scan scratch. Already
  uses `map_init` with a reusable buffer for decompress; the 41 GB
  says other per-call vecs are leaking (probably inside
  `scan_block_ids`, `scan_block_tags`, or the full-parse fallback).
- **`prefill_from_base` at 2.9 GB in one call** is the `NodeLocationIndex`
  - `locations: FxHashMap<i64, (i32,i32)>` + `needed_set: FxHashSet<i64>`.
  Sparse, expected, planet-scale still comfortable under host budget.

## Zlib vs none: where does the 8 s go?

Directly comparable `--bench 1` runs, same commit, same file, same cache
state window:

| UUID | Compression | Wall |
|---|---|---:|
| `f0af4170` | none | **46.1 s** |
| `570dfa69` | zlib:6 | **54.2 s** |

Zlib costs +8.1 s wall. Counter deltas:

| Counter | none | zlib:6 | Δ |
|---|---:|---:|---:|
| `merge_classify_total_ms` | 19,767 | 27,204 | **+7.4 s** |
| `merge_rewrite_recv_total_ms` | 13,651 | 13,895 | +0.2 s |
| `merge_prefill_ms` | 5,819 | 6,268 | +0.5 s |
| `merge_osc_parse_ms` | 1,860 | 1,955 | +0.1 s |
| `merge_final_flush_ms` | 1,354 | 1,179 | −0.2 s |
| `writer_compress_ns` | 8.6 s | **637.5 s** | +629 s CPU (parallel) |
| `merge_reader_send_wait_us` | 1.47 s | 8.63 s | +7.2 s stall |
| `writer_reorder_high_water` | 133 | **1797** | **13× deeper** |
| `writer_bytes_framed` | 26.6 GB | 17.3 GB | zlib output 35 % smaller |

**Classify takes +7.4 s under zlib even though classify doesn't
compress anything.** This is core contention: rayon workers decoding
blobs for classify compete for cores with rayon workers compressing
blocks for the writer. Under `none` the compression pool does almost
nothing; under zlib:6 it burns 629 s cumulative CPU (parallel) and
eats into classify's decode bandwidth. The reader-send stall at 8.6 s
(vs 1.5 s under none) is a second-order effect: consumer is slower, so
the frame channel stays full longer.

The writer's reorder high-water at 1797 (vs 133) says zlib produces
highly out-of-order completions - compression workers finish blobs in
variable time, the reorder buffer queues them until the file-order
consumer catches up. Not a bottleneck per se, just a shape change.

**Implications for ranking:**

- Confirms `--compression none` is the right production default (plan's
  existing position).
- **New rank-#1-adjacent candidate: separate rayon pool for writer
  compression.** Today the pipelined writer uses the global rayon pool,
  same pool as classify + rewrite. Isolating the compression pool
  (dedicated thread pool, N = min(cores, 4)) would let classify keep
  all cores when compression is under-utilised (current `--compression
  none` shape) and would prevent zlib runs from slowing classify. Fixes
  the hidden cross-phase interaction even when the user doesn't opt
  into `none`. Net: estimated 5-7 s save on Europe under zlib, smaller
  under none.
- The zlib:1 default (plan #5) remains opportunistic - lower win than
  the pool-separation idea above and with the same output-compatibility
  caveat.

## Current architecture (reference)

Entry: `merge()` at [rewrite.rs:702](../src/commands/merge/rewrite.rs#L702). The public command name is `apply-changes`; the internal module is called `merge`.

**Setup phase**:

1. Parse OSC → `CompactDiffOverlay` ([rewrite.rs:719](../src/commands/merge/rewrite.rs#L719)).
2. Build `DiffRanges` - sorted upsert + delete ID vecs per type ([rewrite.rs:746](../src/commands/merge/rewrite.rs#L746)).
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

### #1 - (Moot in production, listed for completeness) Default to zlib:1 like renumber does

[renumber_external.rs:118-126](../src/commands/renumber_external.rs#L118) overrides the compression default when the caller didn't specify:

```rust
let effective_compression = if compression == Compression::default() {
    Compression::Zlib(1)
} else {
    compression
};
```

Renumber's in-place rationale: zlib:6 adds ~22 s of backpressure at planet for ~15 % smaller output, which is a bad trade for transient outputs in a pipeline that rewrites again downstream.

**Production impact: zero.** Production runs with `--compression none`. The compression phase is already fast.

**Off-production impact**: any non-production caller that invokes `apply-changes` without a `--compression` flag currently pays zlib:6 unnecessarily. A zlib:1 default would cut that path's wall by ~150-300 s at planet.

**Keep on the list but de-prioritize.** Not on the critical path for the production pipeline. Land it opportunistically if touching the command for any other reason.

### #2 - Parallelize `NodeLocationIndex::prefill_from_base`

[node_locations.rs:112-144](../src/commands/merge/node_locations.rs#L112) is a straight sequential loop over node blobs:

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

`overlaps_needed` ([node_locations.rs:73](../src/commands/merge/node_locations.rs#L73)) is effective at skipping blobs that contain zero needed IDs. But every overlapping blob is decompressed on the main thread, serially, before the main pipeline even starts. For a daily diff touching ~10 M referenced nodes spread across the node ID space, probably 30-50 % of node blobs overlap, giving ~20-30 GB of compressed node data to decompress. At ~500 MB/s single-threaded, 40-60 s. On 6 cores: 10-15 s.

The shape matches [`parallel_classify_accumulate`](../src/commands/mod.rs#L571) exactly - it's the same pattern the geocode builder uses in Pass 1.5 for a dense-decode accumulator ([geocode_index/builder.rs:498](../src/geocode_index/builder.rs#L498)). Reuse it:

- Build a node-only schedule via [`build_classify_schedule`](../src/commands/mod.rs#L429) with `kind_filter = Some(ElemKind::Node)`. Apply the `overlaps_needed` filter at schedule-construction time (header-only blob walk, cheap). The filtered schedule contains only blobs worth decompressing.
- `parallel_classify_accumulate` with per-worker state `S = FxHashMap<i64, (i32, i32)>`. Workers do `pread → decompress → extract_node_tuples → if needed_set.contains(id) { local.insert(id, (lat, lon)) }`.
- Merge: drain each per-worker map into `self.locations`. HashMap insert is last-write-wins; all coords for a given ID are identical, so the merge is straightforward.

**Two nuances**:

- The current code uses `needed_set.remove(&t.id)` to (a) avoid double-insertion and (b) support early-exit via `all_found()`. In parallel land, workers read `needed_set` (shared immutable after build; swap `remove` for `contains`) and insert unconditionally. Early-exit is less useful once blobs are all in flight; drop the `all_found()` check or gate it on an atomic counter polled every N tuples.
- Per-worker map size at peak: ~2-5 MB for a daily diff (10M / 6 workers × ~50 bytes/entry). Merge is a single linear drain. No backpressure.

**Expected win**: ~30-60 s at planet.

**Risk**: low. Pattern is already used in the codebase. Correctness is straightforward (merge is commutative + idempotent for sparse location lookups).

### #3 - Replace the sequential reader thread with parallel pread schedule

[rewrite.rs:297-327](../src/commands/merge/rewrite.rs#L297): `spawn_reader_thread` runs one thread that opens a `FileReader` and streams `RawBlobFrame`s through a 128-deep `sync_channel`. That thread is the only reader. The batch loop decouples reader from workers but does not parallelize the read itself.

At sequential BufReader + blob-header-parse overhead, realistic throughput is ~500 MB/s - 1 GB/s. 87 GB is 90-180 s. Parallel `pread` on NVMe reaches 3-5 GB/s, dropping to 17-30 s.

**Refactor**: replace the reader thread with the same work-stealing pread schedule pattern used in [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615), ALTW's `stage2d_worker`, and geocode's proposed Phase 2a/2b:

- Header-only schedule scan up front, producing `(seq, frame_offset, data_offset, data_size, blob_type, indexdata_hint, tagdata_hint)` tuples. One sequential BufReader pass over the whole PBF, skipping blob bodies - fast (~3-10 s at planet).
- Collapse "reader thread → frame channel → classify workers" into one stage: each worker preads + classifies in the same loop and emits `ClassifyResult` downstream.
- Retain the existing batch structure by having the consumer side pull `ClassifyResult`s in seq order (reorder buffer) rather than pulling raw frames.

**Two wrinkles**:

- **`copy_file_range` path** ([rewrite.rs:790-795](../src/commands/merge/rewrite.rs#L790)) needs `frame.file_offset`. That survives cleanly - the schedule entry has both `frame_offset` (for raw passthrough) and `data_offset` (for pread of the compressed body). Include both in the tuple.
- **Raw-frame ownership** for the zero-copy passthrough move (`write_raw_owned(std::mem::take(&mut frame.frame_bytes))` at [rewrite.rs:938](../src/commands/merge/rewrite.rs#L938)). Workers already own their pread buffer; move it out the same way. The concept of `RawBlobFrame` survives; the difference is *when* the frame bytes are read (worker pread) versus *who* read them (reader thread today).
- **Reader-thread backpressure semantics.** The current `sync_channel(128)` gives 128 blobs of read-ahead. Parallel pread gives `num_workers × per-worker-batch` blobs of concurrent in-flight reads, which is similar or slightly higher. Page cache pressure is the same (reading the same bytes). No new RSS concern.

**Expected win**: ~50-100 s at planet on NVMe. Smaller on spinning disk.

**Risk**: medium. Largest of the three changes. Touches the main loop structure, not just a helper. Preserve the reorder-buffer + batch-boundary logic carefully.

## Overall expected savings

Under `--compression none` at planet:

- #2 alone: ~30-60 s saved. New wall ~11 min.
- #2 + #3: ~80-160 s saved. New wall **~9-10 min**.
- #2 + #3 + #1 (if any caller hits the zlib:6 default): #1 doesn't help production. Off-production callers see an additional ~150-300 s on their paths.

Primary target: **~6-7 min at planet in production (`--compression none`)**, from 8m52s. (Or ~9-10 min if osmium-interop zlib output is required, from 12m33s.)

## What to leave alone

- **The classify phase.** Already rayon-parallel with correct fast-paths (indexdata-based passthrough without decompress at [classify.rs:139](../src/commands/merge/classify.rs#L139), range-overlap secondary check, precise block-overlap check). No restructure needed.
- **The rewrite hot path** (`rewrite_block_parallel`, `emit_create_local`, `write_base_*_local` family). Already uses `pre_seed_string_table` to avoid re-interning, `add_way_raw_bytes_with_locations` to forward raw fields 9/10 byte-for-byte, and `add_relation_raw_bytes` to skip re-parsing members. This path is tightly written.
- **The pipelined writer** (`PbfWriter` + rayon compression + 64 permits). Under `--compression none` the rayon tasks become near-passthrough, but the structure is still correct and sized.
- **The coalescing passthrough buffer** ([rewrite.rs:812](../src/commands/merge/rewrite.rs#L812)). Collapses consecutive raw-frame writes into single sends. Correct at 70-90 % rewrite ratio (small fraction of bytes, but the collapse still matters for channel send overhead).
- **`NodeLocationIndex.locations` as `FxHashMap<i64, (i32, i32)>`.** Lookups are only for OSC ways (few million at daily scale), not base-way refs. Base ways forward their existing fields 9/10 via `write_base_way_local_with_locations`. HashMap at ~240 MB for 10 M entries is the right shape for sparse lookup.
- **`DiffRanges` sorted vecs + `partition_point`.** Already the right shape for range-overlap and inline upsert assignment.
- **`CompactDiffOverlay` / OSC parse.** Single-threaded but small (100-500 MB input, ~1-5 s); not on the critical path.
- **`UpsertCursors` + gap-create / trailing-create logic.** Complex but correct; sequential constraints are fundamental to preserving OSM ID order across passthrough boundaries.
- **Per-rayon-task `PrimitiveBlock` drop** ([rewrite.rs:905](../src/commands/merge/rewrite.rs#L905)). Already frees memory eagerly. No change.
- **`#[cfg(feature = "hotpath")]` phase timers.** Existing measurement scaffolding. Flip them on for the first post-#2 / post-#3 measurement runs.

## Plan of attack

1. **Enable `#[cfg(feature = "hotpath")]` per-phase timers unconditionally** (or at least for the first measurement pass). The inferred breakdown in this doc is a model; measure to ground-truth it before committing to the order of #2 and #3.
2. **Land #2 first** - parallel `prefill_from_base` via `parallel_classify_accumulate`. Smaller refactor, lower risk. Cross-validate output byte-for-byte on Denmark and Europe. Re-measure planet to confirm ~30-60 s save.
3. **Land #3** - parallel pread schedule replacing the reader thread. Preserve `copy_file_range` semantics and raw-frame move-ownership. Cross-validate byte-for-byte. Re-measure.
4. **#1 (zlib:1 default)** is opportunistic - tack it on when touching the command again, or skip if production is the only caller.

Cross-validation: `brokkr verify apply-changes --dataset denmark` if it exists (check `.review.toml` / brokkr for available verify targets). Otherwise: identical output PBF byte-for-byte after the primary merge batch; tail creates that get out-of-order under the existing implementation would be the same out-of-order set. Element-level diff (decompress, compare per-blob element lists sorted by ID) is the fallback.

## Memory budget (planet, post-#2 + #3)

| Component | Size |
|---|---:|
| `CompactDiffOverlay` (daily OSC) | ~500 MB - 1 GB |
| `NodeLocationIndex.locations` | ~200-500 MB |
| `DiffRanges` sorted vecs | ~50-100 MB |
| Per-worker pread + decompress buffers × ~6 | ~200-400 MB |
| Per-worker prefill `FxHashMap` (transient, phase #2 only) | ~50-300 MB |
| Writer pipeline + reorder buffer | ~200-500 MB |
| **Total** | **~1.8-2.5 GB** |

Unchanged from current 1.8 GB, or slightly higher during phase #2 merge. Host budget: irrelevant under 30 GB ceiling.

**Sizing robustness note.** None of the structures above scale with `unique_referenced_nodes` the way the failed [altw-as-renumber](altw-as-renumber.md) `coord_table` did. `NodeLocationIndex` scales with the OSC's own node-ref set (daily-diff-sized, bounded), not with the base PBF's population. No structure here depends on an estimate of the planet-scale referenced-node count. That is why this plan's recommendations survive the 2026-04-16 ALTW reshape failure unchanged.

## Correctness invariants

- **OSM ID ordering.** The main batch loop emits passthrough blobs in file order, rewrite blobs in file order (via the reorder buffer on the rayon mpsc channel), and gap creates before their matching blob's `min_id`. Any parallelization of reader or prefill must preserve file-order output. Phase #3's refactor must keep the reorder buffer intact.
- **`LocationsOnWays` preservation on base ways.** `write_base_way_local_with_locations` forwards raw `lat_data()` + `lon_data()` verbatim. Do not touch this path. Under `--locations-on-ways`, every base way must produce fields 9/10 in the output; the existing logic does this by calling the `_with_locations` variant whenever `loc_map.is_some()`.
- **Zero-coord fallback for missing node refs in OSC ways.** [rewrite.rs:67-70](../src/commands/merge/rewrite.rs#L67): `match locs.get(&node_id) { Some(&loc) => ..., None => locations.push((0, 0)) }`. Preserved under parallel prefill - the merged locations map has the same entries the sequential version would produce.
- **Straight `needed_set.contains` replaces `remove` in parallel prefill.** `contains` is cheaper than `remove`, and parallel workers cannot safely mutate a shared `FxHashSet`. Merge-at-end dedup covers the uniqueness semantic (a node hit by multiple workers will just insert the same `(lat, lon)` twice; last write wins, both values are identical).
- **Early-exit via `all_found()`.** Currently lets the sequential pass stop once all needed IDs are resolved. Under parallel prefill, all workers will have already claimed blobs from the schedule by the time the last needed ID is found. Either drop the early-exit (workers complete their claimed blobs; filters at schedule-construction time have already pruned most non-overlapping blobs) or add an atomic "remaining-needed" counter polled every N tuples. Probably not worth the complexity - the `overlaps_needed` filter already prunes aggressively.
- **`copy_file_range` path** on passthrough blobs ([rewrite.rs:960-970](../src/commands/merge/rewrite.rs#L960)). Under #3 the file offset must still be correct in the replacement schedule. The existing `frame.file_offset` field corresponds to `frame_offset` in the header-only scan - preserve this.
- **Reader-thread graceful shutdown.** The current reader joins at [rewrite.rs:1063](../src/commands/merge/rewrite.rs#L1063). Under #3 there is no separate reader thread to join; the schedule is consumed by the workers themselves, and shutdown is when all workers exit their claim loop.

## Open questions

- **Actual current phase breakdown.** The numbers in this doc are inferred. First step (measurement) either confirms the ordering of #2 and #3 or flips it. If the reader thread is not actually I/O-bound at production NVMe speeds, #3's payoff shrinks.
- **Does `overlaps_needed` prune as aggressively under a daily diff as estimated?** The 30-50 % overlap estimate is heuristic. If the actual overlap ratio is 70-80 %, prefill is genuinely most of a minute of serial work and #2 matters more. If it's 10-20 %, the serial cost is already small and #2 is marginal.
- **Does `--compression none` leave phase 4 measurably free?** Under zstd or zlib, the writer pipeline's rayon compression tasks can dominate. Under `none`, they're near-passthrough. Worth confirming that phase 4 is not still a bottleneck under production settings (e.g. due to output file writes being synchronous to disk rather than to page cache).
- **Does the prefill pre-pass RSS behave under parallel decompress?** Sequential prefill reuses one decompress buffer; parallel prefill needs one per worker (~16-32 MB × 6 = ~100-200 MB transient). Fine under 30 GB, but document the per-worker overhead for completeness.
- **Interaction with `--io-uring`.** Current `spawn_reader_thread` uses `FileReader` (BufReader + File). Under #3's pread-schedule model, workers use `pread` directly; `--io-uring` would need to be plumbed into the worker's read path rather than the reader thread's. Check whether the existing io_uring integration is on the reader side or the writer side; if reader, #3 needs to preserve it.
