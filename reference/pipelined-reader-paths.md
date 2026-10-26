# Pipelined reader paths

Reference for every current caller of the ordered pipelined reader
surfaces on `ElementReader`: the plain
`into_blocks_pipelined` / `for_each_block_pipelined` / `for_each_pipelined`
and the fused `for_each_fused_block` / `for_each_fused_blob`. Both are
one engine, `run_pipeline` in `src/read/pipeline.rs`, generic over the
`DecodeTransform` that runs on the decode worker: the plain surfaces
pass the identity transform `Blocks`, the fused ones a command's
transform. Workload profile, decode shape, and the rule for adding or
converting callers.

## Background

Both surfaces spawn a private rayon decode pool, sized by
`host::sized_workers` from the `DECODE_WORKERS_*` tunables in
`src/tunables.rs` (host parallelism minus a reserve, with a floor and a
fallback), unless the caller sets a thread count. It decompresses and
parses blobs into `PrimitiveBlock`s and delivers them in file order to a
consumer serialized on the calling thread.

**Plain** (`into_blocks_pipelined` and friends): the decode pool
produces `PrimitiveBlock`s and the consumer processes them directly.
Callers today are the `read` bench's pipelined mode, `time-filter`
history, and `build-geocode-index` pass 1.

**Fused** (`for_each_fused_block`, landed 2026-07-12, ADR-0009 in
`decisions/0009-fused-command-transforms.md`): the per-block command
transform runs INSIDE the decode worker - decode and transform on the
same thread - and only the compact `(Vec<OwnedBlock>, stats)` result
crosses to the ordered consumer. There is no second rayon dispatch and
no 64-block materialization. The earlier shape that batch-dispatched
decoded blocks to the **global** rayon pool via `par_iter` (~14 threads
on an 8-core host) is gone; the fused callers below carry
their transform on the decode workers instead. Measured wins at high
blob count: getid `--add-referenced` -7.7 %, getparents FullScan
-6.5 %, tags-filter `-R` -7.0 % (planet-8k, 2026-07-12).

Alternative primitives used elsewhere in the codebase:

- `parallel_classify_phase` / `parallel_classify_accumulate`
  (`src/scan/classify.rs`) - pread workers, no separate decode pool,
  no cross-thread-free churn, no oversubscription. Lower memory
  ceiling and lower thread count than pipelined decode.
- `parallel_classify_phase_ordered` and the ordered pool beneath it
  (`src/ordered_pool.rs`) - the same pread workers, results delivered
  in schedule order and bounded by an admission window (see
  [pipeline.md](pipeline.md) "Ordered pread pool").
- Raw `Frames::next_raw` / `HeaderWalker` (`src/read/frame.rs`,
  `src/read/header_walker.rs`) - sequential or pread-based,
  bypass the decode pool entirely.

## Invariants

**Retention is solved.** `DecompressPool` in `src/read/blob.rs`
(commit `8f6999b`, 2026-03-30) recycles decompression buffers via
`PooledBuffer::drop` instead of cross-thread freeing. The pool caps
its buffer count and per-buffer retained capacity
(`DECOMPRESS_POOL_*` in `src/tunables.rs`). Any new pipelined caller
inherits this for free; no action needed.

**Thread oversubscription is a known architectural concern** but
sequential conversion is *not* the answer. Measured evidence: a
sequential conversion of `getparents` (commit `c912e4d`, reverted)
regressed 4.7× on Denmark (1400 ms vs 300 ms baseline) because
decompression is the dominant cost, not the per-block processing the
sequential path would have left unchanged.

**Rule: do not convert any pipelined path to sequential decode.**
If the 4.7× regression didn't flip for the lightest workload
(getparents - mostly `IdSet::get`), it won't flip for anything
heavier. Adding to the list of pipelined callers is fine; removing
from it is not.

**Admission is bounded.** The decode stage gates spawning decode
tasks (inside a rayon scope on the private decode pool) behind
`decode_ahead` admission permits. A permit is released only after the
decoded item is delivered from the reorder buffer, so queued,
decoding, blocked-on-send, channel-held, and reorder-held items are
capped together. `pipeline_reorder_high_water` records filled slots
and `pipeline_reorder_window_high_water` records the old gap-inclusive
window diagnostic. Those and the other `pipeline_*` counters are
process totals, never reset: every pipelined read adds into one set and
emits the whole set after its stages have joined. Every counter only
grows, so the maximum emitted value of each name is the process total
(for sequential reads, simply the last emission; concurrent reads can
emit out of order), which is how `brokkr sidecar --stalls` reads
`*_wait_ns`. They are
not per-read numbers by design; `src/read/pipeline_metrics.rs` says why.
Dropping `PipelinedBlocks` or returning an error
from a block closure closes the decoded receiver before the scoped
threads join, so stage 1 stops promptly instead of reading the rest of
the file.

## Callers

### `getid --add-referenced` pass 2

- **Surface:** fused (`for_each_fused_block`)
- **Path:** `src/commands/getid/mod.rs`, `--add-referenced` mode only
- **Scale:** the plain include / invert path is the common one and uses
  `HeaderWalker` + raw-frame reads, no pipelined decode at all.
- **Per-block work:** light - `IdSet::get` per element, most
  skipped, a few re-encoded through `BlockBuilder`

### `getparents` FullScan arm

- **Surface:** fused (`for_each_fused_block`)
- **Path:** `src/commands/getparents/mod.rs`, FullScan arm
- **Scale:** high-blob-count encodings only. ADR-0006 dispatches
  low-blob-count planet (~50 k blobs) to the `HeaderWalker` arm; the
  FullScan arm runs at ~150 k+ estimated OSMData blobs (Geofabrik-style
  8k encodings).
