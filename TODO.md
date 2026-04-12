# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`tests/geocode_index.rs` has 6 `#[ignore]` tests — they build a geocode index from the
Denmark PBF and query it. ~154s in release mode. Run with:

    cargo test --release --test geocode_index -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

- [x] **Extract sorted pass1 (`37b7c19`): benchmark and clean up.** Superseded
  by three-phase parallel pread classification in `collect_pass1_generic`.
  The old sequential BlobReader + batch-rayon-merge approach
  (`merge_way_batch_parallel`, `merge_relation_batch_parallel`, etc.) has been
  removed. `collect_pass1_generic` now uses `parallel_classify_phase` for each
  element type (nodes → ways → relations). Smart pass 2 (way dep scan) also
  parallelized via `parallel_classify_phase`. Japan complete: 19.7s → 4.4s
  (4.5x), smart: 24.3s → 5.2s (4.7x). All sub-issues (1-5) are moot — the
  batch helpers, Vec-per-block allocation, and `decode_threads(1)` tradeoff
  no longer exist in the new architecture.

- [x] **`merge --locations-on-ways`: parallelize Phase 2.5 blob scans** —
  Passthrough node blob decompression dispatched to rayon pool. At Denmark
  scale (883 blobs) the improvement is negligible (<5ms) since per-batch
  work is already small, but should help at planet scale with larger scan
  sets. Note: the 12,790 "needed from base" nodes that aren't found are
  untagged nodes dropped by ALTW — they don't exist in the base PBF. This
  is inherent to the LocationsOnWays workflow, not a bug.
  `build_from_diff` already correctly excludes deleted ways (they're removed
  from `way_index` by the OSC parser).

- [x] **Run Germany full profiling suite** (4.7 GB, ~496M elements, commit `1b10bfd`).
  Timing: inspect-tags 23.9s, check-refs 74.1s, merge zlib 6.2s, merge none 4.4s.
  Allocations: merge 293 MB net (17+ GB cumulative churn through rewrite pipeline).
  check-refs is single-threaded consumer bound (74s wall, 73s on one core).
  cat --type (zlib): 61.8s, 10.9 GB RSS, 240 GB cumulative alloc (175 MB net).
  Full results in `reference/performance.md`.

## ~~BlobReader fadvise: gate on `target_os = "linux"` instead of `linux-direct-io`~~ DONE

Done in commit `7acbb1a`. libc now non-optional, fadvise gated on `target_os = "linux"`.

## Cross-pipeline optimization

PrimitiveBlock cross-thread alloc/free retention affects every command using
the pipelined reader at 400K+ blocks (Europe/planet scale). The geocode builder
is the predicted next victim (16 GB DenseMmapIndex + 25 GB retention = OOM).

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [notes/pipelined-reader-retention.md](notes/pipelined-reader-retention.md)
for the April 2026 audit: 6 remaining paths, renumber and cat --type are
the production-relevant ones still using `into_blocks_pipelined`.

## ALTW external join — COMPLETE

Planet validated: **1,462s (24.4 min), 16.7 GB peak anon, 3.9x faster than dense.**
See [notes/altw-optimization-history.md](notes/altw-optimization-history.md).

## ALTW memory optimization — COMPLETE

External join ships as `--index-type external` (or `auto`).
Dense remains the "fast when RAM fits" path. See [notes/altw-optimization-history.md](notes/altw-optimization-history.md).

### Measured baselines (commit `69a127f`, plantasjen, 30 GB RAM + 8 GB swap)

| Dataset | Size | Elements | Time | Notes |
|---------|------|----------|------|-------|
| Europe | 33.6 GB | 4.2B (3.7B nodes, 454M ways, 8.2M rels) | 2565s (43m) | buffered, commit `69a127f` (no pass 0) |
| Europe | 33.6 GB | 4.2B | 2611s (43m) | `--direct-io` (+2%, no benefit), commit `69a127f` |
| Europe | 33.6 GB | 4.2B | 2631s (44m) | buffered, post `3677069` (with pass 0), +2.6% noise |
| Planet | 87.7 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) | 5773s (96m) | buffered, memory-latency-bound, commit `69a127f` |

## Milestone 1: Planet-safe production pipeline — COMPLETE

Every production step validated on 87 GB planet PBF on a 30 GB host:

| Step | Time | RSS |
|------|------|-----|
| cat (indexdata generation) | 497s (8.3 min) | minimal |
| add-locations-to-ways (external) | 1,462s (24.4 min) | 16.7 GB |
| build-geocode-index | 1,346s (22.4 min) | 17.8 GB |
| apply-changes (daily merge, zlib) | 762s (12.7 min) | 1.8 GB |

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks. The write path is the remaining frontier.

### Raw group passthrough

Raw frame passthrough is shipped for extract simple — the 3-phase barrier
pipeline classifies blobs in parallel and writes matching raw frames via
pread workers, bypassing decode+re-encode entirely. Simple extract now
beats osmium (4.4s vs 7.2s Japan, 100s vs 350s Europe sequential baseline).

Raw frame passthrough is now shipped for cat --type (matching blobs
written as raw compressed frames, planet 207s → 43s, 4.8x) and
getid --invert (blobs with no ID-range intersection pass through raw,
Denmark 1.9s → 0.5s, Japan 8.6s → 1.3s). getid include mode skips
decompression of non-intersecting blobs (planet 71.5s → 32.5s, 2.2x).

The remaining opportunity is extending raw passthrough to other
re-encoding commands: tags-filter, renumber, time-filter.
These still fully decode and re-encode via BlockBuilder.
For tags-filter: blobs where ALL elements match the tag expression
could be passed through raw (requires blob-level tag index check).
For renumber/time-filter: every element is modified, so raw passthrough
does not apply — the win here is write-path throughput instead.
See [notes/raw-group-passthrough.md](notes/raw-group-passthrough.md).

