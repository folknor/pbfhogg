# `pbfhogg inspect` optimization opportunities

## Implemented (commit `fc76dfb`, 2026-03-03, dm6)

### Index-only fast path (opportunities 1, 2, 4)

Added two-mode inspect: `try_index_only_scan` (header-only reads, no
decompression) with automatic fallback to `full_decode_scan` (original path).

**Mode selection:**
- `--locations` → FullDecode (needs per-way element data)
- Otherwise → IndexOnly if all blobs have indexdata, else FullDecode

**Implementation:**
- `read_blob_header_only` in `src/commands/mod.rs` — reads 4-byte len +
  BlobHeader, parses indexdata, returns `BlobHeaderInfo`. Caller skips blob data
  via `FileReader::skip()`.
- `try_index_only_scan` in `inspect.rs` — uses `BlobIndex` for element counts,
  block types, ordering segments, ID ranges (inter-blob monotonicity).
  OsmHeader blob still read fully (small, needed for metadata).
- `accumulate_from_index` helper processes one blob's index metadata.
- `HeaderMeta` struct bundles header metadata (eliminated 8-arg function).
- `BlockInfo.raw` is `Option<usize>` — `--blocks` omits Raw column in
  index-only mode.
- `--locations` percentile sort done in place (no clone).

**Output differences in index-only mode:**
- `tagged_node_count` not shown (0 in index-only, suffix omitted)
- `--blocks` table has no Raw column
- `--id-ranges` uses inter-blob monotonicity (reliable for real-world PBFs)

**Measured performance (Denmark 473 MB, 59M elements, 7397 blobs):**
- Index-only: **36ms** (header scan + skip)
- Full decode: **3.9s** (decompress + parse + element walk)
- Speedup: **109x**

### Not implemented

- **IndexPlusNodes mode** (opportunity 2 partial): decode only node blobs for
  tagged_node_count. Deferred — tagged count is a minor detail, and decoding
  node blobs (~60% of file) would reduce the speedup significantly.
- **`elements_skip_metadata()`** (opportunity 3): not needed — index-only mode
  skips element iteration entirely.
- **Buffered `--blocks` output** (opportunity 5): still uses per-line
  `println!`. Low priority.
- **Two-phase blob read** (opportunity 6): implemented the simpler variant —
  `read_blob_header_only` reads header then skips data, rather than a two-phase
  read-then-decide on the same blob.

## Scope

This note targets `src/commands/inspect.rs` and the CLI surface:

- `pbfhogg inspect <file>`
- `--blocks`
- `--id-ranges`
- `--locations`

It focuses on:

1. practical optimization opportunities that look implementable now
2. theoretical opportunities that likely need new metadata/index formats

## Current execution model (why it can be slow)

Current implementation always does a full sequential blob scan:

1. read raw frame (`read_raw_frame`)
2. for each `OsmData` blob: decompress + parse `PrimitiveBlock`
3. iterate every element and update counters/flags

Even when no expensive optional flag is requested, inspect still fully decodes all data blobs. This is correct and simple, but it leaves performance on the table for indexed files.

## Cost profile by flag

## Base inspect (no extra flags)

Still pays full decode + full element walk to compute:

- nodes/ways/relations counts
- tagged node count
- block type sequence (ordering)
- per-type block counters

## `--blocks`

Adds per-block output generation. The heavy cost often becomes output volume (`println!` per block), not only decoding.

## `--id-ranges`

Adds per-element ID monotonicity and min/max tracking. This is CPU-light relative to full decode, but still requires touching all IDs.

## `--locations`

Most expensive optional path:

- iterates all ways
- calls `node_locations().count()` for each
- stores counts in `Vec<u32>`
- clones + sorts the vector to compute min/max/median/p99

This adds both CPU and memory pressure.

## High-confidence opportunities

## 1) Add an index-only fast path for cheap modes

When `frame.index` is present for all blobs and flags do not require per-element details, use blob index metadata directly:

- type/count from `BlobIndex`
- ordering segments from `BlobIndex.kind`
- block totals from raw frame sizes

Potentially decode only node blobs to preserve exact `tagged_node_count` (or skip this if output format changes are acceptable).

Impact:

- large reduction in decompression/parsing work on indexed files
- biggest win for files with many way/relation blobs

## 2) Split scan into capability-driven modes

