# Box 8: Command Layer Algorithms -- Performance Review

## 1. Executive Summary

The command layer (`src/commands/`) contains 15 modules implementing the CLI
feature set. Performance characteristics vary wildly by command:

- **merge** is the most performance-critical command (weekly planet refresh).
  Its 4-phase batch pipeline is well-designed; the main concern is the
  `flush_local` double-copy pattern that adds ~30 GB unnecessary copying at
  planet scale.
- **tags_filter** two-pass mode has a planet-scale memory blocker: `way_dep_node_ids`
  as a sorted `Vec<i64>` can reach ~40 GB for broad filters like `highway=*`.
- **sort** is O(num_blobs) for pre-sorted inputs (the common case) but degrades
  to O(num_elements) for unsorted inputs, where overlap run materialization
  would require ~1 TB for a planet file.
- **extract** uses `IdSetDense` (1 bit/ID) appropriately, but smart mode pass 2
  reads all blob types when it only needs ways.
- **derive_changes** and **diff** load both PBFs entirely into memory as owned
  Vecs, making them fundamentally infeasible at planet scale.

---

## 2. Merge (`src/commands/merge.rs`, ~1429 lines)

### 2A. Pipeline Architecture

Single-pass 4-phase batch pipeline (lines 3-7):
```
Phase 1: Parallel classify       [rayon pool]
Phase 2: Sequential inline assign [main thread, O(log n) per blob]
Phase 3: Parallel rewrite        [rayon pool]
Phase 4: Sequential output       [main thread]
```

The reader thread (lines 1014-1031) uses `mpsc::sync_channel(BATCH_SIZE)` to
decouple I/O from processing. The main loop receives batches of up to 64 raw
frames, processes all 4 phases, then loops.

This is a good design. The sync channel caps memory at ~64 * ~250 KB = ~16 MB
of read-ahead. The 4-phase structure cleanly separates concerns and allows
phases 1 and 3 to use rayon parallelism while phases 2 and 4 maintain sequential
ordering.

### 2B. DiffRanges Binary Search Cost

`DiffRanges` (lines 135-231) stores 6 sorted `Vec<i64>` vectors: 3 for all
affected IDs (upserts + deletes) and 3 for upsert-only IDs.

`range_overlaps()` (lines 207-219) uses `partition_point` (binary search) to
check if any diff ID falls within a blob's `[min_id, max_id]` range. Cost is
O(log D) where D is the number of diff IDs for that element type.

For a daily planet diff (~600K changes): D ~ 200K per type (assuming roughly
equal distribution), so log2(200K) ~ 18 comparisons per blob. With ~43K blobs
(planet), that's ~43K * 18 = ~774K comparisons total. This is negligible --
under 1 ms on modern hardware.

The inline upsert assignment in Phase 2 (lines 1109-1113) does two
`partition_point` calls per NeedsRewrite blob to extract the slice of upserts
within the blob's ID range. Same O(log D) cost. Since only ~8% of blobs need
rewriting (planet daily diff), this is ~3.4K * 2 * 18 = ~122K comparisons.

**Verdict:** DiffRanges is not a bottleneck. The sorted Vec + partition_point
approach is essentially optimal for this access pattern.

### 2C. classify_only Breakdown

`classify_only()` (lines 840-879) has 3 tiers:

1. **Index hit** (line 849-853): Uses `BlobHeader` indexdata to check overlap
   without decompression. Cost: ~100 ns (one `partition_point` on the diff
   vectors). This is the fast path used by ~92% of blobs at planet scale.

2. **Scan-only** (lines 856-865): Decompresses blob data into a reusable
   buffer, calls `scan_block_ids()` to extract min/max ID from raw protobuf
   bytes without full parsing. Cost: ~500 us (decompression) + ~50 us (scan).
   Used when blobs lack indexdata.

3. **Full parse** (lines 867-878): Calls `parse_primitive_block_from_bytes_owned()`
   then `block_overlaps_diff()` for per-element checking. Cost: ~1-2 ms
   (full protobuf parse). Used only when range overlaps but need to confirm
   actual element overlap. The `std::mem::take(buf)` at line 868 avoids
   allocating a new buffer by moving the reusable one into the Bytes wrapper.

