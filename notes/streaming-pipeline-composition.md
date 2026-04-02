# Streaming pipeline composition

## Problem

The production pipeline for a planet update runs 4 commands sequentially,
each reading from and writing to disk:

```
pbfhogg cat input.pbf -o indexed.pbf              # 497s, add indexdata
pbfhogg add-locations-to-ways indexed.pbf -o altw.pbf  # 1462s, coord enrichment
pbfhogg apply-changes altw.pbf diff.osc.gz -o merged.pbf  # 762s, daily diff
pbfhogg build-geocode-index merged.pbf -o geocode/  # 1346s, geocode index
```

Total: ~4067s (67.8 min), plus intermediate disk I/O:
- `indexed.pbf`: ~87 GB written, ~87 GB read
- `altw.pbf`: ~100 GB written, ~100 GB read
- `merged.pbf`: ~87 GB written, ~87 GB read

That's ~461 GB of intermediate I/O that could be avoided if commands
could pipe data internally without PBF encode/decode.

## Why shell pipes don't work

PBF is a binary format with blob-level framing. Unix pipes could
theoretically stream PBF bytes between processes:

```
pbfhogg cat input.pbf | pbfhogg add-locations-to-ways - -o altw.pbf
```

But:
1. **Multi-pass commands can't pipe.** ALTW, extract (complete/smart),
   tags-filter (two-pass) all make multiple passes over the input.
   A pipe is single-pass — can't seek back.
2. **Parallel decode needs random access.** `parallel_classify_phase`
   uses pread for parallel blob access. Pipes are sequential.
3. **Backpressure.** If the producer is faster than the consumer, the
   OS pipe buffer (64 KB - 1 MB) fills up and the producer blocks.
   This is actually fine for streaming, but the multi-pass issue
   dominates.

## What CAN be composed

Some pairs of commands are composable because both are single-pass
and the output of one is the input of the other:

| Producer | Consumer | Composable? | Bottleneck |
|----------|----------|-------------|------------|
| cat (indexdata) | ALTW | No — ALTW is multi-pass |
| cat (indexdata) | apply-changes | Partially — merge is single-pass on base |
| ALTW | build-geocode-index | No — geocode is multi-pass |
| ALTW | extract simple | No — extract uses pread |
| apply-changes | cat (indexdata) | Yes! Both single-pass |
| cat (type filter) | any consumer | Yes — single-pass |

The only clearly composable pair is `apply-changes → cat` (add indexdata
to the merge output). This saves one full PBF write+read (~87 GB).

## Library-level composition

Instead of command-level piping, compose at the library level:

```rust
// Instead of:
//   pbfhogg apply-changes base.pbf diff.osc -o merged.pbf
//   pbfhogg cat merged.pbf -o indexed.pbf
// Do:
//   let merged_blocks = apply_changes_streaming(base, diff);
//   cat_with_indexdata(merged_blocks, output);
```

The producer yields `PrimitiveBlock`s (or raw blob bytes) via an
iterator. The consumer reads from that iterator instead of from disk.
No intermediate file.

### The `BlockStream` abstraction

```rust
trait BlockStream {
    /// Yield the next OsmData blob.
    fn next_block(&mut self) -> Option<Result<StreamItem>>;
}

enum StreamItem {
    /// Decoded PrimitiveBlock — consumer can iterate elements.
    Decoded(PrimitiveBlock),
    /// Raw compressed blob — consumer can write directly.
    RawBlob(Vec<u8>, BlobIndex),
}
```

Commands that produce PBF output would implement `BlockStream` as an
alternative to writing to disk. Commands that consume PBF input would
accept `impl BlockStream` as an alternative to reading from disk.

### Which commands could produce a BlockStream?

- `cat` (type filter): yes — single-pass, yields filtered blocks
- `apply-changes`: yes — single-pass merge, yields merged blocks
- `sort`: yes — single-pass output after permutation
- `extract simple`: partially — the classification is multi-pass,
  but the write phase is single-pass

