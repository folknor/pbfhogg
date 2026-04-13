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
| Stage 4 (parallel P2c assembly) | 269s | 280s | — |
| **Total** | **1,462s** | **1,075s** | **−26%** |

Previous planet result: 1,462s (24.4 min), 16.7 GB peak anon (sidecar `98e71e2b`).

Temp disk: ~4.3 GB Denmark, ~112 GB Europe, ~300 GB planet.

### Planet-scale sizing (theoretical)

| Structure | Count | Entry size | Total |
|-----------|-------|------------|-------|
| COO pairs | 8B | 16 bytes | ~128 GB |
| Coord slots | 8B | 8 bytes | ~64 GB |
| Node buckets (temp) | 256 | ~500 MB each | ~128 GB |
| Slot buckets (temp) | 256 | ~375 MB each | ~96 GB |

Peak temp disk: ~224 GB (node + slot buckets). After cleanup: 64 GB (coord slots only).

## Implementation

`src/commands/external_join.rs`. Correctness verified identical to dense output
on Denmark (10,175,884 elements, 0 differences) and cross-validated against
osmium via `brokkr verify add-locations-to-ways`.
