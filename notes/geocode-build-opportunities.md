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

No internal API needs rewriting. `IdSetDense`, `PrimitiveBlock`, `parallel_classify_accumulate`, the mmap'd coord index, and the output file shapes all stay exactly as they are.

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

Pass 2's choice at [builder.rs:606-611](../src/geocode_index/builder.rs#L606) describes the same arena fragmentation phenomenon and picks the wrong remedy. The sequential decode bound was never a correctness constraint - it was an RSS constraint, and renumber's `mallopt` answers it directly.

**With `M_ARENA_MAX = 2` set, Pass 2 can be parallelized exactly like renumber's pass 1.**

## Opportunities, ranked

### #1 - Parallelize Pass 2 (by far the biggest)

Enable via `mallopt(M_ARENA_MAX, 2)` at the top of [`build_geocode_index`](../src/geocode_index/builder.rs#L363). Same scope and placement as renumber.

Split Pass 2 into two parallel sub-phases. Sorted PBF (`Sort.Type_then_ID`) guarantees all node blobs precede all way blobs, so the phase barrier is clean.

**Phase 2a - parallel node scan.** Pattern: [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615).
- Work-stealing dispatch over node blobs via `AtomicUsize::fetch_add` on a node-only schedule (build from the existing [`build_classify_schedule`](../src/commands/mod.rs#L429) or equivalent, filter to `ElemKind::Node`).
- Pre-compute each node blob's `[ref_rank_start, ref_rank_end)` from indexdata `(min_id, max_id)` + `referenced_nodes.count_below` - same machinery as ALTW's [`build_node_blob_mapping`](../src/commands/altw/stage1.rs#L249). These rank ranges are disjoint by node-sort monotonicity.
- Each worker: pread → decompress → `PrimitiveBlock`. For tuples in sort order, use **counter-based ranking** (`rank = ref_rank_start` at blob entry, increment on `get(id)` hits) to write `coord_mmap[rank*8 ..]` without per-tuple `rank()` calls. Saves ~4 B rank operations at planet.
- Addr-tagged nodes: emit `AddrPoint` to a **per-worker tmp addr_points slice**.
- No synchronization on `coord_mmap`: workers write disjoint rank ranges.

**Phase 2b - parallel way scan.** Pattern: [`stage2d_worker`](../src/commands/renumber_external.rs#L418).
- Work-stealing dispatch over way blobs.
- Each worker: pread → decompress → `PrimitiveBlock`. `coord_mmap` is read-only now; `referenced_nodes.rank_if_set(nid)` per ref resolves coords.
- Emits to **per-worker tmp slices** of `street_ways.bin`, `street_nodes.bin`, `addr_points.bin` (building centroids), `interp_nodes.bin`, `interp_ways` metadata, and `way_geom` entries for admin.

**StringPool.** Each worker holds its own `StringPool` with a worker-local offset space. After join, **sequential merge** into a single final pool and **remap** `name_offset` / `street_offset` / `housenumber_offset` / `postcode_offset` fields in the concatenated tmp files via a `Vec<u32>` per worker mapping worker-local → global offsets. Single pass per record stream. No per-intern mutex.

**Output concatenation.** Per-worker tmp files concatenated in worker order. `street_ways.bin` and `interp_ways.bin` carry `node_offset: u64` fields - rewrite those records once during concatenation, adding each worker's prefix offset into `street_nodes.bin` / `interp_nodes.bin`. One sequential pass per way-record stream.

**Expected win: ~500 s at planet.** Converts the dominant phase from single-core to six-core decompression + decode + writes.

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

### #7 - Shared atomic IdSetDense in Pass 1.5

Pass 1.5 currently uses **per-worker `IdSetDense` accumulation** - each worker holds up to ~1 GB of bitmap chunks during the phase, ~5 GB transient across 6 workers, then merges into a single ~1.5 GB `referenced_nodes`.

Switch to the pattern renumber uses at [renumber_external.rs:166-179](../src/commands/renumber_external.rs#L166): one shared pre-allocated `IdSetDense`, populated concurrently via `set_atomic`. Drops the per-worker residency entirely.

**Expected win: not wall; large transient RSS during Pass 1.5.** Originally framed as "~5 GB transient" against a believed-14.59 GB whole-build peak. With brokkr now reporting short-emitting phases, the true Pass 1.5 peak on planet is **29.5 GB** - this item is **load-bearing for 30 GB hosts**, not optional.

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

1. **Add per-phase `*_ms` counters** (unconditional - not `cfg(feature = "hotpath")`) to ground-truth the wall breakdown assumed above. Measure current planet once to fix the baseline.
2. **Land #1 first** - `mallopt` + Phase 2a/2b parallelization. This is most of the win. Cross-validate the resulting index byte-for-byte against current `main` on Denmark and Europe; re-run planet to measure.
3. **Land #3 + #4 together** - Pass 3 stage B parallel + fine/coarse fusion. They touch the same rewrite surface in `bucketed_cell_assignment`; doing them in one pass avoids two rounds of diff churn.
4. **Land #2** - Pass 1.5 wire-format scanner. Independent; can be interleaved with #3/#4 depending on ergonomics.
5. **Land #5 + #6** - small cell-assignment parallelizations. Low risk.
6. **Revisit #7 and interpolation endpoint CSR** once wall-time targets are met; these are RSS hygiene.

Cross-validation: there's no `brokkr verify` for the geocode index. Byte-for-byte comparison of the output directory across builds (`diff -r old_index/ new_index/`) is the ground truth; ordering within non-ordered output streams (`street_ways.bin`, `addr_points.bin`) may differ after parallelization, so fall back to comparing the `Reader`'s query results on a fixed sample of coordinates.

## Correctness invariants

- **Sorted + indexed PBF precondition.** Already enforced at entry via `require_indexdata`; the sorted-PBF node-before-way invariant is what makes Phase 2a/2b a clean barrier. Preserve.
- **Disjoint rank ranges across node blobs.** Phase 2a writes to `coord_mmap` concurrently without atomics; correctness depends on the per-blob `ref_rank_start` / `ref_rank_end` ranges being disjoint. `debug_assert!(rank < blob.ref_rank_end)` in Phase 2a's inner loop.
- **StringPool offset remapping.** Per-worker pool offsets are meaningless outside that worker; every `*_offset` field in concatenated tmp files must be rewritten during merge. Type-check this by making the per-worker offsets a distinct newtype (`WorkerStringOffset`) that is converted through the remap table into the final `StringOffset`.
- **`node_offset` offset patching.** `StreetWay::node_offset` / `InterpWay::node_offset` fields are worker-local byte offsets into per-worker `street_nodes.bin` / `interp_nodes.bin`; add worker prefix during concatenation. Same newtype trick.
- **Bucket-order cell_id monotonicity.** Pass 3 currently `debug_assert!`s cell_id monotonicity across buckets ([builder.rs:1473](../src/geocode_index/builder.rs#L1473)); preserve this check after parallelization by retaining the final-file concatenation step as the place where the assertion runs.
- **Zero-coord sentinel in way coord resolution.** `coords.filter_map(...)` at [builder.rs:254](../src/geocode_index/builder.rs#L254) drops `(0, 0)` as "missing". Phase 2b must preserve this filter. Unresolved refs (node not in `IdSetDense`) already return `None` from `rank_if_set`; `(0, 0)` after rank-indexed read also continues to drop. No change needed.

## Open questions

- **Pass 2 RSS under `mallopt(M_ARENA_MAX, 2)`.** Renumber demonstrates this fits comfortably in renumber's 3.3 GB peak. Geocode has an additional ~16 GB `coord_mmap` live across Phase 2a/2b and a `way_geom` hashmap that grows through Phase 2b. Expect peak ~18-20 GB during Phase 2b - still under 30 GB with margin, but **measure `referenced_count` on Europe first** before assuming this holds at planet. The ALTW-as-renumber reshape (2026-04-16) assumed a similar sizing for *unfiltered* referenced nodes and OOM'd at Europe because the real count was 4-5× the estimate. Geocode's filter keeps its count smaller, but the same measurement discipline applies.
- **Is the admin-level flood-fill cost uneven enough to matter?** If one polygon (e.g., Russia at admin level 10) dominates, `par_iter` gives only ~2× speedup in practice. If it's measurable and binding, split large-polygon flood-fill into cell-stripe sub-tasks. Leave this decision until after measurement.
- **StringPool merge order determinism.** For byte-for-byte cross-validation of the output against the sequential build, worker-local pool merge order must be deterministic (e.g. worker 0's strings before worker 1's before …). This is a merge-phase detail; specify it up front.
