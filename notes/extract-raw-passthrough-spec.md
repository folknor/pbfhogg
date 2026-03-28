# Extract raw passthrough for fully-contained node blobs

## Problem

Extract simple is 1.65x slower than osmium at Japan scale (11.9s vs 7.2s).
The gap is dominated by PrimitiveBlock construction + BlockBuilder re-encode
for matching elements.

## Approach: blob-level raw passthrough

Three tiers of node blob handling:

| Tier | Condition | Action | Cost |
|------|-----------|--------|------|
| 1 (skip) | Blob bbox outside extract region | Skip entirely | Zero |
| 2 (passthrough) | Blob bbox fully inside extract region | Decompress → scan IDs → write raw | Decompress only |
| 3 (decode) | Blob bbox partially overlaps | Full PrimitiveBlock → classify → re-encode | Full |

Way and relation blobs always use tier 3 (element-level classification).

## Scope (v1)

Per 5-reviewer consensus:
- `extract_simple_single_pass` only (sorted input)
- `Region::Bbox` only (not polygon — bbox containment ≠ polygon containment)
- `clean` must be no-op (raw passthrough preserves metadata)
- Node blobs with v2 bbox indexdata only
- Fallback to tier 3 for anything else

## Architecture

Raw-frame reader path (like merge/ALTW passthrough), NOT `into_blocks_pipelined`.
Sequential BlobReader with three dispatch paths per blob:

```
BlobReader (sequential, raw frames)
  │
  ├─ Node blob, bbox fully inside extract region, no clean:
  │    decompress → extract_node_tuples → populate bbox_node_ids
  │    write raw framed blob via write_raw (from BlobReader's raw bytes)
  │    [Tier 2: skip PrimitiveBlock + BlockBuilder + recompress]
  │
  ├─ Node blob, partial overlap or no indexdata:
  │    decompress → PrimitiveBlock → classify → batch → par_iter write
  │    [Tier 3: current path]
  │
  ├─ Way blob:
  │    decompress → PrimitiveBlock → classify → batch → par_iter write
  │
  └─ Relation blob:
       decompress → PrimitiveBlock → classify → batch → par_iter write
```

### Ordering constraint

Raw passthrough blobs and decoded batches must be written in file order.
Before writing a raw passthrough blob, flush any pending decoded batch.
Before adding to a decoded batch after a raw passthrough, no flush needed
(the raw blob is already written).

### BlobReader raw frame access

`BlobReader::next()` returns `Blob` which owns compressed data but not
the framed bytes (4-byte length + BlobHeader + Blob envelope). Need a
way to get the raw framed bytes for `write_raw`.

Options:
- `Blob::to_raw_frame()` — reconstruct framed bytes from header + blob
- Track blob start/end offsets and pread the raw frame
- New `BlobReader` method that returns `(Blob, Vec<u8>)` with raw bytes

The simplest: use `BlobReader` with a seekable reader. For tier 2 blobs,
after classification, seek back and read the raw frame. Or: the blob
already has the compressed data — reframe it with the original header.

Actually: `reframe_raw_with_index` in writer.rs already reframes a raw
blob. And `Blob` stores the `WireBlobHeader` + `WireBlob`. We can
reconstruct the framed output from these. But that's re-encoding the
frame, not true passthrough.

Simplest approach: read raw frame bytes alongside the Blob. Track the
byte range `[blob_start_offset..blob_end_offset]` and pread when needed
for passthrough. The `Blob` already has `offset()` (start of the frame).
After reading, `BlobReader.offset` points past the blob. The frame bytes
are `input[blob.offset()..reader.offset()]`.

### New infrastructure needed

1. **`BlobBbox::contains(&self, inner: &BlobBbox) -> bool`** — true when
   inner is fully inside self. Trivial addition to blob_index.rs.

2. **Node scanner non-dense support** — current `extract_node_tuples`
   only handles DenseNodes (field 2). Need to also handle plain Nodes
   (field 1) for correctness on non-dense blobs. Or: fall back to tier 3
   for non-dense blobs (check `BlockType::Nodes` vs `BlockType::DenseNodes`
   from indexdata).

3. **Raw frame pread** — for tier 2 blobs, need the original framed bytes.
   Use `FileExt::read_exact_at(blob_offset, frame_size)` from a shared
   file handle (same pattern as P2b-v2).

## Expected impact

Node blobs are ~60% of a PBF. In a regional bbox extract from a sorted
PBF, the spatial blob filter skips most node blobs (tier 1). Of those
that pass intersection, most in the interior are fully contained (tier 2).
Only boundary blobs need full decode (tier 3).

Tier 2 saves: PrimitiveBlock construction + BlockBuilder + output zlib.
Still pays: decompress (for ID scan) + raw frame pread.

Estimate: if 60-70% of intersection node blobs go to tier 2, simple
extract could drop from 11.9s to ~7-8s at Japan scale.

## Validation

1. `brokkr verify extract --dataset denmark` — all three strategies PASS
2. Japan benchmark: target < 9s for simple
3. Test with `--clean` flags — verify tier 2 is disabled
4. Test with polygon region — verify tier 2 is disabled

## Reviewer sign-off

5/5 codex reviewers approved (bugs, perf, arch, correctness, planet).
Key constraints: bbox-only, no clean, node blobs only, raw-frame path.