Tier distribution for a typical daily diff on Denmark (465 MB, 4704 blobs):
- Index hit: ~4300 blobs (91.4%)
- Scan + FalsePositive: ~200 blobs (4.3%)
- NeedsRewrite: ~200 blobs (4.3%)

At planet scale the rewrite fraction drops to ~8%, so classification is
dominated by tier 1 (index hit). Total classify wall time is under 100 ms.

### 2D. rewrite_block_parallel Per-Element Cost

`rewrite_block_parallel()` (lines 651-785) iterates every element in a block,
checking for deletes, modifications, and creates. Per-element cost breakdown:

- **Delete check** (e.g., line 686): `diff.deleted_nodes.contains(&id)` --
  HashSet O(1) lookup, ~30 ns.
- **Modify check** (e.g., line 688): `diff.nodes.get(&id)` -- HashMap O(1)
  lookup, ~40 ns.
- **Upsert cursor advance** (lines 673-681): linear scan through sorted
  upsert slice. Amortized O(1) since both element IDs and upsert IDs are
  sorted, and the cursor only moves forward.
- **Write base element** (e.g., line 702): `write_base_dense_node_local()` --
  copies raw bytes from source block via `add_node_raw()` or `add_way_raw_bytes()`.
  Cost depends on element size: ~200 ns for nodes, ~400 ns for ways.
- **Write modified element** (e.g., lines 690-698): `bb.add_node()` from
  OscNode fields -- must convert f64 lat/lon to decimicro, copy tags.
  ~500 ns for nodes, ~800 ns for ways.

The key optimization is `bb.pre_seed_string_table(block)` at line 662, which
copies the source block's string table so that base element raw byte passthrough
can reference existing string indices without re-encoding strings. This avoids
the O(n) cost of re-inserting every string.

Per-block cost for a typical 8000-element node block where ~5 elements are
modified: ~8000 * 70 ns (ID checks) + 5 * 500 ns (modifications) + 7995 * 200 ns
(raw copies) = ~2.2 ms. The function is well-optimized.

### 2E. Parallel Structure

Phase 1 (lines 1087-1093) and Phase 3 (lines 1127-1142) use rayon's
`par_iter().map_init()` pattern. The `map_init` provides per-thread state:
- Phase 1: `Vec::new()` as a reusable decompression buffer
- Phase 3: `BlockBuilder::new()` as a reusable block encoder

This is correct -- avoids per-blob allocation while keeping thread-local state.

Phase 2 (lines 1095-1124) is inherently sequential because inline upsert
assignment depends on the blob ordering. The O(log n) per-blob cost makes
this fast enough that parallelism is unnecessary.

Phase 4 (lines 1152-1262) is sequential for output ordering. It includes
gap-create emission and passthrough coalescing.

### 2F. Passthrough Coalescing

`coalesce_passthrough()` (called at lines 1211-1220) accumulates consecutive
raw passthrough frame bytes into a single buffer. When a rewrite blob or
type transition is encountered, `flush_passthrough_buf()` writes the entire
accumulated buffer as a single `write_raw_owned()` call.

At 92% passthrough (planet), this collapses ~39K individual channel sends
into far fewer large writes. The comment at lines 1056-1059 documents this.
For a 75 GB planet file with 43K blobs, the coalesced buffer can accumulate
up to ~64 * 250 KB = ~16 MB per batch before flushing.

### 2G. DiffOverlay Memory

`DiffOverlay` (from `src/osc.rs`) stores:
- `HashMap<i64, OscNode>` -- node upserts
- `HashMap<i64, OscWay>` -- way upserts
- `HashMap<i64, OscRelation>` -- relation upserts
- `HashSet<i64>` per type -- delete IDs

For a daily planet diff (~600K changes):
- ~200K nodes * ~120 bytes/OscNode = ~24 MB
- ~200K ways * ~300 bytes/OscWay = ~60 MB
- ~50K relations * ~500 bytes/OscRelation = ~25 MB
- HashSets: ~100K * 72 bytes = ~7 MB
Total: ~116 MB. This is comfortable.

