# Tag Index — PBF Blob-Level Tag Metadata

## Problem

PBF blobs contain ~8000 elements each (MAX_ENTITIES_PER_BLOCK in
block_builder.rs:22), compressed with zlib. To filter by tag (e.g. "give me
all ways with highway=primary"), you must decompress every blob, parse every
element, and test every tag. For selective filters, 90%+ of blobs contain zero
matching elements — but you still paid the decompression cost.

## Precedent: existing indexdata field

pbfhogg already writes a 26-byte index into BlobHeader field 2 (`indexdata`).

### Generation

`scan_block_ids()` (blob_index.rs:142-166) walks the raw PrimitiveBlock wire
format without a full parse. It detects element type from PrimitiveGroup fields
(DenseNodes=field 2, Way=field 3, Relation=field 4, Node=field 1) and extracts
element IDs using delta decoding for dense nodes or direct extraction for
ways/relations. Produces a `BlobIndex` struct:

```rust
pub struct BlobIndex {
    pub kind: ElemKind,   // Node, Way, Relation
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
}
```

### Serialization

Fixed 26 bytes (blob_index.rs:33-45):

```
version (u8)           — 0x01
element_type (u8)      — 0=Node, 1=Way, 2=Relation
min_id (i64 LE)
max_id (i64 LE)
count (u64 LE)
```

### Integration into write path

In `write_primitive_block()` (writer.rs:332-355):

- **Pipelined path**: rayon task calls `scan_block_ids(&uncompressed)` on the
  serialized PrimitiveBlock bytes, passes result to `frame_blob_into()`
- **Sync path**: same on calling thread

`encode_blob_header_into()` (writer.rs:802-817) encodes the header:

```rust
fn encode_blob_header_into(
    blob_type: &str, datasize: i32, indexdata: Option<&[u8]>, buf: &mut Vec<u8>,
) {
    buf.clear();
    encode_bytes_field(buf, 1, blob_type.as_bytes()); // field 1: type
    if let Some(data) = indexdata {
        encode_bytes_field(buf, 2, data);             // field 2: indexdata
    }
    encode_int32_field(buf, 3, datasize);             // field 3: datasize
}
```

### Read path

`WireBlobHeader` (read/blob.rs:108-147) parses fields 1, 2, 3 in a tag loop.
Unknown fields are skipped via `cursor.skip_field(wire_type)`.

`BlobFilter` (blob_index.rs:92-128) checks the deserialized indexdata:

```rust
pub struct BlobFilter {
    pub(crate) want_nodes: bool,
    pub(crate) want_ways: bool,
    pub(crate) want_relations: bool,
}
```

Applied in pipeline.rs:112-118, **after reading the blob from disk but before
decompression**:

```rust
if let Some(ref filter) = blob_filter
    && let Some(idx) = blob.index()
    && !filter.wants(idx.kind)
{
    drop(tx.send((seq, None)));
    return;
}
```

## Proposed: per-blob tag presence in BlobHeader

Add a new field to BlobHeader (field 4, next unused protobuf field number)
containing a compact summary of which tag keys appear in that blob's elements.

### Field 4 availability confirmed

The OSM PBF spec defines BlobHeader with fields 1 (type), 2 (indexdata),
3 (datasize). Field 4 is unused. pbfhogg's parser already handles unknown
fields gracefully via `cursor.skip_field()` — any reader that doesn't know
about field 4 will silently skip it. No breakage.

pbfhogg uses `protohoggr` (hand-rolled protobuf library, no .proto files).
Encoding field 4 is trivial:

```rust
encode_bytes_field(buf, 4, tag_summary_bytes);
// protobuf tag: (4 << 3) | 2 = 0x22, single-byte varint
```

### Wire format (v1 — key-only)

BlobHeader field 4, wire type 2 (length-delimited), custom binary format
(same pattern as indexdata — not a nested protobuf message):

```
version (u8)              — 0x01
key_count (u16 LE)        — number of distinct tag keys in blob
repeated key_count times:
  key_len (u16 LE)
  key (UTF-8, key_len bytes)
```

Raw strings (not string table indices) because the PrimitiveBlock string table
is inside the compressed data — chicken-and-egg problem. Raw strings are
self-contained.

**Key-only (v1) is sufficient for blob-level filtering.** If a blob has the
`highway` key at all, it likely contains diverse values — no selectivity gained
from value-level discrimination at blob granularity.

A future v2 with key+value pairs could be added later (version byte enables
forward compatibility):

```
version (u8)              — 0x02
entry_count (u16 LE)
repeated entry_count times:
  key_len (u16 LE)
  key (UTF-8)
  value_count (u16 LE)
  repeated value_count times:
    value_len (u16 LE)
    value (UTF-8)
```

### Backward compatibility

Protobuf wire format skips unknown fields. Any PBF reader that doesn't know
about field 4 (osmium, osmosis, other tools) will silently ignore it. The
`optional_features` string in HeaderBlock could declare "TagIndex" so aware
readers can check for it.

### Write path: scan_block_tags()

**Recommended approach: new `scan_block_tags()` function in blob_index.rs,
paralleling `scan_block_ids()`.** This is cleaner than modifying BlockBuilder
because:

1. It works on the serialized PrimitiveBlock bytes — same input as
   `scan_block_ids()`
2. Runs in the same rayon task on the pipelined write path
3. Handles all paths uniformly including raw passthrough (merge)
4. No BlockBuilder changes needed

The function would:

1. Parse the StringTable (PrimitiveBlock field 1) — collect string entries
2. Scan each PrimitiveGroup to collect string table indices used as tag keys:
   - **Dense nodes** (field 2): parse `keys_vals` packed field (field 10 of
     DenseNodes), collect non-zero key indices separated by 0 delimiters
   - **Ways** (field 3): for each Way, parse field 2 (keys packed uint32)
   - **Relations** (field 4): for each Relation, parse field 2 (keys packed uint32)
3. Resolve collected indices against the parsed string table
4. Return `HashSet<String>` of unique tag key strings

This mirrors how `scan_block_ids()` (blob_index.rs:142-166) already handles
dense nodes differently from ways/relations.

Integration into `write_primitive_block()` (writer.rs:332-355):

```rust
// Existing:
let indexdata = blob_index::scan_block_ids(&uncompressed);
// New:
let tagdata = blob_index::scan_block_tags(&uncompressed);
```

`encode_blob_header_into()` gains a fourth parameter:

```rust
fn encode_blob_header_into(
    blob_type: &str, datasize: i32,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,  // NEW
    buf: &mut Vec<u8>,
) {
    // ... existing fields 1, 2, 3 ...
    if let Some(data) = tagdata {
        encode_bytes_field(buf, 4, data);
    }
}
```

### Read path

Add `tagdata` field to `WireBlobHeader` (read/blob.rs:108-147):

```rust
pub(crate) struct WireBlobHeader {
    pub blob_type: String,
    pub datasize: i32,
    pub indexdata: Option<Vec<u8>>,
    pub tagdata: Option<Vec<u8>>,  // NEW — field 4
}
```

Add match arm in the parser loop:

```rust
4 => {
    let bytes = cursor.read_len_delimited()?;
    tagdata = Some(bytes.to_vec());
}
```

Expose via `Blob::tag_summary() -> Option<TagSummary>` that deserializes the
tag key set from field 4 bytes. This parsing happens on the BlobHeader (before
decompression), so it is very cheap.

### Blob filter extension

Add a `TagFilter` struct alongside `BlobFilter`, checked in pipeline.rs:

```rust
// Existing type filter
if let Some(ref filter) = blob_filter
    && let Some(idx) = blob.index()
    && !filter.wants(idx.kind)
{
    drop(tx.send((seq, None)));
    return;
}
// NEW: tag filter
if let Some(ref tag_filter) = tag_filter
    && let Some(tag_summary) = blob.tag_summary()
    && !tag_filter.might_match(&tag_summary)
{
    drop(tx.send((seq, None)));
    return;
}
```

`might_match()` checks whether the blob's tag key set intersects the query's
required tag keys. This is a **conservative over-approximation**: if the tag
summary says "no highway key", skip. If it says "has highway key", still need
element-level filtering.

### Filter expression integration

From `tags_filter.rs` (lines 36-131), the expression types and their
blob-level behavior:

| Matcher | Example | Blob-level check |
|---|---|---|
| `KeyOnly { key }` | `"amenity"` | Skip if key absent |
| `KeyPrefix { prefix }` | `"addr:*"` | Skip if no key starts with prefix (scan all keys) |
| `ExactValue { key, value }` | `"highway=primary"` | Can only check key presence (v1). Still need element-level |
| `MultiValue { key, values }` | `"type=multipolygon,boundary"` | Can only check key presence (v1) |
| `NotValue { key, value }` | `"highway!=primary"` | Skip only if key absent entirely |

`blob_filter_from_expressions()` (tags_filter.rs:155-169) already computes the
union of type filters. An analogous `tag_keys_from_expressions()` would extract
the set of required tag keys for the tag filter.

### Consumer integration points

| Command | File | Current filtering | Tag-aware benefit |
|---|---|---|---|
| **tags_filter** | commands/tags_filter.rs:364 | BlobFilter by type | **High**: skip blobs without relevant tag keys |
| **tags_count** | commands/tags_count.rs:38 | BlobFilter by type | **Medium**: skip blobs without target tags |
| **sort** | commands/sort.rs:231 | BlobIndex for classification | **Low**: sort needs all blobs |
| **merge** | commands/merge.rs:290 | BlobIndex for classification | **Medium**: could skip blobs disjoint from diff |
| **extract** | commands/extract.rs | No blob filtering | **Medium**: bbox extraction decompresses everything |
| **Pipeline reader** | read/pipeline.rs:112 | BlobFilter by type | Propagates to all pipelined consumers |
| **Nidhogg ingest** | External | ElementReader with BlobFilter | **Low**: ingest needs everything |
| **Elivagar tilegen** | External | Pipelined reader | **High**: Shortbread layers need specific tag keys |

The most impactful insertion point is the **pipeline reader** (pipeline.rs) —
all pipelined consumers get tag-based skipping automatically.

### Raw passthrough (merge/sort)

`reframe_raw_with_index()` (writer.rs:827) already shows the pattern for adding
metadata to passthrough blobs without decompression. A similar approach would
add tag data: read existing tag summary from BlobHeader field 4 if present,
or compute it from the uncompressed data if needed.

For merge specifically, `classify_blob()` in merge.rs reads raw blob frames
and uses `BlobIndex` from indexdata. If a diff only modifies elements with
certain tags, blobs without those tags could be passed through without
decompression.

### Dense nodes

Dense node blocks use a different tag encoding than ways/relations.

In `WireDenseNodes` (read/wire.rs:391-428), tags are stored as a flat
`keys_vals_data` byte array (PrimitiveBlock DenseNodes field 10) with
interleaved string table indices separated by 0 delimiters:

```
[key_sid, val_sid, key_sid, val_sid, ..., 0, key_sid, val_sid, 0, 0, ...]
```

The `DenseNodeIter` (read/dense.rs:155-233) scans forward through packed
varints, finding 0 delimiters between nodes.

Dense node blocks are actually easy to handle in `scan_block_tags()`: parse
the StringTable, then parse the `keys_vals` packed field, collect all non-zero
key indices, resolve against string table. Very few unique keys expected
(most nodes are bare coordinates for ways).

**Tag sparsity by block type:**
- Dense node blobs: 5-20 unique tag keys (most of ~8000 nodes are tagless)
- Way blobs: 30-80 unique tag keys
- Relation blobs: 20-50 unique tag keys

### Size overhead

Per-blob tag summary size (v1 key-only):
```
1 (version) + 2 (key_count) + sum(2 + key_len for each key)
```

| Block type | Typical unique keys | Average summary size |
|---|---|---|
| Dense node | 5-20 | ~150 bytes |
| Way | 30-80 | ~600 bytes |
| Relation | 20-50 | ~400 bytes |

Average across all block types: ~400 bytes/blob.

| Dataset | Blob count | Total overhead | % of PBF size |
|---|---|---|---|
| Denmark (461 MB) | ~16K | ~6 MB | 1.4% |
| Europe (~32 GB) | ~800K | ~320 MB | 1.0% |
| Planet (~73 GB) | ~2.5M | ~1 GB | 1.4% |

### Relationship to nidhogg's tile-level tag index

See `research/TAG_INDEX_NIDHOGG.md` in nidhogg.

These are complementary:
- **This (pbfhogg)**: coarse, blob-level. Skips decompression during read.
  Helps ingest, tilegen, filter commands.
- **Nidhogg's**: fine, record-level within tiles. Skips record scanning
  during query serving.

pbfhogg's index should be implemented first since it benefits all consumers
and the infrastructure (indexdata, BlobFilter, pipeline) already exists.

## Resolved decisions

1. **Key-only (v1) is sufficient** for blob-level filtering. Value-level adds
   little selectivity at blob granularity since most key-bearing blobs contain
   diverse values.

2. **Raw strings, not string table indices.** The PrimitiveBlock string table
   is inside the compressed data. Raw strings are self-contained.

3. **scan_block_tags() approach, not BlockBuilder modification.** Works on
   serialized bytes like scan_block_ids(), handles all paths uniformly
   including raw passthrough.

4. **Field 4 on BlobHeader.** Next unused protobuf field number. Backward-
   compatible via unknown field skipping.

## Open questions

1. **Existing PBF files**: files written without the tag index still work
   (field 4 absent, readers get None). Could add a `pbfhogg reindex` command
   to add tag metadata to existing PBFs without rewriting element data —
   rewrite blob headers only, blob data stays compressed as-is.

2. **Write performance**: `scan_block_tags()` adds a wire-format scan of each
   serialized PrimitiveBlock. This is lighter than `scan_block_ids()` (no
   delta decoding of IDs needed — just collect string table indices from tag
   key fields). Benchmark on planet to confirm negligible vs zlib.

3. **Interaction with sorted PBFs**: after sorting by type-then-ID, way blobs
   contain only ways — the existing indexdata already enables type filtering.
   Tag index adds value within a type (skip way blobs that don't have highway
   tags).

4. **optional_features declaration**: should the HeaderBlock declare
   "TagIndex" in optional_features? This would let aware readers check for it,
   but could cause strict readers to reject the file if they don't recognize
   the feature. Since it's optional (not required_features), it should be safe.

5. **Prefix matching**: `KeyPrefix` filters (e.g. `addr:*`) require scanning
   all keys in the tag summary. With ~80 keys this is fast, but should
   `scan_block_tags()` sort keys alphabetically to enable binary search?
   Probably not worth it for such small sets.
