# Planet-Scale Geocode Index Builder

Spec for evolving the geocode index builder from an in-memory regional pipeline
to a streaming pipeline that fits in 30 GB RAM for planet (~87 GB PBF).

Parent: [reverse-geocoding-spec.md](reverse-geocoding-spec.md).

## Problem

The current builder holds all intermediate data in RAM. Estimated planet memory:

| Component | Planet estimate |
|---|---|
| `street_ways[].nodes` (coordinate Vecs) | ~5.8 GB |
| `addr_points[]` | ~3.0 GB |
| `interp_ways[].nodes` | ~0.6 GB |
| `way_geom` HashMap (admin) | ~0.4 GB |
| StringPool | ~0.25 GB |
| Cell entry Vecs (fine+coarse, all types) | ~19 GB |
| Dense node index (page cache) | ~16 GB |
| **Total** | **~45 GB heap + ~16 GB page cache** |

OOM on the 30 GB benchmark host. The two dominant costs are geometry
retention (~10 GB) and cell entry accumulation (~19 GB).

## Target

Planet build on a 30 GB host. Target: ~2.5 GB peak heap + ~12 GB page cache.

## Architecture

### Mode split

Two modes, selected automatically by input size or via `--planet` flag:

**Regional mode** (inputs < 10 GB): current 2-scan builder, all in memory.
Fast, simple, no temp files.

**Planet mode** (inputs >= 10 GB, or `--planet`): 3-scan + streaming output +
bucketed cell assignment. Uses temp files for cell entry buckets.

Both modes produce identical output files. The mode split is a build-time
decision, not a format change.

### Planet mode pass structure

**Pass 1: Relations** (unchanged)

Scan relation blobs with `for_each_block_pipelined` + `elements_skip_metadata()`.
Collect `admin_relations` and `needed_admin_ways: IdSetDense`. Memory: negligible.

**Pass 1.5: Referenced node collection** (new, planet mode only)

Scan way blobs with `for_each_block_pipelined` + `elements_skip_metadata()`.
For each way that passes tag-first classification (street, building+address,
interpolation, or admin member), collect all node ref IDs into
`referenced_nodes: IdSetDense`.

This is the same pattern as `add_locations_to_ways` pass 0
(`collect_way_referenced_node_ids`). The IdSetDense is ~1.6 GB for planet's
~2B unique way-node refs.

The dense node index in pass 2 then only writes entries for referenced nodes,
reducing page cache from ~16 GB (all 10.4B nodes) to ~8-10 GB (only the ~2B
referenced by relevant ways). This decision must be made before pass 2 starts —
it cannot be deferred to a "check RSS and add if needed" step, because the
index is populated during pass 2 and there is no way to un-populate irrelevant
entries after the fact.

**Pass 2: Nodes + Ways (streaming output)**

Single pipelined scan over node + way blobs (sorted PBF: nodes before ways).
Instead of accumulating geometry Vecs in memory, **write output data files
directly during the scan**:

**Nodes:**
- Populate the dense node index (referenced nodes only in planet mode).
- For address nodes (`addr:housenumber` + `addr:street`): write the `AddrPoint`
  record (20 bytes) directly to `addr_points.bin` via BufWriter. Intern strings
  into StringPool as today.

**Ways (streets):**
- Tag-first classification (unchanged).
- Write `(i32, i32)` coordinate pairs directly to `street_nodes.bin` via BufWriter.
- Write the `StreetWay` header (14 bytes) to `street_ways.bin` via BufWriter.
- No `RawStreetWay` or `SlimStreetWay` Vec in memory.

**Ways (interpolation):**
- Write coordinate pairs to `interp_nodes.bin` directly.
- Keep slim interpolation metadata in memory (street_offset, interpolation_type,
  node_file_offset, node_count, start_number=0, end_number=0). ~23 bytes per way
  × ~10M = ~230 MB for planet. Needed because interpolation endpoint resolution
  runs after the scan and mutates start_number/end_number.
- After resolution, write `interp_ways.bin`. Then drop the Vec and mmap the file.

