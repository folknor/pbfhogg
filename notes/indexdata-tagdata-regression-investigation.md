# Indexdata/Tagdata Performance Regression Investigation

## Background

At commit `23862d1`, adding blob-level indexdata (42-byte BlobIndex v2) and tagdata
(variable-length tag key set) to blob headers introduced measurable regressions on
Denmark indexed PBF (487 MB) vs the old non-indexed PBF (461 MB):

| Metric | Before | After | Delta |
|---|---|---|---|
| Parallel read (`par_map_reduce`) | 0.31s | 0.45s | **+45%** |
| Write pipelined floor | 6.2s | 7.1s | **+15%** |
| Write sync floor | — | 7.8s | **+26%** |
| Write sync zlib:6 | 14.5s | 16.4s | **+13%** |
| Write sync zstd:3 | 8.1s | 9.9s | **+22%** |

Baselines: read at `90df51f` (461 MB non-indexed), write at `def80d9`.
Current commit: `f419ba1`.

---

## Read-side regression: eager indexdata parsing

### Root cause

`WireBlobHeader::parse()` (`src/read/blob.rs:204`) always eagerly parses field 2
(indexdata) — a 42-byte `copy_from_slice` into `[u8; INDEX_SIZE]` per blob — even
when no caller uses it:

```rust
2 => {
    let bytes = cursor.read_len_delimited()?;
    let len = bytes.len();
    if len == INDEX_SIZE || len == 26 {
        let mut buf = [0u8; INDEX_SIZE];
        buf[..len].copy_from_slice(bytes);  // 42-byte memcpy per blob
        indexdata = Some(buf);
    }
}
```

There is already a `parse_tagdata: bool` parameter that gates field 4 (tagdata) —
field 2 has no such gate.

### Who uses indexdata on read?

| Path | Uses indexdata? | Notes |
|---|---|---|
| `par_map_reduce` (reader.rs:368) | **No** | Collects blobs, decodes in parallel, never touches `blob.index()` |
| `for_each` (reader.rs) | **No** | Sequential iteration, no filter support |
| `for_each_pipelined` without filter | **No** | Pipeline runs but `should_skip_blob` is never called |
| `for_each_pipelined` with `BlobFilter` | **Yes** | `should_skip_blob()` calls `blob.index()` for type+spatial filtering |
| `IndexedReader` (indexed.rs) | **Yes** | Blob-level index for seekable queries |

The `par_map_reduce` path — the one showing +45% regression — **never uses indexdata**.

### What skipping would cost

`skip_field()` for a length-delimited field reads one varint (the length) and advances
the cursor position. No copy, no allocation. Cost: ~2ns vs the current ~10-20ns for
the 42-byte parse+copy.

### Struct size impact

With indexdata, `WireBlobHeader` is ~96 bytes. Without, ~48 bytes. The `Blob` struct
wraps this plus `WireBlob` (~72 bytes) + `Option<ByteOffset>` (16 bytes). In
`par_map_reduce`, `collect_osm_data_blobs` collects all ~16K blobs into a `Vec<Blob>`.
Larger per-blob footprint means worse cache behavior in the rayon parallel phase.

### Why the code is this way (git history)

Indexdata storage evolved across three commits:

| Commit | Date | Storage | Heap alloc? |
|--------|------|---------|-------------|
| `def80d9` | Feb 27 | `Option<Vec<u8>>` | Yes |
| `eeff9c1` | Feb 28 | `Option<[u8; 26]>` | No (inline) |
| `40959b8` | Mar 1 | `Option<[u8; 42]>` | No (inline, v2 with bbox) |

When `parse_tagdata` was added in `cdd2ce5` (Mar 1), indexdata was already cheap
inline storage — no heap allocation. The `parse_tagdata` gate was motivated by
tagdata's variable-length `Box<[u8]>` heap allocation. Indexdata's fixed 42-byte
inline copy looked trivially cheap by comparison, so gating it wasn't considered.
The commit message and TODO.md regression note mention both fields together but
only tagdata was gated. Indexdata has never had a gate in any commit.

