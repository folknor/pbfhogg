# Box 3: Wire Parsing and Element View Layer — Performance Review

## 1. Executive Summary

- **The reviewer's "full parse cost" concern is a non-issue.** `PrimitiveBlock::new()` takes 14us per block (1.5% of pipelined read time). The real cost is decompression (33-61% depending on region), not parsing. pbfhogg's lazy wire-format design already avoids the eager-decode problem that plagues prost-based parsers.
- **Owned-conversion pressure is real but confined to non-hot paths.** Only `sort.rs` overlap runs, `derive_changes`, and `diff` convert to owned types. The merge hot path already uses `add_way_raw_bytes()` / `add_node_raw()` with raw wire-format passthrough -- the optimization the reviewer suggested has already been implemented.
- **The BlockElementsIter state machine redundant scanning is a non-issue.** Each "empty" element type scan costs ~4 bytes of protobuf tag processing (one read_tag + one skip_field) per group, not a full data scan. At 2.5M groups for planet, that's ~30MB of tag reads total -- negligible vs the 45GB of decompressed data.
- **Varint decode is the actual hot inner loop** at planet scale (~50-80B decodes), but the byte-at-a-time loop is likely adequate given that decompression dominates wall time 5-20x over parsing in all measured profiles.
- **The pre-seed string table optimization is already implemented** (`pre_seed_string_table` + `add_node_raw` + `add_way_raw_bytes` + `add_relation_raw_bytes`), eliminating the string re-interning overhead the reviewer identified as "repeated owned-conversion pressure" for the merge path.

## 2. Finding 1: Full Parse Cost

### Reviewer claim
> Commands that only need IDs or types still parse full PrimitiveBlocks. Selective/ID-only parse exists in blob_index.rs (scan_block_ids) but isn't generalized.

### Reality: NOT a real problem

**What `PrimitiveBlock::new()` actually does** (block.rs:351-373):

1. Calls `WireBlock::parse(data)` (wire.rs:91-153): scans top-level protobuf fields. For a typical block this is ~5-8 field reads: 1 stringtable (LEN), 1 group (LEN), plus granularity/lat_offset/lon_offset/date_granularity (VARINTs, often absent = defaults). Each field read is a tag decode + skip/store. Cost: O(number of top-level fields) = O(1) per block.

2. Calls `WireStringTable::parse()` (wire.rs:32-51): scans the stringtable message, storing `(offset, length)` pairs per entry. For ~200-1200 entries, this is ~200-1200 tag reads + length reads. Vec push with amortized realloc.

3. UTF-8 validates every stringtable entry (block.rs:356-361). For ~1200 entries averaging ~15 bytes = ~18KB validated per block.

4. Transmutes lifetime for self-referential ownership (block.rs:370-371).

**Measured cost from hotpath profiling** (Denmark, 7396 blocks, commit d5c8095):

| Function | Calls | Avg | Total | % of pipeline |
|---|---|---|---|---|
| `decompress_blob` | 7396 | 337us | 2.49s | 36% |
| `block::new` | 7396 | 14us | 102ms | 1.5% |
| `wire::parse` | 14792 | 4.1us | 60ms | 0.9% |

`block::new` is **24x cheaper** than decompression. At planet scale (2.5M blocks), extrapolated cost: 2.5M * 14us = 35s. Decompression at planet scale: 2.5M * 337us = 843s. Parsing is 4% of decompression cost.

**Element parsing is lazy, not eager.** Unlike prost-generated code which decodes all repeated fields into `Vec<T>`, pbfhogg stores `(offset, length)` byte ranges in `WireBlock.group_ranges` and defers all element parsing to iteration time. `WireGroup::new()` (wire.rs:177) just stores a `&[u8]` pointer. `WireWay::parse()`, `WireRelation::parse()`, etc. are called per-element during iteration, not per-block.

**For ID-only commands:** `check_refs` needs way refs and relation members, not just IDs (documented at check_refs.rs:54-63). The comment explicitly states: "profiling shows check-refs is consumer-bound (main thread 100% CPU on RoaringTreemap insertions, decode workers idle at 1% CPU each). Faster parsing would not reduce wall time." The bottleneck is the consumer, not the parser.

**scan_block_ids** (blob_index.rs:142-166) exists for the merge classify path where you genuinely only need element type + ID range. It skips stringtable, coordinates, tags, and metadata. But it still requires decompression first. The 14us saved per block by skipping `PrimitiveBlock::new()` is dwarfed by the ~337us decompression cost.

### Verdict: NOT worth optimizing