### Which commands could consume a BlockStream?

- `cat` (indexdata): yes — single-pass passthrough, adds indexdata
- `build-geocode-index`: no — multi-pass (4 passes over the data)
- `add-locations-to-ways`: no — multi-pass (dense: 2 passes,
  external: 4 stages with random access)
- `extract`: no — multi-pass with pread
- `inspect`: yes — single-pass scan
- `check-refs`: yes — single-pass scan
- `diff`: partially — single-pass per type, but needs two inputs

### Composable pipelines

Only single-pass consumers can be composed with single-pass producers.
The multi-pass commands (ALTW, extract, geocode builder) fundamentally
need random access or multiple passes over the same data.

**Feasible compositions:**
1. `apply-changes → cat (indexdata)`: merge output → add indexdata.
   Saves one 87 GB write+read. Implementation: merge's output path
   already produces blob bytes — add indexdata scanning inline.
2. `cat (type filter) → inspect/check-refs`: filter → analyze.
   Saves one write+read. Minor — inspection is fast anyway.
3. `sort → cat (indexdata)`: sort output → add indexdata.
   Saves one write+read.

**Infeasible without architectural changes:**
- `cat → ALTW`: ALTW needs multiple passes over node data
- `ALTW → geocode builder`: geocode needs 4 passes
- `ALTW → extract`: extract needs pread for parallel classification

## Better approach: inline indexdata in merge

Instead of `apply-changes → cat`, make `apply-changes` produce
indexed output directly. This is simpler than a general composition
framework and captures the most valuable composition.

`apply-changes` already writes via `PbfWriter`, which already
computes indexdata for every `write_primitive_block_owned` call. The
output PBF already has indexdata embedded. Wait — let me check:

Actually, looking at the code, `write_primitive_block_owned` accepts
a pre-computed `BlobIndex` and embeds it as indexdata. The merge
command calls `take_owned()` on BlockBuilder which returns the
serialized block + computed BlobIndex. So the merge output already
has indexdata.

**The `apply-changes → cat` composition is already unnecessary** —
merge already produces indexed PBFs. The `cat` step for indexdata
was needed for the initial planet import (raw Geofabrik PBF → indexed),
not for the daily merge pipeline.

## Revised assessment

The streaming composition opportunity is smaller than it appears:

1. **Merge already produces indexed output** — no `cat` step needed
   in the daily pipeline
2. **Multi-pass commands can't consume streams** — ALTW, extract,
   geocode builder all need random access
3. **The initial `cat` for indexdata is a one-time cost** — once the
   planet PBF is indexed, the merge output preserves indexdata

The remaining composition opportunity is:
- `sort → (write with indexdata)`: sort already writes via PbfWriter
  which embeds indexdata. Already composed.
- `extract → (write with indexdata)`: extract already uses
  `write_primitive_block_owned` with pre-computed indexdata. Already
  composed.

**Conclusion:** The codebase already does the most valuable
composition (inline indexdata in all write paths). General streaming
pipeline composition would be a large engineering effort with limited
practical benefit, because the expensive commands (ALTW, extract,
geocode builder) are inherently multi-pass.

## What would actually help

Instead of composition, the remaining I/O reduction opportunities are:

1. **In-memory pipeline for small extracts**: for Denmark-sized
   extracts, keep the ALTW output in memory and pass it directly
   to build-geocode-index. Feasible at ~500 MB - 2 GB scale.
   Not feasible at planet scale (87 GB).

2. **Shared mmap**: commands that read the same PBF multiple times
   benefit from the page cache. The first read is I/O-bound, but
   subsequent reads hit cached pages. This already works transparently
   — no code changes needed.

3. **Zstd for internal PBFs**: 3-5x faster decompress means the
   multi-pass commands spend less time on each pass. See
   [zlib-level-tuning.md](zlib-level-tuning.md).

4. **Reduce pass count**: if ALTW could be done in a single pass
   (not feasible for dense/external join, but maybe for a streaming
   in-memory approach on small datasets), composition would work.