### Fix

Mirror the existing `parse_tagdata` pattern:

1. Add `parse_indexdata: bool` parameter to `WireBlobHeader::parse()`
2. Gate field 2: `2 if parse_indexdata => { ... }`
3. Unmatched field 2 falls through to `_ => cursor.skip_field(wire_type)?` (free)
4. Add `set_parse_indexdata()` to `BlobReader` (mirrors `set_parse_tagdata()`)
5. Default: `false` (matches `parse_tagdata` default)
6. In `pipeline.rs`: `blob_reader.set_parse_indexdata(blob_filter.is_some())`

**Files:** `src/read/blob.rs`, `src/read/pipeline.rs`

---

## Write-side regression: double tag iteration + allocations

### Root cause 1: redundant string table lookups

`add_way`, `add_way_with_locations`, and `add_relation` each have a **pre-loop**
that iterates tags for `tag_key_indices` tracking, then immediately call
`encode_way`/`encode_relation` which iterate tags **again**:

```rust
// Pre-loop in add_way (lines 591-595):
for &(key, _) in tags {
    let key_idx = self.string_table.add(key);     // FxHashMap lookup #1
    self.tag_key_indices.insert(key_idx);
}
encode_way(&mut self.string_table, ..., tags, ...);

// Inside encode_way (lines 1153-1154):
for &(key, _) in tags {
    encode_varint(packed, u64::from(string_table.add(key)));  // FxHashMap lookup #2
}
```

Every tag key gets **two** `string_table.add()` calls (FxHashMap lookups). With 8000
ways/relations per block × ~5-10 tags each = 40K-80K redundant hash lookups per block.

### Root cause 2: per-key heap allocations in tagdata serialization

In `encode_block` (lines 906-918), tagdata is built by:

1. Collecting `tag_key_indices` into `Vec<Box<[u8]>>` — **one heap alloc per unique key**
2. Sorting the `Vec<Box<[u8]>>` by byte content
3. Passing to `TagIndex::from_keys().serialize()`

A typical block has ~50-200 unique keys, so that's 50-200 small heap allocations per
block. Across ~60 blocks for Denmark way blobs, ~3K-12K unnecessary allocations.

### What's NOT a problem

- `track_coords()` (4 conditional i32 comparisons): essentially free, branch-predicted
- `track_id()` (2 conditional i64 comparisons): essentially free
- `tag_key_indices` FxHashSet inserts (u32): individually cheap, the problem is doing
  the string table lookup twice to get the index

### Why the code is this way (git history)

Three commits tell the story, all within a single session:

1. **`ee966cd`** (Feb 26): `encode_way`/`encode_relation` created as standalone fns.
   They already iterate tags twice internally (once for keys, once for values).
   No tag key tracking exists yet.

2. **`182f259`** (Mar 1, 01:42): Tag key tracking added via `scan_block_tags()` — a
   post-hoc wire-format rescan of the entire serialized PrimitiveBlock in `writer.rs`.
   `block_builder.rs` was not modified at all. Cost: 894ms (12.5%) on Denmark cat,
   164ms (22%) on merge.

3. **`cdd2ce5`** (Mar 1, 03:35 — less than 2 hours later): `scan_block_tags()`
   eliminated by tracking tag keys inline in `BlockBuilder` via pre-loops +
   `FxHashSet<u32>`. The pre-loops were the fastest path to eliminating the 12.5%
   rescan overhead — not a deliberate separation-of-concerns decision.

Notably, `add_node` does the tracking **inline** (no pre-loop) because dense node
encoding happens directly in the method body. The pre-loop pattern only exists for
ways and relations because their encoding is delegated to standalone fns that don't
accept a `tag_key_indices` parameter.

### Fix 1: single-pass tag key tracking

Pass `&mut FxHashSet<u32>` to `encode_way`, `encode_way_with_locations`, and
`encode_relation`. Insert key indices during the existing keys loop inside those
functions. Remove the pre-loops from callers. This widens the encode function
signatures but eliminates the separation-of-concerns reason for the double iteration
— the encode functions now participate in index computation, which is the right call
given the per-element cost.