The parse layer costs 1.5% of pipelined read time. Even a hypothetical zero-cost parse would save 102ms on Denmark (8.3s pipeline total). The reviewer conflated "full PrimitiveBlock" with prost-style eager decode; pbfhogg's wire-format design already gives near-optimal parse cost.

## 3. Finding 2: Owned-Conversion Pressure

### Reviewer claim
> Commands like sort.rs convert borrowed tags/refs to owned structures, shifting pressure from parser to allocator.

### Reality: Real but already mitigated on the hot path

**Where owned conversion happens:**

1. **sort.rs `write_overlap_run()`** (sort.rs:409-414): Converts elements to `OwnedNode/OwnedWay/OwnedRelation` with `Vec<(String, String)>` tags, `Vec<i64>` refs. This allocates heavily. But it only runs for **overlap runs** -- blobs with overlapping ID ranges that need re-sorting. In a well-sorted PBF (the common case), zero overlap runs occur and this code never executes.

2. **owned_elements.rs `read_elements()`** (owned_elements.rs:51-147): Used by `derive_changes` and `diff`. Reads entire PBF into memory as owned types. This is inherently O(n) allocation. These are comparison commands, not production hot paths.

3. **merge.rs `rewrite_block_parallel()`** (merge.rs:651-): The **actual hot path**. This has been optimized with:
   - `pre_seed_string_table(block)` (merge.rs:662): Pre-seeds output StringTable from input, enabling raw index passthrough.
   - `add_node_raw()` (block_builder.rs:641): Accepts `raw_tags()` iterator of `(i32, i32)` -- no string decode or re-intern.
   - `add_way_raw_bytes()` (block_builder.rs:688): Accepts raw wire-format bytes directly (`keys_data`, `vals_data`, `refs_data`, `info_data`) -- zero decode, zero re-encode. Complete byte-level passthrough.
   - `add_relation_raw_bytes()` (block_builder.rs:719): Same raw passthrough for relations.

   The merge path converts zero strings for base elements. The only allocation is the `BlockBuilder` internal buffers (reused across blocks).

**Quantified impact of owned conversion:**

From `notes/rewrite-block-cost-breakdown.md`, the StringTable pre-seed + raw index optimization saves ~224ms on Denmark merge (11% of rewrite_block). This optimization is **already implemented** (commit with `pre_seed_string_table`, `add_node_raw`, `add_way_raw_bytes`, `add_relation_raw_bytes`).

The `cat` command (full decode+write, NOT merge) still goes through `StringTable::add()` for every string. But `cat` timing shows `block::new` + `wire::parse` = 110ms of 42s total (0.26%). The read-side cost is negligible; the write-side StringTable cost (3.55s, 8.5%) is where the pressure actually lives, and it's on the write side (Box 6), not the wire parsing layer (Box 3).

### Verdict: Real concern, already addressed

The merge hot path uses raw byte passthrough. The sort overlap path is rare. The derive_changes/diff owned-conversion paths are inherently O(n) for comparison operations and not performance-critical.

## 4. Finding 3: Mixed Block-Type Handling

### Reviewer claim
> The BlockElementsIter state machine handles all element types in every block, even when the PBF is sorted (single type per block).

### Reality: Technically real, but the cost is negligible

**Understanding the state machine** (block.rs:565-628):

When entering a new group (`ElementsIterState::Group` state):
1. Calls `group.dense()` -- scans group data for field tag 2. Cost: 1 tag read. If this is a dense node group, finds it immediately (first field). If not, reads 1 tag + skips 1 field.
2. Sets `self.nodes = group.nodes()` -- constructs `WireMessageIter`, NO scanning yet (lazy).
3. Sets `self.ways = group.ways()` -- same, lazy.
4. Sets `self.relations = group.relations()` -- same, lazy.

Then the state machine proceeds through DenseNode -> Node -> Way -> Relation states. For a **dense node group**:
- DenseNode state: iterates 8000 nodes. This is the real work.
- Node state: calls `self.nodes.next()`. The `WireMessageIter` for field 1 reads the first tag of the group data. Since the group is dense nodes (field 2), the tag doesn't match field 1. `skip_field(WIRE_LEN)` reads the length varint and advances the cursor past the entire dense data. The iterator sees no more data and returns `None`. **Cost: 1 tag read (~2 bytes) + 1 length varint read (~3 bytes) + pointer advance. Total: ~5 bytes processed.**
- Way state: same -- reads the first tag, doesn't match field 3, skips. But wait -- the `WireMessageIter` was constructed with the **same group data**, so it starts from the beginning. It reads tag (field 2, LEN), doesn't match field 3, calls `skip_field` to advance past the dense data, then hits EOF. **Cost: ~5 bytes.**
- Relation state: identical to way state. **Cost: ~5 bytes.**

