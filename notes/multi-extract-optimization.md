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
= 92 % of wall. The originally-hoped lever here was **raw passthrough
for fully-contained node blobs** (item #5) - now **CLOSED 2026-04-20**
by shadow-counter measurement showing 0 / 32,835 node blobs qualify at
planet 5-region. The write-path phases are still the big buckets but
the specific attack is closed; any future push on `NODE_WRITE` /
`WAY_WRITE` wall will need a fresh diagnosis (e.g. shared BlockBuilder
across regions, output-side compression tuning, SIMD re-encode) rather
than the raw-passthrough plan.

**Planet baseline (2026-04-20, commit `57b01f9`, UUID `dad573cb`,
16m11s wall, 5-region `--config --simple`):**

| Phase | Wall | % of total |
|---|---:|---:|
| MULTI_SCHEDULE_SCAN | 6.2 s | 0.6 % |
| MULTI_NODE_CLASSIFY | 45.4 s | 4.7 % |
| **MULTI_NODE_WRITE** | **523.9 s** | **53.9 %** |
| MULTI_WAY_CLASSIFY | 34.1 s | 3.5 % |
| **MULTI_WAY_WRITE** | **347.6 s** | **35.8 %** |
| MULTI_REL_CLASSIFY | 1.5 s | 0.2 % |
| MULTI_REL_WRITE | 12.9 s | 1.3 % |
| **Total** | **971.7 s** | 100 % |

`SCHEDULE_SCAN` is no longer a symptom: the prior 26 s Europe cost was
the `BlobReader::seek_raw` BufReader-discard issue, fixed twice - via
`aa3147c` 2026-04-18 (BlobReaderSource trait preserves the BufReader
buffer across relative skips), then superseded by `57b01f9` 2026-04-20
which migrated `try_extract_multi_single_pass` to HeaderWalker (pread
with `POSIX_FADV_RANDOM`, no buffered scan on this path). Planet now
6.2 s / 602 MB read.

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

### 5. Raw passthrough for fully-contained node blobs - CLOSED 2026-04-20

Shadow counter measurement (planet 5-region `--config --simple`, commit
`57b01f9`, UUID `dad573cb`) shows **0 / 32,835** node blobs qualify for
partial-passthrough. All 32,835 blobs (10.4 B elements) land in
`none_contained`; the existing all-N-contained fast path also fires 0
times at planet 5-region. The "high impact" prediction (90 %+ blobs
interior to at least one region) was wrong.

**Why the math is hostile:** PBF node blobs are ID-sorted, and OSM IDs
are chronological rather than geographic. Nodes within an
8,000-element blob scatter across the planet, so each blob's
geographic bbox is ~planet-wide and cannot be contained in any
sub-planet region under any bbox subdivision. Geography-sorted PBFs
(Hilbert curve over lat/lon) would flip this, but that breaks
`Sort.Type_then_ID` and is not what we ingest.

Shadow counter (`emit_node_passthrough_shadow_counters` and the
`multiextract_node_shadow_*` family) reverted on close. Load-bearing
pin lives in `src/commands/extract/multi.rs::try_extract_multi_single_pass`
just after `MULTI_SCHEDULE_SCAN_END`. Sister precedent in
`src/commands/tags_filter/mod.rs` pass-2 worker.

The existing all-N-contained passthrough path in
`multi_extract_pread_write_nodes` is **kept** for its narrow but real
niche: N=1 extracts and fully-overlapping regions. Don't remove it on
the strength of the planet 5-region 0-count - planet multi-region is
the hardest case, not the only one.

The polygon-safety known issue (raw passthrough is unsafe for polygon
regions because `contained_in` is computed from each slot's bbox, not
polygon geometry) is moot now: there's no partial-passthrough path
left to be unsafe with. The all-N-contained path already requires bbox
regions in practice via the `all_bbox` gate.

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
6. ~~**Raw passthrough for contained blobs**~~ - CLOSED 2026-04-20 via
   shadow counter (0 / 32,835 at planet 5-region). NODE_WRITE is still
   52 % of Europe wall but cannot be attacked through this path; the
   load-bearing pin lives in `src/commands/extract/multi.rs`. Item #5
   above for the full post-mortem.
7. ~~**`BlobReader::seek_raw` BufReader-discard fix**~~ - landed twice.
   First `aa3147c` 2026-04-18 (public `BlobReaderSource` trait;
   `BufReader` impl routes through `seek_relative` instead of the
   buffer-discarding stdlib `Seek` path). Then superseded by
   `57b01f9` 2026-04-20 which migrated `try_extract_multi_single_pass`
   to HeaderWalker - pread with `POSIX_FADV_RANDOM`, no buffered
   scan on this path at all. Planet `MULTI_SCHEDULE_SCAN` is now
   **6.2 s / 602 MB read** (UUID `dad573cb`) versus the prior
   ~26 s Europe baseline at commit `b7cd0e4`.
8. **Spatial index** (only for N > 50, future; not currently active)

## Relationship to other documents

- Columnar node classification → `notes/columnar-integration.md`
- Known issues (strip-4 verify failure, polygon passthrough safety,
  O(workers × regions) scaling) → TODO.md multi-extract section

### Raw passthrough disproven (2026-04-20) - load-bearing record

This section was originally a methodology prerequisite ("measure
qualifying fraction before building"). The measurement has now landed,
the result was zero, and the shadow counter has been reverted. Kept
here as the historical record of what was measured and why the path
is closed; the active load-bearing pin lives in
`src/commands/extract/multi.rs::try_extract_multi_single_pass` (look
for the long comment block right after `MULTI_SCHEDULE_SCAN_END`).

**Measurement:** shadow counter `emit_node_passthrough_shadow_counters`
(landed 2026-04-19, reverted 2026-04-20) emitted
`multiextract_node_shadow_*` counters classifying every node blob into
`all_n_contained` / `partial_contained` / `none_contained`. Planet
5-region `--config --simple` at commit `57b01f9`, UUID `dad573cb`:

| Counter | Value |
|---|---:|
| `blobs_total` | 32,835 |
| `blobs_all_n_contained` | 0 |
| `blobs_partial_contained` | **0** |
| `blobs_none_contained` | 32,835 |
| `elements_total` | 10,447,738,627 |
| Per-region (i=0..4) blobs | 0 each |

**Result:** vanishingly small, exactly like tags-filter's 0 / 50,364
shadow result on planet `w/highway=primary` (commit `a5c6854` added,
`0ef4107` removed; UUID `8c786794`).

**Why the original "geometry is different" prediction was wrong:** the
prediction assumed region-contained blobs would cluster because each
region is a contiguous bbox. That confuses ID-range-monotonicity with
geographic-bbox-monotonicity. PBF blobs are ID-sorted, and OSM IDs are
chronological - a single 8,000-element blob spans the planet
geographically. So a blob's geographic bbox is ~planet-wide and
cannot be contained in a sub-planet region under any subdivision.

**Sister precedents.** Other "shadow-counter said zero, reverted with a
pin" closures: `src/commands/tags_filter/mod.rs` pass-2 worker (the
original tags-filter shadow). The per-group raw-passthrough scaffolding
in `src/write/raw_passthrough.rs` is the design surface if any future
caller (geography-sorted PBFs, single-region overlap configs) ever
needs partial-blob passthrough.