For a weekly diff (5 daily diffs accumulated): ~580 MB. Still fine.

### 2H. Worst-Case Degeneration

The pipeline degenerates when every blob needs rewriting (100% overlap). This
would happen if a diff modifies every element type across the entire ID range.
In practice, daily diffs modify <<1% of elements, and even weekly accumulations
stay under 5%. A full re-import would be better handled by a fresh write rather
than merge.

---

## 3. Extract (`src/commands/extract.rs`, ~1581 lines)

### 3A. IdSetDense Memory

`IdSetDense` (lines 17-92) uses chunked 4 MB byte arrays, each covering
2^25 = 33,554,432 IDs (CHUNK_BITS=22, so 2^22 bytes * 8 bits = 2^25 IDs
per chunk).

Memory is allocated per chunk on first access. For planet node IDs (up to
~12.5B):
- Chunk index = ID >> 25, so max chunk index ~ 12.5B / 33.5M ~ 373
- Worst case (every chunk accessed): 373 chunks * 4 MB = ~1.49 GB per IdSetDense

Extract uses up to 6 IdSetDense instances in smart mode (lines 901-906):
`bbox_node_ids`, `matched_way_ids`, `all_way_node_ids`, `matched_relation_ids`,
`extra_way_ids`, `extra_node_ids`. Worst case: 6 * 1.5 GB = ~9 GB.

In practice, only chunks containing actual IDs are allocated, so for a city
extract from a planet file the memory is much lower. But for a "world minus
one country" extract, most chunks would be populated: ~6-8 GB.

### 3B. Strategy Comparison

| Strategy | Passes | Read mode | ID sets | Planet memory |
|---|---|---|---|---|
| Simple | 2 | into_blocks_pipelined | 3 IdSetDense | ~4.5 GB |
| CompleteWays | 2 | into_blocks_pipelined | 4 IdSetDense | ~6 GB |
| Smart | 3 | into_blocks_pipelined | 6 IdSetDense | ~9 GB |

All strategies use the same parallel batch write pattern (BATCH_SIZE=64)
for their final pass.

### 3C. Sequential Pass 1 Bottleneck

All three strategies use `into_blocks_pipelined()` for pass 1 but process
blocks sequentially on the main thread (e.g., lines 565-591 for simple).
The IdSetDense inserts are O(1) but not thread-safe, so the main thread is
the consumer. Since pipelined delivery already parallelizes decompression,
the main thread bottleneck is the IdSetDense insert loop, which at ~20 ns
per insert processes ~8.5B nodes in ~170 seconds. The decompression pipeline
delivers blocks faster than this, so the main thread is indeed the bottleneck
in pass 1.

A parallel fold+reduce pattern (similar to check_refs or tags_count) could
help, using per-thread IdSetDense instances merged via the `merge()` method
(lines 73-91). The merge method already handles non-overlapping chunks via
move (zero copy) and overlapping chunks via bitwise OR.

### 3D. Missing BlobFilter in Smart Pass 2

Smart pass 2 (lines 920-936) opens a fresh `ElementReader` without a
`BlobFilter`. It only needs way blobs (to resolve `extra_way_ids` node refs),
but it reads and decompresses ALL blob types including nodes and relations.

Fix: add `reader.with_blob_filter(BlobFilter::only_ways())`.

At planet scale, this wastes decompression of ~38K node blobs + ~2K relation
blobs (out of ~43K total). Node blobs alone constitute ~80% of the file.
Fixing this would reduce smart pass 2 time from ~90 seconds to ~15 seconds
(only way blobs, ~5K blobs * 250 KB * decompression cost).

---

## 4. Tags Filter (`src/commands/tags_filter.rs`, ~938 lines)

### 4A. Memory Model

Two-pass mode (lines 565-689) uses sorted `Vec<i64>` for ID storage instead
of BTreeSet or HashSet. The rationale is documented extensively in comments
(lines 581-607):

- `Vec<i64>`: 8 bytes per entry, contiguous, cache-friendly binary search
- `BTreeSet<i64>`: ~40 bytes per entry (node pointers, alignment, balance metadata)
- `HashSet<i64>`: ~72 bytes per entry (hash + metadata + bucket overhead)