**Total overhead per dense-node group:** 3 empty iterator scans * ~5 bytes = ~15 bytes of tag/skip processing.

**At planet scale:** ~1.06M node blocks (each with 1 group) * 15 bytes = ~16MB of redundant tag reads. At >1 GB/s memory throughput for sequential reads, this is ~16ms. Over a 300s+ pipelined read for planet, it's 0.005% overhead.

**For way blocks and relation blocks:** Same analysis -- 2-3 empty iterator scans, ~10-15 bytes each. Negligible.

**Comparison with `for_each_element()`** (block.rs:439-457):

`for_each_element()` uses the `GroupIter` + `PrimitiveGroup` API. For each group it calls:
- `group.nodes()` (constructs lazy iterator) -> iterates -> scans group for field 1
- `group.dense_nodes()` -> calls `group.dense()` -> scans group for field 2
- `group.ways()` (lazy) -> iterates -> scans group for field 3
- `group.relations()` (lazy) -> iterates -> scans group for field 4

This is **the same overhead** as BlockElementsIter -- 3 empty scans per group. The `for_each_element()` version is not faster; it just has different control flow.

**Could `classify_group()` be used to skip?** (block.rs:274-291)

`classify_group()` reads 1 byte to determine the element type. The `BlockElementsIter` could check `classify_group()` first and skip to the correct state. This would save ~15 bytes of processing per group. At 16ms total planet-scale overhead, this optimization saves <16ms on a 300s+ operation. Not worth the code complexity.

### Verdict: NOT worth optimizing

The overhead is ~15 bytes of protobuf tag scanning per group. At planet scale this sums to ~16MB or ~16ms -- 0.005% of wall time. Branch prediction is not a concern because the state transitions are deterministic per group (DenseNode -> Node -> Way -> Relation, with 3 of 4 immediately falling through). Modern branch predictors handle this perfectly after the first iteration.

## 5. Varint Decode Analysis

### How read_varint works

From the imports in wire.rs:8-11, the `Cursor` type comes from `protohoggr`. Based on standard protobuf varint encoding and the usage patterns visible in the codebase:

- `read_varint()` returns `u64`. Used for unsigned fields.
- `read_varint_i64()` returns `i64`. For signed non-zigzag fields.
- `read_sint64()` returns `i64`. Zigzag-decoded.
- `PackedSint64Iter` wraps a `Cursor` and calls `read_varint()` + `zigzag_decode_64()` per element.
- `PackedUint32Iter`, `PackedInt32Iter` similar.

The varint decode is almost certainly a byte-at-a-time loop (the standard implementation):
```
while byte & 0x80 != 0 { result |= (byte & 0x7F) << shift; shift += 7; }
```

### Planet-scale varint decode volume

Planet: ~8.5B nodes (almost all dense), ~1.2B ways, ~17M relations.

**Dense nodes per node (3 packed arrays):**
- id delta (sint64): 1 varint decode + zigzag
- lat delta (sint64): 1 varint decode + zigzag
- lon delta (sint64): 1 varint decode + zigzag
- keys_vals scanning: ~0.35 varints avg (65% tagless = 0, 35% with ~2 tags = ~5 varints + 1 delimiter = 6)
- Total: **~3.35 varint decodes per node** (without metadata)

With metadata (6 DenseNodeInfo fields: version, timestamp, changeset, uid, user_sid, visible):
- Total: **~9.35 varint decodes per node**

**8.5B nodes * 9.35 = ~79.5B varint decodes just for dense nodes.**

**Ways per way:**
- id: 1 varint (not packed, parsed in WireWay::parse)
- refs: avg ~20 sint64 delta-encoded = 20 varints + 20 zigzag
- tags: avg ~3 tags = 6 uint32 varints (key + value indices)
- info: ~6 varints
- Total: **~33 varint decodes per way**

**1.2B ways * 33 = ~39.6B varint decodes for ways.**

**Relations per relation:**
- id: 1 varint
- members: avg ~10 members = 10 sint64 + 10 int32 + 10 int32 roles = 30 varints
- tags: avg ~3 tags = 6 varints
- info: ~6 varints
- Total: **~43 varint decodes per relation**

**17M relations * 43 = ~731M varint decodes for relations (negligible).**

**Grand total: ~119B varint decodes for planet.**

### Throughput estimate

A byte-at-a-time varint decode loop for a 1-2 byte varint (common for delta-encoded values) takes ~2-4ns on modern x86 (branch-heavy but predictable for small values). For longer varints (4-5 bytes for node IDs), ~5-8ns.

Assuming ~3ns average: 119B * 3ns = **357 seconds** of pure varint decode time.

