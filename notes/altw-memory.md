# ALTW Memory Optimization Research

Tracking the effort to make `add-locations-to-ways` work efficiently on
memory-constrained hosts (30 GB RAM + 8 GB swap) at Europe/planet scale.

## The problem

ALTW needs random access to node coordinates by ID. OSM node IDs go up to
~13B, so a direct-mapped array is 128 GB virtual. After pass 0 filtering
(only way-referenced nodes), ~2B nodes remain = ~16 GB touched pages.

On plantasjen (30 GB RAM, 8 GB swap), the 16 GB mmap working set + 33 GB
input file page cache exceeds physical memory. The kernel constantly evicts
and re-faults pages → 96 minutes for planet, CPU mostly idle on page faults.

No other pbfhogg pipeline has this problem. All other commands use bounded-
memory approaches (streaming/batching, 1-bit IdSetDense, RoaringTreemap,
streaming merge-join). ALTW uniquely needs 8 bytes/node random access.

Previous attempts with madvise/fadvise never showed measurable benefit in
this project — the access pattern is too random for kernel hints to help.

## Approaches tried

### Pass 0: referenced-nodes-only index

**Commit**: `b3a98b0` (implemented, merged).

Scans way blobs to build `IdSetDense` bitset (~1.6 GB for planet's ~2B
unique way node refs). `build_node_index_dense` then only inserts nodes
present in the bitset. Reduces touched mmap pages from ~80 GB to ~16 GB.

**Result**: No improvement at Europe scale on plantasjen — 2631s vs 2565s
baseline (+2.6%, noise). 16 GB mmap + 33 GB input still exceeds 30 GB RAM.
Should help on 64 GB hosts where 16 GB fits in physical memory.

### Sparse index: Planetiler-inspired chunk-indexed array

**Commit**: `52d6273` (implemented, merged). `--index-type sparse`.

`SparseArrayIndex` — chunk-indexed (chunk size 256) sparse array.
RAM: `offsets` Vec<u64> + `start_pad` Vec<u8> (~540 MB at planet).
On-disk: compact packed (lat, lon) values file via read-only mmap (~16 GB).
Way lookups are batched and sorted by file offset, converting random I/O
into more-sequential scans via `FxHashMap` pre-resolution.

**Hypothesis**: Batched sorted lookups would convert random mmap access into
sequential scans, eliminating page fault thrashing.

**Result**: Hypothesis **disproven** at Europe scale. Sparse is 2.5x slower
than dense, not faster. The on-disk values file is still 16 GB of mmap,
and while access is more sequential within each batch, the overall working
set still doesn't fit in 30 GB RAM. The sort+hash CPU overhead adds to the
mmap thrashing rather than offsetting it.

### Measured results

All measurements on plantasjen (30 GB RAM, 8 GB swap, NVMe SSD).

#### Denmark (465 MB indexed, 59M elements) — fits in RAM

| Index | Best of 3 | Ratio | Commit |
|-------|-----------|-------|--------|
| dense | 6,826 ms | baseline | `52d6273` |
| sparse | 14,105 ms | 2.07x | `52d6273` |

Overhead is pure CPU (sorting, hashing). No I/O pressure at this scale.
Denmark's small batch sizes (~7.4K blobs) amplify fixed per-batch costs.

#### Japan (2.4 GB indexed, 344M elements) — fits in RAM

| Index | Best of 3 | Ratio | Commit |
|-------|-----------|-------|--------|
| dense | 72,284 ms | baseline | `48a351a` |
| sparse | 71,837 ms | **1.00x** | `48a351a` |

**Key finding**: sparse and dense are identical at Japan scale. The 2x
Denmark overhead vanishes — larger batches (43K blobs) amortize the
per-batch sort + FxHashMap costs. This proves the CPU overhead is not
the bottleneck in the steady state.

#### Europe (33.6 GB indexed, 4.2B elements) — exceeds RAM

| Index | Best of 3 | Median | Ratio | Commit |
|-------|-----------|--------|-------|--------|
| dense | 2,565s (43m) | — | baseline | `69a127f` |
| dense + pass 0 | 2,631s (44m) | — | 1.03x (noise) | `3677069` |
| sparse | 6,453s (107m) | 6,935s (116m) | **2.5x slower** | `52d6273` |

Sparse is significantly worse than dense at Europe scale. The overhead
ratio grew from 2.1x (Denmark) to 2.5x (Europe), indicating the sorted
batch access pattern does NOT help when the mmap is under page pressure.

#### Planet (87.7 GB indexed, 11.6B elements) — far exceeds RAM

| Index | Time | Commit |
|-------|------|--------|
| dense | 5,773s (96m) | `69a127f` |
| sparse | **not tested** | — |

