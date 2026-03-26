# ALTW Bounded-Memory Design

Research document for making `add-locations-to-ways` work with bounded memory
at planet scale (8B way-node refs, 30 GB RAM host).

## Background

ALTW embeds node coordinates into ways. The core operation is a **join**: ways
reference nodes by ID, and ALTW must look up each node's `(lat, lon)`. The
pain is the join, not the decoding.

The current implementation builds a dense mmap index: `index[node_id] = (lat, lon)`.
At planet scale, ~2B way-referenced nodes × 8 bytes = ~16 GB of touched pages.
On a 30 GB host this thrashes: 96 minutes for planet, CPU mostly idle on page faults.

All attempts to fix this within ALTW have failed. See [altw-memory.md](altw-memory.md).

## Key structural insight

ALTW is reconstructing a sparse relationship at runtime from two streams that
are each individually sorted, but **sorted on different keys**:

- Nodes: sorted by node ID.
- Ways: sorted by way ID. Their refs point to node IDs with no locality.

The 16 GB mmap is a hash table for this join — indexed by one key (node ID),
probed in the order of the other key (way-ref order). Random access is
**inherent** to this structure, not an implementation flaw.

In matrix terms: ALTW is doing an out-of-core sparse matrix value fill where
the structural indices and the values live in different orderings.

## Dead end: blob-level node-ID partitioning

Measured with `examples/partition_stats.rs` on Denmark and Japan. **Every way
blob touches nearly every partition** at every partition count (N=2 through 64).

Root cause: sorted PBFs group ways by way ID. A blob of ~8000 ways from
different eras references nodes spanning the entire chronological ID space.
Node-ID partitioning is fundamentally misaligned with PBF blob layout.

This kills any approach that depends on selectively skipping way blobs by
node-ID partition: blob-header metadata, multi-pass with skip, scheduling
hints. See measurement data at the end of this document.

## The design: external join via double radix permutation

Instead of building a giant random-access index, **pre-compute the join**
using sequential I/O and bounded memory.

The paradigms:
- CSR (compressed sparse row) / adjacency array layout
- External radix sort / bucket partition for integer keys
- COO → CSR conversion (coordinate list to compressed sparse row)
- Sort-merge join over external memory
- Scatter/gather as two sequential permutations

### Data structures

**Way-ref stream**: all way-node references in PBF order, as a flat array.
Each way's refs occupy a contiguous slice. A ref's position in this stream
is its `slot_pos`.

**COO pairs**: `(node_id: i64, slot_pos: u64)` — 16 bytes each.
"Node X's coordinates go into slot Y."

**Coord slots**: `(lat: i32, lon: i32)` — 8 bytes each, one per way-node ref.
Initially empty. After the join, `coord_slots[slot_pos]` holds the resolved
coordinates for that ref.

### Planet-scale sizing

| Structure | Count | Entry size | Total |
|-----------|-------|------------|-------|
| COO pairs | 8B | 16 bytes | ~128 GB |
| Coord slots | 8B | 8 bytes | ~64 GB |
| Node buckets (temp) | 256 | ~500 MB each | ~128 GB |
| Slot buckets (temp) | 256 | ~375 MB each | ~96 GB |

Peak temp disk: ~224 GB (node buckets + slot buckets, before cleanup).
After cleanup: 64 GB (final coord slots file only).

### Pipeline

#### Step 1: Way pass — build COO pairs

Stream all way blobs from the PBF. For each way, emit `(node_id, slot_pos)`
pairs into **node buckets** partitioned by high bits of `node_id`.

- Input: way blobs from PBF (~20 GB compressed).
- Output: 256 bucket files, sequential append. Each ~500 MB at planet.
- Memory: write buffers for 256 files (~64 MB total at 256 KB each).
- Also records `way_offsets` if needed for final assembly.

#### Step 2: Node join — merge and re-bucket

For each node bucket b (b = 0..255):
1. Load bucket b into RAM (~500 MB). Sort by `node_id`.
2. Stream node blobs whose IDs fall in bucket b's range.
3. Merge-join: for each node, find all COO pairs with that `node_id`,
   resolve `(lat, lon)`, emit `(slot_pos, lat, lon)` into **slot buckets**
   partitioned by high bits of `slot_pos`.