The clean separation (pass 1 inserts only, pass 2 lookups only) means no
interleaved insert+lookup, making sorted Vec ideal.

### 4B. Planet-Scale Memory Problem

For a broad filter like `highway=*`:
- ~250M ways match (all ways with any highway tag)
- Each matching way contributes ~10 node refs to `way_dep_node_ids`
- `way_dep_node_ids` size: 250M * 10 * 8 bytes = **20 GB**

For even broader filters (e.g., `name=*`):
- ~500M elements match
- `way_dep_node_ids` could reach **40 GB**

After `sort_unstable()` (line 674), the Vec must fit entirely in memory at
peak (unsorted + sorted copy during sort, though `sort_unstable` is in-place
so it only needs the original allocation + O(log n) stack).

The real problem is that `way_dep_node_ids` stores raw `i64` refs including
duplicates (line 643: `way_dep_node_ids.extend(w.refs())`). Many nodes are
referenced by multiple matching ways. `dedup()` at line 675 removes these,
but only after sorting. Peak memory is the unsorted Vec with all duplicates.

**This is the most severe planet-scale blocker in the command layer.**

Mitigation options:
1. Use `IdSetDense` (1 bit per ID, ~1.5 GB for full planet node range)
2. Periodic sort+dedup during pass 1 (e.g., every 10M insertions)
3. Use `RoaringBitmap` (compressed bitset, ~2-3 GB)

Option 1 is the simplest and matches extract's approach. The only reason
tags_filter uses Vec<i64> is historical (preceded IdSetDense implementation).

### 4C. Binary Search Overhead in Pass 2

Pass 2 performs `binary_search()` on every element (lines 463-464):
```rust
let direct = ids.matched_node_ids.binary_search(&dn.id()).is_ok();
let from_way = ids.way_dep_node_ids.binary_search(&dn.id()).is_ok();
```

For planet node processing:
- ~8.5B nodes * 2 binary searches per node
- matched_node_ids: ~250M entries, log2(250M) = ~28 comparisons
- way_dep_node_ids: ~2B entries (after dedup), log2(2B) = ~31 comparisons
- Total: 8.5B * (28 + 31) = ~501B comparisons

At ~0.3 ns per comparison (L1 cache hit) to ~5 ns (L3 cache miss), this
ranges from 2.5 minutes (best case) to 42 minutes (worst case). With
~2B entries in way_dep_node_ids (16 GB), cache misses are likely for the
final levels of binary search. Realistic estimate: **~15-25 minutes** for
pass 2 node processing alone.

Using `IdSetDense` would reduce this to O(1) per lookup (~20 ns), giving
~170 seconds for 8.5B lookups. A ~5-10x improvement.

### 4D. Single-Pass Mode (-R)

Single-pass mode (`tags_filter_single_pass`, called at line 244 when
`omit_referenced` is true) avoids the entire ID collection problem. It uses
the standard BATCH_SIZE=64 parallel pattern with per-block filtering (lines
265-350). No ID sets at all. This mode is fine at any scale.

---

## 5. Sort (`src/commands/sort.rs`, ~638 lines)

### 5A. Blob-Level Permutation Sort

The algorithm (lines 107-111 doc comment):
1. Pass 1: Build blob index (type + min/max ID per blob)
2. Sort index by (type_order, min_id)
3. Pass 2: Write blobs in sorted order

For already-sorted inputs (all Geofabrik downloads), the sort is a no-op --
the permutation is identity, all blobs pass through as raw bytes.

Memory is O(num_blobs): ~43K blobs * ~40 bytes/BlobEntry = ~1.7 MB for planet.

### 5B. Overlap Run Catastrophe

`detect_overlaps()` (lines 296-307) marks adjacent same-type blobs as
overlapping when `max_id[i] >= min_id[i+1]`.

`write_overlap_run()` (lines 391-438) materializes ALL elements in the
overlap run into owned structs:
```rust
let mut nodes: Vec<OwnedNode> = Vec::new();
let mut ways: Vec<OwnedWay> = Vec::new();
let mut relations: Vec<OwnedRelation> = Vec::new();
```