Planet sparse is expected to be even worse given the Europe trend.

## Analysis

### Revised hypothesis (post Japan benchmark)

The Japan result (sparse = dense at 2.4 GB) overturns the initial
conclusion that CPU overhead was the problem:

1. **CPU overhead is negligible in the steady state.** The sort + FxHashMap
   cost per batch is amortized well at Japan scale (43K blobs). Denmark's
   2x slowdown was dominated by fixed per-batch costs on small batches
   (7.4K blobs), not an inherent algorithmic issue.

2. **The Europe failure is purely I/O.** The 2.5x slowdown at Europe scale
   is consistent with page-cache thrash on the values file mmap — the same
   problem as dense, but with the sort+hash overhead on top.

3. **The storage access model is the bottleneck.** Mmap page faults are
   demand-driven and don't benefit from sorted access order. Each fault
   costs ~10μs regardless of whether the next access is nearby or far.
   Sequential wins require readahead, which only triggers when accesses
   hit the *next page*, not just a *nearby page*.

4. **Working set is the same size.** Both dense and sparse touch ~16 GB of
   mmap pages. Sparse packs data more densely, but the total is similar
   because pass 0 already filtered to way-referenced nodes only.

### Implication for pread

This made pread + run coalescing seem promising: if CPU overhead is
acceptable, replacing mmap with explicit coalesced I/O might avoid the
page fault thrashing at Europe scale.

### pread experiment result (commit `4fbf7a8`, reverted in `034422c`)

Implemented `SparseValueReader` abstraction with pread backend. Coalesced
sorted offsets within 128 KB spans into single `read_exact_at` calls with
a reusable scratch buffer. Tested on Japan:

| Backend | Japan best of 3 | vs dense |
|---------|----------------|----------|
| mmap | 71,837 ms | 1.00x |
| **pread** | **79,370 ms** | **1.10x** |
| dense | 71,804 ms | baseline |

Pread is **10% slower** than mmap on hot data. Per-call syscall overhead
outweighs any I/O model benefit when pages are in cache. Median was even
worse (85s vs 73s), suggesting higher variance from syscall scheduling.

**Decision**: pivot to partitioned multi-pass. The evidence is sufficient:
- Sparse+mmap ties dense when values fit in cache (Japan).
- Sparse+pread regresses on that same hot-cache regime.
- The Europe failure mode (2.5x slower) is severe enough that a marginal
  I/O tweak is unlikely to turn it into a good design.
- The only benchmark that could settle the pread question (Europe under
  memory pressure, ~2 hours per run) is not worth the cost for a path
  that already regressed on the in-cache case.

## External review findings

Review of sparse index implementation (post Europe benchmark).

### Finding 1 (high): duplicate/descending node ID corruption

`build_node_index_sparse` assumes strictly increasing, unique node IDs but
does not enforce it. A duplicate ID in the same chunk wraps `gap` to 255;
a descending ID in an earlier chunk overwrites `offsets[chunk_id]` with a
later base. Dense tolerates duplicates (last-write-wins at same slot);
sparse silently corrupts the file.

In practice, sorted indexed PBFs always have strictly increasing node IDs.
But `--force` on unsorted input would trigger this. **Fix**: add a
monotonicity check in `build_node_index_sparse` that errors on non-
increasing IDs. Cheap guard, makes the invariant explicit.

**Status**: fixed in `48a351a`. Returns error on non-increasing IDs with
message directing user to `--index-type dense` for unsorted input.

### Finding 2 (medium): sentinel padding bloat

The on-disk format pads every present chunk to slot 255 with sentinels
(no per-chunk length stored). Each present chunk stores a suffix, not
just live entries. A 256-bit occupancy bitmap + packed values would
eliminate the bloat.

**Status**: deferred — if mmap is replaced (finding 3), the format changes
anyway. Not worth optimizing a format we might discard.

### Finding 3 (medium): mmap is the wrong I/O model

`resolve_batch_locations` does sorted page faults + hash reconstruction,
not a true sequential scan. Mmap faults on demand regardless of access
order; sorting helps TLB locality but each fault still costs ~10μs. The
CPU overhead from sorting + FxHashMap build compounds under page pressure.

Reviewer recommends: switch to coalesced `pread`/`preadv` over page-aligned
runs with `posix_fadvise`, or move to partitioned multi-pass.

**Status**: tested. Coalesced pread regressed 10% on hot data (Japan).
Pivot to partitioned multi-pass. See "pread experiment result" above.

### Dispatch shape

Reviewer found no structural problem with `NodeIndex` / `LocationLookup`
enum dispatch. The issue is the underlying I/O model, not the dispatch.

## Remaining approaches

