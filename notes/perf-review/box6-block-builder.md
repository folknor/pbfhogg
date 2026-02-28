# Box 6: BlockBuilder and Encoding -- Performance Investigation

Host: plantasjen (AMD Ryzen 9 5900X, 32 GB DDR4, Samsung 970 EVO Plus NVMe).
File: `/home/folk/Programs/pbfhogg/src/write/block_builder.rs` (1406 lines).
Build: fat LTO, zlib-ng. All profiling data from `notes/hotpath-profile.md` and
`notes/region-profiles.md` unless noted. No benchmarks run for this review.

## 1. Executive summary

- **String interning is NOT the bottleneck it once was.** The `get()` fast-path
  (line 97) eliminated allocation on cache hits (~99% of calls). The original
  `add()` allocated on every call (27ns avg). After the fast-path, the per-call
  cost dropped to ~10ns for hits. Further, the pre-seed + raw-index API for merge
  (`add_node_raw`, `add_way_raw_bytes`, `add_relation_raw_bytes`) eliminated
  string hashing entirely for the merge hot path. StringTable cost in write-only
  commands (cat, sort) is real but not actionable without an API redesign.

- **The `take()` return type (`&[u8]`) forces a `to_vec()` copy in pipelined mode
  and in merge's parallel rewrite.** This is the single largest remaining
  allocation per block (~130 KB * blocks). At planet scale with 1.19M blocks,
  this is ~155 GB of copy churn. This is the highest-impact finding.

- **MAX_ENTITIES_PER_BLOCK=8000 is correct and tuning it would break
  compatibility for no measurable gain.** The encode/compression pipeline is
  already well-amortized at 8000.

- **Dense metadata backfill is negligible.** It occurs only on no-metadata to
  metadata transitions within a single block, which is rare in practice and
  costs O(count) pushes of zero.

- **Way/relation wire encoding is already near-optimal.** Direct wire-format
  encoding with reusable scratch buffers eliminated all per-element allocation.
  The remaining per-element costs (zigzag, varint, field framing) are
  irreducible computation.

## 2. Finding 1: String interning cost

### Reviewer claim

> StringTable uses FxHashMap with per-string allocation. At planet scale with
> heavily tagged elements, this is the hot path.

### Current state (after multiple optimization rounds)

The StringTable has been optimized across three commits:

1. **`get()` fast-path** (commit f5c5674, line 97): `self.index.get(s)` does a
   hash lookup via the `Borrow<str>` impl on `String` keys, with zero
   allocation. Returns `Some(&idx)` on cache hit. ~99% hit rate per block.

2. **Pre-seed API** (lines 112-123, 630-634): `pre_seed_string_table()` copies
   the input block's string table into the builder. After pre-seeding, input
   index N = output index N (identity mapping).

3. **Raw-index methods** (lines 641-764): `add_node_raw()`, `add_way_raw_bytes()`,
   `add_relation_raw_bytes()` bypass `StringTable::add()` entirely. Used by merge
   for unmodified base elements (>99.9% of elements in rewritten blocks).

### Quantified cost

**For write-only commands (cat, sort, extract, tags_filter):**

From `notes/rewrite-block-cost-breakdown.md`, profiled on Denmark cat:
- Total `add()` calls: 131M across 59.1M elements (2.22 calls/element average)
- Per-call cost: ~10ns (post-fast-path, was 27ns before)
- Total StringTable time: ~1.3s out of 42s wall (3.1%)
- As fraction of add_* time: ~60-80% depending on element type

Per-element breakdown (from `notes/region-profiles.md`, per-element costs):

| Element | add() calls/elem | StringTable ns/elem | Total add_* ns | ST fraction |
|---------|-------------------|---------------------|----------------|-------------|
| Node    | 1.6               | 16                  | 21-43          | 37-76%      |
| Way     | 7                 | 70                  | 205-325        | 22-34%      |
| Relation| 17                | 170                 | 292-836        | 20-58%      |

The wide range in fractions reflects region-dependent tagging density. For nodes,
StringTable is still the dominant cost because there is almost no other work
(push 3 deltas + keys_vals delimiter). For ways, the ref delta-encoding and
wire-format writing dominate.

**For merge (the performance-critical path):**

With pre-seeded raw-index API, StringTable::add is called only for:
- Diff element tags (OSC elements replacing base elements)
- User strings in diff elements
- pre_seed() itself (1200 calls per block, ~8-20ms total across 630 blocks)

From `notes/hotpath-profile.md`, indexdata + zlib parallel rewrite (commit 14034c1):
- `rewrite_block_parallel`: 614 calls, 1.02s total (1.67ms avg)
- `add_way` within rewrite: 1667 calls, 833us total (499ns avg)

Those 1667 way calls are the ~0.1% of ways that are diff replacements (not
raw-bytes passthrough). The raw passthrough path has zero string table cost.

### The slow-path double allocation

The reviewer correctly identified lines 102-106:

