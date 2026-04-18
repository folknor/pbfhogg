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
Denmark 5-region: 2.1 s. Japan 5-region: **7.7 s** (commit `b7cd0e4`,
UUID `08fefe51`). Europe 5-region: **799.9 s** (commit `b7cd0e4`, UUID
`c1ff6ec9`, `--bench 1`). Planet 5-region: 965 s (commit `7e9c2e9`,
UUID `1cd62e90`). Single-pass is still 2.7× faster than 5 sequential
extracts at Japan scale (7.7 s vs ~21 s).

Original bottleneck (sequential write phase) was resolved by converting
to `multi_extract_pread_write` (commit `9f72bcf`).

**Europe phase breakdown (commit `b7cd0e4`, UUID `c1ff6ec9`)** - first
full breakdown after the 2026-04-17 instrumentation landing
(commit `1e8d37b` added `MULTI_EXTRACT_START/END`,
`MULTI_SCHEDULE_SCAN_START/END`, and eight `multi_extract_*` counters):

| Phase | Wall | % of total |
|---|---:|---:|
| MULTI_SCHEDULE_SCAN | 26.0 s | 3.3 % |
| MULTI_NODE_CLASSIFY | 15.8 s | 2.0 % |
| **MULTI_NODE_WRITE** | **413.4 s** | **51.7 %** |
| MULTI_WAY_CLASSIFY | 13.7 s | 1.7 % |
| **MULTI_WAY_WRITE** | **317.5 s** | **39.7 %** |
| MULTI_REL_CLASSIFY | 0.9 s | 0.1 % |
| MULTI_REL_WRITE | 12.1 s | 1.5 % |
| **Total** | **799.4 s** | 100 % |

Write phases dominate Europe: `NODE_WRITE` (52 %) + `WAY_WRITE` (40 %)
= 92 % of wall. **Write-path optimization is the high-impact
opportunity**, not classification. Within write, the obvious lever is
**raw passthrough for fully-contained node blobs** (item #5 below) -
eliminating decode+re-encode on ~90 % of node blobs at Europe could
shave double-digit seconds from the 413 s `NODE_WRITE` phase.

`SCHEDULE_SCAN`'s 26 s at Europe is the `BlobReader::seek_raw`
BufReader-discard issue from TODO.md Performance section (the
header-walk hot path does ~10× file-size amplification at the default
256 KB buffer). Cross-cutting fix, not multi-extract specific.

## Investigated items

### 1. Scratch buffer reuse in write phases - DONE

Commit `19f8bc9`. `new_with_scratch` in all 3 write phases.

### 2. Per-block Vec allocation in way classify closure - DONE

Commit `b7cd0e4` (2026-04-17). Swapped `|| ()` init for
`|| vec![Vec::<i64>::new(); n]`, clear-then-populate-then-drain pattern
mirroring node classify. Inner `Vec<i64>` capacities now amortize
across the ~N blobs each decode worker processes rather than going
through repeated doublings on each block.

**Measured (Japan 5-region `--bench 3`):** MULTI_WAY_CLASSIFY phase
892 ms → 848 ms, **−44 ms / −5 %** on the targeted phase (UUIDs
`8bc1773f` pre-fix vs `08fefe51` post-fix). No total-wall regression
at either Japan or Europe (799.9 s at Europe, within single-shot noise
of the 792.7 s prior-commit baseline `d824d7aa`). Europe phase impact
not paired-benched - the targeted phase is only 1.7 % of wall, so even
a perfect speedup falls below single-shot noise on total.

Mechanism well-understood; proportionally larger savings are available
at planet scale (50-150× more blobs per worker than Japan) but the
phase is not on the critical path there either.

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
4. ~~**Per-worker way classify scratch**~~ - DONE (commit `b7cd0e4`,
   2026-04-17; item #2 above)
5. ~~**Instrumentation sweep**~~ - DONE (commit `1e8d37b`,
   2026-04-17: `MULTI_EXTRACT_START/END`, `MULTI_SCHEDULE_SCAN_START/END`,
   8 `multi_extract_*` counters)
6. **Raw passthrough for contained blobs** (high impact now confirmed:
   NODE_WRITE is 52 % of Europe wall, WAY_WRITE 40 %; polygon safety
   issue still needs resolution first)
7. **`BlobReader::seek_raw` BufReader-discard fix** (cross-cutting, but
   measurable 26 s at Europe in MULTI_SCHEDULE_SCAN - see TODO.md
   Performance section)
8. **Spatial index** (only for N > 50, future)

## Relationship to other documents

- Columnar node classification → `notes/columnar-integration.md`
- Raw passthrough → `notes/raw-group-passthrough.md` (blob-level, not
  per-group; the per-group primitives are unused scaffolding)
- Known issues (strip-4 verify failure, polygon passthrough safety,
  O(workers × regions) scaling) → TODO.md multi-extract section
