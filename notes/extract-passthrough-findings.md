# Extract raw passthrough: findings

## Attempts

Two approaches tried, both regressed:

**Attempt 1: Sequential BlobReader + node-only scanner (commit reverted)**
Replaced pipelined reader with sequential BlobReader. Node blobs classified
via `extract_node_tuples` (no string table). If matches, PrimitiveBlock
constructed from same decompressed data.
Result: Japan 11.9s → 16.5s (+39%). Lost pipelined decode parallelism.

**Attempt 2: Raw-frame reader + blob-level passthrough (commit reverted)**
Replaced pipelined reader with `read_raw_frame`. Node blobs whose bbox is
fully inside the extract region pass through raw (no PrimitiveBlock, no
BlockBuilder, no recompress). Way/relation blobs decoded normally.
Result: Japan 11.9s → 20.4s (+72%). Lost all decode parallelism.

## Root cause

The pipelined reader overlaps I/O + zlib decode across multiple blobs on a
rayon thread pool. Extract's consumer does heavy work (batched `par_iter`
with BlockBuilder + compression). The parallel decode pipeline feeds the
consumer fast enough to keep it busy.

Any approach that replaces the pipelined reader with sequential I/O loses
this parallelism. The passthrough savings (skip PrimitiveBlock + BlockBuilder
+ recompress for contained node blobs) don't compensate for the lost
throughput from parallel decode of way/relation blobs.

## What would work: hybrid pipeline

The pipelined reader needs a new output variant:
```rust
enum PipelineOutput {
    Decoded(PrimitiveBlock),
    Passthrough(Vec<u8>),  // raw framed bytes
}
```

The pipeline's blob filter would check containment and, for fully-contained
node blobs, skip decompression entirely — forward the raw framed bytes as
`Passthrough`. The consumer writes `Passthrough` blobs directly via
`write_raw_owned` and processes `Decoded` blobs normally.

This preserves parallel decode for way/relation/boundary blobs while
getting zero-decode passthrough for interior node blobs. The pipeline
infrastructure change is non-trivial (new enum, new pipeline path,
changed consumer API).

## Current gap vs osmium (Japan, Tokyo bbox)

| Strategy | pbfhogg | osmium | ratio |
|----------|---------|--------|-------|
| simple | 11.9s | 7.2s | 1.65x |
| complete | 12.9s | 11.0s | 1.17x |
| smart | 14.4s | 13.4s | 1.07x |

The gap narrows with strategy complexity because pbfhogg's multi-pass
algorithm is competitive. Simple is dominated by decode+re-encode overhead
where osmium copies raw protobuf groups. Smart is within 7%.

## Decision

Accept the simple gap for now. The hybrid pipeline is a dedicated
infrastructure project — not a quick optimization. Complete-ways (osmium
default) at 1.17x and smart at 1.07x are good enough for production.

Revisit if simple extract becomes a user-facing performance concern.