Instead of one always-decode path, choose scan strategy:

- `Mode::IndexOnly` (no decode)
- `Mode::IndexPlusNodes` (decode node blobs only)
- `Mode::FullDecode` (current behavior)

This keeps behavior explicit and avoids accidental decode work.

## 3) Replace `elements()` with `elements_skip_metadata()` where possible

Inspect does not use metadata payload fields for counting/order/location stats.
Switching to skip-metadata iteration can reduce parsing overhead.

Likely safe targets:

- base element counting
- id-range scans
- location scans

## 4) Optimize `--locations` percentile computation

Current path stores all counts, clones, sorts.
Alternatives:

- exact histogram (`HashMap<u32, u64>` or dense vec if bounded) + percentile reconstruction
- single vector, sort in place (remove clone)

First step with minimal behavior change:

- sort `coord_counts` in place at report time (no clone)

## 5) Reduce `--blocks` output overhead

For very large files, per-line `println!` can dominate.
Use buffered output for the block table:

- build into `String` chunks and `print!` once per chunk
- or write through a locked `stdout` writer

## Medium-confidence opportunities

## 6) Header-first two-phase blob read for selective skip

Current `read_raw_frame` reads full frame bytes eagerly. For index-assisted inspect modes, a two-phase read can:

1. read header + indexdata
2. decide whether blob body is needed
3. skip blob payload when not needed

This reduces memory traffic and userspace copies in fast modes.

## 7) Parallel decode path for heavy flag combinations

For `--locations` and `--id-ranges --locations`, throughput might improve with pipelined/parallel decode (`ElementReader::into_blocks_pipelined`) plus ordered reduction.

Complexity risk:

- ordering segment logic is order-sensitive
- monotonic checks are order-sensitive

Feasible design:

- parallel decode, sequential ordered consume (pipeline/reorder buffer)

## 8) Per-flag targeted scans

If only `--locations` is requested, skip relation-heavy work where possible:

- still count relations via indexdata when present
- decode only way blobs for location stats
- decode nodes only if needed for other outputs

Requires a clear contract for what base inspect must always print exactly.

## Theoretical / uncertain opportunities

These are plausible but need validation or metadata format changes.

## A) Extend indexdata for inspect-specific summaries

Possible new fields per blob:

- tagged-node count
- monotonic flag within blob
- way location-presence summary (ways with/without locations)
- coarse quantile sketch for coords-per-way

Pros:

- inspect could become mostly index-only on indexed files

Cons:

- larger blob headers
- new index versioning and compatibility work
- additional write-time CPU overhead

Uncertainty: medium-high (tradeoff vs indexdata size overhead may not be worth it).

## B) Wire-level specialized scanners instead of full block parse

Like `scan_block_ids`, add targeted scanners:

- tagged-node scanner
- way node_locations-count scanner

Pros:

- avoids full protobuf object construction

Cons:

- more complex, error-prone wire parsing code
- maintenance burden

Uncertainty: medium (can be fast, but code complexity may outweigh gains).

## C) Approximate quantiles for `--locations`

Use t-digest/KLL-style sketch to avoid storing/sorting all counts.

Pros:

- bounded memory

Cons:

- p99/median no longer exact unless clearly documented

Uncertainty: medium-high (depends whether approximation is acceptable for inspect semantics).

## D) Auto strategy selection based on file/index characteristics

At startup, detect:

- fraction of indexed blobs
- requested flags

Then select decode strategy dynamically.

Uncertainty: low technically, medium in maintenance complexity.

## Suggested implementation order

1. ~~`elements_skip_metadata()` swap where valid~~ — not needed (index-only skips elements entirely)
2. ~~`--locations` no-clone percentile path~~ — **done**
3. buffered `--blocks` output
4. ~~capability-driven scan modes + index-only path~~ — **done** (IndexOnly + FullDecode)
5. ~~optional two-phase read for fast modes~~ — **done** (header-only read + skip)
6. optional pipeline decode for heavy modes

## Measurement plan

Use repeatable timing runs on at least one indexed and one non-indexed dataset, comparing:

- base inspect
- `--id-ranges`
- `--locations`
- `--blocks`
- `--id-ranges --locations`

Track:

- wall time
- peak RSS
- output size (for `--blocks`)

Use identical command lines before/after each change and keep runs sequential (no parallel benchmark execution).