Per-element memory:
- `OwnedNode`: id(8) + lat(4) + lon(4) + tags(Vec overhead 24 + content) +
  metadata(Option<OwnedMetadata> ~80) = ~120+ bytes
- `OwnedWay`: id(8) + tags(24+) + refs(Vec 24 + 8*refs) + metadata(~80) = ~200+ bytes
- `OwnedRelation`: id(8) + tags(24+) + members(Vec 24 + ~40*members) + metadata(~80) = ~300+ bytes

For a fully unsorted planet file (worst case, all nodes overlap):
- ~8.5B nodes * 120 bytes = **~1 TB**

This is a known issue documented in the code's O(num_blobs) claim (line 6),
which is only true when there are zero overlaps. The claim should note this
caveat.

In practice, this only matters for:
1. Completely unsorted PBFs (rare -- all major providers sort)
2. Manually concatenated PBFs without re-sorting
3. PBFs from editors (JOSM export) that may have mixed ordering

For the common case (pre-sorted Geofabrik PBFs), overlaps = 0 and the
entire sort is pure passthrough.

### 5C. Separate Owned Element Types

Sort defines its own `OwnedNode`/`OwnedWay`/`OwnedRelation` (lines 67-101)
rather than reusing `owned_elements.rs` from derive_changes/diff. The sort
versions include `OwnedMetadata` (with full user string, changeset, etc.)
while `owned_elements.rs` only stores `version: Option<i32>`.

This duplication is intentional: sort needs full metadata fidelity for
roundtrip correctness, while derive_changes/diff only compare versions.

---

## 6. Batch Size Analysis

### 6A. Where Used

`BATCH_SIZE = 64` appears in 7 commands:
- `merge.rs` line 1011
- `extract.rs` line 525
- `tags_filter.rs` line 254
- `cat.rs` (filtered path)
- `getid.rs` line 17
- `tags_count.rs` line 13
- `add_locations_to_ways.rs` line 251

### 6B. Mechanics

The batch pattern:
```rust
let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
for block in reader.into_blocks_pipelined() {
    batch.push(block?);
    if batch.len() >= BATCH_SIZE {
        // par_iter().map_init(BlockBuilder::new, |bb, block| { ... }).collect()
        batch.clear();
    }
}
```

Each `PrimitiveBlock` is ~1-2 MB decompressed (varies by content). A batch
of 64 blocks holds ~64-128 MB of decoded data. This is the unit of work
dispatched to rayon.

### 6C. Granularity

BATCH_SIZE=64 balances two tensions:
1. **Too small** (e.g., 8): rayon dispatch overhead dominates. Each rayon
   `par_iter()` call has ~1-5 us overhead for work stealing setup.
2. **Too large** (e.g., 512): memory pressure from holding 512 decoded blocks
   (~512 MB to 1 GB), and sequential output stalls waiting for large batches.

64 is reasonable. On a 16-core machine, each rayon thread processes 4 blocks
per batch, giving ~4-8 ms of work per thread -- well above the scheduling
overhead. Memory is ~128 MB per batch, modest relative to other allocations.

Per-command tuning would not yield meaningful gains. The bottleneck is
per-element processing, not batch dispatch.

---

## 7. Per-Command Read Mode Mapping

| Command | Read mode | Reason |
|---|---|---|
| merge | Custom `FileReader` + `read_raw_frame` | Needs raw bytes for passthrough |
| sort | Custom `FileReader` + seek | Needs random access by file offset |
| cat (passthrough) | Custom `FileReader` + raw frames | Raw blob copy |
| cat (filtered) | `into_blocks_pipelined` | Standard decode + filter |
| extract | `into_blocks_pipelined` | Standard decode, multi-pass |
| tags_filter (single) | `into_blocks_pipelined` (via batch) | Parallel filter |
| tags_filter (two-pass) | `into_blocks_pipelined` | Sequential pass 1, parallel pass 2 |
| add_locations_to_ways | `into_blocks_pipelined` | Standard decode, two-pass |
| getid | `into_blocks_pipelined` | Standard decode, optional two-pass |
| check_refs | `for_each_pipelined` | Element-level consumer, no write |
| node_stats | `for_each_pipelined` | Element-level consumer, no write |
| tags_count | `into_blocks_pipelined` (batch) | par_iter fold+reduce |
| derive_changes | `BlobReader::open` (sequential) | Full memory load |
| diff | `BlobReader::open` (sequential) | Full memory load |
| fileinfo | `BlobReader::open` (sequential) | Metadata only, fast |

