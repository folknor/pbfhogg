# ALTW Optimization History

Research path from mmap thrashing to bounded-memory external join for
`add-locations-to-ways` on memory-constrained hosts (30 GB RAM, plantasjen).

## The problem

ALTW needs random access to node coordinates by ID. OSM node IDs reach ~13B,
so a direct-mapped array is 128 GB virtual. After pass 0 filtering (way-
referenced nodes only), ~2B nodes remain = ~16 GB touched pages. On a 30 GB
host, 16 GB mmap + 33 GB input page cache exceeds physical memory. The kernel
constantly evicts and re-faults pages: planet took 96 minutes, CPU mostly idle.

**Key structural insight**: ALTW reconstructs a sparse relationship from two
streams sorted on different keys (nodes by node ID, ways by way ID). The 16 GB
mmap is a hash table for this join. Random access is inherent to the structure,
not an implementation flaw.

## Approaches tried

### 1. Pass 0: referenced-nodes-only filter (commit `b3a98b0`)

Scans way blobs to build `IdSetDense` bitset (~1.6 GB for planet). Only inserts
way-referenced nodes into the dense index. Reduces touched pages from ~80 GB to
~16 GB. **Result**: no improvement at Europe scale (2631s vs 2565s baseline).
16 GB mmap + 33 GB input still exceeds 30 GB RAM. Useful on 64 GB hosts.

### 2. Sparse index: Planetiler-inspired chunk array (commit `52d6273`)

`--index-type sparse`. Chunk-indexed (size 256) sparse array. ~540 MB RAM for
chunk index, ~16 GB on-disk values file via mmap. Way lookups batched and sorted
by file offset via `FxHashMap`.

**Hypothesis**: sorted batched lookups would convert random mmap access into
sequential scans. **Disproven** at Europe scale: 2.5x slower than dense.

**Cross-scale results** (plantasjen):

| Scale | Dense | Sparse | Ratio | Insight |
|-------|-------|--------|-------|---------|
| Denmark (465 MB) | 6,826 ms | 14,105 ms | 2.07x | CPU overhead dominates (small batches) |
| Japan (2.4 GB) | 72,284 ms | 71,837 ms | **1.00x** | CPU overhead amortized; proves overhead is negligible |
| Europe (33.6 GB) | 2,565s | 6,453s | **2.5x** | Page-cache thrash on values mmap, plus sort overhead |

The Japan result was key: sparse = dense when data fits in cache proves the
sort+hash CPU cost is negligible. The Europe failure is purely I/O -- same
page-cache thrashing as dense, but with overhead on top.

### 3. Coalesced pread (commit `4fbf7a8`, reverted `034422c`)

Replaced mmap with explicit coalesced `read_exact_at` calls over 128 KB spans.

| Backend | Japan best of 3 | vs dense |
|---------|----------------|----------|
| mmap | 71,837 ms | 1.00x |
| pread | 79,370 ms | 1.10x |

**10% slower** on hot data. Syscall overhead per read outweighs I/O benefits
when pages are cached. Rejected without Europe testing.

### 4. Partitioned multi-pass (measured, rejected)

Split node ID range into N partitions; skip way blobs not touching the current
partition. **Hypothesis disproven** by measurement.

Measured with `examples/partition_stats.rs`:

| Dataset | Way blobs | N=2 single-part | N=64 median touched |
|---------|-----------|-----------------|---------------------|
| Denmark | 828 | 0 (0.0%) | 62-63 of 64 |
| Japan | 5,363 | 3 (0.1%) | 61-62 of 64 |

**Every way blob touches nearly every partition.** Root cause: sorted PBFs group
ways by way ID. A blob of ~8000 ways from different eras references nodes
spanning the entire chronological ID space. Node-ID partitioning is
fundamentally misaligned with PBF blob layout. This kills any approach depending
on selective way-blob skipping.

## Solution: external join via double radix permutation

**Commit**: `034422c`+ (`--index-type external`). Pre-compute the way-node join
using sequential I/O and bounded memory.

### Pipeline

1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into 256
   node buckets partitioned by high bits of node_id. Memory: ~64 MB.
2. **Node join**: for each bucket, sort by node_id, single-pass merge-join with
   node stream, emit `(slot_pos, lat, lon)` into 256 slot buckets. Memory:
   ~500 MB (one bucket). Nodes read exactly once across all buckets.
3. **Slot reorder**: for each slot bucket, sort by slot_pos, scatter-write to
   final coord_slots file. Memory: ~375 MB.
4. **Assembly**: stream PBF, read matching coords from coord_slots sequentially,
   emit enriched ways. Passthrough for nodes/relations.

Every stage: bounded memory (<1 GB), all sequential I/O, no mmap, no page faults.

### Optimization history

| Optimization | Commit | Impact |
|-------------|--------|--------|
| Single-pass node merge (stage 2) | `a334c72` | 302s -> 25s Denmark (12x) |
| fadvise(DONTNEED) + mmap coord_slots | `165cbb2` | 25s -> 22s Denmark |
| Node-only wire-format scanner (stage 2) | `cf350a9` | Eliminated 25+ GB heap retention |
| Scatter buffer (stage 3) | `cf350a9` | 15x speedup, eliminated 4.69B pwrite calls |
| Sequential readers (stages 1, 4) | `4daf995`, `2873919` | Eliminated 11 GB PrimitiveBlock retention |
| P2b-v2 pread-from-workers (stage 2) | `80e227b` | 301s -> 216s (-28%), anon 20.4 GB -> 1.4 GB |
| P2c parallel assembly (stage 4) | `6b09796` | 432s -> 136s (-68%) |