### ~~1. Partitioned multi-pass~~ (measured, rejected)

Split the node ID range into N partitions, skip way blobs that don't touch
the current partition. Each partition's dense index fits in RAM.

**Hypothesis**: most way blobs only touch a few partitions, so per-partition
passes can skip the majority of way blobs.

**Result**: hypothesis **disproven**. Measured with `examples/partition_stats.rs`
on Denmark (828 blobs) and Japan (5363 blobs). At every partition count
(N=2,4,8,16,32,64), every way blob touches nearly every partition. Denmark:
0% single-partition. Japan: 0.1%. At N=64 the median blob touches 62 of 64
partitions.

**Root cause**: node-ID partitioning is fundamentally misaligned with PBF
blob layout. Sorted PBFs group ways by way ID. A blob of ~8000 ways from
different eras inevitably references nodes spanning the entire chronological
ID space. There is no correlation between a blob's position and its node refs.

This kills not just the metadata approach but **any** approach that depends
on selectively skipping way blobs by node-ID partition. Without skip benefit,
partitioned multi-pass degenerates to N full reads of all way blobs — pure
overhead with no upside.

See [altw-partitioned.md](altw-partitioned.md) for full measurement data.

### ~~2. Coalesced pread~~ (tested, rejected)

Replaced mmap with coalesced pread in `SparseValueReader`. Regressed 10%
on Japan (hot cache). Syscall overhead per read outweighs I/O benefits.
Not tested on Europe — hot-cache regression is sufficient signal to reject.
Implemented in `4fbf7a8`, reverted in `034422c`.

### 3. External join via double radix permutation — IMPLEMENTED

**Commit**: `034422c`+ (implemented, merged). `--index-type external`.

Pre-compute the way-node join using sequential I/O and bounded memory.
Emit COO pairs `(node_id, slot_pos)` from ways, radix-bucket by node_id,
merge-join with nodes, re-bucket by slot_pos, assemble sequentially.

- Memory: <1 GB (one bucket in RAM at a time, ~500 MB max)
- Temp disk: ~224 GB peak (bucket files + coord slots), all sequential I/O
- No mmap, no random access, no page fault thrashing
- Both permutations use integer-keyed radix bucketing (no comparison sort)
- Implementation: `src/commands/external_join.rs` (~580 lines)

**Correctness**: verified identical to dense output on Denmark — 10,175,884
elements, 0 differences (commit `034422c`, plantasjen). Cross-validated
against osmium via `brokkr verify add-locations-to-ways`.

**Denmark result** (465 MB, 60.8M way-node refs, plantasjen):

| Index | Time | Ratio | Commit |
|-------|------|-------|--------|
| dense | 8,168 ms | baseline | `034422c` |
| **external** | **302,069 ms (5m)** | **37x slower** | `034422c` |

The 37x slowdown at Denmark scale is expected — everything fits in RAM,
so dense is optimal. External pays full I/O cost: 256 full PBF reads of
all node blobs in stage 2 (one per bucket), plus temp disk I/O for bucket
files (~1.9 GB node buckets, ~1.9 GB slot buckets, ~487 MB coord_slots).

**Stage 2 is the bottleneck.** Currently re-reads ALL node blobs 256 times
(once per bucket). At Denmark scale this is ~256 × 370 MB = ~92 GB of
redundant PBF decoding. The fix is writing node coords to a compact sorted
sidecar during stage 1, then reading byte ranges in stage 2.

See [altw-partitioned.md](altw-partitioned.md) for full design.

### 4. Larger swap / 64 GB host (infrastructure)

- Dense + pass 0 on a 64 GB host: 16 GB mmap fits in RAM, should be fast
- Larger NVMe swap on plantasjen: 64-128 GB swap would let the kernel
  manage the 16 GB mmap without thrashing, at the cost of SSD wear
- Not a code solution, but may be the pragmatic answer for production

## Next steps

- [x] Fix finding 1: monotonicity check in `build_node_index_sparse` (`48a351a`)
- [x] Benchmark sparse on Japan — proved CPU overhead is negligible at scale
- [x] Implement + benchmark coalesced pread — regressed 10%, rejected
- [x] ~~Design and implement partitioned multi-pass~~ — disproven by measurement
- [x] Implement external join pipeline (`src/commands/external_join.rs`)
- [x] Verify external join correctness on Denmark — identical to dense
- [ ] **Optimize stage 2**: node-coord sidecar to eliminate 256× PBF re-reads
- [ ] Benchmark external join on Japan (medium scale, fits in RAM)
- [ ] Benchmark external join on Europe (the key test — currently 43m with dense)
- [ ] Add O_DIRECT support to bucket file I/O
- [ ] Test dense on 64 GB host — may solve the problem without code changes
