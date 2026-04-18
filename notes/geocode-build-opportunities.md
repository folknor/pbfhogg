# Geocode index builder - optimization plan

> **Scope.** This plan targets wall-time for the *full-rebuild* path —
> `build-geocode-index` against a cold PBF. Complementary effort in
> [incremental-geocode-index.md](incremental-geocode-index.md) targets
> *avoiding* the full rebuild on daily diffs (currently blocked on a
> format-v2 element-ID change; see that doc for the design sketches).
>
> **Rebench status (2026-04-18).** Post-fix planet wall still pending:
> the first attempt on 2026-04-18 OOM-killed at 1m34s in Pass 1.5 and
> is queued in `overnight.sh` round 2. Until that lands, the 1255 s
> planet wall below (commit `7e9c2e9`, UUID
> `1c70850916824749bf1d68ef8970189e`) carries an unaccounted
> all-blobs-scan cost from the `has_indexdata` regression live in
> `4ce7e93..c0ae9a7`. Phase/RSS data is unaffected — only wall.
> Re-measure wall before relying on per-phase totals.

Target: `pbfhogg build-geocode-index` on planet. Current: 20m55s (1255 s) wall [TAINTED], **29.5 GB peak anon RSS** in `GEOCODE_PASS1_5` (commit `7e9c2e9`, sidecar `1c708509`, 2026-04-17). Phase peaks (anon): PASS1 12 MB, **PASS1_5 29.5 GB**, PASS2 13.9 GB, PASS3 10.4 GB. Earlier numbers in this note (14.59 GB / 17.8 GB) under-reported the peak: brokkr previously hid short-emitting phase markers from sidecar output, so PASS1_5's transient peak never surfaced. The peak itself has not regressed - only its visibility.

## Thesis

