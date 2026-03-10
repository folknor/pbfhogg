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

## Analysis: why sparse didn't help

The core assumption was wrong: "sorting lookups by file offset converts
random I/O into sequential I/O." In practice:

1. **Working set is the same size.** Both dense and sparse touch ~16 GB of
   mmap pages. Sparse packs the data more densely, but the total is similar
   because pass 0 already filtered to way-referenced nodes only.

2. **Batch locality is limited.** Each batch contains ~64 blocks × ~8000
   ways × ~4 node refs = ~2M lookups. After sorting by file offset, the
   lookups span a wide range of the 16 GB file — not a contiguous scan.

3. **CPU overhead compounds.** The sort + FxHashMap build per batch adds
   ~100% CPU overhead even when data fits in RAM. Under memory pressure,
   this overhead runs *on top of* the same page fault latency.

4. **Mmap doesn't become sequential.** The kernel's page fault handler
   doesn't benefit from access being "more sequential" — each fault still
   costs ~10μs regardless of access pattern. Sequential wins come from
   readahead, but readahead only helps when the next access is to the
   *next page*, not just a *nearby page*.

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

**Status**: not yet fixed.

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

**Status**: aligns with Europe benchmark results. See "Remaining approaches".

### Dispatch shape

Reviewer found no structural problem with `NodeIndex` / `LocationLookup`
enum dispatch. The issue is the underlying I/O model, not the dispatch.

## Remaining approaches

### 1. Partitioned multi-pass (recommended)

Split the node ID range into N partitions (e.g. by chunk of the values
file). On each pass through the way blobs, only resolve node refs that
fall in the current partition. Each partition's mmap window fits in RAM.

- Memory: single partition + I/O buffers
- I/O: N full reads of way blobs (N = ceil(16 GB / available_ram))
- On plantasjen: N ≈ 2 passes (16 GB / ~10 GB available after OS + input cache)
- Trades extra I/O for guaranteed in-RAM access
- Similar to osmium's `flex_mem` strategy
- Bounded memory by construction — the reliable win

### 2. Explicit pread with readahead (sparse index improvement)

Replace the mmap in `SparseArrayIndex` with explicit `pread()` calls and
coalesced page-aligned reads, with `posix_fadvise(FADV_WILLNEED)` on
upcoming batch ranges.

- Might recover a constant factor if batch sorting has real locality
- Least invasive change to existing sparse code
- Worth testing before the more complex multi-pass approach
- Reviewer and benchmarks both suggest this is a long shot

### 3. On-disk sorted store + merge-join

Sort all (way-referenced) node coordinates by ID into a temporary file.
Sort all way node refs by referenced node ID. Merge-join the two sorted
streams. Memory = just I/O buffers.

- Memory: O(buffer_size), truly constant
- I/O: 2× full write + 2× full read of ~16 GB temp files + external sort
- Slowest approach but works on any host regardless of dataset size
- The external sort could use `--direct-io` effectively (sequential I/O)

### 4. Larger swap / 64 GB host (infrastructure)

- Dense + pass 0 on a 64 GB host: 16 GB mmap fits in RAM, should be fast
- Larger NVMe swap on plantasjen: 64-128 GB swap would let the kernel
  manage the 16 GB mmap without thrashing, at the cost of SSD wear
- Not a code solution, but may be the pragmatic answer for production

## Next steps

- [ ] Fix finding 1: monotonicity check in `build_node_index_sparse`
- [ ] Hotpath profile sparse on Japan (2.4 GB) to identify CPU overhead breakdown
- [ ] Test approach 2 (pread + fadvise) — smallest code change, low confidence
- [ ] Test dense on 64 GB host — may solve the problem without code changes
- [ ] If neither works, implement approach 1 (partitioned multi-pass)
- [ ] Planet sparse test is low priority given Europe results