- Input: 256 node bucket files (read sequentially, one at a time) +
  node blobs from PBF (each node blob read at most once across all buckets).
- Output: 256 slot bucket files, sequential append. Each ~375 MB at planet.
- Memory: one node bucket in RAM (~500 MB) + streaming node decode.

Node buckets can be deleted after this step.

**Implementation note**: the current implementation reads ALL node blobs
per bucket (filtering by ID range), because PBF blob boundaries don't
align with node-ID bucket ranges. This makes stage 2 the bottleneck at
256× the cost. The planned fix is a node-coord sidecar file — see
"Bottleneck analysis" below.

#### Step 3: Slot reorder — build final coord slots

For each slot bucket b (b = 0..255):
1. Load bucket b into RAM (~375 MB). Sort by `slot_pos`.
2. Write entries sequentially to the final `coord_slots` file at the
   correct global positions.

Since slot buckets are processed in order of `slot_pos` high bits, the
writes to `coord_slots` are globally sequential — no random I/O.

- Input: 256 slot bucket files (read sequentially, one at a time).
- Output: one `coord_slots` file, 64 GB, written sequentially.
- Memory: one slot bucket in RAM (~375 MB).

Slot buckets can be deleted after this step.

#### Step 4: Assembly — emit enriched PBF

Stream the original PBF. For nodes and relations: passthrough (same as
current ALTW). For ways: read refs from the PBF, read matching coords
from `coord_slots` (sequential, since both are in way-ref order), emit
enriched ways with `add_way_with_locations`.

- Input: original PBF + `coord_slots` file.
- Output: enriched PBF.
- Memory: standard batch processing buffers.

`coord_slots` can be deleted after this step.

### Why this works

Every stage operates on bounded memory (<1 GB) with sequential I/O:

| Stage | Memory | I/O pattern |
|-------|--------|-------------|
| Way pass | ~64 MB (write buffers) | Sequential read PBF, sequential append buckets |
| Node join | ~500 MB (one bucket) | Sequential read bucket + node blobs, sequential append |
| Slot reorder | ~375 MB (one bucket) | Sequential read bucket, sequential write coord file |
| Assembly | Standard batching | Sequential read PBF + coord file, sequential write |

No mmap. No random access. No page fault thrashing. Both permutations
(node-id order → slot-pos order) use integer-keyed radix bucketing — no
comparison sort needed beyond in-memory sort of ~500 MB chunks.

### Time estimate (planet, NVMe)

| Stage | Bytes moved | Estimated time |
|-------|-------------|----------------|
| Way pass (decode + emit COO) | ~20 GB read + ~128 GB write | ~70s |
| Node join (per bucket) | ~128 GB read + ~80 GB nodes + ~96 GB write | ~150s |
| Slot reorder | ~96 GB read + ~64 GB write | ~80s |
| Assembly (decode ways + write) | ~87 GB read + ~64 GB read + ~88 GB write | ~120s |
| **Total** | | **~7 minutes** |

Compare: current ALTW on planet = 96 minutes (mmap thrashing).
Even with conservative 2× overhead on the I/O estimates, this is far faster.

### Where this runs

The pipeline is designed to run as **ALTW itself**, not as part of `cat`.
Cat remains a simple passthrough indexer. ALTW gains a new `--index-type external`
(or similar) that uses the bucket-join pipeline instead of dense/sparse mmap.

The original PBF is read multiple times (once for way pass, once for node
join, once for assembly). This is fine — sequential re-reads on NVMe are
cheap, and the alternative (buffering the entire file) is worse.

### Comparison with previous approaches

| Approach | Memory | Temp disk | I/O pattern | Denmark | Japan | Planet (est.) |
|----------|--------|-----------|-------------|---------|-------|---------------|
| Dense mmap | 16 GB touched | 128 GB mmap file | Random | 8.2s | 72s | 96m (measured) |
| Sparse mmap | 540 MB + 16 GB | 16 GB mmap file | Sorted random | 14.1s | 72s | ~150m (extrapolated) |
| External (old, 256× re-read) | <1 GB | ~4.3 GB | Sequential | 302s | — | unusable |
| **External (single-pass merge)** | **<1 GB** | **~4.3 GB** | **Sequential** | **25s** | **143s** | **~45-60m (est.)** |
| 64 GB host + dense | 16 GB touched | 128 GB mmap file | Random (fits) | 8.2s | 72s | ~20m (estimated) |