Unlike ALTW, the geocode builder is **structurally well-shaped**. There is no external sort, no redundant decompression loop, no staged disk seam chain. The problem is narrower: **Pass 2 is single-threaded on purpose, and it is doing most of the work**. The documented reason for that choice is a glibc arena retention issue, and the fix for that issue is already in the codebase - in [`renumber_external.rs:95-98`](../src/commands/renumber_external.rs#L95).

One `mallopt(M_ARENA_MAX, 2)` call unlocks Pass 2 parallelization. That is the headline change. Secondary wins live in Pass 1.5 (over-decoding), Pass 3 stage B (sequential bucket merge), and fine/coarse level duplication.

No internal API needs rewriting. `IdSetDense`, `PrimitiveBlock`, `parallel_classify_phase`, the mmap'd coord index, and the output file shapes all stay exactly as they are.

### Landed so far (2026-04-18)

| Commit | Item | Germany wall | Germany Pass 1.5 peak anon |
|---|---|---:|---:|
| `c977b97` | instrumentation baseline | 71.1 s | 20.3 GB |
| `63800d3` | **#7** — Pass 1.5 shared-atomic `IdSetDense` | 65.4 s | **1.75 GB** (−91 %) |
| `88cf796` | **#1 Phase 2a** — `mallopt` + parallel node scan | **49.0 s** | 1.77 GB |

Cumulative Germany −31 %. Pass 1.5 peak anon dropped from the planet-OOM
zone to comfortable. Remaining targets (Phase 2a main-thread merge,
Phase 2b, Pass 3) still to land — see per-item status below.

## Yardstick

| Phase | Wall (measured `7e9c2e9`) [TAINTED — wall only] | Peak Anon | Notes |
|---|---:|---:|---|
| Pass 1 (relations) | 42 s | 12 MB | sequential, tiny input fraction |
| Pass 1.5 (referenced-node collect) | **167 s** | **29.5 GB** | parallel, full `PrimitiveBlock` decode - actual peak, previously hidden by brokkr's short-marker filter |
| Schedule scanner | 16 s | 12 MB | between passes |
| Pass 2 (fused nodes + ways) | **881 s** | 13.9 GB | **single-threaded** (mallopt fix not yet applied here) |
| Pass 3 (cell assignment, both levels) | 141 s | 10.4 GB | rayon compute + sequential bucket merge, run twice |
| **Total** | **1255 s** [TAINTED] | **29.5 GB peak** | |

Breakdown ground-truthed by sidecar markers (`1c708509`). Pass 2 still holds ~70 % of wall and is the headline target. Pass 1.5 is now the **memory** target - its peak is what governs whether the build fits on a 30 GB host without swap.

Target after this plan: **~10-12 min wall at planet, RSS reduced from 29.5 GB Pass 1.5 peak to <16 GB.**

## Development baseline (Germany, 2026-04-18, plantasjen)

Germany is the primary iteration dataset: Pass 2 scan is long enough (40+ s)
for `--bench 1` to resolve parallelization wins above noise, but short enough
to iterate multiple times per hour. Denmark (~5-7 s) is the correctness gate
(`diff -r` against baseline or query-level comparison via the Reader);
Europe (~520 s pre-fix, est. ~360 s post-#1) is pre-landing confirmation;
planet is publish-only.

### Benchmarks

| Commit | Mode | UUID | Wall |
|---|---|---|---:|
| `c977b97` (instrumentation baseline) | `--bench 1` | `e89b1691` | 71.1 s |
| `c977b97` | `--hotpath` | `90a746dd` | 72.2 s |
| `c977b97` | `--alloc` | `0cc2ac56` | 70.3 s |
| `63800d3` (post-#7) | `--bench 1` | `572ae7d5` | **65.4 s** (−8 %) |
| `88cf796` (post-#1 Phase 2a) | `--bench 1` | `e2354bc1` | **49.0 s** (−31 % cumulative) |
| `88cf796` Denmark smoke | `--bench 1` | `d6684457` | 5.0 s |

### Phase breakdown (Germany `--hotpath`, UUID `90a746dd`)

| Function | Calls | Wall | % of total |
|---|---:|---:|---:|
| `run_pass2` | 1 | **42.3 s** | **58.7 %** |
| `run_pass1_5` | 1 | 9.1 s | 12.6 % |
| `bucketed_cell_assignment` | 2 | 7.0 s | 9.7 % |
| `assemble_admin_polygons` | 1 | 6.8 s | 9.5 % |
| `resolve_interpolation_endpoints_mmap` | 1 | 3.2 s | 4.5 % |
| `run_pass1` | 1 | 2.3 s | 3.2 % |
| Remainder (writes, header, smoke test) | — | ~1.5 s | ~2 % |

**Inside `run_pass2` (dominant cost is zlib, not tag/coord work):**

| Inner function | Calls | Wall | % of Pass 2 |
|---|---:|---:|---:|
| `decompress_into` | 62 350 | 14.0 s | 33 % |
| `decompress_blob_raw` | 8 696 | 11.5 s | 27 % |
| **Combined zlib decompression** | | **25.5 s** | **60 %** |
| PrimitiveBlock parse + classify + coord lookup + streaming writes | | ~17 s | 40 % |

Structural confirmation for item #1: **parallelizing Pass 2 is primarily
parallelizing decompression.** At 6 cores with arena contention overhead,
an 80 % parallel win on the 42 s Pass 2 scan is ~34 s saved: Germany wall
71 s → ~37 s, a −48 % reduction. At planet scale the same ratio on an
881 s Pass 2 is ~700 s saved: 1 255 s → ~550 s, **within the plan's
10-12 min target** from item #1 alone.

### Allocation profile (Germany `--alloc`, UUID `0cc2ac56`)

Total: **46.6 GB allocated, 80.9 GB deallocated** during the run (the
deallocation-over-allocation spread is accurate — most allocations free
before the next blob's allocations land; steady-state RSS tracks the
difference, not the sum).

| Function | Calls | Exclusive alloc | % |
|---|---:|---:|---:|
| `parse_and_inline_with_scratch` | 71 157 | 20.1 GB | 46.4 % |
| `decompress_into` | 62 350 | 9.9 GB | 22.9 % |
| `run_pass2` (non-nested) | 1 | 7.1 GB | 16.4 % |
| `bucketed_cell_assignment` (non-nested) | 2 | 3.3 GB | 7.7 % |
| `assemble_admin_polygons` | 1 | 1.5 GB | 3.5 % |
| `resolve_interpolation_endpoints_mmap` | 1 | 683 MB | 1.5 % |
| `assign_admin_cells` | 1 | 490 MB | 1.1 % |
| `run_pass1` | 1 | 180 MB | 0.4 % |

~30 GB of the 46 GB churn lives in wire-parse + zlib-decompress scratch per
blob — exactly the cross-thread alloc/free pattern that `mallopt(M_ARENA_MAX, 2)`
bounds in `renumber_external`. Parallelizing Pass 2 without the mallopt
prelude risks the 25+ GB arena blowup explicitly called out in the
sequential-choice rationale at [pass2.rs:369-374](../src/geocode_index/builder/pass2.rs#L369).

### Memory peaks (Germany)

Pre-#7 (baseline `e89b1691`) and post-#7 (`572ae7d5`) — post-#1 Phase 2a
(`e2354bc1`) is essentially identical to post-#7 since Phase 2a kept
Pass 1.5 unchanged and didn't materially move Pass 2 peaks:

| Phase | Baseline peak anon | Post-#7/#1 peak anon |
|---|---:|---:|
| `GEOCODE_PASS1` | 232 MB | 200 MB |
| `GEOCODE_PASS1_5_SCAN` | **20.3 GB** | **1.77 GB** (−91 %) |
| `GEOCODE_PASS2_RANK_INDEX` | 2.77 GB | 1.78 GB |
| `GEOCODE_PASS2_SCAN_LOOP` (post-#7) / Pass 2a+2b (post-#1) | 3.86 GB | 2.88 GB |
| `GEOCODE_PASS2_ADMIN_ASSEMBLY` | 505 MB | 2.0 GB* |
| `GEOCODE_PASS3_STAGEB` (fine) | 1.98 GB | 2.18 GB |

\* The post-#1 admin-assembly reading includes whatever residency Phase
2a's merge closure held at join time; the pre-# baseline didn't see
parallel decode traffic through the main thread. Not material at Germany
scale but worth watching on Europe/planet.

**Before item #7 landed:** the 20.3 GB Pass 1.5 peak on a 4.7 GB input
was the most urgent finding. It scaled to the 29.5 GB planet peak and
OOM-killed the first 2026-04-18 planet re-bench attempt at 1m34s in
Pass 1.5. Per-worker `IdSetDense` chunks grew independently because
worker-local allocations weren't bounded by the final merged set size;
Germany's 116 M referenced nodes × 6-8 workers × full planet ID range
produced the bloat. Item #7 replaced the per-worker accumulate pattern
with a shared pre-allocated `IdSetDense` populated via `set_atomic` —
same pattern as `renumber_external.rs:166-179`. Measured delta: Pass 1.5
peak 20.3 GB → 1.75 GB (−91 %), wall 6.6 s → 1.1 s (−82 % bonus from
no per-worker merge bottleneck, avg cores 6.5 → 21.4).

**Why #7 landed before #1.** The original plan's step order was
"instrument → #1 → #3+#4 → #2 → #5+#6 → #7 revisit". The updated
sidecar data pushed #7 ahead: without the Pass 1.5 shrink, the 27 GB-RAM
iteration host would have continued OOM-killing on every planet run.
#7 is a small diff on a well-trodden pattern and was the prerequisite
for any reliable planet iteration on this host. See commit `63800d3`.

### Shape counts (Germany)

- 116.3 M referenced nodes (compact index 930 MB)
- 19.8 M address points
- 3.3 M street ways
- 78 interpolation ways
- 43 K admin polygons
- 512 K unique strings (7.5 MB strings data)

## Current architecture (for reference)

**Pass 1** [(builder.rs:402)](../src/geocode_index/builder.rs#L402). Sequential scan of relation blobs via `ElementReader::for_each_block_pipelined` with `BlobFilter::only_relations()`. Collects `RawAdminRelation` list (admin + postal boundaries) and builds `needed_admin_ways` `IdSetDense` from member way IDs. Output volume is small.

**Pass 1.5** [(builder.rs:488)](../src/geocode_index/builder.rs#L488). Parallel scan of way blobs via `parallel_classify_accumulate`. Each worker decodes a full `PrimitiveBlock`, tag-classifies each way, and sets referenced node IDs into a **per-worker `IdSetDense`**. All per-worker sets merged into `referenced_nodes` after join. Rank index built at [builder.rs:562](../src/geocode_index/builder.rs#L562).

**Pass 2** [(builder.rs:552)](../src/geocode_index/builder.rs#L552). **Single-threaded.** Allocates a 16 GB anon `MmapMut` as `coord_mmap` (rank-indexed). One sequential loop ([builder.rs:620](../src/geocode_index/builder.rs#L620)) preads every OsmData blob (minus relations), decompresses, builds a `PrimitiveBlock`, and processes elements:
- For each dense node: if `referenced_nodes.get(id)`, write `(lat, lon)` to `coord_mmap[rank*8 ..]` using `rank(id)`. If the node has addr tags, emit an `AddrPoint` to `addr_points.bin`.
- For each way: classify by tags (highway / building / addr:interpolation / `needed_admin_ways`), resolve coords via `referenced_nodes.rank()` lookups into `coord_mmap`, emit to `street_ways.bin` / `street_nodes.bin` / `addr_points.bin` (building centroids) / `interp_nodes.bin` / `way_geom` hashmap (admin).

The comment at [builder.rs:606-611](../src/geocode_index/builder.rs#L606) explains the sequential choice: glibc arena retention when `PrimitiveBlock` `Vec`s cross thread boundaries grows heap to 25+ GB at planet scale.

**Pass 3** [(builder.rs:725)](../src/geocode_index/builder.rs#L725). [`bucketed_cell_assignment`](../src/geocode_index/builder.rs#L1262) runs twice (fine level 17, coarse level 14). Each run:
- **Stage A**: rayon-parallel `cover_segment` over streets (chunked via `STREET_CHUNK = 100_000`); sequential loops for addr points and interpolation; single-threaded distribute-to-bucket-files step.
- **Stage B**: sequential loop over 256 buckets - read bucket file, sort by cell_id, group, write to `geo_cells` + per-type entry files.

Then admin cells via [`assign_admin_cells`](../src/geocode_index/builder.rs#L1509) - sequential flood-fill BFS per polygon.

## Central observation

[`renumber_external.rs:95-98`](../src/commands/renumber_external.rs#L95) shows the two-line fix for exactly the problem Pass 2 avoids by going sequential:

```rust
#[cfg(target_os = "linux")]
unsafe { libc::mallopt(libc::M_ARENA_MAX, 2); }
```

Documented in-place: without it, renumber's cross-thread `OwnedBlock` Vec traffic grows to ~26 GB anon on planet. With it, under 1 GB. Scoped to the command, other pbfhogg paths unaffected.

Pass 2's historical sequential choice (comment preserved in the old
`pass2.rs:606-611` pre-split) described the same arena fragmentation
phenomenon and picked the wrong remedy. The sequential decode bound was
never a correctness constraint — it was an RSS constraint, and renumber's
`mallopt` answers it directly.

**With `M_ARENA_MAX = 2` set, Pass 2 has been parallelised exactly like
renumber's pass 1.** Phase 2a (nodes) landed `88cf796`. Phase 2b (ways)
still to come; see item #1 Phase 2b below.

## Opportunities, ranked

### #1 - Parallelize Pass 2 (by far the biggest)

Enable via `mallopt(M_ARENA_MAX, 2)` at the top of [`build_geocode_index`](../src/geocode_index/builder/mod.rs#L84). Same scope and placement as renumber.

Split Pass 2 into two parallel sub-phases. Sorted PBF (`Sort.Type_then_ID`) guarantees all node blobs precede all way blobs, so the phase barrier is clean.

**Phase 2a - parallel node scan — LANDED 2026-04-18 (commit `88cf796`).**

Workers decode node blobs via
[`parallel_classify_phase`](../src/commands/mod.rs#L563) with owned-string
output (`NodeBlobOut`: Vec of `(rank, lat, lon)` coord writes + Vec of
`PendingAddrPoint` with `Box<str>` for hn/st/pc). Main thread merge
closure applies coord_mmap writes (disjoint ranks, no races), interns
strings into the shared `StringPool`, and streams `AddrPoint`s to
`addr_points.bin` in blob-sequence order. Output is byte-identical to
the previous sequential path.

Germany measured: Pass 2 scan 42.0 s → ~26 s (−38 %), total wall 65.4 s →
49.0 s (−25 %). Denmark smoke: 5.0 s, smoke-test passes.

**Follow-up (open): main-thread merge bottleneck.** Phase 2a currently
pushes 116 M `(u64, i32, i32) = 16 bytes` coord-write tuples (~1.86 GB
on Germany) through the merge channel — the main thread becomes the
serialization point. Two candidate fixes:

1. **Direct `coord_mmap` writes from workers.** Wrap the raw pointer
   in a `Sync`-safe struct (workers hit disjoint rank ranges, so no
   atomics needed). Est. −5-7 s on Germany; eliminates the 1.86 GB
   channel traffic entirely.
2. **Pre-computed per-blob `[ref_rank_start, ref_rank_end)` slices** of
   `coord_mmap`, threaded through as per-blob worker state. Safer
   (no `unsafe`) but requires extending `parallel_classify_phase` to
   accept per-blob state, or writing a custom thread pool like
   `renumber_external/pass1.rs`.

Option 1 is the simpler change; option 2 is structurally cleaner. Either
way the win is ~5-7 s on Germany; at planet the ratio should be similar
(~50-70 s absolute).

**Phase 2b - parallel way scan (open).** Pattern: [`stage2d_worker`](../src/commands/renumber_external/mod.rs#L249).

Way phase is currently sequential at ~8 s on Germany (embedded inside
Pass 2's ~26 s post-Phase-2a wall). Parallelisation requires:
- Work-stealing dispatch over way blobs.
- Each worker: pread → decompress → `PrimitiveBlock`. `coord_mmap` is read-only now; `referenced_nodes.rank_if_set(nid)` per ref resolves coords.
- Emits to **per-worker tmp slices** of `street_ways.bin`, `street_nodes.bin`, `addr_points.bin` (building centroids), `interp_nodes.bin`, `interp_ways` metadata, and `way_geom` entries for admin.
- **StringPool.** Each worker holds its own `StringPool` with a worker-local offset space. After join, **sequential merge** into a single final pool and **remap** `name_offset` / `street_offset` / `housenumber_offset` / `postcode_offset` fields in the concatenated tmp files via a `Vec<u32>` per worker mapping worker-local → global offsets. Single pass per record stream. No per-intern mutex.
- **Output concatenation.** Per-worker tmp files concatenated in worker order. `street_ways.bin` and `interp_ways.bin` carry `node_offset: u64` fields - rewrite those records once during concatenation, adding each worker's prefix offset into `street_nodes.bin` / `interp_nodes.bin`. One sequential pass per way-record stream.

Lower absolute win than Phase 2a (ways are ~20 % of blob volume in a
sorted PBF, so ~8 s of Germany's Pass 2) but still measurable and
scales to ~80-100 s at planet. Much larger diff surface than the
Phase 2a follow-up above; probably lands as its own commit after the
coord_mmap write change stabilises Phase 2a's numbers.

### #2 - Drop `PrimitiveBlock` decode from Pass 1.5

Pass 1.5 (`parallel_classify_accumulate` at [builder.rs:498](../src/geocode_index/builder.rs#L498)) pulls full `PrimitiveBlock::from_vec_pooled_with_scratch` per blob (via [commands/mod.rs:625](../src/commands/mod.rs#L625)). It needs only `way.tags()` and `way.refs()`.

Build a wire-format `scan_way_tagged_refs(decompressed, tag_predicate, emit)`:
- No UTF-8 validation of the string table.
- Resolve tag keys/values by matching raw bytes in the string table against pre-encoded literal byte patterns (`b"highway"`, `b"name"`, `b"addr:housenumber"`, `b"addr:street"`, `b"addr:interpolation"`, `b"building"`). The existing [`scan_way_refs`](../src/commands/way_scanner.rs#L24) is the ref-only template; add a tag walk that resolves key/val byte offsets against raw string-table bytes.
- Call `emit(way_id, &refs)` only when the tag predicate matches.

The `tags_filter` / `tags_filter_osc` commands already use this byte-match pattern; follow their shape.

Feed `parallel_classify_accumulate` with a variant worker that runs the wire-format scanner instead of building a `PrimitiveBlock`. Alternatively, accept that the decode is part of the worker state and switch to a targeted pread + decompress loop in `parallel_classify_accumulate`'s shape.

**Expected win: ~50-100 s at planet.** Also frees residency during Pass 1.5.

### #3 - Parallelize Pass 3 stage B

Stage B is [a sequential loop over 256 buckets](../src/geocode_index/builder.rs#L1389). Each bucket is independent; bucket partition is the top 8 bits of `cell_id`, so the 256 buckets are already in globally sorted `cell_id` order.

Parallelize with rayon:
- Each bucket task: `read(bucket_path)` → parse → sort by `cell_id` → group → emit per-type streams into **per-bucket tmp output files** (`cells.{i:03}`, `street_entries.{i:03}`, `addr_entries.{i:03}`, `interp_entries.{i:03}`).
- After join: sequential concatenation of the 256 per-bucket tmp files per stream into the final output files. `geo_cells.bin` records contain byte offsets into the entry streams - during concatenation, add the running prefix offset (across buckets) to each record's `street_offset` / `addr_offset` / `interp_offset` before writing. Same pattern as Phase 2b file concatenation.

**Expected win: ~40-80 s at planet.** No correctness ambiguity - ordering preserved by construction.

### #4 - Fuse fine + coarse cell computation

[`bucketed_cell_assignment`](../src/geocode_index/builder.rs#L1262) runs twice ([builder.rs:757](../src/geocode_index/builder.rs#L757), [builder.rs:765](../src/geocode_index/builder.rs#L765)), once per S2 level. The expensive operation is `cover_segment`, called per street/interp segment per level.

A level-14 S2 cell is the unique parent of its level-17 children (the two levels differ by 3 cell-ID bits). If `cover_segment` at level 17 produces cell set `S17`, the correct level-14 cover is `{ parent(c) : c ∈ S17 }` deduplicated - `cover_segment` at a coarser level can only find cells already hit (as parents) by a finer-level cover.

Restructure Pass 3 to **one fused parallel pass**:
- rayon flat_map over segments emits both `(cell_17, way_idx, seg_idx)` and, for each distinct parent of those level-17 cells, `(parent_cell_14, way_idx, seg_idx)`. Per-segment dedup of parents via a small stack set (segments usually touch 1-4 level-17 cells ⇒ 1-2 distinct parents).
- Distribute to two separate bucket trees: `.buckets-level17/` and `.buckets-level14/`.
- Stage B (parallelized per #3) runs on both tree sets independently; reuse the same worker pool.

**Expected win: ~40-60 s at planet.** Halves the Stage A `cover_segment` workload.

### #5 - Parallelize addr-point and interpolation cell assignment

[builder.rs:1330-1344](../src/geocode_index/builder.rs#L1330) (addr points) and [builder.rs:1347-1368](../src/geocode_index/builder.rs#L1347) (interpolation) are sequential loops over per-entry work that is trivially independent. `.into_par_iter().flat_map_iter(...)` + bucket distribute, same shape as streets. At planet, ~20 M addr points and smaller interp count.

**Expected win: 20-60 s at planet.** Straightforward.

### #6 - Parallelize admin cell flood-fill

[`assign_admin_cells`](../src/geocode_index/builder.rs#L1509) iterates polygons sequentially, running a full BFS per polygon. Per-polygon work is independent. `polygons.par_iter().flat_map(...)`, merge into a single `Vec<AdminCellEntry>` at end. Work is uneven (large countries at admin level 10 dominate), but rayon work-stealing handles that.

**Expected win: 20-60 s at planet.**

### #7 - Shared atomic IdSetDense in Pass 1.5 — LANDED 2026-04-18 (commit `63800d3`)

Previously used **per-worker `IdSetDense` accumulation** — each worker
held unbounded bitmap chunks across the full planet ID range, and 6-8
workers independently allocated up to ~1 GB each. Pass 1.5 peak anon
measured 20.3 GB on Germany (4.7 GB input) and 29.5 GB at planet — the
latter OOM-killed the first 2026-04-18 planet re-bench attempt.

Switched to the pattern used by [`renumber_external/pass1.rs`](../src/commands/renumber_external/pass1.rs):
a single pre-allocated `IdSetDense` shared across workers, populated
concurrently via `set_atomic` (`AtomicU8::fetch_or` under the hood). A
new single-header-pass helper `build_way_schedule_and_max_node_id`
provides both the way-blob schedule and the max node ID needed for
`pre_allocate(max_node_id)`, avoiding two separate header walks.
`parallel_classify_accumulate` → `parallel_classify_phase` with unit
worker state; the classify closure captures `&IdSetDense` and writes
directly.

Germany measured deltas:
- Pass 1.5 peak anon: **20.3 GB → 1.75 GB (−91 %)**.
- Pass 1.5 wall: **6.6 s → 1.1 s (−82 %)** — unexpected bonus win from
  removing the per-worker merge bottleneck. Avg cores 6.5 → 21.4.
- Whole-run wall: 71.1 s → 65.4 s (−8 %).

Planet projection: ~29.5 GB → ~4-5 GB at Pass 1.5 peak (referenced_nodes
final size plus per-worker PrimitiveBlock decode residency). Unblocks
reliable planet iteration on 27 GB hosts — the original motivation.

## Local changes worth keeping on the list

### Interpolation endpoint resolution: flatter spatial index

[`resolve_interpolation_endpoints_mmap`](../src/geocode_index/builder.rs#L970) builds a transient `FxHashMap<u64, Vec<u32>>` mapping S2 cell IDs to address-point indices. At planet this is ~1 GB heap (~150 M address points across ~10 M distinct S2 cells, each with an individually allocated `Vec`).

A CSR-style layout (one contiguous offsets array + one contiguous values array, sorted by cell_id, binary-search lookup) would roughly halve the peak. The structure is short-lived (built during resolution, dropped immediately after), so this is peak-heap reduction, not throughput.

**Peak heap during this phase: ~2.5 GB. The transient index is the largest contributor.** Not on the wall-time critical path; keep as a follow-up once #1-#4 land.

## What to leave alone

- **The ~16 GB anon `coord_mmap`.** Structurally similar to (a proposed version of) ALTW's coord table, but **sized by geocode's filtered `referenced_count`** - only nodes referenced by geocode-relevant ways (streets, building addrs, interp, admin). At planet this is well below ALTW's total unique-referenced count (~10 B, measured 2026-04-16 when an ALTW reshape attempt OOM'd at Europe with a 29 GB coord table). Geocode's tag-filter pre-narrowing is what keeps this structure viable in RAM; do not copy this pattern to a command that touches **all** way refs. Right size, right indexing; do not try to compact or partition. **Any future plan change that alters the filter's breadth must re-measure `referenced_count` at planet before assuming the RAM footprint stays similar.**
- **`IdSetDense`.** Used correctly. The only change is the per-worker→shared-atomic swap in #7, which is a usage-pattern tweak, not an API change.
- **`PrimitiveBlock` in Pass 2.** Once Pass 2 is parallel, full-decode cost amortizes across cores. A wire-format tagged scanner would duplicate tag-resolution logic for modest gain - save as a possible post-parallelization tweak if profiling shows tag iteration hot.
- **PbfWriter.** Not used. Outputs are raw binary files.
- **Pass 1 (relation scan).** Tiny fraction of wall. Not worth parallelizing.
- **Output file formats.** The on-disk layout is consumed by a mature `Reader`; do not change byte-level shapes to accommodate build-time parallelism. All parallelization above is tmp-file + sequential-concatenation.

## Plan of attack

**Landed (2026-04-18):**

1. ~~**Add per-phase `*_ms` counters** + hotpath annotations to ground-truth
   the wall breakdown.~~ Done in `c977b97`. Marker coverage: per-phase
   START/END pairs on Pass 1 / 1.5 / 2 / 3 plus inner sub-phase markers
   inside Pass 1.5 (schedule, scan), Pass 2 (rank_index, scan_loop,
   flush_mmap, admin_assembly, interp_resolve, write), and Pass 3
   (admin_cells, fine/coarse ×{stagea_streets, stagea_addr, stagea_interp,
   stageb}, admin_index). `#[hotpath::measure]` on `run_pass1`,
   `run_pass1_5`, `run_pass2`, `bucketed_cell_assignment`,
   `assign_admin_cells`, `assemble_admin_polygons`,
   `resolve_interpolation_endpoints_mmap`, `write_admin_data`,
   `write_admin_index`, `build_geocode_index` itself. Per-element hot
   loops (`process_dense_node` / `process_way`) intentionally skipped.
2. ~~**Land #7** ahead of #1 to unblock planet iteration on 27 GB hosts.~~
   Done in `63800d3`. Pass 1.5 peak 20.3 GB → 1.75 GB on Germany.
3. ~~**Land #1 Phase 2a** — `mallopt` + parallel node scan.~~ Done in
   `88cf796`. Pass 2 scan 42 s → ~26 s on Germany; total wall 65.4 s →
   49.0 s.

**Next (in order of estimated ROI):**

4. **Item #1 Phase 2a follow-up — direct `coord_mmap` writes from workers.**
   The main-thread merge closure currently serialises 1.86 GB of
   coord-write traffic; moving writes into workers via disjoint rank
   ranges should recover ~5-7 s on Germany (~50-70 s at planet).
   Smaller scope than Phase 2b; land first.
5. **Item #1 Phase 2b — parallel way scan.** Per-worker tmp files plus
   offset-patched concatenation for `street_nodes.bin` /
   `interp_nodes.bin`, per-worker `StringPool` with sequential remap.
   Est. ~5 s on Germany (~80-100 s planet). Larger diff surface — land
   after the Phase 2a follow-up stabilises.
6. **Items #3 + #4 together** — Pass 3 stage B parallel + fine/coarse
   fusion. They touch the same rewrite surface in
   `bucketed_cell_assignment`; doing them in one pass avoids two
   rounds of diff churn. Est. ~4 s on Germany (~80-140 s planet).
7. **Item #5 + #6** — small cell-assignment parallelisations (addr
   points / interpolation / admin flood-fill). Low risk, est. ~1-2 s
   on Germany, larger at planet.
8. **Item #2 — Pass 1.5 wire-format scanner.** Pass 1.5 is down to 1.1 s
   on Germany after item #7, so the expected win is ≤0.5 s — low ROI
   vs implementation cost. Re-evaluate at planet scale once items #3-#6
   land; if planet Pass 1.5 stays > 20 s after the RSS fix, reconsider.
9. **Interpolation endpoint CSR** — RSS hygiene, not wall. Revisit once
   wall-time targets are met.

Cross-validation: there's no `brokkr verify` for the geocode index.
Byte-for-byte comparison of the output directory across builds
(`diff -r old_index/ new_index/`) is the ground truth *for Phase 2a*
(output order preserved by `parallel_classify_phase`'s reorder buffer).
Once Phase 2b ships per-worker-tmp-file concatenation, ordering within
non-ordered output streams (`street_ways.bin`, `addr_points.bin`) will
differ, so fall back to comparing the `Reader`'s query results on a
fixed sample of coordinates.

## Correctness invariants

- **Sorted + indexed PBF precondition.** Already enforced at entry via `require_indexdata`; the sorted-PBF node-before-way invariant is what makes Phase 2a/2b a clean barrier. Preserve.
- **Disjoint rank ranges across node blobs.** Phase 2a writes to `coord_mmap` concurrently without atomics; correctness depends on the per-blob `ref_rank_start` / `ref_rank_end` ranges being disjoint. `debug_assert!(rank < blob.ref_rank_end)` in Phase 2a's inner loop.
- **StringPool offset remapping.** Per-worker pool offsets are meaningless outside that worker; every `*_offset` field in concatenated tmp files must be rewritten during merge. Type-check this by making the per-worker offsets a distinct newtype (`WorkerStringOffset`) that is converted through the remap table into the final `StringOffset`.
- **`node_offset` offset patching.** `StreetWay::node_offset` / `InterpWay::node_offset` fields are worker-local byte offsets into per-worker `street_nodes.bin` / `interp_nodes.bin`; add worker prefix during concatenation. Same newtype trick.
- **Bucket-order cell_id monotonicity.** Pass 3 currently `debug_assert!`s cell_id monotonicity across buckets ([builder.rs:1473](../src/geocode_index/builder.rs#L1473)); preserve this check after parallelization by retaining the final-file concatenation step as the place where the assertion runs.
- **Zero-coord sentinel in way coord resolution.** `coords.filter_map(...)` at [builder.rs:254](../src/geocode_index/builder.rs#L254) drops `(0, 0)` as "missing". Phase 2b must preserve this filter. Unresolved refs (node not in `IdSetDense`) already return `None` from `rank_if_set`; `(0, 0)` after rank-indexed read also continues to drop. No change needed.

## Open questions

- ~~**Pass 2 RSS under `mallopt(M_ARENA_MAX, 2)`.**~~ **Resolved for
  Phase 2a.** Germany Pass 2 peak anon (post-Phase-2a, commit `88cf796`)
  measured 2.88 GB — much smaller than the 18-20 GB pre-landing
  projection. `mallopt(M_ARENA_MAX, 2)` plus the Phase 2a main-thread
  merge (which absorbs the cross-thread free traffic) keeps the arena
  bounded. Scaling linearly to planet: ~30 GB coord_mmap + way_geom +
  decode residency = still the governing number, but the arena is not
  a separate risk. Revisit this for Phase 2b once per-worker tmp-file
  residency lands.
- **Is the admin-level flood-fill cost uneven enough to matter?** If one polygon (e.g., Russia at admin level 10) dominates, `par_iter` gives only ~2× speedup in practice. Germany admin flood-fill (item #6 target) is 1.3 s at the development baseline — small enough that imbalance is probably invisible. Re-measure on planet after #6 lands.
- **StringPool merge order determinism.** For byte-for-byte cross-validation of the output against the sequential build, worker-local pool merge order must be deterministic (e.g. worker 0's strings before worker 1's before …). Only matters once Phase 2b lands (Phase 2a interns on the main thread, so StringPool ordering matches the sequential path).