- **Per-block work:** light - membership check per element, matched
  parents re-encoded through `BlockBuilder`

### `tags-filter -R <expr>` single-pass

- **Surface:** fused (`for_each_fused_block`)
- **Path:** `src/commands/tags_filter/mod.rs` single-pass branch
- **Scale:** every element touched
- **Per-block work:** heavy - tag iteration, expression match
  against N expressions via `element_matches`, matching elements
  re-encoded through `BlockBuilder`
- **Related:** the two-pass production path (`--add-referenced-ways`)
  is separate and uses pread workers via `parallel_classify_phase`;
  it doesn't go through this code.

### `add-locations-to-ways` decode-all fallback

- **Surface:** fused (`for_each_fused_block`)
- **Path:** `src/commands/altw/mod.rs`, `write_output_decode_all`
- **Scale:** triggered only by `--force` on non-indexed PBFs.
  Production `--index-type external` uses pread workers.
- **Per-block work:** heaviest of any pipelined caller. Every
  element processed:
  - Nodes: tag check + conditional `BlockBuilder` write
  - Ways: collect refs, look up every node location from the
    index or pre-resolved map, `add_way_with_locations`
  - Relations: full member collection + `BlockBuilder` write

### `digest`

- **Surface:** fused (`for_each_fused_blob`, with `DigestTransform`)
- **Path:** `src/commands/digest.rs`
- **Scale:** every blob; the transform sees every blob, not only
  element blocks, and returns a per-blob summary
- **Per-block work:** the summary is computed on the decode worker;
  only the summary crosses to the ordered consumer, which absorbs it

### `time-filter` history path

- **Surface:** plain (`for_each_pipelined`)
- **Path:** `src/commands/time_filter/mod.rs`, history mode
- **Scale:** history PBF scan; every element touched
- **Per-block work:** medium - timestamp compare, `PendingGroup`
  latest-version tracking, re-encode survivors through `BlockBuilder`

### `read` bench, pipelined mode

- **Surface:** plain (`for_each_pipelined`)
- **Path:** `cli/src/lib.rs`, `BenchReadMode::Pipelined`
- **Scale:** whole file; counts elements by kind
- **Per-block work:** trivial

### `build-geocode-index` pass 1

- **Surface:** plain (`for_each_block_pipelined`)
- **Path:** `src/geocode_index/builder/pass1.rs`, `only_relations()`
- **Scale:** filtered relation scan
- **Per-block work:** bounded relation metadata extraction

## Callers that use something else (deliberately not pipelined)

These paths were evaluated and chose non-pipelined primitives, and
should stay that way:

- `inspect --nodes`, `inspect --tags` - `parallel_classify_accumulate`
  / `_phase` (pread workers)
- `inspect` default / `inspect --indexed` - `HeaderWalker`
  (pread-only header walk)
- `check --refs`, `check --ids --full` - `parallel_classify_phase`
- `check --ids` streaming default - `parallel_classify_phase`
- `tags-filter` two-pass - pread workers
- `cat --type` / `cat --clean` re-encoding - pread classification
  and writer-side reframe paths
- `extract` (simple / complete / smart / multi) - pread workers
- `sort` - direct pread per blob
- `diff`, `derive_changes` (aka `diff --format osc`) - shard-based pread
  for indexed inputs at every `-j` (one shard at `-j 1`); `StreamingBlocks`
  for unindexed inputs
- `apply-changes` - descriptor-first scanner + worker pool, pread
  per blob (`src/commands/apply_changes/`)
- `renumber` - external-join style stages (`pass1.rs`, `stage2.rs`,
  `wire_rewrite.rs`, `relations.rs`). Previously a pipelined caller;
  converted to pread workers when the external-join rewrite landed.
- ALTW external stages 1-4 (`src/commands/altw/external/`) -
  pread workers
- `getid` single-pass (include / invert) - `HeaderWalker` +
  raw-frame reads
- `getparents` walker arm - `HeaderWalker` (pread-only). The FullScan
  arm IS a fused pipelined caller (see Callers above); ADR-0006 picks
  the arm by blob count.
- `time-filter` snapshot path - `parallel_classify_phase_ordered`

## Adding a new pipelined caller

Decision order:

1. **Is per-blob work a pure function of one blob?** If yes, prefer
   `parallel_classify_phase` (per-blob result),
   `parallel_classify_phase_ordered` (per-blob result needed in schedule
   order; never reorder `parallel_classify_phase`'s output by hand, which
   bounds nothing) or `parallel_classify_accumulate` (per-worker state).
   Lower memory
   ceiling, no oversubscription, no double rayon pool. Accumulate
   costs worker-count × per-worker state size at peak, so pick
   phase when the accumulator grows with blob count. See the safety
   envelope in the classify module doc.
2. **Does the consumer need residuals or streaming merge across
   blobs?** (e.g. merge-join, ID-remap with cross-blob state). If
   yes, pipelined decode is the right primitive. Retention is
   handled for you by `DecompressPool`.
3. **Does the consumer run a per-block transform (decode, then
   re-encode or filter)?** Use the fused surface `for_each_fused_block`:
   the transform runs on the decode worker and only the compact
   `(Vec<OwnedBlock>, stats)` result crosses to the ordered consumer -
   no second rayon pool, no 64-block materialization. This replaced the
   earlier decode-then-`par_iter` shape (see the four fused callers
   above). A transform that must also see non-element blobs uses
`for_each_fused_blob`.

Do not convert an existing pipelined caller to sequential decode
"to avoid oversubscription" without a fresh benchmark showing the
getparents conclusion has flipped. The 4.7× Denmark regression is
the gate.