```rust
let next_idx = self.strings.len() as u32;
match self.index.entry(s.to_owned()) {          // allocation 1: HashMap key
    Entry::Occupied(e) => *e.get(),
    Entry::Vacant(e) => {
        self.strings.push(e.key().clone());     // allocation 2: Vec entry
        e.insert(next_idx);
        next_idx
    }
}
```

On a cache miss (1% of calls), this does two String allocations. However:

**Math:** At planet scale (10B elements, 2.22 add/elem = 22.2B calls), ~1% are
misses = 222M slow-path calls. Two allocations each at ~15 bytes average:
222M * 2 * (40 + 15) = ~24 GB of allocation churn (allocator metadata + string
bytes). At ~50ns per malloc+free: 222M * 2 * 50ns = **22 seconds**.

But this is spread across the entire planet write. The 22.2B fast-path calls at
~10ns each = **222 seconds** total StringTable time. The slow-path's 22 seconds
is ~10% of that. Eliminating one of the two allocations would save ~11 seconds
at planet scale.

**Could the double allocation be eliminated?** Yes. The `Vec<String>` (`strings`)
stores clones of HashMap keys. Instead, it could store `u32` indices into the
HashMap (but HashMap doesn't provide stable indices). Alternatively, the Vec
could store `&str` borrows of the HashMap keys, but that creates self-referential
borrowing. The cleanest fix: store `Arc<str>` in both, sharing the allocation.
But `Arc<str>` adds 16 bytes of refcount overhead per entry, and the clone cost
(~15ns for a short string) is replaced by `Arc::clone` (~3ns atomic increment).
Saving: ~12ns * 222M = ~2.7 seconds at planet scale. Marginal.

**Practical assessment:** The double allocation is a real but minor inefficiency.
It saves ~11 seconds at planet scale against a 222-second StringTable total.
Given that the merge path (where performance matters most) bypasses
StringTable::add entirely for base elements, this is **low priority**.

### FxHash vs ahash

The comment at lines 30-66 explains the choice well. Key facts:
- FxHash: multiply-rotate, ~2-3ns for short strings, no state initialization
- ahash: AES-NI based, ~3-4ns for short strings, requires random state init
- SipHash (std): ~8-10ns for short strings

FxHash is ~1ns faster per call than ahash for the short-string workload (3-30
byte OSM tag keys/values). At 22.2B calls (planet-scale fast path), the
difference is ~22 seconds. Not transformative, but FxHash is the correct choice
for this trusted-input write-side scenario.

### pre_seed() cost

`pre_seed()` (lines 112-123) iterates the input block's string table and calls
`add()` for each entry. With ~1200 unique strings per block, all calls are
slow-path misses (table is empty). Per-block cost: 1200 * ~50ns (slow-path
including allocation) = ~60us. Across 630 rewritten blocks in Denmark merge:
~38ms. At planet scale (1.1M rewritten blocks): ~66 seconds.

But this is inherent: the same 1200 strings would be inserted as the first 1200
unique elements are processed. pre_seed() front-loads this cost, which is
beneficial because it enables raw-index passthrough for all subsequent elements.
The net effect is a time savings (avoids 14,300 hash+probe operations per block).

### Verdict

**Real but already addressed for the critical path.** The merge hot path uses
raw indices. Write-only commands (cat, sort) still pay the full ~10ns per add()
call, but they are dominated by compression (57% of wall time). StringTable
improvements for write-only commands would improve the no-compression floor
(currently ~6.2s on Denmark) by at most ~1.3s (3.1% of 42s), or ~21% of the
6.2s floor. This is meaningful but requires an API redesign (e.g., accepting raw
tag indices from the read side in all commands, not just merge).

## 3. Finding 2: Block size tuning

### Reviewer claim

> Fixed MAX_ENTITIES_PER_BLOCK=8000: Compatible with osmium but may not be
> optimal for all compression and cache behaviors.

### Analysis

**MAX_ENTITIES_PER_BLOCK** is defined at line 22. This value matches osmium's
hardcoded limit and is the de facto standard across the OSM toolchain.

**Compression impact:** A typical 8000-entity block produces ~100-200 KB of
uncompressed protobuf, which compresses to ~16-64 KB with zlib:6. Zlib's
compression ratio is optimized for this range. Larger blocks would improve
compression slightly (more context for the LZ77 window) but increase memory
per-block and reduce the parallelism granularity of the pipelined writer.

**Cache behavior:** The uncompressed block (~130 KB) fits comfortably in L2
cache (256 KB on Zen 3). Doubling to 16000 entities would push it to ~260 KB,
spilling into L3. The take() encoding pass iterates all dense Vecs linearly,
so L2 residency is beneficial.

**Pipeline granularity:** The pipelined writer dispatches one rayon task per
block. With 8000 entities and ~7400 blocks for Denmark, there is ample
parallelism (7400 tasks across 8-12 rayon threads). Reducing block count by
increasing block size would reduce parallelism but not meaningfully -- the
limiting factor is compression throughput, not task dispatch overhead.

**Compatibility:** Every OSM tool (osmium, osmosis, osmconvert, osm2pgsql,
Planetiler) expects blocks of up to 8000 entities. Larger blocks would be valid
protobuf but untested by the ecosystem, risking subtle parsing issues.

**Quantified impact of tuning:** Even a 2x larger block (16000 entities) would
save at most 1 take() call per 2 blocks. take() costs ~19-468us per call
(depending on commit/workload). At 3700 saved calls: ~70ms-1.7s on Denmark.
But the L2 cache miss penalty would likely offset this.

### Verdict

**Not real. No action needed.** The 8000 limit is the correct choice: ecosystem
compatible, cache-friendly, and provides sufficient pipeline granularity. Tuning
it would trade marginal compression improvement for compatibility risk and cache
penalties.

## 4. Finding 3: Dense metadata handling

### Reviewer claim

> Backfill logic for mixed metadata/no-metadata adds branch and vector
> maintenance overhead.

### Analysis

Three code paths handle metadata mixing (lines 436-522):

1. **backfill_default_dense_metadata()** (lines 488-498): Called when the first
   metadata-bearing node arrives but `count > 0` previous nodes had none. Pushes
   zeros to 6 Vecs for `count` entries.

2. **push_default_dense_metadata()** (lines 506-522): Called per node without
   metadata when the block already has metadata. Pushes default values to 6 Vecs,
   with delta-encode transitions back to zero.

3. **add_dense_metadata() / add_dense_metadata_raw()** (lines 452-481, 749-764):
   Normal path -- pushes metadata fields to 6 Vecs with delta encoding.

**When does mixing occur?** Only in merge's `rewrite_block` when a block
contains both base elements (with metadata) and diff elements (without metadata,
since OSC elements are added with `metadata: None`). For cat/sort/extract, all
elements either have metadata (from the input PBF) or don't.

**Frequency in merge:** In Denmark merge, ~630 blocks are rewritten. Within
each rewritten block, ~0.1% of elements are from the diff (without metadata)
and ~99.9% are base elements (with metadata). The transition from metadata to
no-metadata happens at most once per block (when the first diff element in
sorted order appears).

**Cost of backfill_default_dense_metadata():** This is called when the very
first node with metadata is encountered, but previous nodes had none. This means
we go from no-metadata to has-metadata. In merge, this effectively never happens
because base nodes (which come first in sorted order) already have metadata.

**Cost of push_default_dense_metadata():** Called for each diff node without
metadata in a block that has metadata. At most ~10 diff nodes per block in
Denmark. Cost: 6 Vec pushes + 5 negation operations = ~15ns per call. Total
across all rewritten blocks: ~630 blocks * ~10 nodes * 15ns = ~95us. Negligible.

**Branch cost:** The `if let Some(meta) = metadata` check at line 436 and the
`if self.has_dense_metadata` check at line 443 are predictable branches. Modern
CPUs predict these correctly >99% of the time because the pattern within a block
is almost always consistent (metadata present for all nodes, with rare transitions).
Branch misprediction penalty (~15 cycles = ~3ns) * ~10 mispredictions per block
* 7400 blocks = ~222us. Negligible.

### Verdict

**Not real.** The backfill and branch costs are negligible (~100us total on
Denmark merge). The code is correct and handles edge cases robustly. No
optimization needed.

## 5. Dense node encoding analysis

### add_node() cost breakdown (lines 396-450)

Per-node operations:
1. **Assert + block_type set** (lines 405-409): assert is optimized away in
   release (no-op after first call). Block type set: 1 store. ~0.5ns.

2. **Delta encoding** (lines 411-421): 3 subtractions + 3 pushes to Vec<i64>
   + 3 stores for last_* state. Pushes are amortized O(1) since Vecs are
   pre-allocated to 8000. ~3ns.

3. **Tag interning** (lines 424-430): For each tag pair, 2 `string_table.add()`
   calls + 2 `push(i32)` to dense_keys_vals. Then push(0) delimiter. For a
   tagless node: just push(0). ~10ns * num_add_calls + 2ns push overhead.

4. **Metadata** (lines 436-447): When present, `add_dense_metadata()` does 6
   Vec pushes + 4 subtractions + 4 stores. ~5ns. Plus 1 `string_table.add(user)`
   = ~10ns. Total with metadata: ~15ns.

**Per-node total model:**
- Tagless node without metadata: 0.5 + 3 + 2 + 0 = ~5.5ns
- Tagless node with metadata: 0.5 + 3 + 2 + 15 = ~20.5ns
- Tagged node (2 tags) with metadata: 0.5 + 3 + (4*10 + 5*2) + 15 = ~68.5ns

**Measured from profiling:**
- Norway/Japan: 21ns (massive tagless node populations)
- Switzerland/Malta: 26-27ns
- London: 30ns (urban tagged nodes)
- Denmark: 43ns (older build, before some optimizations)

The model matches: Japan (85%+ tagless, ~5.5ns) * 0.85 + (2 tags, ~68ns) * 0.15
= ~14.9ns. Measured 21ns -- the gap is metadata overhead (most nodes have
metadata in real PBFs, adding ~15ns to tagless nodes = ~20ns, closer to 21ns).

### dense_keys_vals reallocation

Pre-allocated to `MAX_ENTITIES_PER_BLOCK * 2` = 16,000 entries (line 307).

For 8000 nodes with average 2 tags each: 8000 * (2*2 + 1) = 40,000 entries.
This would require reallocations: 16K -> 32K -> 64K. At ~4 bytes per i32:
64K * 4 = 256 KB final capacity.

**However:** Most blocks are dominated by tagless nodes. Planet data is ~85%
tagless nodes (way geometry). For a typical node block: 6800 tagless nodes
(6800 entries from delimiters) + 1200 tagged nodes * 5 entries avg = 6000.
Total: ~12,800 entries. Well within the 16,000 pre-allocation.

London is the worst case among profiled regions: denser tagged nodes. But even
London's average is ~30ns/node, suggesting the pre-allocation still covers most
blocks. Blocks that exceed 16K entries would reallocate once (16K -> 32K), at a
cost of ~1us per realloc (memcpy of ~64 KB). With perhaps ~5% of node blocks
exceeding the pre-allocation: 7400 * 0.6 node blocks * 0.05 * 1us = ~222us.
Negligible.

### Dense metadata Vec reuse across blocks

After `take()`, `reset()` (lines 864-895) clears all dense Vecs without
deallocating. `clear()` sets len=0 but retains capacity. For a sequence of node
blocks, this is perfect: after the first block, all Vecs have 8000 capacity and
never reallocate.

When switching from nodes to ways, the 13 dense Vecs (ids, lats, lons,
keys_vals, 6 metadata Vecs) retain their capacity:
- 3 Vecs of i64: 8000 * 8 = 192 KB
- 1 Vec of i32 (keys_vals): 16000 * 4 = 64 KB
- 6 metadata Vecs: various, ~192 KB total
- Total: ~448 KB retained but unused during way/relation processing.

This is inconsequential. 448 KB is tiny compared to the process's working set.
The Vecs would only be freed on `drop(BlockBuilder)` which happens once at
program exit.

## 6. Way/Relation encoding analysis

### add_way() cost breakdown (lines 528-552, 972-1022)

`add_way()` delegates to the free function `encode_way()` (line 972). Per-way:

1. **elem_scratch.clear()** (line 983): set len=0. ~0.5ns.

2. **encode_int64_field(elem, 1, id)** (line 986): field tag (1 byte) + varint
   encode of id. For typical way IDs (>1B): 5-6 bytes, ~2ns.

3. **Tags encoding** (lines 989-1001): Two passes over tags. For each tag:
   - `string_table.add(key)` -> ~10ns (fast path)
   - `encode_varint(packed, ...)` -> ~2ns
   Total per tag pair: ~24ns (2 add + 2 varint).
   Then `encode_bytes_field(elem, 2/3, packed)` for keys and vals: ~3ns each.
   For 3 tags: 3 * 24 + 6 = ~78ns.

4. **Info encoding** (lines 1003-1007): `encode_info_to()` (lines 945-968) writes
   5-6 fields to info_scratch. Includes 1 `string_table.add(user)` (~10ns) + 5
   field encodes (~2ns each). Total: ~20ns. Then `encode_bytes_field(elem, 4, info)`:
   ~3ns. Total: ~23ns.

5. **Refs delta encoding** (lines 1009-1018): For each ref, one subtraction +
   `zigzag_encode_64()` + `encode_varint()`. zigzag: 1 shift + 1 XOR = ~1ns.
   varint: branch + 1-5 byte writes = ~2ns. Total per ref: ~5ns.
   For 20 refs: 20 * 5 + 3 (encode_bytes_field overhead) = ~103ns.

6. **Group write** (line 1021): `encode_bytes_field(group_buf, 3, elem)` wraps
   the element as a PrimitiveGroup Way field. ~3ns.

**Per-way total model (3 tags, 20 refs, with metadata):**
0.5 + 2 + 78 + 23 + 103 + 3 = ~210ns.

**Measured:** Japan 205ns, Denmark 219ns, Norway 222ns, London 325ns.

Japan matches perfectly (42.9M ways, lean tagging). London's 325ns reflects
denser urban tagging (~8 tags/way avg): 8 * 24 + 6 = ~198ns for tags alone,
plus metadata and refs = ~326ns. Model holds.

### add_relation() cost breakdown (lines 593-617, 1122-1189)

Per-relation, `encode_relation()` is structurally similar to ways but with three
member arrays instead of refs:

1. **Tags** (lines 1136-1148): Same as ways. ~24ns per tag pair.

2. **Info** (lines 1150-1153): Same as ways. ~23ns.

3. **Member roles** (lines 1156-1164): For each member, `string_table.add(role)`
   (~10ns) + `encode_varint(packed, role_sid)` (~2ns) = ~12ns per member.

4. **Member IDs** (lines 1166-1173): Delta encode + zigzag + varint. ~5ns per
   member.

5. **Member types** (lines 1176-1184): varint(0/1/2). ~2ns per member.

**Per-member total:** 12 + 5 + 2 = ~19ns.
**Per-relation total model (3 tags, 10 members):** 72 + 23 + 190 + 9 = ~294ns.

**Measured:** Norway 292ns (matches perfectly, many simple relations), London
836ns (TfL routes with 30-100 members). For a 50-member relation:
72 + 23 + 50*19 + 9 = ~1054ns. London's 836ns suggests average ~40 members.

### Raw bytes passthrough performance

`add_way_raw_bytes()` (lines 688-711, 1100-1118) and `add_relation_raw_bytes()`
(lines 719-746, 1196-1218) skip all string interning and delta encoding. They
write pre-encoded protobuf field bytes directly:

```rust
fn encode_way_raw_bytes(group_buf, elem, id, keys_data, vals_data, refs_data, info_data) {
    elem.clear();
    encode_int64_field(elem, 1, id);          // ~2ns
    encode_bytes_field(elem, 2, keys_data);   // ~3ns (length prefix + memcpy)
    encode_bytes_field(elem, 3, vals_data);   // ~3ns
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);    // ~3ns
    }
    encode_bytes_field(elem, 8, refs_data);   // ~3ns
    encode_bytes_field(group_buf, 3, elem);   // ~3ns
}
```

Per-way raw: ~17ns (vs ~210ns for encode_way with string interning + delta encoding).
**12x faster** for unmodified base elements in merge.

For a 20-ref way, `refs_data` is ~60-100 bytes of pre-encoded packed sint64. The
`encode_bytes_field` just writes a field tag + length varint + memcpy of the raw
bytes. This is optimal -- the raw bytes are already in the correct wire format.

### Relation role string overhead

The reviewer asked about relations with 1000 members doing 1000 string lookups.
With the raw-bytes path, this is zero cost (roles_sid_data is pre-encoded). With
the normal path (add_relation, line 1161): 1000 * 12ns = ~12us per relation.
For a long bus route, this is measurable but not a hot spot because there are
few such relations per block (relations are ~0.1% of elements).

Even in London (the most relation-heavy profiled region), add_relation totals
26ms across 31K relations -- 0.2% of wall time. The 836ns/relation average
reflects the higher member counts, but the absolute contribution is negligible.

## 7. take() and serialization analysis

### take() flow (lines 772-804)

```rust
pub fn take(&mut self) -> io::Result<Option<&[u8]>> {
    self.encode_buf.clear();
    // 1. Encode string table (field 1)
    self.string_table.encode_to(&mut self.encode_buf, &mut self.elem_scratch);
    // 2. Encode PrimitiveGroup (field 2)
    match block_type {
        DenseNodes => self.encode_dense_nodes_group(),  // builds into encode_buf
        Ways | Relations => encode_bytes_field(&mut self.encode_buf, 2, &self.group_buf),
    }
    self.reset();
    Ok(Some(&self.encode_buf))
}
```

### Active buffer inventory during take()

**For dense node blocks (encode_dense_nodes_group, lines 820-862):**

| Buffer | Purpose | Typical size |
|--------|---------|--------------|
| encode_buf | Final PrimitiveBlock output | ~130 KB |
| group_buf | DenseNodes body (intermediate) | ~100 KB |
| elem_scratch | DenseInfo body / PrimitiveGroup wrapper | ~40-80 KB |
| packed_scratch | Individual packed field content | ~32 KB |

Peak concurrent memory: ~380 KB. All buffers are reusable across take() calls.

The encoding uses a 3-level nesting pattern:
1. Packed field data into packed_scratch (cleared per field)
2. DenseInfo/DenseNodes submessage into elem_scratch/group_buf
3. PrimitiveGroup into elem_scratch (reused), then into encode_buf
4. PrimitiveBlock into encode_buf (final output)

The `elem_scratch` buffer is cleverly reused at multiple nesting levels. It holds
DenseInfo at level 2 (lines 830-846), then is cleared and reused as the
PrimitiveGroup wrapper at level 3 (lines 857-858). This avoids a separate buffer.

**For way/relation blocks:**

Much simpler. `group_buf` already contains all encoded Way/Relation messages
(accumulated incrementally via add_way/add_relation). take() just wraps it:

```rust
encode_bytes_field(&mut self.encode_buf, 2, &self.group_buf);
```

Two buffers active: encode_buf (~130 KB) + group_buf (~120 KB). elem_scratch
is only used for the string table encoding.

### String table encoding (lines 131-141)

```rust
fn encode_to(&self, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) {
    scratch.clear();
    for s in &self.strings {
        encode_bytes_field_always(scratch, 1, s.as_bytes());
    }
    encode_bytes_field(buf, 1, scratch);
}
```

For ~1200 strings averaging 12 bytes: scratch fills to ~1200 * (1 + 1 + 12) =
~16.8 KB. Then encode_bytes_field wraps it with a field tag + length varint
(~3 bytes) into buf. Total encode cost: 1200 * ~5ns (tag + length + memcpy)
+ 1 memcpy of ~17 KB = ~10us per block. At 7400 blocks: ~74ms. This is included
in take()'s measured 3.46s (8% of wall).

### reset() cost (lines 864-895)

`reset()` clears 10+ Vecs plus the FxHashMap. All use `clear()` which sets
len=0 without deallocation. For the FxHashMap, `clear()` is O(capacity) --
it must zero the control bytes (or equivalent) for all buckets.

After a full node block with ~1200 unique strings, the FxHashMap has grown to
capacity ~2048 (next power of 2 above 1200). `clear()` on 2048 buckets is
~2048 * 1 byte = ~2 KB of zeroing, taking ~100ns. For a London block with 3000+
unique strings, capacity grows to ~4096. Clear cost: ~200ns.

Across all blocks, reset() is dominated by the HashMap clear (~100-200ns) plus
~13 Vec clears (~50ns total). Per-block cost: ~200-300ns. At 7400 blocks: ~2ms.
Completely negligible.

### StringTable capacity growth across blocks

After a London node block with 3000 unique strings, the FxHashMap retains ~4096
bucket capacity. The next block might only have 800 unique strings, wasting ~3200
buckets (~25 KB). But this capacity is never reclaimed (clear() doesn't shrink).

Over a planet-scale write: the maximum capacity seen for any block determines the
steady-state allocation. With planet data, some blocks (urban areas) might have
4000+ unique strings, growing the map to 8192 buckets (~65 KB). This capacity
persists for all subsequent blocks.

Total wasted memory: at most ~65 KB (one HashMap) + ~200 KB (one Vec<String> at
max capacity). Completely negligible relative to process RSS (~20-130 MB).

### take() timing from profiling

From `notes/hotpath-profile.md`:

| Workload | Calls | Avg | Total | % Wall |
|----------|-------|-----|-------|--------|
| cat Denmark (commit d5c8095) | 7,396 | 468us | 3.46s | 8.3% |
| cat Denmark (commit 75e8edd, pipelined) | 7,378 | -- | 4.9 GB alloc* | -- |
| merge Denmark (indexdata+zlib, commit 14034c1) | 8,019 | 29us | 233ms | 7% |
| merge Denmark (indexdata+none) | 7,407 | 81us | 597ms | 31% |

*Cumulative alloc includes pipelined rayon thread overhead.

The dramatic drop from 468us to 29us between cat and merge (parallel rewrite)
reflects that merge rewrite blocks are smaller (mixed base + diff elements, most
elements use raw-bytes passthrough so group_buf is smaller) and that the parallel
rewrite's take() runs on rayon workers with warm thread-local buffers.

## 8. Additional findings

### 8.1 take() returns &[u8] -- forced to_vec() copy (HIGH IMPACT)

**This is the highest-impact finding in this investigation.**

`take()` (line 772) returns `Option<&[u8]>` borrowing from `self.encode_buf`.
This borrow prevents the caller from calling any `&mut self` method (like
`add_*()`) until the borrow is dropped. The standard usage pattern is:

```rust
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
```

In **sync mode** (writer.rs lines 347-355), `write_primitive_block` writes
directly -- no copy needed. The borrow works perfectly.

In **pipelined mode** (writer.rs lines 326-346):
```rust
let uncompressed = block_bytes.to_vec();  // COPY: ~130 KB
```
The rayon task needs owned data, so the borrowed slice must be copied.

In **merge parallel rewrite** (merge.rs lines 400-403):
```rust
fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) -> MergeResult<()> {
    if let Some(bytes) = bb.take()? {
        output.push(bytes.to_vec());          // COPY: ~130 KB
    }
}
```
Same problem: local output Vec needs owned data.

**Cost quantification:**

| Workload | Blocks | Avg block size | Copy total | At planet scale |
|----------|--------|----------------|------------|-----------------|
| Denmark cat (pipelined) | 7,400 | ~130 KB | ~960 MB | ~155 GB |
| Denmark merge (parallel rewrite) | 630 | ~130 KB | ~82 MB | ~143 GB* |

*Planet merge at 92% rewrite = 1.1M blocks * 130 KB = 143 GB of copies.

At ~0.8 GB/s memcpy throughput (for L2-miss copies): 155 GB / 0.8 = ~194 seconds
for planet cat. 143 GB / 0.8 = ~179 seconds for planet merge.

**Fix:** `take()` could return `Vec<u8>` by swapping `encode_buf` with an empty
Vec (or using `std::mem::replace`). The caller receives an owned Vec. The
BlockBuilder gets a new empty Vec for the next block, which grows on the next
take(). After one block, the "new" Vec has settled at the right capacity (the
old one was returned, the replacement grows once).

Alternatively, `take()` could return a bespoke type that wraps the Vec and
returns it to the BlockBuilder on drop (like a pool). This would preserve buffer
reuse while providing ownership to the caller.

The most practical approach: add `take_owned(&mut self) -> Option<Vec<u8>>`
that swaps encode_buf, returning the owned Vec. Callers that need ownership
(pipelined writer, parallel rewrite) use `take_owned()`. Callers that can work
with a borrow (sync writer) continue using `take()`.

**Cost of the swap approach:** After the first block, each take_owned() call:
1. `std::mem::replace(&mut self.encode_buf, Vec::new())` -- ~2ns
2. Returns the old Vec to the caller (zero-copy move)
3. Next take_owned() call: encode_buf is empty, needs to grow (~130 KB alloc)
4. Steady state: 1 alloc per block for the new encode_buf

This trades buffer reuse (0 allocs after warmup) for 1 alloc per block. But the
alloc (~130 KB from the allocator, ~200ns) is far cheaper than the memcpy
(~130 KB, ~10-50us for L2-miss data). The net savings is the eliminated memcpy.

On Denmark: 7400 blocks * ~130 KB memcpy = ~960 MB of eliminated copies. At
~10us per 130 KB copy: ~74ms saved. At planet scale: ~194 seconds saved.

**This directly connects to Box 5 (writer.rs) finding about the to_vec copy.**

### 8.2 encode_packed_sint64 cost at planet scale

Each `encode_packed_sint64` call iterates the input slice, zigzag-encodes each
value, then varint-encodes each value. From the call sites in
`encode_dense_nodes_group()`:

| Field | Entries per block | Operations per entry | Total ops per block |
|-------|-------------------|----------------------|---------------------|
| dense_ids (field 1) | 8000 | 1 zigzag + 1 varint | 16,000 |
| dense_lats (field 8) | 8000 | 1 zigzag + 1 varint | 16,000 |
| dense_lons (field 9) | 8000 | 1 zigzag + 1 varint | 16,000 |
| dense_timestamps (field 2) | 8000 | 1 zigzag + 1 varint | 16,000 |
| dense_changesets (field 3) | 8000 | 1 zigzag + 1 varint | 16,000 |
| **Total per node block** | | | **80,000** |

Plus way refs across all way blocks. At planet scale:
- 8.5B nodes / 8000 per block = 1.0625M node blocks * 80K ops = **85B zigzag+varint ops**
- 800M ways * ~20 refs average = 16B ref entries, each needing zigzag + varint = **32B ops**
- Total: **~117B zigzag+varint operations**

At ~3ns per zigzag+varint pair (shift, XOR, branch, 1-5 byte writes): **~351 seconds**.

This is the irreducible encode floor. It cannot be optimized away -- these
operations produce the actual protobuf wire format. SIMD varint encoding could
help (~2x speedup for long packed fields), but the implementation complexity is
high and the benefit is bounded by the overhead of scatter/gather for variable-
length varints.

For comparison: planet cat wall time is ~42s * 80x = ~56 minutes. The ~351s
encode floor is ~10.4% of that, consistent with take()'s 8.3% share in profiling
(take includes string table encoding overhead too).

### 8.3 Metadata user string hash overhead

The reviewer noted that `add_dense_metadata()` calls `self.string_table.add(meta.user)`
for every node (line 474). With planet data, most nodes are by a small set of
users (mechanical edits by bots). The string table deduplicates, but the hash
lookup still happens.

**Quantification:** 8.5B nodes * 1 add(user) call * ~10ns (fast-path) = **85 seconds**
at planet scale. This is real but not the dominant cost -- it's ~25% of the total
StringTable time for nodes.

For the merge path: raw-index API passes `user_sid` as a pre-mapped integer
(lines 749-764), so zero hash cost.

For write-only commands: this is baked into the 21-43ns add_node per-call cost.
Not independently optimizable without changing the Metadata<'a> struct to carry
an optional raw user_sid.

### 8.4 Way tags iterate twice

`encode_way()` (lines 989-1001) iterates the tags slice twice -- once for keys,
once for values:

```rust
for &(key, _) in tags {
    encode_varint(packed, u64::from(string_table.add(key)));
}
encode_bytes_field(elem, 2, packed);
packed.clear();
for &(_, val) in tags {
    encode_varint(packed, u64::from(string_table.add(val)));
}
```

This is required by the protobuf format: keys and values are separate packed
repeated fields (field 2 and field 3). There is no way to interleave them.

However, the double iteration means each tag's `(&str, &str)` pair is accessed
from memory twice. For 3 tags, the 6 pointers (48 bytes) easily fit in L1.
For tags_buf slices from the commands, the slice itself is contiguous. No
measurable overhead from the double iteration.

### 8.5 group_buf growth for way/relation blocks

For way blocks, `group_buf` accumulates all 8000 ways' encoded bytes. A typical
way (3 tags, 20 refs) encodes to ~80-120 bytes. 8000 ways: ~640 KB - 960 KB.

`group_buf` starts empty (line 331: `Vec::new()`) for each BlockBuilder. On the
first way block, it grows through doublings: 0 -> initial -> ... -> ~1 MB.
After take()/reset(), group_buf is cleared but retains capacity. Subsequent way
blocks reuse the capacity.

The only concern: the first way block after construction incurs ~10-15
reallocations (doublings from 0 to ~1 MB). At ~200ns per realloc (including
memcpy of the growing buffer): ~3us. Negligible.

For the node-to-way transition at type boundaries: group_buf was unused during
node processing (capacity 0). The first way block builds it up from scratch.
This is the intended behavior -- no wasted capacity during node processing.

## 9. Cross-box interactions

### Box 5 (writer.rs): take() return type forces to_vec()

The `write_primitive_block()` method (writer.rs line 325) receives `&[u8]` from
take() and must call `.to_vec()` for the pipelined rayon task. If take() could
return `Vec<u8>`, the pipelined path would be zero-copy (move the Vec into the
rayon closure).

In sync mode, write_framed_blob() (writer.rs lines 458-493) receives `&[u8]` and
writes directly to the output. No copy. The borrow return type is correct here.

**Proposed solution:** Add `take_owned()` that returns `Vec<u8>`. Use it in
pipelined paths. Keep `take()` returning `&[u8]` for sync paths. Or: change
`write_primitive_block` to accept `Vec<u8>` (owned) instead of `&[u8]` (borrowed),
pushing the ownership boundary to the caller.

### Box 3 (wire parsing): BlockBuilder output must round-trip

BlockBuilder's encoding must produce bytes that `WireBlock::parse()` can decode.
This is verified by the roundtrip tests (`tests/roundtrip.rs`). The wire-format
encoding in block_builder.rs mirrors the read-side wire.rs parsing:
- Dense nodes: packed sint64 for ids/lats/lons, packed int32 for keys_vals
- Ways/Relations: field tags match read-side expectations
- String table: repeated bytes field at PrimitiveBlock field 1

The hand-rolled wire encoding is strictly correct by construction (same field
numbers, same wire types, same encoding conventions as the OSM PBF spec).

### Box 8 (commands): merge uses raw APIs, other commands use standard APIs

The performance gap between the two paths is significant:

| API path | Per-way cost | Use case |
|----------|-------------|----------|
| add_way() | ~210ns | cat, sort, extract, tags_filter |
| add_way_raw_bytes() | ~17ns | merge (base elements) |
| add_way() with OSC data | ~210ns | merge (diff elements) |

The 12x speed difference comes from skipping string interning + delta encoding.
For commands that do full decode+reencode (cat, sort), the raw API is not
applicable because the elements are already decoded to string form.

A potential optimization for cat/sort: add a "clone block" API that copies an
entire PrimitiveBlock's wire-format bytes directly when no transformation is
needed. This would bypass BlockBuilder entirely for passthrough blocks.

## 10. Recommended actions (prioritized)

### P0: Add take_owned() to eliminate pipelined to_vec() copy

**Impact:** ~960 MB/run on Denmark, ~155 GB at planet scale. ~74ms on Denmark,
~194 seconds at planet scale.
**Complexity:** Low. Add one method to BlockBuilder, change callers.
**Risk:** Low. Straightforward ownership transfer.

### P1: No action needed on StringTable for merge

The pre-seed + raw-index optimization is already implemented and deployed.
StringTable::add is no longer on the merge hot path for base elements.

### P2: No action needed on block size tuning

8000 is correct. No compatibility, cache, or performance reason to change it.

### P3: No action needed on dense metadata backfill

Negligible cost (~100us on Denmark merge). Correct handling of edge cases.

### P4: Consider SIMD varint encoding for encode_packed_* (speculative)

**Impact:** Potentially 2x on the ~351-second planet-scale encode floor.
**Complexity:** High. Requires platform-specific SIMD code paths.
**Risk:** Medium. varint encoding is variable-width, hard to SIMD effectively.
**Verdict:** Research only. Not actionable without profiling on actual planet data.

### P5: Consider eliminating double String allocation in slow path

**Impact:** ~11 seconds at planet scale (from ~22s slow-path allocation to ~11s).
**Complexity:** Medium. Requires changing StringTable's data structure.
**Risk:** Low but the savings are marginal relative to total write time.
**Verdict:** Low priority. The slow path is 1% of calls. Other bottlenecks
(compression at 57%) dominate.

### Not recommended

- **Block size tuning:** compatibility risk, no measurable gain.
- **ahash replacement:** ~22 seconds at planet scale, adds a dependency for the
  same trusted-input scenario where FxHash is already optimal.
- **Pre-allocating dense_keys_vals higher:** wastes memory on the 95% of blocks
  that don't need it. The rare reallocation costs ~1us.
- **Alternative HashMap for StringTable (IndexMap, etc.):** same performance
  characteristics as FxHashMap + Vec. No net benefit.