**Key observations:**
- Commands needing raw bytes (merge, sort, cat passthrough) use custom read
  loops. This is correct -- `ElementReader` abstracts away raw frame access.
- `derive_changes` and `diff` use sequential `BlobReader` without pipelining.
  Since they load everything into memory anyway, pipelining would not help
  (the bottleneck is memory, not decode throughput).
- `check_refs` and `node_stats` use `for_each_pipelined` because they don't
  need block-level batching -- they process individual elements.

---

## 8. Additional Findings

### 8A. flush_local Double-Copy Pattern

The `flush_local` helper appears in 4 commands (extract, tags_filter, merge,
cat). The pattern:

```rust
fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) {
    if let Some(bytes) = bb.take()? {    // bytes: &[u8] (borrow of encode_buf)
        output.push(bytes.to_vec());      // COPY 1: &[u8] -> Vec<u8>
    }
}
```

Then in the caller:
```rust
for block_bytes in &output.blocks {
    writer.write_primitive_block(block_bytes)?;  // block_bytes: &Vec<u8>
}
```

And `write_primitive_block` (writer.rs line 330):
```rust
let uncompressed = block_bytes.to_vec();  // COPY 2: &[u8] -> Vec<u8>
```

So each serialized block is copied twice:
1. `take()` returns `&[u8]` (borrow of internal `encode_buf`) -> `to_vec()` into
   the output Vec
2. `write_primitive_block` copies the `&[u8]` into a `Vec<u8>` for the rayon
   spawn closure (needs ownership for `'static` lifetime)

At planet scale with ~43K blocks * ~200 KB average = ~8.6 GB of serialized
block data, this means ~17 GB of unnecessary copying.

The `flush_block` helper in `mod.rs` (lines 31-39) has the same issue but only
one copy -- it calls `write_primitive_block(bytes)` where `bytes: &[u8]` is
the borrow from `take()`, so it gets copy 2 but not copy 1.

**Fix options:**
1. Make `take()` return `Vec<u8>` instead of `&[u8]` (give up encode buffer
   reuse). This eliminates copy 1 but loses the allocation reuse benefit.
2. Add `write_primitive_block_owned(Vec<u8>)` to `PbfWriter` that takes
   ownership, eliminating copy 2. Combined with option 1, both copies gone.
3. Keep `take()` as `&[u8]` but add `take_owned() -> Vec<u8>` that swaps in a
   new buffer. Callers in parallel paths use `take_owned()`, sequential callers
   keep `take()` for buffer reuse.

Option 3 is best: preserves buffer reuse for sequential flush_block calls
while eliminating both copies for parallel paths.

### 8B. owned_elements.rs Full-Memory Load

`owned_elements::read_elements()` (line 56) uses `BlobReader::open()`
(sequential, no pipelining) and loads every element into owned Vecs with
String-cloned tags. For Denmark (465 MB PBF):
- ~52M nodes * ~80 bytes = ~4.2 GB
- ~7.4M ways * ~200 bytes = ~1.5 GB
- ~120K relations * ~300 bytes = ~36 MB
- Total: ~5.7 GB for ONE file

derive_changes loads TWO files: ~11.4 GB.
diff loads TWO files: ~11.4 GB.

For planet: ~8.5B nodes * 80 bytes = ~680 GB per file. Completely infeasible.

The code documents this limitation (derive_changes.rs lines 50-54):
> "This works for country-scale extracts but will OOM on planet-scale
> (~80 GB) files."