Functions to modify:
- `encode_way()` (line 1134): add `tag_keys: &mut FxHashSet<u32>`, insert at line 1153
- `encode_way_with_locations()` (line 1191): same
- `encode_relation()` (line 1282): same, insert at line 1298
- `add_way()` (line 580): remove pre-loop lines 591-595
- `add_way_with_locations()` (line 617): remove pre-loop lines 631-635
- `add_relation()` (line 658): remove pre-loop lines 670-674

### Why the tagdata serialization code is this way (git history)

The `Vec<Box<[u8]>>` representation traces back to a structural constraint documented
in a since-deleted design document (`TAG_INDEX_PBFHOGG.md`, created at `63590c5`,
deleted in `182f259`). The key resolved decision:

> **Raw strings, not string table indices.** The PrimitiveBlock string table is
> inside the compressed data. Raw strings are self-contained.

The tag index lives in the BlobHeader (uncompressed), but the string table lives
inside the compressed PrimitiveBlock. You can't reference string table indices from
outside the compressed blob. So `TagIndex` was designed around raw byte strings from
the start — `Vec<Box<[u8]>>` is the natural representation for sorted unique byte
strings that must be self-contained.

The initial implementation (`182f259`, Mar 1 01:42) used `scan_block_tags()` in
`writer.rs` to walk the already-serialized PrimitiveBlock, parse its string table,
and produce `TagIndex`. When this was replaced by inline tracking in `cdd2ce5`
(Mar 1 03:35), the `encode_block` code converts string table indices → raw bytes
to match the existing `TagIndex::from_keys().serialize()` API. This preserved the
`TagIndex` wire format as a single source of truth, at the cost of per-key `Box<[u8]>`
allocations that the scan-based approach also had.

### Fix 2: zero-alloc tagdata serialization

Replace the `Vec<Box<[u8]>>` path with index-based sorting:

1. Add `tag_key_scratch: Vec<u32>` field to `BlockBuilder` (reused across blocks)
2. Drain `tag_key_indices` into the scratch vec
3. Sort by `|a, b| strings[*a].cmp(strings[*b])` (compare string table entries by ref)
4. Serialize directly: write version + count header, then for each sorted index write
   `key_len (u16 LE) + key bytes` from the string table

This eliminates all per-key `Box<[u8]>` allocations. It does duplicate the wire format
knowledge from `TagIndex::serialize()`, but the format is trivial (3-byte header +
repeated length-prefixed strings) and the write path is the only producer — the read
side still uses `TagIndex::deserialize()` unchanged. The alternative would be adding a
`serialize_from_indices(&StringTable, &[u32])` method to `TagIndex`, but that couples
`TagIndex` to `StringTable` which is a write-side-only type.

**File:** `src/write/block_builder.rs`

---

## Verification plan

1. `brokkr check` — clippy + unit tests
2. `brokkr check -- --ignored` — roundtrip Denmark (validates read/write path changes)
3. `brokkr verify merge` — merge output identical to osmium
4. `brokkr verify cat` — cat output identical to osmium
5. `brokkr bench read --dataset denmark --runs 5` — measure parallel read recovery
6. `brokkr bench write --dataset denmark --runs 5` — measure write floor recovery

---

## Estimated impact

**Read-side:** The +45% parallel read regression should be mostly recovered. The
remaining gap (if any) comes from the inherently larger blob headers on disk (~57 bytes
vs ~13 bytes for non-indexed), which is ~0.3% of blob data and negligible.

**Write-side:** The double-iteration fix saves ~40K-80K hash lookups per block. The
allocation-free tagdata serialization saves 50-200 heap allocs per block. Together these
should recover a meaningful fraction of the +15-26% floor regression. The remainder is
the inherent cost of reading from a larger indexed PBF (~1s extra decode overhead from
~26 MB more data).
