# Multi-extract optimization research

## Current architecture

Single-pass multi-extract (`try_extract_multi_single_pass` in
`src/commands/extract.rs`) reads the PBF once and writes N output files.
Three-phase barrier: nodes → ways → relations.

Each phase has two sub-steps:
1. **Classification** - `parallel_classify_phase` with pread workers
   (parallel decode, per-region ID collection)
2. **Write** - `multi_extract_pread_write` with pread workers (parallel
   decode, N BlockBuilders per worker, reorder buffer for ordering)

## Performance profile (current)

Both classification and write phases use parallel pread workers.
Denmark 5-region: 2.1s. Japan 5-region: 8.1s. Single-pass is now
2.7x faster than 5 sequential extracts at Japan scale (8.1s vs 22s).

Original bottleneck (sequential write phase) was resolved by converting
to `multi_extract_pread_write` (commit `9f72bcf`).

## Investigated items

### 1. Scratch buffer reuse in write phases - DONE

Commit `19f8bc9`. `new_with_scratch` in all 3 write phases.

### 2. Per-block Vec allocation in way classify closure - OPEN

Line 868: `vec![Vec::new(); n]` allocates N empty Vecs per block per
worker inside the classify closure, with `|| ()` init (no per-worker
state).

**Current state (investigated April 2026):** Node classification (line
764) uses proper per-worker init with `DenseNodeColumns` + scratch Vecs
that are cleared between blocks - no per-block allocation. Relation
classification (line 922) uses `parallel_classify_accumulate` with
per-worker `IdSetDense` accumulation - also no per-block allocation.

Only way classification still allocates per-block. Fix: change the init
from `|| ()` to `|| vec![Vec::<i64>::new(); n]`, pass as `&mut S`,
clear inner Vecs between blocks (same pattern as node classification).

**Impact:** Minor - Vec<Vec<i64>> is small. But it's an inconsistency
with the other two phases. Mechanical fix.

### 3. Columnar node classification - DONE

Shipped for multi-extract (line 764). `DenseNodeColumns::new()` +
`collect_matching_ids_multi_bbox`. Measured: multi-extract Japan node
classify 1081ms → 748ms (-31%). See `notes/columnar-integration.md`.

### 4. Parallel decode in write phases - DONE

Commit `9f72bcf`. `multi_extract_pread_write` replaces sequential
BlobReader in all 3 write phases. Denmark 5-region: 6.7s → 2.1s (3.2x).
Japan 5-region: 32.5s → 8.1s (4.0x).

### 5. Raw passthrough for fully-contained node blobs - OPEN

Infrastructure is in place: `NodeBlobInfo` tracks per-region containment,
`multi_extract_pread_write_nodes` handles passthrough via ReorderBuffer
interleaving. Currently only fires when a blob is contained in ALL N
regions (useful for N=1 or fully-overlapping regions).

Per-region passthrough for disjoint strips needs a hybrid decode+raw
consumer path: decode once, write raw to contained regions, route
elements to non-contained regions.

**Impact:** High at planet scale. 90%+ of node blobs are interior to at
least one region in a typical multi-extract configuration.

**Known issue from TODO.md reviewer findings (2026-04-09):** Raw
passthrough is unsafe for polygon regions - `contained_in` is computed
from each slot's bbox, not polygon geometry. For polygon or
multipolygon extracts, "blob bbox contained in region bbox" does not
prove every node is inside the polygon. Pre-existing issue.

### 6. Spatial index for large N - OPEN

Currently O(N) per element for region classification (linear scan
through N bboxes). For N > 50, a grid index would be better:
3600×1800 cells of 0.1°, precompute overlapping regions per cell.
Per-element lookup becomes O(1) grid probe + check overlapping
regions in that cell.

**Impact:** Only matters for large N (50+ regions). For typical
multi-extract (5-20 regions), linear scan is fine.

## Priority order

1. ~~**Scratch buffer reuse**~~ - DONE (commit `19f8bc9`)
2. ~~**Parallel decode in write phases**~~ - DONE (commit `9f72bcf`)
3. ~~**Columnar node classification**~~ - DONE (line 764)
4. **Raw passthrough for contained blobs** (high impact, moderate effort,
   polygon safety issue needs resolution first)
5. **Per-worker way classify scratch** (mechanical fix, minor impact)
6. **Spatial index** (only for N > 50, future)

## Relationship to other documents

- Columnar node classification → `notes/columnar-integration.md`
- Raw passthrough → `notes/raw-group-passthrough.md` (blob-level, not
  per-group; the per-group primitives are unused scaffolding)
- Known issues (strip-4 verify failure, polygon passthrough safety,
  O(workers × regions) scaling) → TODO.md multi-extract section