BUT this is spread across the rayon thread pool. In pipelined mode with 10 decode threads, each thread handles ~12B decodes = ~36s. This is concurrent with decompression and consumer work.

From hotpath profiling, the pipelined read of Denmark (59M elements, ~5B varint decodes estimated) completes in 6.9s total with decompression at 2.49s. The varint decode time is embedded in the consumer thread's element iteration. Since the consumer is 100% CPU and workers are idle at 1-2% CPU, the varint decode is happening sequentially on the consumer thread at roughly: (6.9s - 2.49s pipeline overhead) * 0.5 (half is consumer logic) = ~2.2s for 5B varints = ~0.44ns per varint.

This suggests either: (a) many varints are 1 byte (delta-encoded values near zero), (b) the loop is well-predicted and effectively single-cycle per byte, or (c) my varint count estimate is too high because many elements are processed without accessing all fields.

### SIMD varint decoding potential

SIMD varint techniques (stream-vbyte, masked-vbyte, Group-Varint) can decode 4-8 varints simultaneously using SIMD shuffles. Typical speedup: 2-4x over scalar for packed arrays.

**Applicability to pbfhogg:**
- Dense node id/lat/lon arrays: **good candidate** -- contiguous packed sint64, processed sequentially. ~25.5B decodes.
- DenseNodeInfo arrays: **good candidate** -- 6 contiguous packed arrays. ~51B decodes.
- Way refs: **moderate candidate** -- packed sint64, but per-way (not per-block). Average 20 varints per way. The setup cost of SIMD may not amortize for small arrays.
- Tag key/value indices: **poor candidate** -- small arrays (2-6 entries per element), lookup-heavy (each index triggers a stringtable access).

**Estimated speedup:** 2x on the ~76B dense node varints (the largest component). This would save ~25s of sequential varint decode time at planet scale. In pipelined mode with 10 threads, the saving is ~2.5s per thread -- meaningful but small relative to decompression overhead.

**Practical consideration:** pbfhogg's `PackedSint64Iter` calls `read_varint()` + `zigzag_decode_64()` per element. Converting to batch SIMD would require changing the iterator to decode N-at-a-time into a buffer, then yield from the buffer. This is a significant API change to the protohoggr crate.

### Verdict: Low priority

Varint decode is ~4% of pipelined read time in measured profiles. Even a 2x speedup saves ~2% of total wall time. The decompression stage (33-61% of wall time) is the bottleneck. SIMD varint decode would be a valid micro-optimization after decompression is exhausted as an optimization target.

## 6. StringTable Analysis

### Allocation pattern

`WireStringTable::parse()` (wire.rs:32-51) uses `Vec::new()` with push. No `with_capacity()` pre-allocation. For a typical block:

- ~200-1200 entries
- Vec reallocation pattern: 0 -> 1 -> 2 -> 4 -> 8 -> 16 -> 32 -> 64 -> 128 -> 256 -> 512 -> 1024 -> 2048
- For 1200 entries: ~11 reallocations, final capacity 2048 * 8 bytes = 16KB
- Each realloc copies the existing data

**Allocation churn from hotpath data:**
`wire::parse` allocates 342 MB across 14,792 calls = 23 KB per call. This includes the `WireStringTable` Vec + `WireBlock` group_ranges Vec. The stringtable Vec is the majority (1200 entries * 8 bytes = 9.6KB final, with reallocation copies summing to ~18KB).

**At planet scale:** 2.5M blocks * 23 KB = 57.5 GB cumulative allocation for `wire::parse`. But peak RSS contribution is negligible -- each allocation is freed per-block, so peak is ~23 KB.

