# Pipelined reader paths

Reference for every current caller of `ElementReader::into_blocks_pipelined`
(`src/read/pipeline.rs`). Workload profile, batch shape, and the rule
for adding or converting callers.

## Background

`into_blocks_pipelined` spawns a private rayon decode pool
(`available_parallelism() - 2` threads) that decompresses and parses
blobs into `PrimitiveBlock`s and sends them through a channel. Most
consumers then batch-dispatch the blocks to the **global** rayon pool
via `par_iter`. Two concurrent pools means ~14 threads on an 8-core
host, which is deliberate: the decode pool stays busy producing the
next batch while the global pool processes the current one.

Alternative primitives used elsewhere in the codebase:

- `parallel_classify_phase` / `parallel_classify_accumulate`
  (`src/scan/classify.rs`) - pread workers, no separate decode pool,
  no cross-thread-free churn, no oversubscription. Lower memory
  ceiling and lower thread count than pipelined decode.
- Raw `read_raw_frame` / `HeaderWalker` (`src/read/raw_frame.rs`,
  `src/read/header_walker.rs`) - sequential or pread-based,
  bypass the decode pool entirely.

## Invariants

**Retention is solved.** `DecompressPool` in `src/read/blob.rs`
(commit `8f6999b`, 2026-03-30) recycles decompression buffers via
`PooledBuffer::drop` instead of cross-thread freeing. The pool caps
at 64 buffers × 4 MB retained capacity. Any new pipelined caller
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

## Callers

### `getid --add-referenced` pass 2

- **Path:** `src/commands/getid/mod.rs`, two-pass mode only
- **Scale:** niche. The single-pass include / invert path is the
  common one and uses `HeaderWalker` + raw-frame reads, no pipelined
  decode at all.
- **Per-block work:** light - `IdSet::get` per element, most
  skipped, a few re-encoded through `BlockBuilder`

### `tags-filter -R <expr>` single-pass

- **Path:** `src/commands/tags_filter/mod.rs` single-pass branch
- **Scale:** every element touched
- **Per-block work:** heavy - tag iteration, expression match
  against N expressions via `element_matches`, matching elements
  re-encoded through `BlockBuilder`
- **Related:** the two-pass production path (`--add-referenced-ways`)
  is separate and uses pread workers via `parallel_classify_phase`;
  it doesn't go through this code.

### `add-locations-to-ways` decode-all fallback

- **Path:** `src/commands/altw/dense.rs` at the decode-all branch
- **Scale:** triggered only by `--force` on non-indexed PBFs.
  Production `--index-type external` uses pread workers.
- **Per-block work:** heaviest of any pipelined caller. Every
  element processed:
  - Nodes: tag check + conditional `BlockBuilder` write
  - Ways: collect refs, look up every node location from the
    index (dense mmap or pre-resolved map), `add_way_with_locations`
  - Relations: full member collection + `BlockBuilder` write
  - Sparse index: `resolve_batch_locations` pre-resolves all
    way-node coordinates via a sorted sequential scan before
    par_iter

### `cat --type` re-encoding branch

- **Path:** `src/commands/cat/mod.rs`, non-passthrough side of the
  split (blobs that don't fully match the filter and must be
  re-encoded)
- **Scale:** full-match blobs go through the pread passthrough
  schedule; this branch handles partial-match blobs only
- **Per-block work:** heaviest batch work in the codebase. Each
  rayon worker does type filter + `BlockBuilder` re-encode + zlib
  compress + `frame_blob_pipelined` - all inside the `par_iter`.
  Compression is CPU-heavy enough that the pipelined decode overlap
  genuinely wins.

### `getparents`

- **Path:** `src/commands/getparents/mod.rs`
- **Scale:** whole-file scan for ways / relations referencing a
  given ID set
- **Per-block work:** light (ID-set lookups)
- **History:** sequential conversion attempted in `c912e4d`,
  reverted - 4.7× regression on Denmark. This result is the
  load-bearing evidence for the "do not convert" rule above.

### `time-filter`

- **Path:** `src/commands/time_filter/mod.rs` - both history mode
  (`for_each_pipelined`) and snapshot mode (`into_blocks_pipelined`
  + batch dispatch)
- **Scale:** whole-file scan; every element touched
- **Per-block work:** medium - timestamp compare, `PendingGroup`
  latest-version tracking, re-encode survivors through `BlockBuilder`

### `check --ids` streaming default

- **Path:** `src/commands/check/verify_ids.rs`,
  `reader.for_each_pipelined(...)`
- **Scale:** whole-file scan
- **Per-block work:** light (ID ordering + uniqueness check)
- **Related:** `--full` (bitmap duplicate detection) switches to
  `parallel_classify_phase`; see the list below.

## Callers that use something else (deliberately not pipelined)

These paths were evaluated and chose non-pipelined primitives, and
should stay that way:

- `inspect --nodes`, `inspect --tags` - `parallel_classify_accumulate`
  / `_phase` (pread workers)
- `inspect` default / `inspect --indexed` - `HeaderWalker`
  (pread-only header walk)
- `check --refs`, `check --ids --full` - `parallel_classify_phase`
- `tags-filter` two-pass - pread workers
- `extract` (simple / complete / smart / multi) - pread workers
- `sort` - direct pread per blob
- `diff`, `derive_changes` (aka `diff --format osc`) - `StreamingBlocks`
  for the sequential path; shard-based pread for the parallel path
  (`DiffOptions::num_shards >= 2`)
- `apply-changes` - descriptor-first scanner + worker pool, pread
  per blob (`src/commands/apply_changes/`)
- `renumber` - external-join style stages (`pass1.rs`, `stage2.rs`,
  `wire_rewrite.rs`, `relations.rs`). Previously a pipelined caller;
  converted to pread workers when the external-join rewrite landed.
- ALTW external stages 1-4 (`src/commands/altw/external/`) -
  pread workers
- `getid` single-pass (include / invert) - `HeaderWalker` +
  raw-frame reads

## Adding a new pipelined caller

Decision order:

1. **Is per-blob work a pure function of one blob?** If yes, prefer
   `parallel_classify_phase` (per-blob result) or
   `parallel_classify_accumulate` (per-worker state). Lower memory
   ceiling, no oversubscription, no double rayon pool. Accumulate
   costs worker-count × per-worker state size at peak, so pick
   phase when the accumulator grows with blob count. See the safety
   envelope in the classify module doc.
2. **Does the consumer need residuals or streaming merge across
   blobs?** (e.g. merge-join, ID-remap with cross-blob state). If
   yes, pipelined decode is the right primitive. Retention is
   handled for you by `DecompressPool`.
3. **Is the consumer dispatching blocks to a par_iter?** If yes,
   pipelined decode is fine and the decode/process overlap is
   measurably positive (see cat and ALTW decode-all).

Do not convert an existing pipelined caller to sequential decode
"to avoid oversubscription" without a fresh benchmark showing the
getparents conclusion has flipped. The 4.7× Denmark regression is
the gate.