A streaming merge-join would fix this but is a significant refactor. Since
planet-scale derive_changes/diff is not a current use case (the merge command
applies diffs, it doesn't generate them), this is low priority.

### 8C. DenseMmapIndex

`add_locations_to_ways.rs` DenseMmapIndex (lines 94-159):
- 8 bytes per slot (lat i32 + lon i32)
- Default capacity: 16B entries = 128 GB virtual
- Demand-paged: only pages with written data consume physical memory
- Sentinel: (0, 0) means unset (~116 nodes at null island are false negatives)

For planet (~8.5B nodes, max ID ~12.5B):
- Physical RSS: ~68 GB (8.5B * 8 bytes, though sparse ID ranges mean less)
- Requires `vm.overcommit_memory=1` or sufficient memory

The Hash alternative uses `FxHashMap<i64, (i32, i32)>`:
- ~24 bytes per entry (8 key + 8 value + 8 hash/metadata overhead)
- ~8.5B entries * 24 bytes = ~204 GB (worse than Dense)

Dense is the correct choice for planet scale. The overcommit requirement
is documented (line 38-41) with a fallback suggestion.

### 8D. check_refs RoaringTreemap

`check_refs.rs` uses `RoaringTreemap` (not `RoaringBitmap`) because planet
node IDs exceed `u32::MAX` (lines 83-85). The code documents this clearly.

Memory for planet:
- ~10B node IDs: RoaringTreemap compresses dense sequential IDs to ~2 bits
  per entry, so ~2.5 GB
- Way + relation IDs: ~130 MB combined
- Total: ~2.7 GB

The command is consumer-bound (documented at lines 61-63): the main thread
runs at 100% CPU on RoaringTreemap insertions while decode workers idle.
`for_each_pipelined` delivers elements faster than the main thread can insert
them. This is fine -- the bottleneck is the Roaring insert, and there's no way
to parallelize it without per-thread sets + merge (which would add merge cost).

### 8E. getid BTreeSet

`getid.rs` uses `BTreeSet<i64>` for user-specified ID sets (line 25) and
dependency node IDs (line 210). Since user-specified ID sets are small
(typically tens to thousands of IDs), BTreeSet is fine.

The `dep_node_ids` (line 210) could grow large with `--add-referenced` on
many ways, but this is bounded by the number of requested ways (user
specified), not by the total file size. Not a planet-scale concern.

### 8F. tags_count fold+reduce Pattern

`tags_count.rs` uses `par_iter().fold().reduce()` with per-thread `FxHashMap`
(lines 49-50). Each thread accumulates a local count map, then maps are
merged pair-wise. This is the textbook parallel reduction pattern.

For planet-scale with all tags: ~50K unique key-value pairs * ~100 bytes =
~5 MB per thread. With 16 threads: ~80 MB. Plus merge cost: O(T * K) where
T is thread count and K is unique keys. This is not a bottleneck.

---

## 9. Cross-Box Interactions

### 9A. Box 1 (Wire-Format Parsing)

The command layer depends heavily on Box 1's zero-copy parsing:
- `elements()` and `elements_skip_metadata()` iterate without allocation
- `PackedIter` for refs and tags avoids materializing Vecs
- `WireStringTable` offset-based string lookup

The `pre_seed_string_table()` optimization in merge (line 662) ties into
Box 1's string table representation -- it copies raw offsets rather than
decoded strings.

### 9B. Box 5 (Write Path: BlockBuilder)

`BlockBuilder::take()` returns `&[u8]` -- a borrow of the internal encode
buffer. This forces the double-copy pattern documented in 8A. The write path
design (Box 5) constrains command layer efficiency.

`can_add_node()` / `can_add_way()` / `can_add_relation()` capacity checks
(called throughout commands before each add) are O(1) -- just a counter
comparison. No performance concern.

### 9C. Box 6 (Write Path: PbfWriter)

`PbfWriter::write_primitive_block()` performs the second copy (line 330 of
writer.rs). The pipelined writer's need for `'static` data in rayon spawns
forces the `to_vec()`. An `Arc<[u8]>` or ownership-transfer API could
eliminate this.

`write_raw()` and `write_raw_owned()` bypass this issue entirely -- raw
passthrough bytes are already owned. This is why merge's passthrough path
has no double-copy issue; only rewritten blocks suffer.

### 9D. Box 4 (Pipelined Read)

`into_blocks_pipelined()` delivers owned `PrimitiveBlock`s. The pipeline's
3-stage architecture (IO -> decompress -> reorder) is transparent to commands.
Commands that use `for_each_pipelined` get single-element delivery, which
is appropriate for consumers that don't need block-level batching.

Commands using `into_blocks_pipelined()` + manual batch collection are doing
two levels of buffering (pipeline reorder buffer + batch Vec). This is fine --
the pipeline buffer is bounded (8 blocks default), and the batch Vec is
bounded by BATCH_SIZE.

---

## 10. Recommended Actions

### P0 -- Planet-Scale Blockers

**P0-1: tags_filter way_dep_node_ids memory** (tags_filter.rs lines 608-611)
Replace `Vec<i64>` with `IdSetDense` for `way_dep_node_ids` in two-pass mode.
Reduces worst-case memory from ~40 GB to ~1.5 GB. The existing `IdSetDense`
from extract.rs can be reused directly (move to a shared module or
`mod.rs`). Also eliminates the sort+dedup cost between passes (IdSetDense
handles duplicates inherently). The matched_node_ids, matched_way_ids, and
matched_relation_ids can remain as sorted Vec<i64> since they scale with
match count (bounded by filter selectivity), not with total way ref count.

### P1 -- Measurable Improvements

**P1-1: flush_local double-copy elimination** (mod.rs, extract.rs, tags_filter.rs, merge.rs)
Add `take_owned() -> Vec<u8>` to BlockBuilder alongside existing `take()`.
Add `write_primitive_block_owned(Vec<u8>)` to PbfWriter. Use in all parallel
paths. Estimated saving: ~17 GB copying at planet scale, ~2-5% wall time for
write-heavy commands.

**P1-2: extract smart pass 2 BlobFilter** (extract.rs line 923)
Add `.with_blob_filter(BlobFilter::only_ways())` to the ElementReader in
smart pass 2. Eliminates decompression of ~80% of blobs (nodes + relations).
Estimated saving: ~60 seconds at planet scale for smart extract.

### P2 -- Worthwhile When Convenient

**P2-1: extract pass 1 parallel fold+reduce** (extract.rs lines 565-591)
Convert sequential IdSetDense accumulation to parallel fold with per-thread
IdSetDense + merge. The `merge()` method already exists (lines 73-91).
Would speed up pass 1 by ~2-4x on multi-core systems.

**P2-2: tags_filter pass 2 IdSetDense for lookups** (tags_filter.rs lines 463-464)
After fixing P0-1, use IdSetDense for pass 2 lookups as well (O(1) vs
O(log n) binary search). Reduces pass 2 node processing from ~20 minutes
to ~3 minutes at planet scale.

**P2-3: sort overlap run streaming** (sort.rs lines 391-438)
Instead of materializing all elements in an overlap run, use a priority-queue
merge of blob iterators. Reduces worst-case memory from O(elements in run)
to O(blobs in run). Low priority since sorted inputs have zero overlaps.

### P3 -- Low Priority / Future

**P3-1: derive_changes / diff streaming** (derive_changes.rs, diff.rs)
Convert from full-memory load to streaming merge-join. Only matters if
planet-scale derive_changes is needed. Currently documented as
country-scale-only.

**P3-2: owned_elements pipelined read** (owned_elements.rs line 56)
Switch from `BlobReader::open()` to `ElementReader::into_blocks_pipelined()`.
Would speed up derive_changes/diff by ~2-3x via parallel decompression. Low
priority since these commands are limited by memory, not decode speed.

**P3-3: check_refs parallel insertion** (check_refs.rs lines 124-176)
Use per-thread RoaringTreemap with post-pass merge via bitwise OR. Would
eliminate the consumer-bound bottleneck. Low priority since check-refs is
an infrequent diagnostic command.

**P3-4: sort overlap run documentation** (sort.rs line 6)
Add caveat to the O(num_blobs) claim: "O(num_blobs) when no overlaps exist;
degrades to O(num_elements) for overlap runs."
