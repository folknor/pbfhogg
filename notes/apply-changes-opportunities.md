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

The wins fall in two phases. **Phase 1 (landed, infrastructure):** parallel `NodeLocationIndex::prefill_from_base`; streaming parallel reader; classify-path instrumentation. Took 154.9 s -> 140.3 s and surfaced the real bottleneck via measurement. **Phase 2 (pending, structural):** replace the per-batch `par_iter().collect()` + serial main-thread drain with a streaming pipeline that fuses classify + rewrite on the same worker; main thread off the critical path. See the "External review synthesis" section - two independent reviewers converged on 40-55 s wall at planet as the realistic floor. Compression-level tuning is out of scope here; production runs with `--compression none`.

No internal API rewrites. `IdSetDense` is not used here (location index is a sparse HashMap keyed by node ID, which is right for the sparse-lookup pattern). `PbfWriter` is used correctly. `parallel_classify_accumulate` and `pass1_parallel_scan` are the patterns to reuse.

Target after this plan: **~6-9 min at planet under `--compression none`**, RSS unchanged (~1.8-2.2 GB).

## Yardstick

| Command | Wall | Peak RSS | Notes |
|---|---:|---:|---|
| `apply-changes --locations-on-ways` (pre-#2 baseline, `--compression none`) | 154.9 s | 1.8 GB | planet altw + OSC 4913, commit `b7ed0e1`, UUID `b91009ae` |
| `apply-changes --locations-on-ways` (post-#2, `--compression none`) | **144.4 s** | 1.8 GB | parallel prefill, commit `52c2c4b`, UUID `e81a9316` |
| `apply-changes --locations-on-ways` (current, `--compression zlib`) | 12m33s | 1.8 GB | historical: zlib encode (osmium-interop default), not re-measured |

The two `--compression none` rows were measured on the same host
(plantasjen), same dataset, same OSC, different commits. The older
8m52s / 12m33s numbers from a prior host/commit combination are
preserved below for historical continuity in the "Thesis" paragraph
but superseded by the rows above for quantitative comparison.

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

1. **Classify** (19.8 s @ Europe, 43 % wall; **70.9 s @ planet, 49 % wall**) - newly elevated. Was "leave alone" in the inferred plan; measurement says it's the biggest bucket.
2. **Rewrite throughput** (13.65 s @ Europe, 30 % wall; **43.7 s @ planet, 30 % wall**) - consumer blocked on rayon results. Also not originally on the plan's ranked list.
3. ~~**Prefill** (5.8 s @ Europe, 13 %; 20.5 s @ planet, 13 % pre-#2) - plan #2 target.~~ **Landed 2026-04-18 (commit `52c2c4b`), planet phase 20.5 s -> 6.6 s (-14 s), overall wall -10.5 s.**
4. **Reader parallelisation** (1.47 s stall @ Europe, 3 %; planet `merge_reader_send_wait_us=14.0 s`, 10 % wall) - plan #3 target; payoff bigger at planet than Europe predicted.

The top two rows (classify, rewrite) weren't on the ranked
opportunities list in the inferred plan. Before committing to an
approach, the next measurement should separate classify's work into
its three fast paths (indexdata hit / scan-only / precise decompress)
to see whether the time is dominated by decompress of the 8 % of
blobs that get rewritten, or by the scan-only path across the 92 %
that pass through.

## Measured: Planet altw + `--locations-on-ways` (baseline + post-#2)

Commit `b7ed0e1` (baseline) and `52c2c4b` (post-#2), 2026-04-18,
plantasjen. Planet altw (92.6 GB), OSC seq 4913, `--compression none`
(production default). `--bench 1`.

| Run | UUID | Commit | Wall |
|---|---|---|---:|
| Baseline (sequential prefill) | `b91009ae` | `b7ed0e1` | **154.9 s** |
| After #2 (parallel prefill)   | `e81a9316` | `52c2c4b` | **144.4 s** |

Wall delta: **-10.5 s (-6.8 %)**.

### Prefill phase delta (from `brokkr sidecar --compare b91009ae e81a9316`)

| Metric | Baseline | Post-#2 | Delta |
|---|---:|---:|---:|
| `MERGE_PREFILL` span wall | 20 544 ms (1.0c) | **6 606 ms (5.1c)** | **-13.9 s (-68 %)** |
| `merge_prefill_ms` counter | 20 544 | 6 606 | -13.9 s |
| Decode threads | 1 | 22 | - |
| Blobs decompressed | 42 119 | 42 119 | 0 |
| Blobs skipped (range overlap) | 9 315 | 9 315 | 0 |
| Nodes found | 97 392 | 97 392 | 0 |

Prefill phase shrank 14 s; overall wall 10.5 s. The ~3.4 s gap is the
new path's extra header-only pass (to build the blob schedule)
plus thread startup overhead. The prior sequential path folded
header scan and decode into one BlobReader iterator so paid the
header walk implicitly. Within the 10-20 s ceiling predicted for #2.

### Phase distribution at planet (post-#2, UUID `e81a9316`)

From the sidecar counters. Same shape as Europe's revised ranking
scaled up.

| Phase | Wall (ms) | % of 144 400 ms wall |
|---|---:|---:|
| `merge_osc_parse_ms` | 4 925 | 3.4 % |
| `merge_prefill_ms` | 6 606 | 4.6 % |
| `merge_classify_total_ms` | **70 942** | **49.1 %** |
| `merge_phase2_inline_total_ms` | 166 | 0.1 % |
| `merge_rewrite_recv_total_ms` | **43 714** | **30.3 %** |
| `merge_rewrite_spawn_total_ms` | 1 522 | 1.1 % |
| `merge_output_write_total_ms` | 1 413 | 1.0 % |
| `merge_final_flush_ms` | 1 593 | 1.1 % |

Classify + rewrite-recv together = **79 % of wall at planet** (vs 73 %
at Europe). The top-ranked targets (classify, rewrite) are confirmed
at scale. Prefill has dropped from 13 % (Europe, pre-#2) to 4.6 %
(planet, post-#2) - no further useful gain available there.

### Shape counters (planet post-#2)

| Counter | Value |
|---|---:|
| `merge_reader_frames_sent` | 197 585 |
| `merge_reader_blocked_sends` | 1 826 (0.92 %) |
| `merge_blobs_passthrough` | 120 132 |
| `merge_blobs_rewritten` | 77 453 |
| `merge_blobs_index_hit` | 104 908 |
| `merge_bytes_passthrough` | 51.7 GB |
| `merge_bytes_rewritten` | 67.1 GB (56.5 % rewrite ratio) |
| `merge_loc_needed` | 6 803 426 |
| `merge_loc_from_diff` | 2 468 812 |
| `merge_loc_from_base` | 97 392 |
| `merge_loc_missing` | 4 237 222 (62 %) |

Note the `merge_loc_missing` ratio: 62 % of nodes referenced by OSC
ways still aren't in either the base or the OSC. Missing refs fall
back to `(0, 0)` coords per
[rewrite.rs:67-70](../src/commands/merge/rewrite.rs#L67). Worth
spot-checking that this matches osmium's semantics before claiming
parity on `--locations-on-ways` output.

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

**Implications for ranking:** confirms `--compression none` is the
right production default. Compression-level tuning is out of scope
for this workstream since production already runs with `none`.

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

### #2 - Parallelize `NodeLocationIndex::prefill_from_base` (landed 2026-04-18, commit `52c2c4b`)

**Result:** prefill phase 20.5 s -> 6.6 s (-14 s, -68 %), overall wall
154.9 s -> 144.4 s (-10.5 s, -6.8 %) on planet altw + OSC 4913. See
"Measured: Planet" section above. Actual landed shape differs
slightly from the sketch below: schedule construction is inline in
`prefill_from_base` rather than reusing `build_classify_schedule`,
because the `overlaps_needed` filter needs per-blob indexdata access
at schedule-build time and adding that to the shared helper would
have bloated its signature for one caller. `parallel_classify_accumulate`
is also not used directly - the parallel path uses
`extract_node_tuples` on raw decompressed bytes, which is cheaper than
the full `PrimitiveBlock` construction that helper does. Retained for
historical context as a record of the planning/measurement/landed
arc.

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

### #4 - Classify slow-path reduction (next target)

Classify is now the biggest bucket at planet: **70.9 s / 144.4 s = 49 % of wall** (post-#2). It's main-thread wall, not cumulative CPU - the per-batch pipeline is serial on the main thread (classify -> inline-assign -> rewrite-spawn -> rewrite-recv), so classify and rewrite-recv wall add directly. Together they're 79 % of wall.

The classifier runs three paths per blob (see [classify.rs:129](../src/commands/merge/classify.rs#L129)):

1. **Fast path** - indexdata present + range miss -> `Passthrough` without decompress. Should be nanoseconds per call.
2. **Scan path** - indexdata range overlapped or absent. Decompress, then `scan_block_ids` for a tighter range. If that range misses -> `Passthrough`. Decompress cost only.
3. **Full parse path** - scan overlapped too (or produced nothing). Full `parse_primitive_block_from_bytes_owned` + `block_overlaps_diff`. If no element matches -> `FalsePositive`; else `NeedsRewrite`.

#### Current blob-count split at planet (UUID `e81a9316`)

| Path | Blobs | % of 197 585 | Counter |
|---|---:|---:|---|
| Fast-path passthrough | 104 908* | 53 % | `merge_blobs_index_hit` (conflated, see below) |
| Scan-path passthrough | 0* | 0 % | n/a (every blob has indexdata; counter reports `blobs_scan_only` for the no-indexdata case only) |
| FalsePositive (full parse, no match) | 15 224** | 8 % | derived: `blobs_passthrough - blobs_index_hit - blobs_scan_only` |
| Rewrite (full parse + re-encode) | 77 453 | 39 % | `merge_blobs_rewritten` |

*`merge_blobs_index_hit` today counts *both* "fast-path" *and* "scan-path after indexdata-loose overlap": any `Passthrough` where the original frame had indexdata. The scan-path-through-indexdata-loose count can't be separated without new instrumentation. At planet the two are probably dominated by fast-path (indexdata is tight on PBFs pbfhogg produces), but we don't know.

**`merge_blobs_scan_only` only counts the no-indexdata case (`has_indexdata: false`). At planet every OSM blob carries indexdata, so this is always 0. The counter name is misleading.

**FalsePositive has no explicit counter.** The delta subtraction works today but obscures a distinct path - these blobs pay the full-parse cost for nothing. Worth tracking directly.

#### Instrumentation landed (`b769996`) and measured (UUID `e49b6182`)

New per-path counters: `merge_blobs_classify_{fastpath,scan_pass,false_positive,rewrite}` (explicit blob counts) and `merge_classify_{decompress,scan,parse,precise}_ns` (cumulative ns summed across rayon workers, divided by observed parallelism to back out wall share). Fast-path is not instrumented (few-ns range check; atomic-add would dominate).

Planet run at `b769996`, 2026-04-18, plantasjen, `--bench 1`, `--compression none`. Wall: **136.8 s** (a further ~8 s drop vs `52c2c4b`'s 144.4 s, likely run-to-run variance). Classify phase wall: **71.1 s**.

**Per-sub-step cumulative CPU:**

| Sub-step | Cumulative CPU | % of classify CPU |
|---|---:|---:|
| `merge_classify_decompress_ns` | **206.3 s** | **70.0 %** |
| `merge_classify_precise_ns`    | 56.9 s       | 19.3 % |
| `merge_classify_parse_ns`      | 21.1 s       | 7.2 % |
| `merge_classify_scan_ns`       | 10.7 s       | 3.6 % |
| **Total measured CPU**         | **295.0 s**  | |

**Per-path blob counts** (explicit):

| Path | Blobs | % of 197 585 |
|---|---:|---:|
| `fastpath` | 104 908 | 53.1 % |
| `scan_pass` | **0** | 0.0 % |
| `false_positive` | 15 224 | 7.7 % |
| `rewrite` | 77 453 | 39.2 % |

**Findings:**

1. **Classify is CPU-undersaturated, not CPU-bound.** 295 s of rayon-worker CPU in 71 s of main-thread wall = **4.15 cores average** (out of 22 available). The phase is wall-limited by how work reaches rayon, not by raw work volume. With 15 864 batches averaging 12.5 blobs/batch, most per-batch `par_iter` calls can't use more than a handful of cores before running out of work. **This is the biggest lever.**
2. **Decompress is 70 % of classify CPU.** Nothing to optimise inside the decompress call (zlib is zlib); what matters is how many blobs reach it. The 92 677 slow-path blobs (FalsePositive + rewrite) each cost ~2.23 ms to decompress; there's no way to skip decompress if we need the bytes for rewrite or precise check.
3. **`scan_block_ids` is pure waste at planet: 0 `scan_pass` hits, 10.7 s CPU burned.** When indexdata is present and said overlap, `scan_block_ids` returns a range that always overlaps too (indexdata is already tight on pbfhogg-emitted PBFs). Skipping the scan when we already have indexdata saves 10.7 s CPU / 4.15 cores ≈ **2.6 s wall**. Trivial fix.
4. **Precise check (`block_overlaps_diff`) costs 57 s CPU** - more than parse (21 s). 92 677 calls x 614 us avg. Driven by `HashSet::contains` over diff IDs for every element in the block. Candidates: iterate diff IDs against the block's sorted element IDs (linear merge) instead; or replace `FxHashSet` lookup with `IdSetDense` for the diff nodes/ways/relations if dense enough; or do a wire-format ID scan that avoids materialising `PrimitiveBlock` at all.
5. **Parse is only 21 s CPU (7 %)** - smaller than expected. Deferring parse to Phase 3 would save barely 5 s wall at current parallelism. Not worth the refactor on its own.

#### Revised ranking (post-measurement)

By estimated wall impact at planet, assuming current ~4.15 cores classify utilisation (numbers change if item 1 lands first):

| # | Change | Est. wall save | Risk | Notes |
|---|---|---:|---|---|
| 1 | **Classify parallelism: cash in the CPU slack** | **~25-35 s** | medium | Bigger/coarser batches, or pipeline restructure so classify isn't per-batch main-thread serial. See sub-proposal below. |
| 2 | Skip `scan_block_ids` when indexdata was present | **~2.6 s** | trivial | Drop the scan call when `frame.index.is_some()`; rely solely on the precise check. Cheap first win. |
| 3 | Wire-format FalsePositive scan | **~3.4 s** | low | Replace parse+precise for 15 k blobs with a lightweight ID-only wire walk (analogous to `extract_node_tuples` but for way/relation IDs). Saves ~5 s parse CPU + ~9 s precise CPU. |
| 4 | Reduce `block_overlaps_diff` per-call cost | unknown | low-med | 57 s CPU total. Try linear sorted-merge vs HashSet lookups; or dense ID sets. Needs a micro-bench before committing. |
| 5 | Pipeline restructure (subsumes reader #3) | ~40-60 s | high | Collapses reader → classify → rewrite into one work-stealing pipeline. Also tackles rewrite-recv (43 s wall, 30 %). Only after items 1-4. |

Item 1 detail: the cheapest experiment is to bump `BATCH_MIN_BLOBS` / widen `BATCH_BYTE_BUDGET` so each `par_iter` has more work. Risk is memory: peak in-flight bytes grows with batch size. Likely safe under the 1.8-2.5 GB budget documented below. If that doesn't saturate cores, the only remaining fix is pipeline restructure (item 5). Instrument first by emitting per-batch classify CPU and per-batch blob count, so we can tell which `par_iter` calls leave cores idle.

## Overall expected savings

Under `--compression none` at planet:

- #2 (parallel prefill): **landed, measured -10.5 s wall (154.9 s -> 144.4 s on `52c2c4b`).**
- Scan-skip + sorted-merge: landed (`da1c45e`), ~-2 s wall (mostly variance).
- Parallel reader + merge batch budget: landed (`c97d6b5`, `bfac63b`), net zero wall. Kept as infrastructure for the streaming rewrite.

Updated primary target after the **external-review-driven streaming pipeline + fuse classify+rewrite** (next planned move): **40-55 s wall at planet daily OSC** (`--compression none`), from the current 140.3 s. CPU-budget floor from reviewer 2: classify CPU 270 s / 22 cores = 12.3 s classify wall under the streaming shape, plus ~3-4 s rewrite, plus pre-loop phases (~10 s) and writer wall-bound work running in parallel (~30 s). See "External review synthesis" section above.

**Weekly OSC end-state estimate (reviewer 2):** **70-90 s wall at planet weekly OSC** with streaming + worker-emits-framed (Q2) + prefill fusion (Q4) + parallel OSC parse (Q7). Current weekly (linear scaling of serial phases) probably ~250 s+.

**Follow-up wins on top of the streaming rewrite:**

- Worker-emits-framed-bytes: frees the `PIPELINE_DISPATCH_PERMITS=64` cap, small commit after streaming. More valuable under zlib and at weekly scale.
- Prefill fusion: -5 s at daily, -30 s+ at weekly. Clean local change after streaming lands.
- Splice-in-place for low-touch rewrites: ~1.5-2 s daily; less valuable at weekly.
- Parallel OSC parse: only matters at weekly scale (20-30 s).

## External review synthesis (2026-04-18)

Two independent reviewers (`perf` / `arch` / `planet` archetypes) converged strongly on the same diagnosis and next move after seeing the planet post-parallel-reader plateau at 140.3 s. Their writeups are extensive; this section folds the claims, reasoning, and follow-up ideas into one place so we don't lose them.

### Diagnosis: the 4.1-core classify plateau is self-inflicted

The shape of the main loop (one `batch.par_iter().collect()` followed by a serialised drain in file order) has two structural properties that together cap classify wall:

1. **Per-batch Amdahl on classify.** `par_iter().collect()` returns only when the slowest blob in the batch finishes. Batches mix a handful of heavy overlap blobs (decompress + precise, ~2 ms each) with many cheap fastpath blobs (ns each). Rayon schedules the heavy ones one-per-core; remaining cores sit idle waiting for the tail blob. At ~5-6 heavy blobs per batch, effective utilisation is ~5-6 cores - matching the measured 295 s CPU / 72.5 s wall = 4.1 cores.
2. **Phase-exclusive pool usage.** Within a batch: classify runs (pool busy on classify), then rewrites run (pool busy on rewrites), then the main thread walks slots in order. By the time the next classify starts, all previous rewrites have drained. Neither phase overlaps with itself or with the next batch. Each phase sees 22 cores in principle but each is individually bounded by its own barrier.

**Direct consequence:** batch size knobs can't fix this. Adding more items to a batch doesn't shrink the tail; measured at 128 MB -> 512 MB, classify wall was unchanged (72.5 s vs 70.3 s) and the bottleneck shifted to writer-permit pressure. The plateau is structural to the batch shape, not a rayon pathology.

**Reviewer 2's CPU-budget lower bound:** classify CPU 270 s / 22 cores = **~12.3 s classify wall** if the barrier is removed. That, plus the rewrite recv stall disappearing (main thread off the critical path), yields an estimated **40-55 s wall at planet**, vs current 140 s - roughly 3x speedup vs the ~18 % that pipelined batches alone would give.

### Primary rewrite: streaming pipeline, batches deleted

Replace `collect_batch` + `par_iter().collect()` + the per-batch slot-drain with continuous stages connected by bounded ordered channels:

```
[Reader]  --RawBlobFrame (seq)-->
  [Classifier pool: N workers]     each worker: decompress + precise check.
                                   If NeedsRewrite, do the rewrite inline (fuse).
                                   Emit (seq, ClassifiedItem) where ClassifiedItem =
                                       { RawPassthrough(frame) | Rewritten(blocks, stats) }
     --ClassifiedItem (seq)-->
  [Reorder / gap-create actor]     single thread. Consumes in seq order.
                                   Emits gap creates, handles type transitions,
                                   forwards frames/blocks to writer pipeline.
     --OutputChunk (seq)-->
  [PbfWriter pipeline]             existing, unchanged.
```

**Why this is the right move, not "another" move:**

- **No Amdahl barrier.** When worker A finishes its blob, it immediately pulls the next one. Cores stay busy as long as the reader has frames. Classify wall becomes `max(reader_bandwidth, 270 s / 22) ≈ 12-15 s`, vs 72.5 s today.
- **Rewrite fuses into classify naturally.** Today's flow decodes a `PrimitiveBlock` in one worker, ships it back to main thread via `NeedsRewrite(PrimitiveBlock, BlobIndex)`, main thread does ~50 ns of `partition_point` upsert-range compute, then re-spawns into rayon, which schedules yet another worker to rewrite the block it already had. Two dispatches, one cross-thread `PrimitiveBlock` handoff, one temporary allocation alive across the gap. If a worker already decoded the block, let it finish the job - pass `Arc<DiffRanges>` + `Arc<CompactDiffOverlay>` to the worker and the inline upsert-range compute happens inline.
- **Main thread leaves the critical path.** Today it owns type transitions, gap creates, passthrough coalescing, ordered rewrite-recv, passthrough flush, rewrite-block writes - a serial chain that shows up as `merge_rewrite_recv_total_ms = 39 s`. Under the new shape, a tiny dedicated reorder/gap-create actor owns only in-order sequencing + gap-create emission. Almost no CPU.
- **Coalescing gets better, not worse.** The reorder actor coalesces consecutive `RawPassthrough` frames into a single `write_raw_chunks` - same `Vec<Vec<u8>>` output shape as today. Coalescing now runs per-run-of-passthroughs in file order, not per-batch-truncated. Batches today arbitrarily break up long passthrough runs.

**CPU-budget walk (reviewer 2):**

- Classify CPU: 270 s -> `270 / 22 = 12.3 s` wall
- Rewrite CPU: (39 s recv is a lower bound on rewrite wall-in-pool today; real rewrite CPU probably 60-80 s) -> `3-4 s` wall with 22 cores
- Prefill 4.9 s + OSC parse 4.9 s + header ~0 s = unchanged serial pre-phase
- Writer wall: `--compression none` writes ~92 GB in ~30 s wall on decent NVMe, runs fully in parallel with classify

**Realistic floor: 40-55 s.** We've been at 140 s.

**Risk inventory and mitigations:**

- **Gap-create ordering.** Gap creates must be emitted in a specific seq position (before the blob whose `min_id` exceeds the cursor). The reorder actor sees items in seq order, so it can inject gap-create blocks between forwarded items. Same invariant as today; moved to a different owner.
- **Type transitions.** Same: the reorder actor owns the last-type state.
- **Prefill** for `--locations-on-ways`: stays as a serial pre-phase before the pipeline starts. Already fast (~5 s) and its `Arc`'d output just gets handed to classifier workers.
- **Backpressure / RSS.** Bound the `(seq, ClassifiedItem)` channel capacity **in bytes, not count**, since `Rewritten` items carry decoded + re-encoded data. A budget of ~1 GB in flight keeps us well under the 30 GB host limit.
- **Error propagation.** Workers send `Result<ClassifiedItem, _>`; reorder actor propagates. Existing pattern in the codebase.
- **Rayon pool partitioning.** The two reviewers diverge here (see Q3 follow-up subsection). Reviewer 1 recommends a dedicated rayon pool (~19 worker threads) following the [`src/read/pipeline.rs:127`](../src/read/pipeline.rs#L127) pattern. Reviewer 2 recommends the default global pool with work-stealing. Both agree reader, pread workers, and reorder actor run on dedicated (non-rayon) threads, and that "shared pool with priorities" is wrong. Decide at implementation time; for apply-changes specifically (no other concurrent rayon user) the two positions collapse to the same hardware assignment.

**Scope.** Net delete of ~300 lines from rewrite.rs; net add of ~400 lines split across a new `streaming.rs` + reorder actor. Not a prototype - commit, measure, keep or revert.

### Cheap disambiguation experiment (attempted, reverted 2026-04-18)

Attempted implementation: `rayon::scope` + per-blob `scope.spawn` + `mpsc::sync_channel(batch_len)` + `ReorderBuffer` replacing `batch.par_iter().collect()`. Keeps phase 2/3/4 unchanged. Committed as `<pending-sha>`.

**Result: reverted.** Denmark verify passed (correctness OK), but planet `--bench 1` never completed: killed at 10 min wall (vs ~140 s baseline) with last marker `WAIT_REWRITE_RESULT_END` and only 5.6 GB / 92 GB read, 5.4 GB written. Child RSS was 1.2 GB, 50 threads. Not a deadlock - the process was making forward progress, just pathologically slowly.

**Likely cause (not proven):** per-batch `rayon::scope` overhead at scale. Planet has ~11 k batches; 18-way scope.spawn + waiting-for-all-to-complete + channel allocation + ReorderBuffer allocation per batch. Even a few ms of per-batch overhead puts the total minutes over baseline. The `rayon::scope` barrier semantics also still wait for the tail task per batch - the shape isn't meaningfully different from `par_iter().collect()` in that respect.

**What this tells us:** the "cheap" experiment as reviewers sketched it (swap dispatch shape, keep batch loop) doesn't cleanly isolate the batch-barrier hypothesis at planet scale. A better experiment would be:

- Pre-allocate the reorder buffer + channel **once**, outside the batch loop, reused across batches.
- Or: abandon the "keep batches" constraint and go straight to the streaming rewrite - the real payoff shape anyway.

**Doesn't disprove the plateau hypothesis** - it just means this particular minimal-change form doesn't prove it either way. The CPU-budget math (270 s / 22 = 12.3 s classify wall floor) still stands as the streaming pipeline's theoretical target.

**Sequencing takeaway:** skip the cheap experiment; go direct to the streaming pipeline. Accept that we can't cheaply disambiguate "batch barrier vs non-scaling CPU" before committing to the restructure. Mitigation: instrument the streaming pipeline with per-phase counters (already planned) so we can detect non-scaling CPU early and pivot if the 40-55 s target turns out unreachable.

### Follow-up wins (order by when they make sense)

The streaming rewrite above is the headline. After it lands, these become the next targets:

#### Worker emits framed bytes (both reviewers, Q2 follow-up)

Highest-confidence follow-up after the streaming rewrite. Moves compression + framing into the classifier worker that already holds the uncompressed bytes. Details in the Q2 follow-up subsection above; ~50-line commit; works for `none`, `zlib`, and `zstd`. Extra payoff under zlib: removes the `PIPELINE_DISPATCH_PERMITS=64` cap that bottlenecks concurrent compressor tasks today.

#### Prefill fusion into the streaming pipeline's node phase (reviewer 2, Q4 follow-up)

Classifier workers decompressing node blobs opportunistically extract coords for IDs in `needed_set` into thread-local maps. Reorder actor detects the node→way transition and merges per-worker maps into `Arc<loc_map>` before gating way-blob workers. Details in Q4 follow-up subsection above. **Deletes the prefill phase entirely:** ~5 s at daily, ~30 s+ at weekly scale.

#### Parallel OSC parse (reviewer 2, Q7 follow-up, weekly only)

Only relevant if the standard pipeline processes a week of OSCs in one run. Single-threaded [`load_all_diffs`](../src/osc.rs#L1036) parse scales linearly with OSC count; at 7x it becomes a 30-40 s serial phase. Shape: parse each OSC concurrently into its own overlay, then merge overlays with newer-wins semantics. Each OSC is independent work. Estimated 20-30 s wall at weekly scale.

#### Splice-in-place for low-touch rewrites (reviewer 2)

In `rewrite_block_parallel` ([rewrite.rs:710-826](../src/commands/merge/rewrite.rs#L710)), every `NeedsRewrite` blob is **fully decoded and fully re-encoded**, even if only one of its ~8 000 elements is touched by the diff. At planet with a daily OSC, the modal "needs rewrite" blob has 1-3 affected elements out of 8 000.

**The change.** For blobs where the precise check finds `<=K` affected elements (say K=64), splice: walk the raw decompressed wire bytes for the `DenseNodes` / `Ways` / `Relations` `PrimitiveGroup`, emit runs of unaffected elements raw (via the existing raw-group passthrough scaffolding at [`src/read/block.rs:507`](../src/read/block.rs#L507) + [`src/write/raw_passthrough.rs`](../src/write/raw_passthrough.rs#L1)), and only decode+re-encode the affected ones.

**Budget.** Attacks the 25 s classify parse + the estimated 60-80 s rewrite CPU. Estimated save: **30-50 s CPU, ~1.5-2 s wall** on top of the streaming rewrite at daily scale. Not a headline, but sizeable.

**Weekly scale: less valuable.** Rewrite blobs touch more elements per blob under a weekly OSC; the near-passthrough population (`<=K` affected elements) shrinks. If the standard cadence becomes weekly, this item demotes.

**Don't land before the streaming rewrite** - the rewrite moves this code to a different owner and landing twice is waste.

#### Steal `copy_file_range` coalescing from ALTW (reviewer 1)

`add_locations_to_ways` already coalesces contiguous `copy_file_range` runs at [`src/commands/add_locations_to_ways.rs:1331`](../src/commands/add_locations_to_ways.rs#L1331). Merge still does per-blob `copy_file_range` writes at [rewrite.rs:1266](../src/commands/merge/rewrite.rs#L1266). Useful, secondary.

#### Use the existing alloc-optimised parse path (reviewer 1)

`classify_only` currently parses via [`src/read/blob.rs:1307`](../src/read/blob.rs#L1307) (`parse_primitive_block_from_bytes_owned`), but [`src/read/block.rs:432`](../src/read/block.rs#L432) exposes an alloc-optimised variant already used elsewhere. Small slow-path parse shave.

#### Exact-membership metadata or sidecar (reviewer 1, conditional)

Current on-disk metadata gives per-blob type + ID range only ([`src/blob_index.rs:56`](../src/blob_index.rs#L56)), so pure creates inside an existing blob range force slow-path decode - this is the documented FalsePositive case at [`src/commands/merge/classify.rs:48`](../src/commands/merge/classify.rs#L48).

**Only worth pursuing if FalsePositives are a material share of slow-path work.** At planet today: 15 224 FalsePositive blobs / 92 677 slow-path = 16 %. Not negligible, but small next to the 77 453 rewrites that fundamentally need the slow path. A format/index project, not a quick cleanup.

If pursued: either a wire-format exact-overlap scanner on decompressed bytes (skips full parse for FalsePositives) or a per-blob membership sketch in indexdata (rejects FalsePositives without decompress at all).

#### Ideas explicitly ruled out

- **Pre-built schedule sidecar.** Reviewer 2: "header walk at planet is ~1-2 s. Not the bottleneck. Skip." (Our measured scanner cost was 43 s when run as a pre-block, but that was specifically the blocking pre-scan shape - under the streaming reader and later streaming pipeline, the scanner runs concurrently with workers, and its wall cost is subsumed.)
- **Separate thread pool for decompress alone.** Becomes moot under the streaming rewrite: classify and rewrite fuse, no seam to separate.
- **Revert parallel reader or 512 MB batch budget.** Both reviewers: no. Parallel reader is needed infrastructure for the streaming pipeline. The 512 MB budget is neutral-at-worst and becomes irrelevant once batches go away - ripping it out costs us later.
- **Writer-permit loosening.** The 20x `writer_permit_wait_ns` at 512 MB is a symptom of too-large coalesced flushes hitting a 32-deep channel, not an independent bottleneck. Fixed automatically when the reorder actor coalesces per-run rather than per-batch; runs are smaller and distributed in time.
- **Compression-level tuning (zlib:1 default).** Out of scope for this workstream: production uses `--compression none`, the ecosystem default is zlib:6 and stays.

### Recommended sequence (both reviewers agree)

1. ~~**Cheap disambiguation experiment.**~~ Attempted 2026-04-18 and reverted (10 min+ regression at planet while Denmark verify passed). See "Cheap disambiguation experiment" subsection for post-mortem. Skipping this step and going direct to streaming.
2. **Full streaming pipeline + fuse classify+rewrite.** Same commit architecturally - don't land them separately. Expected 40-55 s wall.
3. **Worker-emits-framed-bytes.** Small, high-confidence commit after the streaming rewrite. Removes the `PIPELINE_DISPATCH_PERMITS=64` cap on concurrent compressor tasks; frees the writer's second rayon dispatch. See the Q2 follow-up subsection.
4. **Prefill fusion into the streaming pipeline's node phase.** Clean local change once streaming is in place. Deletes the prefill phase entirely (~5 s today, ~30 s+ at weekly scale). See the Q4 follow-up subsection.
5. **(weekly OSC only) Parallel OSC parse.** New priority at weekly scale: single-threaded XML+gzip at 7x becomes a 30-40 s serial phase. Parallel-parse-then-merge-overlays is a real target. See the Q7 follow-up subsection.
6. **Splice-in-place for low-touch rewrites.** ~1.5-2 s wall at daily. **Less valuable at weekly** (more elements touched per rewrite blob shrinks the near-passthrough population).
7. **`copy_file_range` coalescing, alloc-optimised parse.** Opportunistic local shaves.
8. **Writer path tuning** (direct-io vs `to_path_uring`): only after the streaming rewrite if bench points there. Don't start with "bigger internal queues" - that's a symptom-fix. See Q1 follow-up.

After step 2, the next investigation target becomes reader throughput or writer pipeline - both currently invisible because the main-loop noise dominates.

### Second review round (Q1-Q7 follow-ups)

After the initial reports, a follow-up round probed seven targeted questions about the streaming rewrite's downstream implications. The answers refine the plan above; this subsection records the concrete new material. Where the two reviewers diverged, the divergence is noted.

#### Q1: Writer as the new wall floor

Both reviewers: **measure before optimising.** The existing writer stack is already well-shaped.

Reviewer 1: "The existing writer stack is good enough to let the streaming rewrite land first, but only if rewritten blobs stop going through `write_primitive_block_owned`." The plumbing already has the right shapes: buffered/direct/io_uring selection in [`src/commands/mod.rs:864`](../src/commands/mod.rs#L864), bounded write-ahead + dispatch permits in [`src/write/writer.rs:31`](../src/write/writer.rs#L31), and an io_uring backend aimed at `Compression::None` on fast storage in [`src/write/writer.rs:408`](../src/write/writer.rs#L408) and [`src/write/uring_writer.rs:1`](../src/write/uring_writer.rs#L1). If the writer *does* become the new floor, the next pass in order: (1) preframed rewrite output (Q2), (2) contiguous passthrough `copy_file_range` coalescing like [`src/commands/add_locations_to_ways.rs:1331`](../src/commands/add_locations_to_ways.rs#L1331), (3) benchmark buffered vs io_uring on target NVMe. **Do not start with "bigger internal queues"** - if disk bandwidth is the wall, queue depth is rarely the real lever.

Reviewer 2: NVMe ceiling gives 20-30 s for the 92 GB write. The `sync_channel(WRITE_AHEAD=32)` → writer_thread → `BufWriter(256 KB, File)` path is fine until the write queue is actually the bottleneck, which would show up as `pipeline_send_wait_ns` blowing up. If `bytes_written/s` sits near the NVMe ceiling after streaming lands, we're done for the `--compression none` case. If not, flip to `to_path_uring` (infrastructure is already there: `uring_writer.rs`, 64x256 KB registered buffers + `O_DIRECT`) before anything else. Bigger internal queues are a symptom-fix.

Under `--compression zlib` the writer's CPU cost dominates anyway; the channel is almost never the bottleneck. Writer optimisation as a topic is mostly a `--compression none` concern.

#### Q2: Worker emits framed bytes (`write_raw_owned`) instead of `OwnedBlock`

Both reviewers: **do it.** Small-to-medium internal change, high-confidence win, works under all compression modes (not a `--compression none` trick).

The key enabler already exists: [`frame_blob_pipelined`](../src/write/writer.rs#L1102) takes uncompressed bytes + compression + indexdata + tagdata on per-thread scratch and returns `FramedBlobParts`. This is exactly the shape the fused worker needs.

**Minimal change (reviewer 1 + 2 agree):**

- Change merge worker output from `Vec<OwnedBlock>` to `Vec<Vec<u8>>`.
- In the worker, after `bb.take_owned()` yields `(block_bytes, index, tagdata)`, call `frame_blob_pipelined(&block_bytes, &compression, Some(&index.serialize()), tagdata.as_deref())?.into_vec()` and emit that `Vec<u8>` as the rewritten output.
- The reorder actor forwards it via the existing raw-passthrough path ([`write_raw_owned`](../src/write/writer.rs#L715) / `write_raw_chunks`).

**Why this is more valuable under zlib, not less (reviewer 2):** today every rewritten blob pays one `rayon::spawn` + one `PIPELINE_DISPATCH_PERMITS` acquire + compression + one channel send. Moving compression into the worker that already owns the uncompressed bytes saves the dispatch, saves the permit dance, and keeps the block resident on one core's L2/L3 from encode through frame. Under zlib the **64-permit pool was capping concurrency at ~64 in-flight rayon tasks**; fusing compression into the classifier pool lets all 22 workers run compression without that cap.

**Public API doesn't move** - `write_primitive_block_owned` stays for callers that want compression dispatched internally; streaming-merge just stops using it.

**Endpoint refinement (reviewer 1):** a crate-private `write_framed_parts_owned` that accepts `FramedBlobParts` directly would avoid the final `into_vec()` flatten copy. Still internal-only.

**Don't land before the streaming rewrite** - land #1 first, this is a ~50-line commit once the worker exists.

#### Q3: Pool partitioning - **divergence**

This is the one place the reviewers give different recommendations. Both agree the reader scanner, pread workers, and reorder actor run on **dedicated (non-rayon) threads**. The disagreement is about the classify + rewrite + writer-compression work.

**Reviewer 1:** dedicated rayon pool, not the global one. Pattern: [`src/read/pipeline.rs:127`](../src/read/pipeline.rs#L127) already shows the dedicated-pool shape; reuse it. Under `Compression::None`, size around **19 worker threads**, leaving 3 for reader/scanner, ordered emitter, and writer thread/kernel slack. Under zlib, either fuse compression into the blob workers (stay one dedicated pool) or use two distinct pools - but **do not use "one shared pool with priorities"** because rayon doesn't give the scheduling control that implies.

**Reviewer 2:** single shared rayon pool for classify + rewrite + writer-compression. Work-stealing across one pool smoothes imbalance better than two pools with fixed capacities. Two pools create artificial capacity walls - if classify is quiet and writer-compress is backlogged, a split pool can't rebalance. The only thing worth sizing explicitly is pread worker count; `decode_threads = cores - 2` is fine.

**Practical note.** Apply-changes has no other major rayon consumer running concurrently, so "dedicated pool with N threads" and "default global pool with N threads" collapse to the same hardware assignment in practice. The substantive difference is insulation-from-other-rayon-code, which isn't a concern here. **Decide at implementation time; not a high-stakes fork.** Both reviewers agree that "shared pool with priorities" is the wrong answer.

#### Q4: Prefill overlap - **one fake win, one real win**

Reviewer 1: don't overlap `prefill_from_base` with OSC parse. The skip logic at [`src/commands/merge/node_locations.rs:73`](../src/commands/merge/node_locations.rs#L73) needs a *complete, sorted* `needed_sorted` set, and the scan is one-way through the node section ([`node_locations.rs:121`](../src/commands/merge/node_locations.rs#L121)) - starting early with an incomplete set risks skipping a node blob that later turns out to be required, which is unrecoverable without a rescan. With multiple diffs it gets worse because later diffs overwrite earlier state ([`src/osc.rs:979`](../src/osc.rs#L979)). If parse time matters, the right move is upstream: squash diffs before merge, not overlap inside it.

Reviewer 2: agrees the "overlap with OSC parse" framing is a ~1 s fake win. **But there's a larger, real win in a different shape** - *fuse prefill into the streaming pipeline's node phase*. In a sorted PBF, all node blobs precede all way blobs. The streaming pipeline decompresses every node blob that overlaps the diff anyway. Today prefill separately decompresses a subset of node blobs (those overlapping `needed_set`) purely to extract coordinates for LOW. The two passes read overlapping data.

**The restructure.** While classifier workers process node blobs, they opportunistically extract coordinates for any ID in `needed_set` they encounter. Each worker accumulates into a thread-local `FxHashMap<i64, (i32, i32)>`. When the pipeline transitions from nodes to ways (reorder actor detects this in seq order), merge the worker maps into `Arc<loc_map>`, and gate way-blob workers on it being ready.

**Gains.**

- Deletes the prefill phase entirely - **5 s saved at daily**.
- Decompresses fewer node blobs overall (blobs the pipeline was going to decompress anyway now do double duty).
- **At weekly scale (Q7) where `needed_set` is ~7x larger, prefill would otherwise balloon from 5 s to 30+ s.** Fusion keeps it at approximately zero.

**The dependency chain becomes:** OSC parse → refs collected → `Arc<needed_set>` handed to classifier workers → pipeline runs. The header walk for prefill's schedule disappears; workers decide opportunistically per-blob.

**One real complication.** The node→way transition needs a barrier so no way worker starts before `loc_map` is finalised. The reorder actor detects the transition from the indexdata `blob.kind` change (trivially detectable in a sorted base PBF) and owns the barrier.

**Reconciling with reviewer 1:** reviewer 1 declined the specific "overlap with OSC parse" shape because of the skip-logic concern. Reviewer 2's fusion shape sidesteps that concern entirely - the pipeline visits every node blob by necessity, so there's no risk of "skip a blob that turns out to be required". The two positions aren't contradictory.

**Sequencing.** Land streaming without this first (keep prefill as a serial pre-phase). Then fuse it as a follow-up - ~5 s at daily, proportionally more at weekly.

#### Q5: Trailing creates - no special case, both reviewers agree

The reorder actor owns `UpsertCursors` and treats end-of-stream as the current trailing-create block at [`rewrite.rs:1435`](../src/commands/merge/rewrite.rs#L1435): once the last blob has been emitted, flush the remaining upserts for the current and later kinds.

Reviewer 1 and reviewer 2 both note the existing `types_to_flush` match at [`rewrite.rs:1440-1453`](../src/commands/merge/rewrite.rs#L1440) already encodes the cases: `None` → flush all three, `Some(Node)` → flush Node+Way+Rel, etc. Port it verbatim to the actor's post-loop.

**Testing target (reviewer 2):** an empty-base-PBF case where `last_type` stays `None` and the actor has to emit gap/trailing creates for all three kinds with no frames ever seen. Today's code handles this via the `None` arm; the actor needs to preserve it. Write a test for it.

#### Q6: Backpressure budget - whole pipeline, with concrete carve-up

Both reviewers: **~1 GB in flight is a whole-pipeline budget, not a single channel cap**, and backpressure must propagate all the way to the scanner or the classifier side will buffer indefinitely behind a slow writer.

Reviewer 1: the current `WRITE_AHEAD=32` and `PIPELINE_DISPATCH_PERMITS=64` at [`src/write/writer.rs:31`](../src/write/writer.rs#L31) are count bounds, not a memory model. A single byte-credit budget across the whole graph is the right shape. If backpressure doesn't propagate to the reader scanner, the classifier side silently buffers.

Reviewer 2: concrete carve-up of the ~1 GB budget across stages:

| Stage | Channel | Capacity | Per-item UB | Budget |
|---|---|---:|---|---:|
| Reader → Classifier | `(seq, RawBlobFrame)` | 64 | ~1 MB compressed | 64 MB |
| Classifier workers in flight | decompress buf + `PrimitiveBlock` + rewrite scratch | 22 | ~20 MB | 440 MB |
| Classifier → Reorder | `(seq, ClassifiedItem)` | 64 | ~4 MB rewritten / 1 MB passthrough | ~128 MB |
| Reorder → Writer | `PipelineItem` | `WRITE_AHEAD=32` | ~4 MB | 128 MB |
| Writer buffered | `BufWriter` + uring buffers | - | - | ~16 MB |
| **Total** | | | | **~780 MB** |

Worst case; actual RSS is lower because many blobs are small. Leaves meaningful headroom under the 28 GB host limit.

**Backpressure chain (reviewer 2):** writer thread can't keep up → its receiver fills → reorder actor's send blocks → reorder's receiver fills → classifier workers' send blocks → they stop pulling new frames → reader channel fills → pread workers' send blocks → they stop pulling from `dispatch_rx` → scanner's send on `dispatch_tx` blocks → scanner stops reading headers. Halts cleanly end-to-end **provided every channel is bounded.**

**Failure mode to avoid (reviewer 2):** any unbounded channel or any `par_iter().collect()` pattern. `collect()` materialises the whole batch and breaks the bound. Streaming sidesteps this entirely; if it creeps back in later, flag it immediately - one unbounded buffer anywhere defeats the whole chain.

**Per-worker unbounded accumulator caveat (reviewer 2).** Under the prefill-fusion change (Q4), the per-worker coord extraction map needs size discipline too. For planet daily/weekly, `needed_set` is bounded by OSC way-refs (at most millions of entries); not a practical concern at this scale, but worth the discipline.

#### Q7: Weekly OSC - same first move, different priorities after

Both reviewers: **streaming pipeline is still the right first move at weekly scale.** What changes is the priority order after it lands.

**Reviewer 2's concrete changes from daily to weekly:**

1. **OSC parse becomes a real phase.** Single-threaded XML + gzip at 7x data: probably 30-40 s serial. Today's 5 s hides; weekly's 35 s doesn't. [`load_all_diffs`](../src/osc.rs#L1036) parses files sequentially into one overlay - that sequential loop becomes a bottleneck. **Parallelise it:** parse each OSC concurrently into its own overlay, then merge overlays with newer-wins semantics. Each OSC is independent work. Merge pass is a few seconds over the combined overlays. Same story for `write_streaming` in [`merge_changes.rs:184`](../src/commands/merge_changes.rs#L184). This is a real target at weekly scale, separate from the streaming pipeline work.
2. **`DiffRanges` scales linearly.** Sort-dedup of ~7x more IDs; still fast. Not a new bottleneck.
3. **Passthrough-to-rewrite ratio shifts downward.** Today ~60 % passthrough at planet. Weekly planet: probably ~70-80 % passthrough blobs at most, but higher *rewrite* share per affected blob. More rewrites, fewer passthroughs, bigger rewrite CPU. The 4.1-core classify plateau gets proportionally worse (more heavy blobs per batch). **Case for streaming gets stronger** at weekly.
4. **Prefill `needed_set` is ~7x larger.** More node blobs to scan, more `FxHashMap` pressure. **This is where Q4's fusion becomes critical** - prefill as a separate pass could balloon from 5 s to 30+ s, but fused into the pipeline's node phase it costs approximately nothing.
5. **Writer output roughly unchanged in absolute bytes.** Still ~92 GB out. Writer is not more of a bottleneck weekly than daily.
6. **Per-rewrite-blob cost goes up** - more elements per blob get modified. This hits the rewrite CPU budget. It also makes Q2 (worker-emits-framed) *more* valuable because more blobs go through the rewrite path.

**Prioritisation shift at weekly scale:**

- Streaming pipeline: **more valuable**, not less.
- Worker-emits-framed (Q2): **more valuable** (more blobs benefit).
- Prefill fusion (Q4): **more valuable** (avoids a 30+ s phase growth).
- Parallel OSC parse: **a new priority** that doesn't exist at daily. Plausibly 20-30 s of headline wall to recover.
- Splice-near-passthrough rewrites: **less valuable at weekly** - rewrite blobs touch more elements per blob, so the near-passthrough population shrinks. Demote.

**Reviewer 1 note on weekly:** diff-squashing becomes genuinely interesting as a **formal upstream stage** rather than paying XML parse + overwrite churn inside the critical path every run. "Squash diffs to one final overlay / binary delta" can be a separate command that runs once per week and emits a single pre-merged diff file that apply-changes then consumes as if it were a daily. Orthogonal to the streaming rewrite but may be the right long-term shape if weekly is the standard cadence.

**Reviewer 2's revised end-state estimate for weekly planet:** **70-90 s wall** with streaming + parallel OSC parse + prefill fusion + worker-framing. Current weekly (linear scaling of serial phases) probably ~250 s+.

## What to leave alone

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

1. ~~**Enable `#[cfg(feature = "hotpath")]` per-phase timers unconditionally**~~ **Done.**
2. ~~**Land parallel prefill (#2).**~~ **Landed 2026-04-18 (`52c2c4b`), planet -10.5 s wall.**
3. ~~**Land classify instrumentation (#4).**~~ **Landed 2026-04-18 (`b769996`).** Measurement at UUID `e49b6182` - findings in #4 section above.
4. ~~**Skip `scan_block_ids` when indexdata was present**~~ **Landed 2026-04-18 (`da1c45e`)**, saved 10.7 s CPU but only ~2 s wall (mostly lost to variance).
5. ~~**Bump classify batch size**~~ **Landed 2026-04-18 (`bfac63b`) - 512 MB merge batch budget.** Raised average batch from ~13 to ~18 overlap blobs but classify wall was unchanged (72.5 s vs 70.3 s). **Diagnosis from external review: the plateau is structural to the batch barrier, not fixable with batch-size knobs** - see "External review synthesis" section.
6. ~~**Parallel reader (#3)**~~ **Landed 2026-04-18 (`c97d6b5` streaming variant)**. Net-zero wall change vs sequential reader. Kept as infrastructure for the streaming pipeline below.
7. ~~**Cheap disambiguation experiment**~~ **Attempted + reverted 2026-04-18.** `rayon::scope` + per-batch spawn + mpsc + ReorderBuffer regressed from ~140 s to 10 min+ at planet (Denmark verify passed). See "Cheap disambiguation experiment" subsection for post-mortem. Doesn't disprove the plateau hypothesis, but rules out this form of cheap check. **Going direct to streaming pipeline instead.**
8. **Streaming pipeline + fuse classify+rewrite.** Delete the batch loop; replace with reader -> classifier-pool (fused decompress + precise + rewrite) -> reorder/gap-create actor -> writer. Main thread off the critical path. Estimated **40-55 s wall at planet daily** (from 140 s). See "Primary rewrite: streaming pipeline" subsection for risk inventory and CPU-budget walk.
9. **Worker-emits-framed-bytes.** Small high-confidence commit after the streaming rewrite: change worker output from `Vec<OwnedBlock>` to `Vec<Vec<u8>>` framed via [`frame_blob_pipelined`](../src/write/writer.rs#L1102) on the worker. Frees the `PIPELINE_DISPATCH_PERMITS=64` cap; removes the writer's second rayon dispatch. See Q2 follow-up subsection.
10. **Prefill fusion into streaming pipeline's node phase.** Classifier workers opportunistically extract coords for `needed_set` IDs while decompressing node blobs; reorder actor barriers the node→way transition and finalises `Arc<loc_map>`. Deletes prefill phase. Estimated **-5 s daily, -30 s+ weekly**. See Q4 follow-up subsection.
11. **(weekly OSC only) Parallel OSC parse.** Parse each OSC concurrently into its own overlay; merge overlays newer-wins. Current [`load_all_diffs`](../src/osc.rs#L1036) serialises this. Estimated **-20-30 s wall at weekly scale**; no win at daily.
12. **Splice-in-place for low-touch rewrites.** Follow-up to the streaming rewrite: for `NeedsRewrite` blobs with `<=K` affected elements, splice via raw-group passthrough instead of full decode + re-encode. Estimated **~1.5-2 s wall** at daily. Less valuable at weekly (more elements touched per rewrite blob).
13. **`copy_file_range` coalescing from ALTW**, **alloc-optimised parse path** via [`src/read/block.rs:432`](../src/read/block.rs#L432). Opportunistic local shaves.
14. **Writer path tuning** (direct-io vs `to_path_uring`). Only if post-streaming bench points there. Infrastructure is already wired in [`src/commands/mod.rs:864`](../src/commands/mod.rs#L864) + [`src/write/uring_writer.rs:1`](../src/write/uring_writer.rs#L1); flip the variant, don't grow queues.
15. **Exact-membership metadata / sidecar.** Only if FalsePositives remain material after the streaming rewrite. Currently 16 % of slow-path blobs; not negligible but not headline either. Format/index project, not a quick cleanup.
16. **Diff squashing as a formal upstream stage (reviewer 1, weekly only).** Consider making "squash N diffs to one final overlay / binary delta" a separate command that runs once per cadence and emits a single pre-merged diff that apply-changes then consumes as a daily. Orthogonal to the streaming rewrite but may be the right long-term shape if weekly is standard.

Cross-validation: `brokkr verify merge --dataset denmark` (note: name is `merge`, not `apply-changes` - the subcommand in `brokkr verify` predates the rename). Otherwise: identical output PBF byte-for-byte after the primary merge batch; tail creates that get out-of-order under the existing implementation would be the same out-of-order set. Element-level diff (decompress, compare per-blob element lists sorted by ID) is the fallback.

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

### Memory budget (projected, post-streaming rewrite)

Reviewer 2's carve-up of the ~1 GB in-flight pipeline budget (copy of the table from the Q6 follow-up subsection, duplicated here for discoverability):

| Stage | Channel | Capacity | Per-item UB | Budget |
|---|---|---:|---|---:|
| Reader → Classifier | `(seq, RawBlobFrame)` | 64 | ~1 MB compressed | 64 MB |
| Classifier workers in flight | decompress buf + `PrimitiveBlock` + rewrite scratch | 22 | ~20 MB | 440 MB |
| Classifier → Reorder | `(seq, ClassifiedItem)` | 64 | ~4 MB rewritten / 1 MB passthrough | ~128 MB |
| Reorder → Writer | `PipelineItem` | `WRITE_AHEAD=32` | ~4 MB | 128 MB |
| Writer buffered | `BufWriter` + uring buffers | - | - | ~16 MB |
| **Pipeline total** | | | | **~780 MB** |

Plus the pre-loop phase structures above (OSC overlay, DiffRanges, NodeLocationIndex.locations): **~1.5-2.3 GB**. Plus pipeline **~780 MB**. Total projected RSS under streaming: **~2.3-3.1 GB**. Well under 28 GB host limit.

## Correctness invariants

- **OSM ID ordering.** The main batch loop emits passthrough blobs in file order, rewrite blobs in file order (via the reorder buffer on the rayon mpsc channel), and gap creates before their matching blob's `min_id`. Any parallelization of reader or prefill must preserve file-order output. Phase #3's refactor must keep the reorder buffer intact.
- **`LocationsOnWays` preservation on base ways.** `write_base_way_local_with_locations` forwards raw `lat_data()` + `lon_data()` verbatim. Do not touch this path. Under `--locations-on-ways`, every base way must produce fields 9/10 in the output; the existing logic does this by calling the `_with_locations` variant whenever `loc_map.is_some()`.
- **Zero-coord fallback for missing node refs in OSC ways.** [rewrite.rs:67-70](../src/commands/merge/rewrite.rs#L67): `match locs.get(&node_id) { Some(&loc) => ..., None => locations.push((0, 0)) }`. Preserved under parallel prefill - the merged locations map has the same entries the sequential version would produce.
- **Straight `needed_set.contains` replaces `remove` in parallel prefill.** `contains` is cheaper than `remove`, and parallel workers cannot safely mutate a shared `FxHashSet`. Merge-at-end dedup covers the uniqueness semantic (a node hit by multiple workers will just insert the same `(lat, lon)` twice; last write wins, both values are identical).
- **Early-exit via `all_found()`.** Currently lets the sequential pass stop once all needed IDs are resolved. Under parallel prefill, all workers will have already claimed blobs from the schedule by the time the last needed ID is found. Either drop the early-exit (workers complete their claimed blobs; filters at schedule-construction time have already pruned most non-overlapping blobs) or add an atomic "remaining-needed" counter polled every N tuples. Probably not worth the complexity - the `overlaps_needed` filter already prunes aggressively.
- **`copy_file_range` path** on passthrough blobs ([rewrite.rs:960-970](../src/commands/merge/rewrite.rs#L960)). Under #3 the file offset must still be correct in the replacement schedule. The existing `frame.file_offset` field corresponds to `frame_offset` in the header-only scan - preserve this.
- **Reader-thread graceful shutdown.** The current reader joins at [rewrite.rs:1063](../src/commands/merge/rewrite.rs#L1063). Under #3 there is no separate reader thread to join; the schedule is consumed by the workers themselves, and shutdown is when all workers exit their claim loop.
- **Streaming-pipeline invariants (pending).** Under the streaming rewrite, the reorder actor becomes the owner of: file-order output emission, `UpsertCursors`, gap-create ordering (emit gap creates with `id < item.min_id` before the item), type-transition flush (when `last_type != current_item.kind`, flush remaining upserts of prev type), trailing-create flush on channel close (flush all kinds still not flushed, per existing [`rewrite.rs:1440-1453`](../src/commands/merge/rewrite.rs#L1440) match). Empty-base-PBF edge case (`last_type == None` forever) is covered by the existing `None` arm of `types_to_flush`; port it verbatim and write a test.
- **Backpressure propagates to the scanner (pending).** Every channel in the streaming pipeline must be bounded. If writer stalls, its receiver fills → reorder actor send blocks → classifier-to-reorder send fills → workers stop pulling → reader channel fills → pread workers stop → scanner's dispatch send blocks → scanner stops. Any unbounded channel or `par_iter().collect()` introduction defeats the chain.
- **Node→way transition barrier (pending, only if prefill fusion lands).** Under Q4 fusion, no way-blob worker may run before the per-worker coord maps are merged into `Arc<loc_map>`. The reorder actor detects `blob.kind` flipping from Node to Way in the seq-ordered stream and publishes the merged map atomically before releasing any way-blob classify work.

## Open questions

- **Actual current phase breakdown.** The numbers in this doc are inferred. First step (measurement) either confirms the ordering of #2 and #3 or flips it. If the reader thread is not actually I/O-bound at production NVMe speeds, #3's payoff shrinks.
- **Does `overlaps_needed` prune as aggressively under a daily diff as estimated?** The 30-50 % overlap estimate is heuristic. If the actual overlap ratio is 70-80 %, prefill is genuinely most of a minute of serial work and #2 matters more. If it's 10-20 %, the serial cost is already small and #2 is marginal.
- **Does `--compression none` leave phase 4 measurably free?** Under zstd or zlib, the writer pipeline's rayon compression tasks can dominate. Under `none`, they're near-passthrough. Worth confirming that phase 4 is not still a bottleneck under production settings (e.g. due to output file writes being synchronous to disk rather than to page cache).
- **Does the prefill pre-pass RSS behave under parallel decompress?** Sequential prefill reuses one decompress buffer; parallel prefill needs one per worker (~16-32 MB × 6 = ~100-200 MB transient). Fine under 30 GB, but document the per-worker overhead for completeness.
- **Interaction with `--io-uring`.** Current `spawn_reader_thread` uses `FileReader` (BufReader + File). Under #3's pread-schedule model, workers use `pread` directly; `--io-uring` would need to be plumbed into the worker's read path rather than the reader thread's. Check whether the existing io_uring integration is on the reader side or the writer side; if reader, #3 needs to preserve it.