### Final results (plantasjen, 30 GB RAM)

| Index | Denmark (465 MB) | Japan (2.4 GB) | Europe (33.6 GB) | Planet (87.7 GB) |
|-------|-----------------|----------------|------------------|------------------|
| dense | 8,168 ms | 72s | 2,565s (43m) | 5,773s (96m) |
| sparse | 14,105 ms | 72s | 6,453s (107m) | not tested |
| external | **12.3s** | 143s | **577s (9.6m)** | **1,462s (24.4m)** |
| ext/dense ratio | 1.5x slower | 2.0x slower | **4.5x faster** | **3.9x faster** |

**Crossover**: between Japan (2.4 GB, dense 2x faster) and Europe (33.6 GB,
external 4.5x faster). Dense thrashes when mmap working set + input page cache
exceeds physical memory.

### Memory profile (Europe, sidecar `bc38a079`, commit `6b09796`)

| Stage | Duration | Anon peak | Notes |
|-------|----------|----------|-------|
| Stage 1 (way pass) | 128s | 70 MB | Sequential reader |
| Stage 2 (node join) | 221s | 1.4 GB | Pread-from-workers, bucket sort |
| Stage 3 (slot reorder) | 91s | -- | Scatter buffer |
| Stage 4 (assembly) | 136s | 7.3 GB | Pread-from-workers, parallel assembly |

### Planet-validated (commit `abcc736`)

**1,075s (17.9 min), 8.7 GB peak anon. Dense planet: 5,773s (96 min). 5.4x faster.**

| Stage | Baseline (planet) | Optimized (planet) | Improvement |
|-------|------|------|------|
| Stage 1 (two-pass: IdSetDense + rank-bucketed emission) | 333s | 136s | −59% |
| Stage 2 (pipelined counting-sort merge) | 612s | 469s | −23% |
| Stage 3 (parallel pwrite scatter) | 247s | 167s | −32% |
| Stage 4 (parallel P2c assembly + wire-format way reframe) | 269s | 280s | — |
| **Total** | **1,462s** | **1,075s** | **−26%** |

### April 2026 optimization sprint

Europe: **608s → 422s (−31%)**. Planet: **1,462s → 1,075s (−26%)**.
Peak anon: 16.7 GB → 8.7 GB (planet, −48%).

| Optimization | Commit | Impact (Europe) |
|-------------|--------|----------------|
| Parallel stage 1 (per-worker bucket shards, AtomicUsize dispatch) | `de75000` | 117s → 45s (−62%) |
| Rank-bucketed counting sort (O(n) replaces O(n log n) comparison sort) | `df09a62` | stage 2: 262s → 218s |
| Parallel stage 3 (pwrite to pre-sized coord_slots) | `74edbfd` | 108s → 64s (−41%) |
| Pipelined stage 2 bucket loader | `e1ba970` | stage 2: 218s → 181s |
| Fused rank_if_set + parse-free bucket prep | `06f2a30` | stage 2: 181s → 140s |
| Wire-format way reframe (stage 4) | `a705fde` | stage 4 assemble: −40% CPU |
| Shard consolidation (reverted — net loss) | — | +67s overhead |

Key architectural changes:
- **COO pair format**: `(node_id, slot_pos)` → `(rank, slot_pos)`. Dense rank
  space enables O(n) counting sort instead of O(n log n) comparison sort on
  sparse i64 node IDs.
- **Two-pass stage 1**: pass A builds shared atomic `IdSetDense` of referenced
  node IDs; pass B emits rank-bucketed records with `slot_pos` pre-computed
  from sidecar prefix sums. Workers are fully independent — no global
  sequential allocator.
- **Wire-format way reframe**: stage 4 way blobs skip full PrimitiveBlock
  decode + BlockBuilder re-encode. Only decodes field 8 (refs) to count refs
  and look up coords; appends fields 9+10 (lat/lon) directly. Saves ~40% of
  way assembly CPU. Node/relation blobs stay on full decode path.
- **IdSetDense::rank_if_set()**: fused get+rank in one lookup, eliminating
  double chunk traversal in the merge loop.

Stage 4 sub-phase profiling (Europe, commit `1313ead`):
- `s4_way_coord_lookup_ms`: 504s cumulative (51% of way reframe)
- `s4_way_parse_way_ms`: 6s (negligible)
- `s4_way_reassemble_ms`: 6s (negligible)
- `s4_way_parse_block_ms`: 0s (negligible)
- Volume: 4.69B refs across 453M way messages

The remaining stage 4 cost is the irreducible per-ref inner loop (~10.7 ns/ref):
varint decode + mmap coord fetch + zigzag encode × 2. Structural overhead
has been eliminated.

### Temp disk (rank-bucketed architecture)

| Structure | Count | Entry size | Total |
|-----------|-------|------------|-------|
| Rank records | 4.69B (Europe) | 16 bytes | ~75 GB |
| Coord slots | 4.69B | 8 bytes | ~37 GB |
| Slot buckets (temp) | 256 | varies | ~37 GB |

Peak temp disk: ~112 GB (Europe). After cleanup: 37 GB (coord slots only).

## Implementation

`src/commands/external_join.rs`. Correctness verified identical to dense output
on Denmark (10,175,884 elements, 0 differences) and cross-validated against
osmium via `brokkr verify add-locations-to-ways`.