**Why interp_ways.bin is deferred:** The `start_number` and `end_number` fields
are resolved by matching interpolation endpoints against nearby address points.
This spatial join runs after the scan (it needs the complete `addr_points.bin`).
Writing the file during the scan would leave those fields as zeros with no way
to patch them.

**Ways (buildings with address):**
- Compute centroid, write `AddrPoint` to `addr_points.bin` directly.

**Ways (admin members):**
- Collect into `way_geom` FxHashMap as today (bounded by admin way count).

**After the scan:**
- Flush all BufWriters.
- Mmap `street_ways.bin`, `street_nodes.bin`, `addr_points.bin` read-only.
- Write `strings.bin`.
- Drop `referenced_nodes` IdSetDense (no longer needed).

### Interpolation endpoint resolution

Uses `addr_points.bin` (mmap'd) for spatial lookup. Builds a transient
cell-to-offset index: for each address point, compute its S2 cell and store
`(cell_id, byte_offset_in_addr_points_bin)` in an `FxHashMap<u64, Vec<u32>>`.

**Transient memory estimate:** ~150M address points across ~10M distinct S2 cells.
150M u32 indices = 600 MB. ~10M Vec objects (24 bytes overhead) = 240 MB. FxHashMap
overhead for 10M entries = ~200 MB. **Total: ~1 GB transient.** Created during
resolution, dropped immediately after. Heap peaks at ~2.5 GB during this phase.

After resolution, write `interp_ways.bin` with resolved start/end numbers, then
mmap it.

**Note on spatial index structure:** The `FxHashMap<u64, Vec<u32>>` is object-heavy
(~10M individually allocated Vecs). This is acceptable for v1 — the ~1 GB transient
cost fits in budget and the structure is short-lived. A future optimization could
use a flatter representation (sorted `Vec<(u64, u32)>` with binary search, or a
compact CSR-style array) to reduce allocator overhead and pointer chasing.

### Ring assembly + admin data

Assemble admin polygons from `way_geom` + `admin_relations`. Simplify with
Douglas-Peucker. Write `admin_polygons.bin` and `admin_vertices.bin`. Drop
`way_geom` and `admin_relations`.

`way_geom` is estimated at ~400 MB for planet (~2M admin-member ways, ~20 nodes
average). This needs measurement. Planet mode v1 assumes it fits. If measurement
shows it exceeds 1 GB, that is a separate follow-up — the fallback (spill to temp
file keyed by relation ID, process relation-by-relation) is well-understood but
not specced here to avoid a placeholder escape hatch.

### Pass 3: Cell assignment (bucketed)

Instead of accumulating all cell entries into global Vecs and sorting, partition
into **256 buckets** by the top 8 bits of the S2 cell ID.

**Bucket key:** `bucket = (cell_id >> 56) as u8`.

**Ordering proof:** S2 CellID is a u64 encoding face (top 3 bits, values 0-5) +
Hilbert curve position (remaining bits) + a trailing sentinel bit. Numeric sort
of CellID equals the canonical S2 spatial ordering. The bucket key extracts the
top 8 bits of this u64. Since the bucket key is a prefix of the numeric sort key,
`bucket_i < bucket_j` implies every cell ID in bucket i is numerically less than
every cell ID in bucket j. Processing buckets in ascending order therefore
produces globally sorted output without a merge step.

**Note on empty buckets:** S2 has 6 faces (0-5), so face values 6-7 (binary 110,
111) are unused. Bucket keys 0xC0–0xFF (top 2 bits = 11) will always be empty —
64 of 256 buckets. The remaining ~192 active buckets still give good partitioning.
Empty buckets are expected and should not be treated as a bug.

**Validation requirement:** The monotonic-prefix ordering argument eliminates an
entire merge phase. Since correctness depends on this property, the implementation
must include a debug assertion in tests: after writing all bucket outputs, verify
that the concatenated cell IDs in geo_cells.bin are in strictly ascending order.
Run this on a sampled dataset (Denmark) to confirm bucket order + intra-bucket
sort equals global numeric sort.

**Bucket storage and write path:**

Each bucket is backed by a **temp file**. The cell assignment phase runs in two
stages:

Stage A (parallel computation, single-threaded emission): Rayon `par_iter` over
ways/addresses computes cell entries per way. Each rayon task returns a small
local Vec of entries (typically 2-10 per way). The caller thread (which owns the
`par_iter().collect()`) receives the collected results and distributes entries
to the appropriate bucket files sequentially. No Mutex needed — bucket file
writes happen on a single thread after parallel computation completes.

Concretely: `par_iter().flat_map(|way| compute_entries(way)).collect::<Vec<_>>()`
produces all entries, then a single-threaded loop appends each entry to the
correct bucket's BufWriter. This is simpler than Mutex'd concurrent writes and
avoids thread-local buffer memory (~400-800 MB on many-core machines).

For memory: the collected Vec holds all entries for one batch of ways before
distribution. With rayon's work-stealing, entries are produced and collected
incrementally. If total entry count becomes a concern, process ways in chunks
(e.g., 100K ways at a time) and distribute after each chunk.

Stage B (sequential bucket processing): After all cell entries are produced,
process each non-empty bucket (0..255) in order:
1. Read the bucket file into memory (~100 MB average, ~250 MB worst case)
2. Sort entries by cell_id within the bucket
3. For each unique cell_id, write the geo_cells record and the corresponding
   street/addr/interp entry records to the output files (appending — since
   buckets are processed in order, the output is globally sorted)
4. Drop the bucket data and delete the temp file

Peak memory for cell processing: one bucket at a time = ~250 MB worst case.

The fine-level and coarse-level indices use separate sets of buckets. Each bucket
has **three temp files** (street, addr, interp) — not one mixed stream. This
matches the merged geo-cell writer which needs the three entry types separated
to write independent entry files and compute per-cell offsets. Total: up to
256 × 2 levels × 3 types = 1536 temp files, though ~64 buckets per level are
empty (unused S2 faces), so ~1152 active files in practice. Admin cell entries
are small enough to accumulate in memory as today.

**Coordinate access during cell assignment:** Read from mmap'd `street_nodes.bin`,
`interp_nodes.bin`, `addr_points.bin`. Iterate mmap'd `street_ways.bin` to get
(node_offset, node_count) for each way. No heap geometry data needed.

### File write order

```
Pass 1:      (nothing written)
Pass 1.5:    (nothing written, just IdSetDense)
Pass 2:      street_nodes.bin, street_ways.bin, addr_points.bin,
             interp_nodes.bin (all streamed during scan)
             strings.bin (after scan)
Resolution:  interp_ways.bin (after endpoint resolution)
Assembly:    admin_vertices.bin, admin_polygons.bin
Pass 3:      geo_cells.bin, street_entries.bin, addr_entries.bin,
             interp_entries.bin, coarse_geo_cells.bin,
             coarse_street_entries.bin, coarse_addr_entries.bin,
             coarse_interp_entries.bin, admin_cells.bin,
             admin_entries.bin (all written bucket-by-bucket)
             geocode_header.bin (last)
```

## Memory budget at planet scale

Peak heap occurs during interpolation endpoint resolution (~1 GB transient
spatial index on top of base allocations):

| Component | Heap | Notes |
|---|---|---|
| StringPool | 250 MB | 50 MB data + 200 MB FxHashMap |
| Interp slim metadata | 230 MB | Dropped after resolution |
| Interp spatial index (transient) | 1,000 MB | During resolution only, then dropped |
| way_geom (admin) | 400 MB | Dropped after assembly |
| admin_polygons | 50 MB | Simplified vertices |
| admin_relations | 30 MB | Dropped after assembly |
| Cell bucket (peak, one at a time) | 250 MB | During pass 3 only |
| IdSetDense (referenced nodes) | 1,600 MB | During pass 1.5 + pass 2, then dropped |
| **Peak heap (during resolution)** | **~2.5 GB** | |
| **Peak heap (during cell assignment)** | **~1.0 GB** | After resolution + admin dropped |

| Component | Page cache |
|---|---|
| Dense node index (referenced only) | ~10 GB |
| Mmap'd output files | ~2 GB |
| **Total page cache** | **~12 GB** |

**Grand total peak: ~2.5 GB heap + ~12 GB page cache = ~14.5 GB.** Fits in
30 GB with ~15 GB headroom for OS, file cache, and variance.

### Disk space

During the build, the following files coexist on disk:

| | Size |
|---|---|
| Input PBF | 87 GB (may be on a different disk) |
| Output index files | ~27 GB |
| Temp bucket files | ~19 GB (deleted after pass 3) |
| **Total (same disk as output)** | **~46 GB** |
| **Total (input on same disk)** | **~133 GB** |

The temp bucket files are created during pass 3 and deleted as each bucket is
consumed. Peak temp disk is ~19 GB.

## Implementation order

1. **Per-phase RSS reporting.** Add `read_rss_kb()` between every phase boundary,
   gated behind `#[cfg(feature = "hotpath")]`. Run Germany to establish baseline.

2. **Stream data files during pass 2.** Write street_nodes.bin, street_ways.bin,
   addr_points.bin, interp_nodes.bin during the scan. Defer interp_ways.bin to
   after interpolation resolution. Mmap output files for cell assignment. Drop
   the `RawStreetWay` / `RawAddrPoint` Vecs entirely. Validate: identical output
   on Denmark, lower RSS on Germany.

3. **Bucketed cell assignment.** Replace global cell entry Vecs with 256 temp-file
   buckets per level (fine + coarse). Mutex'd BufWriters per bucket. Process
   buckets sequentially in order. Validate: identical output, bounded RSS.

4. **Referenced node collection (pass 1.5).** Add way-only scan before pass 2
   to collect referenced node IDs into IdSetDense. Filter dense index writes.
   This is part of planet mode, not a conditional fallback.

5. **Mode split.** Add `--planet` flag (or auto-detect from input size). Regional
   mode uses the current in-memory builder. Planet mode uses steps 2-4.

6. **Test on Germany.** Expect ~1-2 GB RSS.

7. **Test on planet.** Expect ~2.5 GB peak heap + ~12 GB page cache.

## Risks

- **way_geom size at planet.** Estimated ~400 MB. Must measure. Planet mode v1
  assumes it fits. If measurement shows >1 GB, a follow-up spec will address it.

- **Bucket size variance.** S2 face 1 covers most of Europe + Africa. Worst-case
  bucket may be 3-4× average. With ~192 active buckets and ~19 GB total, worst
  case is ~250 MB per bucket. Acceptable.

- **Temp disk for buckets.** ~19 GB for fine + coarse street entries at planet.
  Must have sufficient free disk (see disk space section above).

- **Interpolation transient index.** ~1 GB heap for the cell-to-offset FxHashMap
  during resolution. This is the heap peak. If planet address point counts are
  higher than estimated, this grows proportionally. Fallback: iterate addr_points
  linearly for each interpolation endpoint (slower, zero extra memory).

- **Mutex contention on bucket writers.** ~192 active buckets, entries distribute
  roughly uniformly, lock held for microseconds per append. Contention should be
  low. If profiling shows otherwise, switch to per-thread local Vecs with
  single-threaded flush (trades ~400 MB memory for zero contention).

## What not to do

- **External merge sort.** Bucketing is simpler and sufficient. The bucket key
  `(cell_id >> 56)` is a prefix of numeric sort order — processing buckets in
  order produces globally sorted output. No k-way merge reader needed.

- **Concurrent string pool.** The pipelined closure runs on one thread. 250 MB
  at planet is fine.

- **Parallel admin assembly.** ~400 MB for ~300K relations is bounded. Not worth
  parallelizing.

- **Sparse node index.** Not needed with referenced-node-only dense index
  reducing page cache from ~16 GB to ~10 GB. Revisit only if measurement shows
  the ~10 GB still causes thrashing on 30 GB hosts.