The external join trades wall time for bounded memory (<1 GB) and sequential I/O.
At planet scale on a 30 GB host, external should be faster than dense (which
thrashes at 96 min). The single-pass node merge (commit `a334c72`) eliminated
the 256× PBF re-read bottleneck without needing a sidecar file.

## Partition selectivity measurement (disproven hypothesis)

Measured with `examples/partition_stats.rs`. The original hypothesis was that
blob-level partition metadata could enable selective way-blob skipping.

### Denmark (828 way blobs, 60.8M refs)

| N | Single-partition blobs | Median partitions touched |
|---|------------------------|--------------------------|
| 2 | 0 (0.0%) | 2 of 2 |
| 8 | 0 (0.0%) | 8 of 8 |
| 16 | 0 (0.0%) | 16 of 16 |
| 64 | 0 (0.0%) | 62-63 of 64 |

### Japan (5363 way blobs, 353.9M refs)

| N | Single-partition blobs | Median partitions touched |
|---|------------------------|--------------------------|
| 2 | 3 (0.1%) | 2 of 2 |
| 8 | 3 (0.1%) | 8 of 8 |
| 16 | 3 (0.1%) | 16 of 16 |
| 64 | 3 (0.1%) | 61-62 of 64 |

Every way blob touches nearly every partition. Node-ID partitioning is
fundamentally misaligned with PBF blob layout. This killed the partition
metadata approach but motivated the external join design above.

## Implementation status

Implemented in `src/commands/external_join.rs` (~580 lines). Available as
`--index-type external` in `add-locations-to-ways`.

### Denmark results (465 MB, 60.8M refs, plantasjen)

| Index | Time | Ratio | Commit |
|-------|------|-------|--------|
| dense | 8,168 ms | baseline | `034422c` |
| external (old, 256× re-read) | 302,069 ms (5m2s) | 37x | `034422c` |
| **external (single-pass merge)** | **24,799 ms (25s)** | **3.5x** | `a334c72` |

### Japan results (2.4 GB, 344M elements, plantasjen)

| Index | Time | Ratio | Commit |
|-------|------|-------|--------|
| dense | 72,284 ms | baseline | `48a351a` |
| **external (single-pass merge)** | **143,275 ms (2.4m)** | **2.0x** | `a334c72` |

**Correctness**: identical to dense — verified via `brokkr verify add-locations-to-ways`.

**Temp disk**: ~1.9 GB node buckets + ~1.9 GB slot buckets + ~487 MB
coord_slots = ~4.3 GB peak (Denmark). Cleaned up automatically on completion.

### Bottleneck history

**Original stage 2 (commit `034422c`)**: re-read ALL node blobs 256 times
(once per bucket). Denmark: 256 × ~370 MB PBF node data = ~92 GB of
redundant decoding. 280s of the 302s total.

**Fix (commit `a334c72`)**: single-pass node merge. Since PBF nodes are
sorted by ID and buckets partition the ID space into ascending ranges,
a single pass through the node stream processes all 256 buckets. Each
node is read exactly once. No sidecar file needed.

The originally planned fix was a node-coord sidecar file (16 bytes/node,
sorted, with range reads). The single-pass merge is simpler and avoids
the extra temp disk (~32 GB for referenced-only sidecar at planet).

## Next steps

- [x] Measure partition selectivity on Denmark and Japan — disproven
- [x] Design external join pipeline (this document)
- [x] Implement full pipeline (`src/commands/external_join.rs`)
- [x] Verify correctness on Denmark — identical to dense
- [x] Benchmark on Denmark — 302s (37x slower, stage 2 bottleneck identified)
- [x] **Optimize stage 2**: single-pass node merge (commit `a334c72`, 12x speedup)
- [x] Benchmark optimized external on Denmark — 25s (3.5x dense)
- [x] Benchmark on Japan — 143s (2.0x dense)
- [ ] Benchmark on Europe (the key test — currently 43m with dense)
- [ ] Add O_DIRECT support to bucket file I/O (planet-scale page cache bypass)
- [ ] Test dense on 64 GB host — may solve the problem without code changes