Four per-group raw passthrough primitives are committed as scaffolding
for partial-match blobs (e.g., extract boundary blobs where some groups
match and some don't). Currently unused — blob-level passthrough handles
the common case. See `notes/raw-group-passthrough.md` "Infrastructure":
- `PrimitiveBlock::raw_group_bytes(index)` — raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` — raw StringTable bytes
- `PrimitiveBlock::block_scalars()` — granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs` — assemble
  PrimitiveBlock from raw components

### Write-path throughput

After raw group passthrough, `BlockBuilder` (`src/write/block_builder.rs`)
and `PbfWriter` (`src/write/writer.rs`) are the next bottleneck for commands
that must re-encode partial-match groups. Opportunities: SIMD varint encoding
in `src/write/wire.rs` (the write-side protobuf primitives), zlib compression
level tuning, and reducing per-element overhead in
`BlockBuilder::add_node/add_way/add_relation` (string table construction
is the hot path — FxHashMap lookup + Rc<str> alloc per unique string).
See [notes/SIMD.md](notes/SIMD.md) for the varint research.

**Zlib level tuning:** extremely low priority. Investigated multiple
times in the project's history with no actionable outcome. Default
level 6 matches osmium and is the right choice for interop. zstd is
better for internal pipelines but the production pipeline already
works. See [notes/zlib-level-tuning.md](notes/zlib-level-tuning.md).

### Published benchmark matrix

Denmark/Japan/Europe/planet benchmarks for every command. Time, RSS,
temp disk, compression mode. Regression CI to prevent backsliding.

### Parallel classification for other commands

The parallel pread + lightweight scanner + send compact results pattern
from simple extract applies to any sequential collection pass:

- [x] **tags-filter two-pass pass 1** — parallel classification for pass 1.
  Europe: 363s → 39s pass 1. Closure+deps scans also parallelized
  (88s → parallel). Total: 363s → 107.5s (-70%). Pass 2 write 31s.
- [x] **extract complete/smart pass 1** — `collect_pass1_generic` in
  `src/commands/extract.rs` now uses three-phase parallel pread
  classification (nodes → ways → relations). Smart pass 2 (way dep
  scan) also parallelized via `parallel_classify_phase`. Japan:
  complete 19,701ms → 4,400ms (4.5x), smart 24,300ms → 5,200ms
  (4.7x). Verified via `brokkr verify extract` (all strategies pass).
- [x] **getid --add-referenced pass 1** — scans ways for ref collection.
  Converted to parallel pread classification via `parallel_classify_phase`.
  Workers scan way blobs for matching IDs and collect node refs.
  Verified via `brokkr verify getid-removeid`.

### Reviewer findings (2026-03-29, 10 reviewers across 5 archetypes)

**Do ASAP:**

- [x] **Simple extract node schedule missing spatial filter** — fixed:
  `BlobDesc` now stores the blob bbox, and the node_schedule partition
  applies the spatial bbox filter to skip node blobs outside the extract
  region. Flagged by 8/10 reviewers.

- [x] **`blob_index::scan_block_ids` collapses mixed-type blobs** — fixed:
  `scan_block_ids` now returns `None` when groups have different element
  types. Mixed-type blobs fall through to full decode in all fast paths.
  Flagged by 3/10 reviewers.

**Do soon:**

- [x] **Stats undercount for raw passthrough blobs** — fixed: extract
  passthrough updates `nodes_in_bbox` from indexdata count, getid
  --invert updates per-type stats from indexdata count. BlobDesc now
  stores `count` field. Flagged by 5/10 reviewers.

- [x] **`parallel_classify_phase` doc comment: merge order** — fixed:
  doc comment now states "merge is called in arbitrary worker-completion
  order, not blob file order." Flagged by 4/10 reviewers.

- [x] **Simple extract: non-indexed sorted blobs in all three schedules**
  — documented as intentional: non-indexed blobs must be in all three
  schedules because the type is unknown without decompression. Each
  phase's classify closure skips non-matching elements. Triple
  decompression is acceptable since this path is only reachable via
  `--force` on non-indexed PBFs. Flagged by 2/10 reviewers.

- [x] **`decompress_buf` not reused in `parallel_classify_phase`** — fixed:
  workers now use per-worker `DecompressPool` with `pool_get_pub` +
  `from_vec_pooled`. Buffer is returned to pool on PrimitiveBlock drop
  and reused next iteration. Eliminates ~780 GB cumulative alloc churn
  at Europe scale. Flagged by 8/10 reviewers.

**Do later:**

- [x] **Hybrid batching for pread workers** — CLOSED, not worth it.
  The ~8s regression claim was a stale estimate from pipelined reader
  vs pread worker comparison, not a measured mutex cost. Actual mutex
  overhead: ~50ms for 500K blobs (6 workers, ~100ns uncontended futex).
  Batch drain would save <1s on a 37s pass — not justified for the
  added complexity. crossbeam channels would be simpler but also
  no measurable benefit at current contention levels (~0.15% of CPU).
  Perf review (2 reviewers, 2026-04-09): unanimous close.
  See [notes/hybrid-batching-research.md](notes/hybrid-batching-research.md).

- [ ] **Tags-filter raw passthrough via lightweight ID scanner** — the
  `count_in_range >= blob_count` check was unsound (extraneous IDs from
  other blobs inflate count). The correct approach: a cheap wire-format
  ID-only scanner per blob that verifies every element ID is in the
  included set without full PrimitiveBlock decode. If all match, raw
  passthrough. Only worth implementing if broad filters (e.g.,
  `building=*`) are a common use case. Flagged by 3/6 reviewers.

- [x] **Duplicated consumer drain in tags-filter pass 2** — refactored
  into `drain_ready` closure. Extract's `pread_execute` still has the
  duplication (different stats type). Flagged by 1/6 reviewers.

- [ ] **`pread_execute` opens a new `Arc<File>` per call** — simple extract
  calls it 3 times for the same input file. Could share the file handle
  across phases. Minor (~1µs per open). Flagged by 1/10 reviewers.

- [ ] **Simple extract phase 3 relation classify is sequential** — "needs
  full PrimitiveBlock (member access)" comment at `extract.rs` ~line 1472.
  Could use `parallel_classify_phase` like complete/smart phase 3.
  Relations are ~2K blobs at Europe — small gain but inconsistent with
  other strategies. Flagged by 1/10 reviewers.

- [ ] **No `fadvise(DONTNEED)` after pread in `parallel_classify_phase`** —
  external join's stage 2 workers call fadvise per pread, classify
  workers don't. At Europe scale (~2 GB compressed) this is fine. At
  planet scale (~87 GB) could accumulate page cache. Low priority since
  current planet-scale paths don't use `parallel_classify_phase` for
  heavy scans. Flagged by 1/10 reviewers.

- [x] **Schedule-building boilerplate dedup** — `build_classify_schedule`
  in `commands/mod.rs` replaces 5 inline copies across getid, tags-filter,
  extract. Callers with custom filtering (spatial, tagdata) keep their
  own schedule builders. Flagged by 1/10 reviewers.

- [x] **tags-filter pass 1 blob-level tag index** — done in commit
  `b7ef585`. Pass 1 schedule builder uses `tagdata` filtering to skip
  blobs whose tag index provably lacks required tag keys. Flagged by
  2/10 reviewers.

- [x] **`collect_relation_member_closure` early return on empty set** —
  call site guarded by `has_included_relation` check. Skips schedule
  building + file open when no relations matched. Flagged by 1/10.

- [x] **`way_scanner` way_id parsing inconsistency** — fixed: uses
  `read_varint_i64()` consistent with canonical WireWay. Flagged by
  1/10 reviewers.

- [ ] **Simple extract node_scanner skips non-dense Node messages** —
  `node_scanner.rs` only parses DenseNodes (line 15, 43). On legacy
  PBFs with field-1 Node messages, `bbox_node_ids` would be incomplete,
  cascading into missing ways and relations. Not reachable in practice
  (all modern PBFs use DenseNodes). Flagged by 1/10 reviewers.

- [x] **Duplicate comment in extract.rs** — removed duplicate Pass 3
  comment. Flagged by 1/10.

### Smaller items

- [x] `merge --locations-on-ways` node scanner — already uses
  `extract_node_tuples` from `node_scanner.rs` with `par_iter` for
  parallel decompress+extract. No PrimitiveBlock construction.
- [x] `node_stats.rs` — converted from `for_each_pipelined` to sequential
  BlobReader with DecompressPool. Eliminates cross-thread retention.
  Diagnostic command — single-threaded decode is acceptable.
- [x] `getid::parse_ids_from_pbf` (`src/commands/getid.rs`) —
  converted to `parallel_classify_phase`, eliminating cross-thread
  PrimitiveBlock retention for `--id-file` PBF parsing.
- [x] **getid --invert raw frame passthrough** — blobs whose ID range
  has no intersection with requested IDs pass through as raw frames.
  Denmark 1.9s → 0.5s (3.8x), Japan 8.6s → 1.3s (6.6x).
- [x] **getid include ID-range blob skip** — skip decompression of
  blobs whose ID range doesn't intersect requested IDs. Planet
  71.5s → 32.5s (2.2x).
- [ ] **getid include: pread skip for non-matching blobs** — the include
  path now skips decompression via ID-range filtering (planet 71.5s →
  32.5s), but still sequentially reads the entire file to check each
  blob's header. A header-only scan + pread of only matching blobs
  would reduce planet from 32.5s to under 1s (only 3-9 blobs need
  reading). Low priority — 32.5s is already fast for planet-scale.
- [x] `tags_count.rs` — converted from pipelined reader + rayon batch to
  sequential BlobReader with DecompressPool. Removes rayon batch
  infrastructure (count_batch, merge_two_maps, merge_counts). Diagnostic
  command — single-threaded decode is acceptable.
- [ ] `tags_count.rs` parallel path — `parallel_classify_phase` with
  per-worker CountMap accumulation. Tag counting is order-independent,
  so the merge is straightforward. Would restore parallel decode for
  unfiltered `inspect tags` on planet. Low priority.
- [ ] ALTW dense pass 2 decode-all fallback (`write_output_decode_all` in
  `src/commands/add_locations_to_ways.rs` ~line 1045) — uses
  `into_blocks_pipelined` processing all blobs. 25+ GB retention at planet.
  Only triggers with `--force` on non-indexed PBFs. Niche but the last
  unmitigated retention path.
- [x] Extract relation classify parallelization — converted from sequential
  BlobReader to `parallel_classify_phase` via `build_classify_schedule`.
  Last sequential phase in simple extract eliminated.
- [x] **tags-filter closure + way dep scans** —
  `collect_relation_member_closure` and `collect_way_node_dependencies`
  converted to `parallel_classify_phase`. Closure uses collect-then-merge
  to avoid borrow conflict (workers read `included_relation_ids`,
  merge phase writes). Europe two-pass: 157.6s → 107.5s. Full journey
  from sequential: 366.7s → 107.5s (3.4x). Verified via `brokkr verify
  tags-filter`.

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Multi-extract

Single-pass multi-extract shipped for simple strategy on sorted input
(commit `542aad0`). Reads PBF once, classifies each element against N
regions, writes to N sync-mode PbfWriters. 3-phase barrier (nodes →
ways → relations) with per-region IdSetDense + BlockBuilder. Memory:
N × ~1.5 GB at planet scale. Falls back to sequential for unsorted
input or --clean. Verified via `brokkr verify multi-extract`.

**Known issues:**

- [ ] **strip-4 verify failure** — `brokkr verify multi-extract --regions 5`
  on Denmark: strip-4 has 1 fewer node than sequential (41643 vs 41644).
  Passes with 3 and 4 regions. Only fails with 5 regions where strip
  boundaries fall at exact integer longitudes (8,9,10,11,12,13). Likely
  a floating-point rounding issue in brokkr's bbox strip generation,
  not a pbfhogg bug. Pre-existing since multi-extract shipped.

**v2 improvements:**
See [notes/multi-extract-optimization.md](notes/multi-extract-optimization.md)
for full analysis of 6 optimization opportunities.

- [x] **Parallel decode** — write phases converted from sequential
  BlobReader to pread-from-workers via `multi_extract_pread_write`.
  Workers decode blobs in parallel, classify against N regions, produce
  N × Vec<OwnedBlock>. Consumer routes to N sync-mode writers via
  ReorderBuffer. Denmark 5-region: 6.7s → 2.0s (3.4x). Japan 5-region:
  32.5s → 8.1s (4.0x). Single-pass now 2.7x faster than 5 sequential
  extracts at Japan scale (8.1s vs 22s).
- [ ] **Spatial index** — grid or R-tree over regions for O(1)
  per-element lookup instead of O(N). Required for 200+ regions where
  linear scan becomes the bottleneck. Simple grid (3600×1800 cells of
  0.1°, precompute overlapping regions per cell) is sufficient.
- [ ] **Complete/smart strategies** — per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [ ] **Raw passthrough** — infrastructure in place: `NodeBlobInfo`
  tracks per-region containment, `multi_extract_pread_write_nodes`
  handles passthrough via ReorderBuffer interleaving. Currently only
  fires when a blob is contained in ALL N regions (useful for N=1 or
  fully-overlapping regions). Per-region passthrough for disjoint
  strips needs hybrid decode+raw consumer path — decode once, write
  raw to contained regions, route elements to non-contained regions.

**Reviewer findings (2026-04-09, 6 reviewers across 3 archetypes):**

- [x] **`std::mem::take` on worker output Vecs defeats capacity reuse** —
  fixed: `drain(..).collect()` preserves inner Vec capacity across
  worker loop iterations. Both `multi_extract_pread_write` and
  `multi_extract_pread_write_nodes`. Flagged by 4/6 reviewers.
  Sweep review note: `drain(..).collect()` still allocates a fresh
  destination Vec per handoff — a swap/pool approach would eliminate
  that too. Not worth it unless profiling shows handoff churn.
- [x] **Passthrough `frame_buf.clone()` for each of N writers** —
  fixed: `write_raw(&frame_buf)` borrows instead of cloning. Sync-mode
  writers just call `write_all` — zero heap copies. Both passthrough-only
  path and `write_consumer_item`. Flagged by 3/6 reviewers.
  Sweep review note: depends on sync-writer invariant — if multi-extract
  ever switches to pipelined `to_path` writers, `write_raw` falls back
  to `to_vec()` copy per writer (performance regression, not correctness).
- [x] **Per-closure `refs_buf` allocation** — fixed: `block_fn` signature
  extended with `&mut Vec<i64>` scratch parameter, allocated once per
  worker thread and reused across blobs. Way closure uses it for refs.
  `members_buf` in relation closure cannot be hoisted due to
  `MemberData<'a>` lifetime tied to PrimitiveBlock — remains per-blob
  (small: ~480 bytes). Flagged by 2/6 reviewers.
- [ ] **Raw passthrough unsafe for polygon regions** — `contained_in`
  is computed from each slot's bbox, not polygon geometry. For polygon
  or multipolygon extracts, "blob bbox contained in region bbox" does
  not prove every node is inside the polygon — can raw-copy
  out-of-polygon nodes. Pre-existing issue, not introduced by the
  allocation fixes. Flagged by sweep review (bugs/codex).
- [ ] **O(workers × regions) scaling for large N** — each worker
  allocates N BlockBuilders (~500 KB each). At N=50, ~200 MB across
  8 workers. At N=100+, ~400 MB. Monitor but acceptable for typical
  use (5-20 regions). Flagged by 2/6 reviewers.

### Export (GeoJSON/GeoPackage)

The bridge to the GIS ecosystem. Streaming PBF → GeoJSON/GeoJSONSeq
export. The pieces exist in the codebase:
- Reader: `ElementReader` for element iteration
- Geometry: `src/geo.rs` has point-in-polygon, ring assembly from way
  refs, Douglas-Peucker simplification
- Coordinates: `Way::node_locations()` from enriched PBFs (ALTW output),
  or inline coordinate resolution via the dense/external index
- Multipolygons: relation member assembly is in extract's smart strategy

The export command would iterate elements, resolve geometry (points for
nodes, linestrings for ways, polygons for multipolygon relations), and
write GeoJSON features to stdout or a file. Tag mapping (which tags
become GeoJSON properties) needs a configuration model.
See [notes/geojson-export-design.md](notes/geojson-export-design.md)
for the v1 design: GeoJSONSeq from ALTW-enriched PBFs, streaming
single-pass, tag expression and bbox filtering.

### Command surface

- [x] `inspect --show <id>` — display a single element by ID with all
  metadata, tags, refs, members. Uses blob-level indexdata to skip
  non-matching blobs, early exit on sorted PBFs. Accepts n<id>, w<id>,
  r<id>, or node/<id>, way/<id>, relation/<id>.
- [ ] Resolve or document known semantic differences in verify output.
  Three commands have known diffs: extract (relation inclusion criteria),
  diff (14-element version comparison), check-refs (occurrences vs unique).
  See `brokkr verify all` output and README cross-validation section.
- [ ] Auto-selection: `--index-type auto` exists (dense vs external).
  Extend to other decisions: sequential vs pread-from-workers based on
  available RAM and blob count; compression level based on output target;
  batch size based on core count. Config or heuristic, not manual flags.
- [ ] Migration guide from other tools — command mapping table, behavioral
  differences, indexdata workflow explanation. Build on existing
  `reference/osmium-parity.md`.
- [ ] **`renumber` external path — optimization roadmap (2026-04-11).** Six
  commits landed the external renumber implementation (pass 1 + stages
  2a-2d + relation R1/R2). First planet measurement on 2026-04-11
  (commit `e156e97`, UUID `c5d00c22`): **3,456 s (57.6 min)**, peak
  anon 2.79 GB, all element counts correct. That's 2.6× the design
  estimate of ~1,300 s. Memory is well under the 4 GB target; wall time
  is the outstanding issue.

  Reviewer brief sent to planet+perf+arch after the first measurement
  revised the optimization plan. Two new levers nobody had in my
  original 3-item list, plus honest revisions of the parallelization
  and radix-sort savings. See
  [notes/renumber-planet-scale.md](notes/renumber-planet-scale.md)
  "Optimization roadmap — reviewer consensus" for the full analysis;
  summarizing here for tracking.

  **Accepted target: ~24 min wall for single `--bench 1` run** (down
  from 57.6 min). Not the original 20-min framing — reviewers agreed
  20 min isn't reachable without output compression changes, and
  production uses `--compression none` anyway which is faster.
  24 min puts renumber in the same ballpark as ALTW external
  (24m22s) and build-geocode-index (22m26s) at planet scale.

  **Priority-ordered task list** (unanimous reviewer ordering):

  - [x] **Stage 2b radix sort** — commit `cc80442`. Replaced
    `sort_unstable_by_key(|p| p.old_node_id)` with LSD radix sort over
    5 × 8-bit passes (40 bits of u64 key coverage, headroom to 1 T
    node ids vs the ~13 B current max).
  - [x] **Halve the map-bucket record format** — commit `a478ae8`.
    Pass 1 and stage 2d now emit only the 8-byte `old_id`; the
    corresponding `new_id` is reconstructed at stage 2b / R2B read
    time as `start_id + cumulative_bucket_index`. Touches
    `load_old_id_bucket`, `stage2b_process_bucket`, the emission hot
    loops, and `compute_bucket_new_id_starts`.
  - [x] **Bucket-level parallelism in stage 2b** — commit `37ff902`.
    Two workers compete for source buckets via an atomic counter;
    each writes to its own slot-bucket shard (`slot-a` / `slot-b`)
    so stage 2c reads from both.
  - [x] **Shared worker-pool pattern**: schedule + pread + worker
    decode + per-worker `BlockBuilder` + per-worker output
    `Vec<OwnedBlock>` + `ReorderBuffer` on the main thread +
    `write_primitive_block_owned`. **Not `for_each_block_pipelined`**
    — the earlier attempt (f7033d3 / d656284 / 184cd5d, later
    reverted) OOMed at 26 GB anon RSS on planet via cross-thread
    `PrimitiveBlock` retention. The current pattern keeps
    PrimitiveBlocks entirely on worker threads and only transfers the
    bounded `Vec<OwnedBlock>` output via a bounded mpsc channel,
    matching `src/commands/external_join.rs` stage 4 which already
    runs planet-scale without OOM. Inlined per stage (pass 1, stage
    2a, stage 2d) rather than extracted — the three stages have
    subtly different per-blob outputs so a single helper would have
    needed at least three generic parameters.
  - [x] **Pass 1 parallel decode** — commit `8ec298c`. Two
    range-partitioned workers, each owning a `node_map` shard,
    emitting `Vec<OwnedBlock>` per blob. Range split preserves the
    per-shard sort invariant that stage 2b's merge-join depends on
    (shard A's old_ids are all disjoint from and less than shard B's,
    so shards read as a concatenated sorted run via
    `load_old_id_bucket_shards`).
  - [x] **Stage 2d parallel decode** — commit `34a6b7c`. Same range-
    partitioned two-worker pattern as pass 1; each worker owns a
    `way_map` shard. The shared `Arc<Mmap>` new_refs file and the
    shared `blob_slot_starts` Vec give workers the correct per-blob
    slot cursor regardless of dispatch order; the sequential path's
    per-blob drift check is preserved inside each worker.
  - [x] **Stage 2a parallel way scan** — commit `e7219f0`. Different
    shape because `scan_way_refs` takes raw decompressed bytes: N=6
    work-stealing workers decompress and scan in parallel, sending
    a compact `Vec<i64>` (ref old_node_ids in slot order) per blob
    through an mpsc. Main thread runs `ReorderBuffer` + single-
    threaded bucket emits + sidecar writes. Cross-thread budget:
    ~384 KB per Vec × 32 channel slots = ~12 MB in flight.
  - [x] **R2B radix sort** — superseded by IdSetDense rank. (mirror of stage 2b for relation member
    merge-join). Re-uses `stage2b_node_merge_join` via the existing
    slice API, so it already benefits from the stage 2b radix sort
    — technically this item is already done. Left open as a
    follow-up to wire the relation R2a emission into the parallel
    `stage2a_way_ref_pass`-shaped pattern if the planet profile
    shows it as a significant floor after the April 2026 rewrite.

  **Latest measurement: 960 s (16.0 min)** on commit `7839303`
  (2026-04-12). **−72% vs the 3,456 s baseline.** DenseNodes
  wire-format rewriter (pass 1), way splice rewriter (stage 2d),
  4-worker parallelism across all stages, parallel pwrite (stage 2c),
  radix 4 passes, batch bucket writes, schedule reuse, mallopt
  M_ARENA_MAX=2. Stage 2b (288 s, 30%) is the remaining #1 target.
  Peak anon 13.2 GB (stage 2b 4 workers).

  **Pass 1 deep-dive (round 3 reviewer consensus, 2026-04-12):**

  Instrumented pass 1 with per-phase counters (pread / decompress /
  parse / process / send) on workers and write timing on consumer.
  Five planet runs with different configurations:

  | Config | Wall | Process (cum) | Peak Anon |
  |---|---:|---:|---:|
  | 2w, add_node, ARENA=2 | 709 s | 1,049 s | 298 MB |
  | 4w, add_node, ARENA=2 | **416 s** | 1,174 s | 486 MB |
  | 4w, add_node_raw, no limit | 730 s | 2,291 s | 1,014 MB |
  | 4w, add_node_raw, ARENA=2 | 1,048 s | 3,617 s | 1,027 MB |
  | 4w, add_node, no limit | ~700 s | — | 26 GB (arena frag) |

  Key findings:
  - **`process` is the bottleneck** at 1,174 s cumulative = ~294 s/worker
    = 113 ns/node. Consumer write is only 16 s — massive headroom.
  - **4 workers scales well** (709→416 s, 1.7×). Consumer not the limiter.
  - **`add_node_raw` + `pre_seed_string_table` is a regression** — the
    per-block pre_seed overhead (Rc::from allocs + HashMap inserts)
    exceeds the per-node tag-lookup savings. With ARENA=2, the extra
    malloc contention makes it catastrophically slow (1,048 s).
  - **glibc arena fragmentation confirmed** as the 26 GB growth cause:
    OwnedBlock Vec<u8> allocated on pass1 worker, freed on rayon
    thread. MALLOC_ARENA_MAX=2 caps it at 486 MB.
  - **Planet-claude's cache analysis:** add_node touches ~12 memory
    regions per node, working set ~536 KB per block — barely fits L2
    (512 KB on Zen 3). L1 misses (~5 ns × 15 accesses × 10.4B) ≈
    780 s cumulative. Likely the dominant process cost.

  Priority-ordered pass 1 task list:

  - [x] **DenseNodes wire-format rewriter** — commit `dc13a7b`. (perf-codex
    + planet-claude, unanimous). Stop using BlockBuilder for pass 1. Renumber only
    changes node IDs — coords, tags, metadata, string table are
    preserved verbatim. New `reframe_dense_with_new_ids` function:
    (1) decompress blob → raw protobuf bytes, (2) parse DenseNodes
    wire format to locate packed ID field + extract old_ids for bucket
    emission, (3) generate new packed ID deltas (sequential renumber =
    delta of 1 = single varint byte `0x02` repeated for all but the
    first node), (4) copy lat/lon/keys_vals/denseinfo/string table raw
    bytes verbatim, (5) re-frame as PrimitiveBlock. Eliminates all 12
    dense arrays, string table HashMap, metadata construction, tag
    iteration. Per-node cost drops from ~113 ns to ~10-15 ns.
    Estimated process: 1,174 → ~200 s. ~200-300 LoC. Risk: blocks
    with non-default granularity/lat_offset/lon_offset must assert and
    fall back to full decode+re-encode. (planet-claude)
  - [x] **4 workers for pass 1.** Measured: 709→416 s (1.7×). Consumer
    headroom confirmed (16 s of 416 s wall). K-way merge in stage 2b
    generalizes cleanly to 4 shards. Done in dirty iteration, needs
    commit.
  - [x] **`mallopt(M_ARENA_MAX, 2)` inside `renumber_external()`** —
    commit `dc13a7b`.
    Scopes the arena limit to external renumber only. 1 LoC:
    `unsafe { libc::mallopt(libc::M_ARENA_MAX, 2); }`. Not a global
    env var — other commands are unaffected. (planet-claude)
  - [x] **Re-apply `from_vec_with_scratch`** — superseded by wire-format rewriters. In pass1_worker and
    stage2d_worker (committed as `bcd7cbc`, reverted during dirty
    iteration). Eliminates PrimitiveBlock::new .to_vec() copy.
  - [x] **Batch bucket writes per block.** Accumulate old_ids into a
    local `[u8; 64000]` stack buffer, flush once per block. Saves
    7999/8000 BufWriter calls per block. ~12 s wall. (planet-claude)
  - [x] **Per-block negative-id check via indexdata min_id.** If
    `min_id >= 0`, skip per-element `reject_negative_id`. ~3 s wall.
    (planet-claude)
  - [x] **Dense-node block-type fast path.** Superseded by rewriter. If block is DenseNodes,
    skip the `Element` match dispatch entirely. ~minor. (planet-codex,
    perf-codex)
  - [x] **Current-bucket fast path for old_id emission.** Superseded by IdSetDense::set. Node IDs are
    sorted, `node_id_bucket` is monotone. Track active bucket + end
    range, skip division for nodes in the same bucket. (perf-codex)
  - [ ] **`reframe_buf` recycling across blobs.** Both pass1_worker and
    stage2d_worker `mem::take` the reframe_buf into OwnedBlock each blob,
    losing capacity. A ping-pong pair or consumer→worker return channel
    would keep the buffer hot across 1.3M+ blobs. (perf-codex round 3+4)

  **Next-round optimization levers (round 2 reviewer consensus, 2026-04-12):**

  - [x] **4 workers for pass 1 / stage 2d.** Pass 1: measured at 416 s
    (4 workers, ARENA=2). Stage 2d: not yet measured with 4 workers.
    Consumer is not the limiter for either stage.
  - [x] **Parallel stage 2c.** Slot buckets map to disjoint output ranges
    in the flat new_refs file. Workers can preallocate via `ftruncate`
    and `pwrite` independent bucket ranges concurrently. ~10% of total
    wall (197 s), worth doing once the bigger wins are squeezed.
    (arch-codex, planet-claude)
  - [x] **Stage 2c/2d pipeline overlap.** Superseded — stage 2c eliminated by IdSetDense fusion. Stage 2d could start consuming
    buckets from the new_refs file while stage 2c is still scattering
    later buckets. Feasible because stage 2d reads slots sequentially
    and stage 2c writes by bucket (sequential within each bucket range).
    Complex to implement but would overlap ~197 s of stage 2c with
    stage 2d. (arch-claude)
  - [x] **Schedule reuse across stages.** Renumber rebuilds blob schedules
    in pass 1, stage 2a, stage 2d, and relation passes independently.
    Extract already solved this via `Pass1Result` plumbing — apply the
    same pattern to renumber. (perf-codex)
  - [ ] **`direct_io` flag honored in pread stages.** `stage2d_worker`
    and `relation_r1_r2a_fused` take `_direct_io` but open the input
    with plain `File::open` + `read_exact_at`. Should use O_DIRECT
    when the flag is set, for cache discipline on planet-scale hosts.
    (arch-codex)
  - [ ] **Check output disk target.** `brokkr env` shows `target=hdd`.
    At 88 GB / 100 MB/s sequential write = 880 s of pure write time —
    potentially the dominant bottleneck for pass 1 and stage 2d if
    compression throughput exceeds HDD write bandwidth. Moving output
    to NVMe (or tmpfs) and copying afterward is a zero-code-change
    lever worth ~300-500 s if confirmed. (planet-claude)

  **Smaller / defensive followups (non-blocking for planet bench):**

  - [x] **`fadvise(SEQUENTIAL)` before full bucket reads** — superseded, bucket reads eliminated. In
    `load_coo_bucket` / `load_single_old_id_bucket`. Small win on cold
    cache scenarios.
  - [x] **Sparse-file `new_refs` via `set_len` + `pwrite`** in stage 2c.
    Subsumed by the parallel stage 2c rewrite (ftruncate + pwrite +
    sparse holes for empty buckets).
  - [ ] **Add `scan_relation_members` fast-path** for R2a/R2d, analogous
    to `scan_way_refs`. Would avoid full PrimitiveBlock decode in the
    relation scans. Moderate win; not blocking planet correctness.
  - [ ] **`MADV_DONTNEED` on mmap'd `new_refs` files after stage2d/R2d
    completes** so the kernel evicts the working set pages before the
    next stage. Affects RSS reporting more than actual performance but
    improves the planet sidecar profile.
  - [ ] **Clean up stale comments** still describing the range-split /
    sorted-concat model in `renumber_external.rs` (around the pass 1
    arch comment block and stage 2d doc comments). Not a bug but
    confusing for future optimization work. (perf-codex)

  **Round 4 reviewer findings (2026-04-12, perf-codex + planet-claude):**

  - [x] **Hoist `group_ranges` / `scalar_fields` to worker scratch** — commit `67fafac`. In
    both reframe functions. Currently per-blob allocations (1.3M node
    blobs + 17K way blobs). Trivially reusable — `group_ranges` usually
    has 1 entry, `scalar_fields` ~20 bytes. (planet-claude, perf-codex)
  - [x] **Redundant radix pass in stage 2b** — commit `7839303`.
    Reduced from 5 to 4 passes. Within one bucket, ID range ≈ 55M <
    2^32, so byte 4 (bits 32-39) was constant = no-op shuffle.
    (perf-codex)
  - [x] **Fuse stage 2a + 2b via IdSetDense rank lookup** — commit `9ec5eda`. (unanimous
    perf + planet reviewer consensus, 2026-04-12). Replace the entire
    CooPair bucket → radix sort → merge-join pipeline with a single
    fused way scan that resolves refs inline via `IdSetDense::rank()`.
    Pass 1 builds a global bitset of all old node IDs (~1.6 GB) +
    rank prefix sums (~1 GB) = 2.6 GB. The fused scan replaces both
    stage2a_way_ref_pass and stage2b_node_merge_join: for each way
    ref, `new_id = start_node_id + rank(old_node_id)`. O(1) per ref
    via popcount (~10-20 ns). Deletes ~520 LoC, eliminates 128 GB
    CooPair temp disk + 166 GB node_map shard files. Estimated:
    stage 2a (119 s) + 2b (288 s) = 407 s → ~170 s fused. Total
    960 → ~683 s (11.4 min). IdSetDense with rank() already exists
    in `src/commands/id_set_dense.rs` from the geocode builder.
    (planet-claude, perf-codex)
  - [x] **Way_id_set for R2B** — commits `c5c0e08`, `ae45fd6`. Build a second `IdSetDense` during
    stage 2d (set all old_way_ids, build_rank_index). Replaces R2B's
    merge-join with O(1) rank lookup. Estimated R2B: 68 → ~10 s.
    Eliminates way_map shard bucket files. (planet-claude)
  - [ ] **`reframe_buf` recycling across blobs.** Both pass1_worker
    and stage2d_worker `mem::take` the reframe_buf into OwnedBlock
    each blob, losing capacity. Ping-pong pair or consumer→worker
    return channel. (perf-codex round 3+4)
  - [ ] **Consumer drain-rate instrumentation.** Measure time blocking
    on `rx.recv()` vs time in `write_primitive_block_owned`.
    Distinguishes worker-bound vs consumer-bound. (planet-claude)
  - [x] **Pre-compute ref deltas in stage 2c.** Superseded — stage 2c eliminated. Store deltas (not
    absolutes) in the flat `new_refs` file. Shifts 12.4B delta
    computations from stage 2d (per-blob, hot reframe loop) to stage 2c
    (per-slot, cold, I/O-bound). Would let the way rewriter skip
    delta-encoding entirely and copy pre-computed packed bytes. Complex
    but large savings if stage 2d reframe remains the bottleneck.
    (planet-claude)
  - [x] **Merge node_map directly from raw byte slices** — superseded by IdSetDense. In stage 2b
    instead of materializing `Vec<i64>` per shard. The streams are
    forward-only; a byte cursor per shard with inline varint-to-i64
    decode would eliminate the parse-into-Vec allocation. (perf-codex)
  - [ ] **Consumer drain-rate instrumentation.** Measure time spent
    blocking on `rx.recv()` vs time spent in `write_primitive_block_
    owned`. Distinguishes worker-bound vs consumer-bound pipelines.
    (planet-claude)
  - [ ] **Finer stage 2d reframe instrumentation.** Split `reframe_ms`
    into `way_parse_ms`, `ref_lookup_ms`, `ref_encode_ms`, `frame_ms`
    to identify which sub-step dominates after the splice optimization
    lands. (perf-codex)

  **Defensive asserts / hardening:**

  - [x] **`debug_assert!(node_map.is_sorted_by_key(|p| p.old_id))`** — superseded, node_map eliminated. In
    stage 2b after loading a node_map bucket. The merge-join relies on
    this invariant (emission order = sorted input node order within a
    bucket). Cheap in debug, zero in release.
  - [ ] **`relation_map.len()` upper-bound warning.** At planet we see
    ~14M relations; design doc targets `<4 GB` peak RSS. If OSM grows
    past ~50M relations, log a warning at R1 completion.
  - [ ] **Scratch dir concurrent-from-same-process collision risk.**
    `ScratchDir::new(parent, name)` uses only `parent + name + pid`,
    so two concurrent `renumber_external()` calls from the same process
    would share the same scratch path and clobber each other. Add a
    random/sequence suffix or include a per-call nonce. Not a problem
    today (CLI is one-shot), flagged for library users.

  **Ergonomics / architecture:**

  - [ ] **`BucketWriters::write_pair(&mut self, bucket, bytes)` helper**
    in `external_radix.rs` to hide the `.writers[b].as_mut()?.write_all`
    + `entry_counts[b] += 1` pattern used at 4+ call sites across pass
    1, stage 2a, stage 2d, relation R2a. Current direct field access
    was chosen for hot-loop clarity but the consolidation has no
    measurable cost and removes duplication.
  - [x] **Promote `CooPair` / `ResolvedEntry` to `external_radix.rs`** — superseded, most of stage 2b deleted. As
    generic `IdSlotPair<K>` / `ResolvedEntry<V>` **when a third caller
    appears.** Two callers (external_join, renumber_external) isn't
    enough to justify the abstraction cost; wait for the next external-
    bucket command (external sort, external dedupe, etc.) before
    unifying.
  - [ ] **`RenumberStats.orphan_refs_preserved: u64` counter.** Way refs
    and relation members whose `old_id` isn't in the corresponding map
    fall through with `resolved_id = old_id`, matching the in-memory
    path and osmium. Count them so the CLI summary can warn if many
    orphans leak old-ids into the new-id space. Non-zero orphan count
    on a self-contained planet extract probably indicates a malformed
    input.
  - [ ] **Document orphan-ref policy** in `renumber_external.rs` module
    docs: "orphan refs pass through with their old id, matching
    in-memory behavior and osmium's semantics. Consumers that assume
    new IDs are dense starting at `start_*_id` must tolerate mixed
    old/new id spaces in the output."

  **Test gaps:**

  - [ ] **Non-indexed input test.** All current test PBFs are indexed
    (written via `write_test_pbf_sorted` which emits indexdata). Add a
    test that strips indexdata from the input so stage 2a / stage 2d /
    R2a / R2d hit the full-decode fallback path.
  - [ ] **Non-dense `Element::Node` element path.** Current test helpers
    always use DenseNode via `BlockBuilder::add_node`. Pass 1's
    `Element::Node(n)` branch (non-dense) is only reachable via
    externally-produced PBFs. Either construct such an input or
    document that the branch is dead-ish outside real-world inputs.

- [ ] **`renumber` planet-scale refactor — design written, implementation pending.**
  Current `src/commands/renumber.rs` (153 LoC) is a single-pass in-memory
  implementation with three `FxHashMap<i64, i64>` mappings (`node_map`,
  `way_map`, `relation_map`). Planet memory math: node_map ~250 GB + way_map
  ~28 GB + relation_map ~340 MB = **~278 GB total** at 10.4B nodes / 1.17B
  ways / 14.1M relations × 24 bytes per hashbrown entry. OOM-kills within
  ~60 seconds of the pass 1 scan on a 32 GB host; not even safe on a 256 GB
  server.

  Recommended architecture: 3-pass external join modeled after
  `src/commands/external_join.rs` (the ALTW external-index code), with
  256-bucket radix partition of node_map/way_map tuple files on disk and
  in-memory `relation_map` (small enough to stay in RAM). Relation
  forward-reference handling via a deliberate two-pass relation phase.
  Estimated work: **~1.5-3 weeks** comparable to the ALTW external
  development arc. Temp disk footprint ~185 GB at planet scale. Expected
  planet wall time ~22 min, in the same ballpark as ALTW external
  (24 min) and build-geocode-index (22 min).

  Full design document with memory math, pass structure, prior-art analogy,
  gotchas, testing plan, and work breakdown:
  [notes/renumber-planet-scale.md](notes/renumber-planet-scale.md). Read
  that and `notes/altw-optimization-history.md` (the prior art) before
  implementing.

  Pre-implementation tasks (from the design doc's "Open questions" section):

  - [x] **Read libosmium's renumber source** — done 2026-04-11 via
    Opus Explore agent on `research/libosmium/` and
    `research/osmium-tool/`. Finding: osmium is in-memory-only
    (bespoke `id_map` class in `command_renumber.cpp`, sorted vector
    + unordered_map overflow, 8 bytes per ID floor), explicitly
    documented in the upstream manpage as "needs >32 GB RAM for
    planet." No external-join prior art to copy; our design is novel
    relative to the reference. Research also validated our two-pass
    relation handling (osmium does the same thing), our sorted-input
    requirement (osmium enforces the same), and our `--start-id`
    CLI surface (osmium has the identical three-tuple form plus a
    bonus negative-countdown mode we could match). Full findings in
    `notes/renumber-planet-scale.md` "Prior art: osmium renumber"
    section, including two bonus findings: osmium `apply-changes`
    does not blob-passthrough (confirms the ~15x pbfhogg speedup is
    structural), and osmium `derive-changes` has a real bug at
    `command_derive_changes.cpp:184` (raw int64 ID comparison without
    type discrimination — source of the "1243 missing deletes" in our
    README cross-validation section).
  - [x] **Extract `ScratchDir` / `BucketWriters` from
    `src/commands/external_join.rs`** — done 2026-04-11. Moved to
    `src/commands/external_radix.rs` (not `src/external_radix.rs` as the
    design doc proposed — kept it alongside the other `commands/` sibling
    modules). Extracted items: `ScratchDir` (now takes a `name` parameter
    so `external-join` and `renumber-external` get distinct scratch
    directories), `BucketWriters`, `NUM_BUCKETS`, `BUCKET_BUF_SIZE`,
    `advise_dontneed_file`. Fields are `pub(crate)` so both callers can
    use the same direct-field-access patterns as before. Left in
    `external_join.rs`: `CooPair`, `ResolvedEntry`, `load_coo_bucket_into`,
    and `MAX_NODE_ID` — these are ALTW-specific payload types, not shared
    scaffolding. `brokkr check` passes; `brokkr add-locations-to-ways
    --index-type external` runs end-to-end on Denmark (10.3s, no missing
    locations, matches baseline). Resolves design doc open question #2.

### Ecosystem

- [x] crates.io release (protohoggr + pbfhogg + pbfhogg-cli).
- [ ] CI with benchmark regression guard.
- [ ] API documentation for library consumers.
- [ ] PyO3 Python bindings (read/write API for the Python ecosystem).
- [ ] Packaged "planet on 32 GB" reference pipeline (documented, runnable).

### Non-traditional optimization research

Ordered by reviewer consensus (6 reviewers, 3 archetypes: perf, arch, planet).
The first three form a dependency chain. The last two are independent
hardware-level tuning. Investigate allocators and columnar together as
Milestone A, SIMD as Milestone B, huge pages and NUMA as Milestone C.

**Milestone A: data layout + allocation (investigate together)**

- [ ] **Global allocator investigation** — jemalloc and mimalloc were
  previously benchmarked at <1% wall time difference on Denmark (483 MB)
  and removed as CLI features (they broke `--all-features` builds due to
  duplicate `#[global_allocator]` definitions). Re-investigate at planet
  scale where allocator behavior under cross-thread free patterns and
  high churn may differ. Meta/Facebook has restarted active jemalloc
  development — revisit `tikv-jemallocator` and `mimalloc` when the
  arena/scratch work is complete and the remaining alloc profile is
  clearer. Measure RSS and wall time on planet add-locations-to-ways,
  merge, and build-geocode-index.

- [ ] **1. Custom allocators (per-block arena)** — 4/6 reviewers ranked 1st.
  See [notes/arena-allocator-research.md](notes/arena-allocator-research.md)
  for full landscape, alloc profiling data, and 5-step implementation plan.
  Key finding: `parse_and_inline` generates ~829 MB alloc churn (Japan) /
  ~14 GB (planet est.) from two temp `Vec<(u32, u32)>` per block. Step 1
  (thread-local scratch Vecs) eliminates ~97% of this with zero risk.
  Steps 2-5 escalate to bumpalo, columnar layout, pipelined reader
  re-enablement. Top crate candidates: `bumpalo` (v3.20, zero deps,
  stable), `bump-scope` (v2.2, scoped sub-allocations), or hand-rolled
  50-line bump allocator.

**Scratch buffer reuse audit (step 1 of arena research):**

`parse_and_inline` scratch is done (829 MB → 48 MB, -94%). The following
per-iteration allocations remain across the codebase, ordered by impact:

- [x] **`write_single_node/way/relation` tag Vec** — DONE. Iterator-based
  BlockBuilder API (commit `bb15e66`) eliminates the per-element
  `tags.collect::<Vec>()`. Callers pass `element.tags()` directly.
  Dual-buffer single-pass encoding for way/relation tag fields.
  See [notes/blockbuilder-iterator-api.md](notes/blockbuilder-iterator-api.md).

- [x] **Block-pair merge-join v2 (borrowed element merge)** —
  Japan diff: 86.4s → 52.9s (39% faster), 80.7 GB → 40.6 GB cumulative
  alloc (50% less). Commit `66990c3`, plantasjen. Borrowed element
  comparison via `&str` iterators from PrimitiveBlock string table —
  zero String allocation for the 98.8% Equal path. Remaining 24.1 GB
  is protobuf parsing overhead (`parse_and_inline_with_scratch`).
  See [notes/fill-buffer-optimization.md](notes/fill-buffer-optimization.md)
  and [notes/block-pair-merge-join-plan.md](notes/block-pair-merge-join-plan.md).
- [x] **Block-pair merge-join v1 (compressed byte comparison)** —
  skip decode entirely for matching blobs by comparing compressed bytes.
  Overlapping blob pairs with identical min_id/max_id/count AND
  compressed bytes emit `BlobEqual(count)` without decompression.
  Denmark diff: 20s → 10s (2x). Enabled for `diff --suppress-common`
  and `derive_changes` (always). Diff without `--suppress-common`
  falls through to element-level (needs per-element IDs for output).
- [x] **`stream_merge` metadata allocation waste** — resolved by v2.
  The block-pair path uses `element_version()` on borrowed elements,
  avoiding OwnedMetadata construction for the Equal path. Only changed
  elements (~1.2%) are materialized via `convert_node`/`convert_way`/
  `convert_relation`. Previous description: `convert_node`,
  `convert_way`, `convert_relation` in `stream_merge.rs` allocate
  `OwnedMetadata` for every element, but the equality checks
  (`nodes_equal`, `ways_equal`, `relations_equal`) don't compare
  metadata — only tags, coords, refs, members. Metadata is only used
  by `version()` for diff output formatting. For the 98.8% Equal
  path, metadata allocation is pure waste. Fix: defer metadata to
  `version_only` (already done in stream_merge, but `sort.rs`
  `read_dense_node`/`read_way`/`read_relation` still allocate full
  `OwnedMetadata` with timestamp/changeset/uid/user String).

- [x] **`element_merge_pair` return consumed counts** —
  `element_merge_pair` now returns `(old_consumed, new_consumed)`.
  `merge_decoded_pair` uses these directly instead of re-scanning
  via `count_elements_up_to` (removed). Flagged by 4/8 reviewers.

- [x] **`has_indexdata()` only checks first blob** — fixed: both
  `has_indexdata()` (mod.rs) and `check_sorted_and_indexed()` (diff.rs)
  now scan ALL data blob headers. Uses header-only reads with seeks
  (no decompression, no blob data I/O). Returns false if any data blob
  lacks indexdata, correctly falling back to the element-stream path
  for diff/derive_changes. Flagged by 2/8 reviewers.
  Sweep review note: `check_sorted_and_indexed` duplicates the
  index-scan logic from `has_indexdata` — mild maintenance drift risk.
  Could extract a shared helper if more callers appear.

- [x] **`diff` redundant header reads** — `check_sorted_and_indexed`
  reads sorted flag + indexdata from a single file open per input.
  Replaced 6 file opens with 2 in both `diff()` and `derive_changes()`.

- [x] **Pipelined reader `from_vec_pooled`** — converted to
  `from_vec_pooled_with_scratch` via `thread_local!` storage in
  rayon spawn closures. Scratch persists across blobs per thread.

- [x] **Remaining `PrimitiveBlock::new()` call sites** — all converted
  to `new_with_scratch` in commit `ea1ab6e`: check_refs, ALTW,
  stream_merge, geocode pass 2, cat fallback, getid workers.
  `new_with_scratch`. Mechanical.
  **Stale note:** `parse_primitive_block_from_bytes_owned` (used by
  merge classify workers at `merge.rs` ~line 1176 and ALTW fallback)
  still calls `PrimitiveBlock::new()` internally. These are rayon
  closures — would need `thread_local!` scratch. Low frequency
  (merge: only for diff-overlapping blobs, ALTW: `--force` only).

- [x] **cat/getid per-blob allocations inside loop** — hoisted
  decompress_buf, BlockBuilder, and output_blocks outside the per-blob
  loop in cat_type_passthrough and filter_by_id/filter_by_id_invert
  (commit `ea1ab6e`).

- [x] **Geocode pass 3 bucket merge** — hoisted 3 partition Vecs
  (streets, addrs, interps) outside while loop (commit `ea1ab6e`).

- [x] **Merge per-element tag Vecs** — all `osc.tags().collect()` in
  merge.rs eliminated by iterator API change (commit `bb15e66`).
  Callers pass `osc.tags()` directly.

- [x] **`scan_block_ids` / `scan_block_tags`** — same as above, not
  feasible due to lifetime constraints on `Vec<&[u8]>`. Negligible.

- [x] **`extract_node_tuples` / `scan_way_refs` group_starts** — converted
  to `&mut Vec<(usize, usize)>` scratch parameter. All callers updated
  across ALTW, external_join, merge, extract.

- [x] **`scan_block_ids` / `scan_block_tags` groups Vec** — NOT FEASIBLE.
  `Vec<&[u8]>` borrows from function parameter `raw: &[u8]`, lifetime
  changes each call. Cannot pass scratch from outer scope. Typically
  1-3 entries — negligible allocation.

- [ ] **Geocode pass 3 stage A par_iter** — per-way `Vec::new()` inside
  `flat_map_iter` closure (`builder.rs` ~line 1226). Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** — 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work.

- [ ] **Borrowed XML writer Vec elimination** — `write_borrowed_way_xml`
  and `write_borrowed_relation_xml` in `elements_xml.rs` still collect
  refs and members into `Vec`s. Could use `.peekable()` like tags to
  iterate directly. Low priority (~8 refs/way, ~10 members/relation).

- [x] **2. Columnar batch processing** — shipped for extract node
  classification. `DenseNodeColumns` decodes IDs/lats/lons into
  contiguous arrays. `collect_matching_ids_multi_bbox` does single-pass
  N-region bbox test. Used in multi-extract and single-extract.
  Measured: multi-extract Japan node classify 1081ms → 748ms (-31%).
  See [notes/columnar-integration.md](notes/columnar-integration.md).

- [x] **Smart-extract planet memory blocker — CLOSED 2026-04-11, ship
  as-is.** The 2026-04-10/11 investigation (4 reviewer rounds, 6
  commits) shipped a 29% wall improvement on Europe smart extract
  (254s → 181s) and also delivered complete −17% and simple −15% via
  the same `0b085b1` PASS1 schedule reuse. Planet measured on 2026-04-11
  at commit `cadc3e6`, UUID `2d028196`, plantasjen (32 GB, 27.9 GB
  avail), Europe bbox, `--bench 1` single sample: **279s wall / 11.17
  GB peak anon RSS.** The Europe×2.6 = 26-28 GB projection was wrong
  by ~2.4× because peak anon is dominated by PASS3 write work
  (bbox-sized), not PASS1 scanning the input file. Per the round-4
  decision tree, < 25 GB = ship as-is. The reusable packet pool,
  compact payload, malloc_trim-at-boundary, and bumpalo arena options
  from the round-4 mitigation menu are all **not needed** for this
  workload and have been closed out.

  Caveat: measured with Europe bbox. A substantially larger bbox
  (beyond continent scale) would grow PASS3's touched working set
  and could push peak anon higher. If extract-on-planet ever becomes
  a recurring operation for bboxes > Europe, re-measure. Whole-planet
  bbox isn't a real workload — use `cat` passthrough.

  See [notes/parallel-classify-regression.md](notes/parallel-classify-regression.md)
  for the full investigation history, mechanism analysis (cold-arena-page
  residency cascade), and the historical mitigation menu preserved
  as reviewer-context rather than outstanding work.

**Milestone B: vectorization (after columnar layout stabilizes)**

- [ ] **3. SIMD** — universal agreement: comes after columnar. Columnar
  now shipped for extract (single + multi-region). ASM inspection
  confirms LLVM does NOT autovectorize the bbox classify loop — the
  `push()` side effect prevents vectorization entirely.

  **Codegen finding:** explicit AVX2 intrinsics are the only path.
  The multi-bbox loop is a better SIMD target than single-bbox: N
  region tests per node amortizes setup (N=5 with AVX2 8-wide ≈ 1.6
  nodes of all 5 tests per vector op). Single-bbox is only 2.8% of
  total Europe extract time — not worth it alone.

  SIMD becomes worthwhile when:
  - The classify loop is a larger fraction of runtime (after write-path
    optimization makes classify the bottleneck)
  - Multiple consumers use columnar arrays (multi-region, polygon PIP)
  - Batch varint decode in protohoggr (different SIMD target, broader
    impact across all commands)

  Varint SIMD research (notes/SIMD.md) previously closed — scalar beats
  SIMD for individual LEB128 varints. Batch varint decode into contiguous
  arrays is a different problem (columnar enables this).

**Milestone C: hardware-level tuning (where perf counters justify it)**

- [ ] **4. Huge pages** — `MAP_HUGETLB` (2 MB pages) for large mmap'd
  structures. Dense ALTW index (128 GB virtual, ~16 GB touched): 4 KB
  pages cover 8 MB via TLB, 2 MB pages cover 4 GB. Geocode index mmap
  reader, external join temp files. 5-15% speedup for random-access
  patterns. Note: dense ALTW is deprecated at planet scale in favor of
  external join. Requires hugepage availability (`sysctl` config) or
  `madvise(MADV_HUGEPAGE)` for THP. Linux-only.

- [ ] **5. NUMA-aware memory placement** — last by unanimous agreement
  (6/6). Only matters on multi-socket servers. Current benchmark host
  (plantasjen) is single-socket. Pread-from-workers pattern already has
  natural NUMA affinity (thread-local allocations, first-touch policy).
  `set_mempolicy(MPOL_BIND)` / `mbind()` for explicit placement.
  Candidates: pipelined reader decode pool, dense ALTW index interleave,
  external join scatter buffers. 10-20% on dual-socket, 0% on
  single-socket. Requires per-host tuning and NUMA hardware to validate.

**Separate track (GPU, independent of milestones A-C):**

- [ ] **GPU-accelerated point-in-polygon for geocode builder** — Pass 2
  tests billions of nodes against admin boundary polygons. NVIDIA's
  cuSpatial has production-quality PIP (winding number, handles holes).
  Depends on columnar batch processing for efficient host-to-device
  transfer. Rust interop via `cudarc`. Feature-gate behind `cuda`.
  Planet: 2.5B nodes, polygon set ~100 MB. Only worthwhile at
  Europe/planet scale. No precedent in OSM tooling.

### Research / stretch ideas

- [ ] Incremental geocode index update (daily diff → index patch, no full rebuild).
  See [notes/incremental-geocode-index.md](notes/incremental-geocode-index.md)
  for 4 approaches analyzed. Recommended: v1 append-only delta index with
  query-time merge (simplest, no format changes), v2 S2 cell-level partial
  rebuild (better query perf, proportional to diff size).
- [ ] Incremental extract update (`extract --apply-changes` — base extract + OSC +
  region → updated extract without re-reading planet).
  See [notes/incremental-extract.md](notes/incremental-extract.md)
  for 4 approaches. Recommended: apply-changes on region extract +
  re-extract to filter (approach 3). ~10s vs 862s. Needs
  `--allow-missing` flag for apply-changes.
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
  See [notes/spatial-index-in-pbf.md](notes/spatial-index-in-pbf.md)
  and [notes/way-blob-bbox-speculation.md](notes/way-blob-bbox-speculation.md).
  Node blob header scan is already fast (~0.5s planet). Way blob spatial
  bboxes are limited by chronological ID ordering (~30% skip for Denmark,
  not 50-80%). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [x] Streaming pipeline composition — CLOSED, limited benefit.
  The codebase already does the most valuable composition (inline
  indexdata in all write paths). Multi-pass commands can't consume
  streams. See [notes/streaming-pipeline-composition.md](notes/streaming-pipeline-composition.md).
- [ ] Zstd as default compression for internal pipelines — extremely
  low priority. Investigated multiple times, production pipeline works.
- [ ] Dense ALTW compact rank-indexed array (same pattern as geocode builder —
  better locality on hosts where dense currently works, reviewers split 1/8).
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [ ] History-file support — decide in-scope or explicitly out-of-scope.

## Release prep

### crates.io blockers

- [x] **Publish `protohoggr` first** — published as 0.2.0. `version = "0.2"` added alongside path dep.
- [x] **Add `version` to CLI path dep** — `cli/Cargo.toml` has `version = "0.2"` on pbfhogg dep.
- [x] **Clarify license** — dual MIT/Apache-2.0. Cargo.toml, README, and LICENSE-MIT updated.

### Testing

See `notes/test-plan.md` for the full pre-release test matrix (feature permutations,
I/O modes, CLI commands) and `reference/performance.md` for consolidated baselines.

- [ ] **Diff element_stream fallback path untested** — all test PBFs are
  indexed because `PbfWriter::write_primitive_block` unconditionally adds
  indexdata. The `diff_element_stream` fallback (non-indexed inputs) has
  no direct coverage. Needs a `write_test_pbf_non_indexed` helper that
  either strips indexdata post-write or uses `write_blob` directly.

- [ ] **Test fixture infrastructure** — current `write_test_pbf` /
  `write_test_pbf_sorted` helpers create minimal PBFs (1-3 elements per
  type, single block). Needed: (1) a sorted+indexed fixture generator
  for commands that require indexdata (merge, extract, diff, ALTW),
  (2) larger multi-block fixtures (~100 elements, 3-5 blocks) to exercise
  batch boundaries, blob classification, and passthrough coalescing,
  (3) a fixture with metadata (version, changeset, timestamp, uid, user)
  for CleanAttrs / time_filter / diff verbose testing.

- [ ] **Fuzz testing** — PBF parsing (`PrimitiveBlock::from_vec`), OSC
  parsing (`parse_osc_file`), and wire-format decoders (`Cursor`,
  `WireBlock`, `WireInfo`) accept untrusted input. `cargo-fuzz` targets
  for these entry points would catch panics, OOM, and logic errors on
  malformed data. Also fuzz the roundtrip path (write → read → compare).

### Cross-validation known diffs

Three `brokkr verify` commands show known differences vs osmium. These are semantic
disagreements, not bugs — but should be investigated and either fixed or documented
before release.

- [x] **Planet-scale merge on 32 GB host** — **762s (12.7 min), 1.8 GB RSS.** 86% rewrite, 3.4M diff entries. Validated.
- [x] **`cat --type` planet validated** — Raw frame passthrough: 43.7s,
  no OOM, pure I/O-limited copy (commit `573ef71`, plantasjen). Previous
  decode+re-encode path OOM'd at 30 GB host; raw passthrough avoids
  decode entirely. Planet: 207s → 43s (4.8x).

### Cross-pipeline optimization audit (commit `398b1a4`)

Findings from code audit + outside review of transferring geocode builder
optimizations (block-pipelined + skip_metadata, tag-first classification,
FxHash, pass fusion, clone/alloc cleanup) to other commands.

**getid** (moderate impact, low risk):
- [x] Replace `dep_node_ids: BTreeSet<i64>` with `IdSetDense` in `getid_with_refs`.
  O(log n) → O(1) per node lookup. Also removed dead `strip_tags_ids` parameter.
  Commit `a704f5c`.
- [x] Use `elements_skip_metadata()` in `getid_with_refs` pass 1 and
  `parse_ids_from_pbf`. Commits `a704f5c`, `58e38d8`.
- [ ] Audit pass fusion for `--add-referenced` / `--invert` flows — checked:
  cannot fuse (pass 2 needs complete dep_node_ids before deciding which nodes
  to emit). Two-pass structure is inherent to the data dependency.

**merge** (low impact, low risk):
- [x] Use `elements_skip_metadata()` in `block_overlaps_diff`. Commit `b90e8ef`.

**extract --smart** (verified — already optimized):
- [x] Audit: no std HashMap/HashSet in hot paths. Uses IdSetDense throughout.
- [x] Verify: all classification passes use `elements_skip_metadata()` (confirmed:
  lines 1242, 1305, 1382, 723, 742, 752, 763, 1022, 1054, 1086).
- [ ] Check for opportunities to reduce repeated full-file traversals in relation
  closure expansion. (Inherent to transitive closure — may not be reducible.)

**tags_filter** (verified — already optimized):
- [x] Verified: tag-first classification in place. Way refs collected only after tag
  match (line 580). `elements_skip_metadata()` in all collection passes.
- [x] Audit: std HashSet only in cold-path expression parsing (line 28-29, once at
  startup). Not worth changing.

**add-locations-to-ways** (verified — already optimized):
- [x] Audit: `elements_skip_metadata()` used in all scan passes (lines 411, 839,
  859, 882, 1072). Only the write path (line 1129) uses `elements()` (correct —
  needs full metadata for output).
- [x] Audit: FxHashMap already used in all hot paths (lines 1028, 1035, 1066).
  IdSetDense for ID sets.
- [ ] Tag-first rejection in rewrite phase: ALTW processes all ways unconditionally
  (no tag-based filtering). Not applicable — every way gets location enrichment.
- [ ] Clone/allocation in batch processing: passthrough coalescing uses raw bytes,
  no cloning. Batch slot dispatch is enum-based. Already well optimized.

**inspect** (verified — already optimized):
- [x] `elements_skip_metadata()` in `--locations` without `--extended`: done.
  Also converted `scan_data_blob` to `new_with_scratch` for scratch buffer reuse.
  Index-only fast path already skips decompression for the common case.
- [x] Audit: `inspect tags` uses FxHashMap for counting (tags_count.rs). No std hash
  in hot paths.

**check_refs** (verified — no action):
- Consumer-bound (RoaringTreemap insertions, decode workers idle at 1% CPU).
  Block-pipelined + skip_metadata would not reduce wall time.
- [x] Audit: uses RoaringTreemap for all ID sets (optimal). No std hash in hot paths.
- [ ] Re-evaluate if consumer bottleneck shifts after RoaringTreemap improvements.

**sort, cat** (no action):
- Already optimal — blob-level passthrough, single-pass, or need full metadata for output.

### Geocode index builder — COMPLETE

Planet validated: **1,346s (22.4 min), 14.6 GB anon, 17.8 GB RSS.**
Europe: 568s (9.5 min), 7.5 GB RSS. O_DIRECT is 8% slower (page cache
prefetch helps sequential reads). Sidecar `6887288a`.

### README badges (after publishing)

- [ ] crates.io version badge — `https://img.shields.io/crates/v/pbfhogg`
- [ ] docs.rs badge — `https://img.shields.io/docsrs/pbfhogg`
- [ ] CI status badge — `https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`
  (requires GitHub Actions CI workflow)

### Other

- [x] Add LICENSE-APACHE copyright header — addressed by dual MIT/Apache-2.0 licensing
- [x] Add a CHANGELOG.md before first tagged release
## Post-v0.1 review — COMPLETE

All 8 priorities resolved (2026-04-10). Priorities 1-6 done, 7-8 skipped
by reviewer consensus. Remaining low-priority items (members_scratch,
borrowed XML writer Vec elimination) moved to scratch buffer audit above.

- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