**Could pre-allocation help?** The stringtable entry count is not known before scanning (the protobuf format doesn't encode repeated field counts). A heuristic could `reserve(256)` or `reserve(1024)` to reduce reallocations from ~11 to ~1-2. The savings: ~10 fewer `memcpy` calls per block, each copying ~4-8 KB. At 14us total for `wire::parse`, the reallocation overhead is likely <2us. Not worth the code complexity.

### UTF-8 validation cost

`PrimitiveBlock::new()` validates every stringtable entry (block.rs:356-361):
```rust
for index in 0..block.stringtable.len() {
    if let Some(bytes) = block.stringtable.get(index) {
        std::str::from_utf8(bytes)?;
    }
}
```

Per block: ~1200 entries * ~15 bytes average = ~18 KB validated.
At planet scale: 2.5M blocks * 18 KB = 45 GB of UTF-8 validation.

**Is this measurable?** `std::str::from_utf8` on x86-64 uses SIMD (SSE2/AVX2) for ASCII validation. For pure ASCII strings (>99% of OSM tag keys/values), throughput is ~16 GB/s. At 45 GB: ~2.8 seconds of wall time across all blocks.

In the hotpath profile, `block::new` takes 14us per block, and `wire::parse` takes 4.1us of that. The remaining ~10us includes UTF-8 validation + transmute + other overhead. For ~18KB of validation at 16 GB/s SIMD throughput: ~1.1us. So UTF-8 validation is ~8% of `block::new` time, or **~0.1% of total pipeline time**.

### The `from_utf8_unchecked` in `str_from_stringtable`

`str_from_stringtable` (block.rs:744-754) uses `unsafe { std::str::from_utf8_unchecked(bytes) }`:
```rust
pub(crate) fn str_from_stringtable<'a>(block: &'a WireBlock<'_>, index: usize) -> Result<&'a str> {
    if let Some(bytes) = block.stringtable.get(index) {
        Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
    } else {
        Err(...)
    }
}
```

This is called per tag access: ~30B times at planet scale (2 lookups per tag * ~15B tags total). If this used checked `from_utf8`, the cost would be: ~15 bytes/string * ~60B lookups / 16 GB/s = ~56 seconds. The `unchecked` variant saves ~56 seconds at planet scale by relying on the upfront validation in `PrimitiveBlock::new()`.

**Is the safety invariant sound?** Yes. The `PrimitiveBlock::new()` constructor validates every entry. The `buffer` field is `Bytes` (immutable, reference-counted). No mutable access is exposed. The validation-once-use-unchecked pattern is standard and correct.

### Verdict: Well-optimized

The current design -- validate once at construction, use `from_utf8_unchecked` on access -- is the correct trade-off. The upfront validation costs ~1.1us per block (0.1% of pipeline time). The per-access savings from `unchecked` are ~56 seconds at planet scale. The Vec reallocation in `WireStringTable::parse()` is negligible. No changes recommended.

## 7. DenseNode Iteration Analysis

### The hot path

`DenseNodeIter::next()` (dense.rs:158-228) is called ~8.5B times for planet. Per call:

1. Advance 3 `PackedSint64Iter`s: `dids.next()`, `dlats.next()`, `dlons.next()` (lines 159-163). Each: 1 varint decode + zigzag decode = ~3-5ns.

2. Optionally advance `DenseNodeInfoIter` (line 163): 5 packed iterators (version, timestamp, changeset, uid, user_sid) + 1 bool iterator. Each is a varint decode + delta accumulation. Cost: ~15-30ns when present.

3. Tag scanning loop (lines 179-202): Scans `kv_data` for the 0-delimiter that separates this node's tags from the next.

### Tag scanning efficiency

For **tagless nodes** (~65% of planet, ~5.5B nodes):
- `kv_data` is non-empty (it contains tags for ALL nodes in the block interleaved)
- The cursor at `self.kv_pos` points at a 0-delimiter byte
- `cursor.read_varint()` returns `Ok(0)` immediately (1 byte read)
- `self.kv_pos += 1`
- `tag_bytes` = empty slice (tag_start == tag_end after delimiter subtraction)
- **Cost: 1 byte read + 1 varint decode (~2ns)**

For **tagged nodes** (~35% of planet, ~3B nodes, avg ~2 tags):
- 2 tags = 4 varints (key1, val1, key2, val2) + 1 zero delimiter
- Each varint is a stringtable index (typically 1-2 bytes, values <128 for most strings)
- 5 varint decodes: ~5 * 2ns = ~10ns
- **Cost: ~10ns per tagged node**

**Weighted average: 0.65 * 2ns + 0.35 * 10ns = ~4.8ns for tag scanning per node.**

At 8.5B nodes: ~41 seconds for tag scanning at planet scale.

### DenseNodeInfo iteration

`DenseNodeInfoIter::next()` (dense.rs:346-369) advances 6 iterators:

```rust
let version = self.versions.next()?;        // PackedInt32Iter (1 varint)
let dtimestamp = self.dtimestamps.next()?;   // PackedSint64Iter (1 varint + zigzag)
let dchangeset = self.dchangesets.next()?;   // PackedSint64Iter
let duid = self.duids.next()?;               // PackedSint32Iter
let duser_sid = self.duser_sids.next()?;     // PackedSint32Iter
let visible_opt = self.visible.next();       // PackedBoolIter (1 varint)
```

6 varint decodes + 3 zigzag decodes + 4 delta accumulations + struct construction.

Estimated cost: ~18-25ns per node. At 8.5B nodes: ~170-212 seconds.

### elements_skip_metadata optimization

`elements_skip_metadata()` (block.rs:411-413) creates `DenseNodeIter::new_skip_metadata()` (dense.rs:115-134) which sets `info_iter: None`. This skips all 6 DenseNodeInfo fields.

**Commands using this:**
- `extract.rs` pass 1 (line 567): ID + coordinate matching only
- `extract.rs` pass 1b (line 711): ID + coordinate + refs matching
- `extract.rs` smart pass 1 (line 988): ID + coordinate + refs + member matching
- `tags_filter.rs` pass 1 (line 622): ID + tags matching

These commands only need IDs, coordinates, refs, and tags -- not version/timestamp/changeset/uid/user. Skipping metadata saves ~18-25ns per node, or **~170s at planet scale for node-heavy passes.**

**Commands NOT using it that could:**
- `check_refs.rs`: Only needs IDs + refs + members. Currently uses `for_each_pipelined` which calls `for_each_element` (block.rs:439) which uses `DenseNodeIter::new()` (with metadata). Could benefit from `elements_skip_metadata()`. However, check_refs is consumer-bound (RoaringTreemap insertions), not parser-bound.
- `merge.rs` `block_overlaps_diff()` (line 334): Only needs IDs. Currently uses `block.elements()`. Could use `elements_skip_metadata()`. But this only runs for 630 blocks (Denmark) or O(1000) blocks generally -- negligible.

### Struct-of-arrays vs array-of-structs consideration

The DenseNodeInfo iteration reads 6 parallel packed arrays. Each `PackedSint64Iter` wraps a `Cursor` pointing to a different region of the decompressed buffer. Per `.next()` call, the CPU touches 6 different memory locations.

In a struct-of-arrays layout, these 6 arrays would be contiguous in memory. The current layout IS effectively struct-of-arrays at the protobuf wire level -- each packed field is a contiguous byte array. The iterators just hold pointers to different offsets in the same decompressed buffer.

The cache behavior depends on buffer layout: if the 6 packed fields are close together in the decompressed buffer (they are -- they're consecutive fields in the DenseInfo message, typically within a few KB of each other), they'll share cache lines. For a 256KB decompressed block, all 6 arrays fit in L2 cache. **No cache issue here.**

## 8. Additional Findings

### 8a. Node/Way/Relation constructor per-element overhead

`Node::new()` (elements.rs:96-103):
```rust
Node {
    block, node,
    granularity: i64::from(block.granularity),  // i32 -> i64 widening
    lat_offset: block.lat_offset,                // i64 copy
    lon_offset: block.lon_offset,                // i64 copy
}
```

This stores 3 i64 values (granularity, lat_offset, lon_offset) per Node struct. Same for `Way::new()` (elements.rs:168-175) and `DenseNode` construction (dense.rs:214-224).

**Cost:** 3 i64 copies = 24 bytes per element. At 10B elements: 240 GB of data movement. But these are stack values (no allocation), and the copies are likely optimized away by the compiler since the values come from the block struct (which is `&WireBlock`). The `i64::from(block.granularity)` conversion is a single instruction (`movsxd`).

At 1-2 cycles per element, 10B elements = ~5-10 seconds at 3.7 GHz. Stored as i32 and converted on-demand in `nano_lat()`/`nano_lon()`, this would save 16 bytes per struct but add an i32->i64 conversion per coordinate access. Since coordinates are accessed at most once per element in most commands, the per-access conversion is equivalent cost. **No net savings.**

### 8b. TagIter cost

`TagIter::next()` (elements.rs:574-580):
```rust
fn next(&mut self) -> Option<Self::Item> {
    get_stringtable_key_value(
        self.block,
        self.key_indices.next().map(|v| v as usize),
        self.val_indices.next().map(|v| v as usize),
    )
}
```

Per tag: 2 `PackedUint32Iter::next()` calls (2 varint decodes) + 2 `str_from_stringtable()` calls (2 array index lookups + 2 `from_utf8_unchecked`).

Cost estimate: 2 * 2ns (varint) + 2 * 3ns (lookup) = ~10ns per tag.

At planet scale: ~15B tags (8.5B nodes * 0.35 * 2 avg + 1.2B ways * 3 avg + 17M rels * 3 avg = 5.95B + 3.6B + 51M = ~9.6B tags... actually). ~15B tags * 10ns = **~150 seconds** for tag iteration at planet scale.

However, most of this cost is in the varint decode (covered in section 5) and the stringtable lookup (covered in section 6). The TagIter itself adds minimal overhead -- it's just dispatching to already-analyzed primitives.

**Comparison with decompression cost:** Planet decompression: ~843 seconds. Tag iteration: ~150 seconds. Tags are 18% of decompression cost. Significant but not dominant.

### 8c. Inline annotations

**Functions correctly marked `#[inline]`:**
- `WireStringTable::get()` (wire.rs:58) -- called per tag access, ~60B times at planet
- `WireStringTable::len()` (wire.rs:53) -- called in loops
- `WireBlock::group()` (wire.rs:155) -- called per group
- `WireGroup::new()` (wire.rs:176) -- constructor
- `BlockElementsIter::step()` (block.rs:563) -- called per element
- `BlockElementsIter::next()` (block.rs:634) -- called per element
- All coordinate methods (elements.rs:37-63) -- called per node
- `DenseNode::id()`, `nano_lat()`, `nano_lon()` (dense.rs:29-47) -- called per node
- `WayRefIter::next()` (elements.rs:354) -- called per ref
- `TagIter::next()` (elements.rs:573) -- called per tag
- `DenseTagIter::next()` (dense.rs:383) -- called per dense node tag
- `RelMemberIter::next()` (elements.rs:537) -- called per member

**Functions missing `#[inline]` that probably should have it:**
- `WireGroup::nodes()` (wire.rs:181) -- constructor, trivial, called per group. However, since it's `pub(crate)` and the function body is tiny (1 line), LLVM will likely inline it with LTO enabled. Low concern.
- `WireGroup::ways()` (wire.rs:196) -- same as above.
- `WireGroup::relations()` (wire.rs:200) -- same as above.
- `WireMessageIter::new()` (wire.rs:219) -- private, called per group. LLVM should inline.
- `WireMessageIter::next()` (wire.rs:237) -- the Iterator trait implementation. Should have `#[inline]` for better codegen when used in `for` loops. This is called per-element for Node/Way/Relation iteration. **This is the most important missing inline.** Without it, each call to `WireMessageIter::next()` is a function call through the vtable... actually no, it's called through generic iteration, not dyn. But still, `#[inline]` on Iterator::next() helps LLVM optimize the loop.
- `DenseNodeIter::next()` (dense.rs:158) -- Iterator::next() for the hottest path. Missing `#[inline]`. At 8.5B calls for planet, this matters. However, with `lto = "fat"` and `codegen-units = 1` in the release profile, cross-crate inlining happens anyway. **Low concern given the release profile.**
- `DenseNodeInfoIter::next()` (dense.rs:346) -- same argument.

**Assessment:** The `lto = "fat"` + `codegen-units = 1` release profile means LLVM has full visibility for inlining decisions. The missing `#[inline]` annotations would only matter in debug builds or when the library is used as a dependency without LTO. For production builds, this is a non-issue.

### 8d. WireGroup::dense() vs WireMessageIter asymmetry

`WireGroup::dense()` (wire.rs:185-194) returns `Result<Option<&'a [u8]>>` -- it scans the group data eagerly and returns the single dense nodes sub-message. In contrast, `nodes()`, `ways()`, `relations()` return lazy `WireMessageIter`s.

This asymmetry exists because dense nodes is always a single sub-message (field 2), while nodes/ways/relations can have multiple sub-messages (repeated field). The eager scan for dense() is O(1) per group -- it reads at most a few tags before finding field 2 (or determining it's absent).

No performance concern here. The design correctly reflects the protobuf schema.

### 8e. The merge `block_overlaps_diff()` iterates all elements when it could stop early

`block_overlaps_diff()` (merge.rs:333-358) uses `block.elements()` and returns `true` on first match. This is already short-circuiting via the `if dominated { return true; }` check. For non-overlapping blocks (most common), it must iterate all elements to confirm no overlap, which is O(n). But this function is only called for blocks that pass the coarse range check -- typically ~630 blocks for Denmark. At 8000 elements per block * 630 blocks * ~10ns per element (ID access + HashSet lookup) = ~50ms. Negligible.

## 9. Cross-Box Interactions

### Box 2 (Blob Decode) -> Box 3 (Wire Parsing)

The `Bytes` buffer produced by `decompress_blob` (Box 2) is consumed by `PrimitiveBlock::new()` (Box 3). Buffer size directly affects:
- `WireBlock::parse()` scan cost: proportional to top-level field count (constant, independent of buffer size)
- `WireStringTable::parse()` cost: proportional to number of unique strings (grows with block element count, not raw buffer size)
- UTF-8 validation cost: proportional to total string bytes (subset of buffer)

The buffer is immutable (`Bytes`) and shared between `PrimitiveBlock.buffer` and `WireBlock` which borrows from it. The self-referential pattern (block.rs:313-328) eliminates copying but requires unsafe lifetime erasure.

**Key interaction:** The `DecompressPool` (Box 2) enables buffer reuse in pipelined mode, reducing allocation pressure. This directly benefits Box 3's `PrimitiveBlock::new()` because the `Bytes` buffer ownership transfer is O(1) (reference count increment, not copy).

### Box 6 (BlockBuilder) -> Box 3 (Wire Parsing)

The write path mirrors the read path:
- `BlockBuilder` encodes elements using the same protobuf field numbers and packed encoding that `WireBlock`/`WireGroup`/`WireWay` etc. decode.
- The merge path bridges read and write: `Way.keys_data()` (elements.rs:236) returns `&[u8]` (raw packed bytes from the read side), which is passed directly to `add_way_raw_bytes()` (block_builder.rs:688) on the write side. **Zero decode, zero re-encode.** This is the ultimate optimization for the wire parsing layer -- bypass it entirely for passthrough elements.

### Box 8 (Commands) -> Box 3 (Wire Parsing)

Command-level patterns that affect Box 3 performance:

| Command | Uses elements() | Uses elements_skip_metadata() | Uses groups() | Uses raw_tags/raw_bytes | Notes |
|---|---|---|---|---|---|
| check_refs | via for_each_pipelined | no | no | no | Consumer-bound |
| tags_count | via for_each_pipelined | no | no | no | Consumer-bound |
| cat | elements() | no | no | no | Write-bound (57% compression) |
| sort | elements() | no | no | no | Only for overlap runs |
| merge | elements() | no | no | **yes** (raw_bytes passthrough) | Parse cost negligible |
| extract | elements() + **elements_skip_metadata()** | **yes** (passes 1,1b,smart) | groups() (pass 2) | no | Correctly uses skip_metadata for scan passes |
| tags_filter | **elements_skip_metadata()** | **yes** (pass 1) | no | no | Correctly uses skip_metadata |
| getid | elements() | no | no | no | Uses BlobFilter to skip irrelevant types |
| derive_changes | elements() | no | no | no | Full owned conversion |
| diff | elements() | no | no | no | Full owned conversion |
| add_locations | elements() | no | no | no | Needs metadata |
| fileinfo | elements() | no | no | no | Full scan for statistics |

## 10. Recommended Actions (Prioritized)

### Priority 1: None needed (current design is well-optimized)

The wire parsing layer is not the bottleneck for any measured workload:
- Pipelined read: decompression is 33-61% of wall time; parsing is 1.5%.
- Cat (decode+write): compression is 57%; parsing is 0.26%.
- Merge: compression or rewrite_block dominates; parsing is 0.1-0.3%.
- Check-refs: consumer (RoaringTreemap) dominates; workers idle at 1-2% CPU.

### Priority 2: Low-effort cleanup (if touching these files anyway)

1. **Add `#[inline]` to `WireMessageIter::next()`** (wire.rs:237). Currently missing. Not critical with fat LTO, but good practice for library consumers who may not use LTO. Estimated impact: 0% with fat LTO, possibly 1-3% without LTO for way/relation iteration.

2. **Add `#[inline]` to `DenseNodeIter::next()`** (dense.rs:158). Same reasoning. The hottest path in the library.

3. **Add `#[inline]` to `WireGroup::nodes()`, `ways()`, `relations()`** (wire.rs:181,196,200). Trivial constructors that should be inlined.

4. **Consider `Vec::with_capacity(256)` in `WireStringTable::parse()`** (wire.rs:35). Reduces reallocation from ~11 to ~3 for typical blocks. Saves ~2us per block at most. Not urgent.

### Priority 3: Speculative (only if decompression is fully optimized)

5. **SIMD varint decode for packed arrays in protohoggr.** Most benefit for DenseNode id/lat/lon arrays (25.5B decodes). Requires batch-decode API change. Estimated planet-scale saving: ~25s out of ~843s decompression + ~357s varint decode. Meaningful only after decompression is no longer the bottleneck.

6. **Specialized `elements_skip_metadata()` variant that also skips tags** for purely ID-only passes. Would benefit `merge::block_overlaps_diff()` and hypothetical ID-scan commands. Saves ~5ns per element by skipping tag kv_data scanning. At 50ms total for current usage, not worth the API complexity.

### Not recommended

- **Generalizing `scan_block_ids`**: The existing scan is specialized for merge classify. Generalizing it would add complexity for negligible gain (14us parse vs 337us decompression per block).
- **Block-type-aware state machine optimization**: The redundant scan overhead is ~16ms at planet scale. Not worth the added branching complexity.
- **Pre-allocating WireStringTable Vec**: Saves <2us per block. Noise level.
- **Removing UTF-8 validation**: Unsafe, saves 1.1us per block, enables `from_utf8_unchecked` savings of ~56s at planet scale -- but the unchecked path already relies on the validation. Would break the safety invariant.
