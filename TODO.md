# pbfhogg TODO

## Active optimization plans (high priority)

Four planet-scale command plans are in notes, each with a ranked set of opportunities and a plan of attack. Read the plan before touching the command.

- [x] ~~**[notes/altw-as-renumber.md](notes/altw-as-renumber.md)**~~ - **EXPERIMENT FAILED (2026-04-16).** Implemented as `src/commands/altw_v2.rs`, OOM-killed at Europe. Measured unique-referenced count was 3.6 B → 29 GB coord table (plan estimated 2 B / 16 GB at planet; real planet ~10 B / ~80 GB). The in-RAM-coord-table thesis is disproven for Europe+; the existing 4-stage external-sort shape is load-bearing and correct. See the plan document's top-of-file notice for the post-mortem. **Active ALTW work moves to** [notes/altw-structural-reports.md](notes/altw-structural-reports.md) **(specific-seam items).**

- [ ] **[notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md)** - `build-geocode-index`. Current: **1346 s / 14.59 GB RSS** at planet. Target: **~10-12 min / same RSS**. Single-threaded Pass 2 is the dominant phase; `mallopt(M_ARENA_MAX, 2)` + parallel node/way split unlocks it. Plus parallelizing Pass 3 stage B and fusing fine+coarse cell computation.

- [ ] **[notes/check-refs-opportunities.md](notes/check-refs-opportunities.md)** - `check --refs`. Current: **1254 s / 1.8 GB RSS** at planet. Target: **~6-10 min / ~2 GB RSS**. ~30-line diff: swap three `RoaringTreemap`s for `IdSetDense`. Profiling diagnosis already in the source; the author reached for the wrong container. Parallelization as a renumber-shaped three-phase scan is a secondary follow-up.

- [ ] **[notes/apply-changes-opportunities.md](notes/apply-changes-opportunities.md)** - `apply-changes --locations-on-ways`. Current: **762 s / 1.8 GB RSS** at planet under production `--compression none`. Target: **~9-10 min / same RSS**. Already mostly well-shaped; two incremental parallelizations remaining - `NodeLocationIndex::prefill_from_base` and the sequential reader thread. No reshape needed.

Measurement-first on every one: turn on `#[cfg(feature = "hotpath")]` counters (or add unconditional `*_ms` counters) to ground-truth the inferred per-phase breakdowns before committing to the order of landing items within a plan.

## Correctness concerns spotted while spelunking

Surfaced during the optimization-plan reviews above. Ordered by severity - first two look like real latent bugs, the rest are silent-fail edge cases and documentation debt.

- [ ] **Geocode Stage B silently truncates cell entries to 65535** - [builder.rs:1432](src/geocode_index/builder.rs#L1432), :1446, :1456 all do `let count = streets.len().min(u16::MAX as usize) as u16; street_out.write_all(&count.to_le_bytes())?; for e in &streets[..count as usize] { ... }`. If a single S2 cell accumulates more than 65535 street segments, addr points, or interp segments of one type, the excess is silently dropped - the on-disk count says 65535 and the reader reads exactly that many. Unlikely at street level 17 (~150 m cells) but conceivable at coarse level 14 (~1 km cells) in dense urban areas. Fix: bump the on-disk count field to `u32` (three bytes per cell of output growth, negligible), or emit a hard error when the cap is hit.

- [ ] **Geocode Pass 1.5 may violate the `parallel_classify_accumulate` contract** - [commands/mod.rs:568-569](src/commands/mod.rs#L568) says explicitly "NOT safe for dense paths - per-worker Vec or `IdSetDense` accumulation is unbounded for node/way classify." But [geocode_index/builder.rs:498](src/geocode_index/builder.rs#L498) uses it for **way classify with per-worker `IdSetDense`**, which is the exact case the comment forbids. The measured 14.59 GB peak RSS at planet suggests it doesn't OOM in practice, but either the comment is stale (and the usage is fine, documentation debt) or the usage is latent bug territory that will bite on a denser input. Needs audit: either migrate Pass 1.5 to `parallel_classify_phase` (per-blob merge, bounded) or update the contract comment. The fix folds cleanly into [notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md) since Pass 1.5 is already on the rewrite list there.

- [ ] **`(0, 0)` coord sentinel collides with Null Island** - both ALTW stage 2 and geocode Pass 2 use `(lat_e7 == 0, lon_e7 == 0)` as the unresolved-coordinate sentinel. A real OSM node at 0°, 0° (Null Island, off the African coast) exists - periodically created by broken edits, tests, and GPS-zero errors. Ways referencing such a node get that ref silently dropped: [geocode builder.rs:262](src/geocode_index/builder.rs#L262) `if lat == 0 && lon == 0 { None }` drops the ref entirely; [altw stage2.rs:501](src/commands/altw/stage2.rs#L501) `let is_resolved = lat != 0 || lon != 0;` counts it as unresolved. Both optimization plans inherit the pattern. Real fix would be a presence bitmap alongside the coord array - not worth it unless observed in production. Document as known limitation so it doesn't get rediscovered.

- [ ] **`IdSetDense::set_atomic` panics on indexdata max_id mismatch** - [id_set_dense.rs:85](src/commands/id_set_dense.rs#L85) does `self.chunks[cid].as_ref().expect("set_atomic: chunk not pre-allocated")`. Renumber pre-allocates to `pass1_schedule.last().map_or(0, |t| t.max_id)` - correct when indexdata accurately reports per-blob max_id. If a corrupted or mismatched indexdata header understates the actual max element id in the blob, pass 1 panics instead of returning a clean error. Affects `renumber_external` today; affects `check_refs` once the `RoaringTreemap` → `IdSetDense` swap in [notes/check-refs-opportunities.md](notes/check-refs-opportunities.md) lands. Robustness nit: either verify max_id during the schedule scan or replace the panic with a `Result` path that the worker can propagate.

- [ ] **`u16` truncation of way `node_count` in geocode** - [builder.rs:283, :331](src/geocode_index/builder.rs#L283) both do `let nc = coords.len().min(u16::MAX as usize) as u16;` while the write loop emits ALL coords. For ways with >65535 refs, the reader uses the truncated `node_count`, reading only the first 65535 coords. Subsequent ways' byte offsets are computed on the full length so the stream stays consistent - only the tail of this one way is silently dropped. OSM convention caps ways at 2000 refs in practice, so unreachable today. Noted as "defensive coding that silently fails if convention changes"; either switch to `u32` or emit a hard error on cap.

- [ ] **Interpolation way `(start_number = 0, end_number = 0)` is both unresolved-sentinel and valid input** - [builder.rs:289-290, :1012-1016](src/geocode_index/builder.rs#L289). If `resolve_interpolation_endpoints_mmap` fails to match either endpoint, both stay at 0. A reader distinguishing "unresolved interp way" from "interp way starting at house 0" sees identical bytes. Niche - "0" as house number is rare but exists in some regions. Document or add a separate boolean `resolved` field.

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` - it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`tests/geocode_index.rs` has 6 `#[ignore]` tests - they build a geocode index from the
Denmark PBF and query it. ~154s in release mode. Run with:

    cargo test --release --test geocode_index -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` - it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Investigate planet regressions (2026-04-17)

Overnight rerun on commit `28fd26c` (plantasjen, UUIDs below) measured
large deltas vs the README.md planet table.

| Row | README | New (`28fd26c`) | Δ | UUID |
|---|---|---|---|---|
| `tags-filter highway=primary` | 52s | 153.2s | +195% | `d71445a6` |
| `cat --type way` (raw passthrough) | 44s | **73.8s** | **+69%** | `127fdf1e` |
| `getid --invert` | 83s | 89.1s | +7% | `3edfefb5` |
| `cat` (indexdata generation) | 497s | 477.1s | -4% | `d7857f19` |
| `diff --format osc` (osc 4913, cache hit) | (no planet row) | 6245.2s (~104m) | n/a | `aaaf31ec` |

- [x] **`tags-filter highway=primary` is NOT a regression, workload changed.**
  Baseline `0e44e017` (commit `7818c0f`, 2026-03-29) used `-R
  w/highway=primary` (single-pass, `--omit-referenced`) → 52.5 s. The
  2026-04-17 rerun used default two-pass (includes referenced nodes) →
  153 s. Sidecar `d71445a6 --durations` confirms: PASS1 27.8 s, WAYDEPS
  29.4 s, PASS2 75.3 s - the WAYDEPS + PASS2-emit work is exactly what
  `-R` skips. README row label `tags-filter highway=primary` is
  ambiguous; the new number is the honest two-pass measurement. If we
  want both in README, label them `-R` vs default explicitly.

- [ ] **`cat --type way` planet regression - 54 s -> 74 s cold (+37%).**
  The README 43.7 s baseline was `--bench 3` (warm best-of-3); today's
  single-shot `--bench 1` at the same commit `573ef71` is 53.9 s cold
  (UUID `2a5c6c3b`). True cold-vs-cold regression is 54 s -> 74 s, not
  44 s -> 74 s.

  Narrowed bisect window: **`573ef71..a496e81`**. Probe at `a496e81`
  (UUID `a58333e3`) already measured 73.8 s - identical to current
  `28fd26c` - so the regression predates `a496e81` and is NOT in any of
  the write-path commits (`603385e`, `4ed7e52`, `9695ad5`, `b45b731`,
  etc.) that followed. The suspect window is 40-ish commits between
  `573ef71` (2026-03-30, 54 s) and `a496e81` (protohoggr 0.4.0 bump,
  74 s); the intermediate commits that pinned protohoggr 0.2.1/0.3.0
  should now build against crates.io since the path-dep was removed.
  Phase breakdown at current tip: CAT_SCAN_START 52.2 s + CAT_SCAN_END
  21.5 s, 95 GB read + 28 GB written.
- [ ] **Rerun `add-locations-to-ways --index-type external` planet** - the
  2026-04-17 overnight hit EMFILE (`rank-W13-047: Too many open files`)
  during stage-1 shard creation. Stage 1 opens `num_shard_workers *
  NUM_BUCKETS` (=256) files; the failing worker index 13 implies at
  least 14 workers × 256 = 3584 FDs. Shell `ulimit -n` is 524288 on this
  host, so either the pbfhogg process inherited a lower limit or the
  FD accounting is higher than expected. Verify `/proc/<pid>/limits`
  for the child and rerun. Row in README still reads 953s / 11m38s from
  the `3d977a0` baseline.
- [ ] **Add a planet `diff --format osc` row** somewhere - currently only
  the Denmark 73.1s value at `reference/performance.md:352` exists. The
  new 6245s / 104m planet number wants a home (likely alongside the
  other planet rows). Deferred until the regressions above are resolved
  so the table lands in one coherent update.

## Next up (2026-04-13)

- [ ] **Multi-extract way classify per-worker scratch** - line 868
  uses `|| ()` init, allocates `vec![Vec::new(); n]` per block.
  Node and relation phases already use per-worker state. Fix: change
  init to `|| vec![Vec::<i64>::new(); n]`, clear between blocks.
- [ ] **diff v3: non-overlapping block skip** - use indexdata min/max
  ID to skip decode for blocks entirely OldOnly or NewOnly (misaligned
  boundaries). Additive on shipped v1+v2. Low risk. Note:
  derive_changes must still decode OldOnly (needs element IDs for
  OSC XML delete output).
- [ ] **`--allow-missing` for apply-changes** - the single prerequisite
  for incremental extract (~10s vs 862s). Insert new elements that
  don't exist in the base PBF, then re-extract to filter to bbox.

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** - Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

## Cross-pipeline optimization

Cross-thread buffer retention is **solved** - `DecompressPool` (commit
`8f6999b`) recycles decompression buffers in the pipelined reader. The
remaining architectural concern is thread oversubscription (two concurrent
rayon pools: decode + batch processing), not retention.

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [notes/pipelined-reader-retention.md](notes/pipelined-reader-retention.md)
for the April 2026 audit. Sequential conversion was attempted for
getparents (commit `c912e4d`) and reverted - 4.7x regression on
Denmark (1400ms vs 300ms). Decompression dominates, not per-block
processing. **No remaining pipelined paths should be converted to
sequential.** Renumber converted separately (external join
architecture, not driven by retention/oversubscription).

## Milestone 1: Planet-safe production pipeline - COMPLETE

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks. The write path is the remaining frontier.

### Raw group passthrough

Raw frame passthrough is shipped for extract simple - the 3-phase barrier
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
does not apply - the win here is write-path throughput instead.
See [notes/raw-group-passthrough.md](notes/raw-group-passthrough.md).

Four per-group raw passthrough primitives are committed as scaffolding
for partial-match blobs (e.g., extract boundary blobs where some groups
match and some don't). Currently unused - blob-level passthrough handles
the common case. See `notes/raw-group-passthrough.md` "Infrastructure":
- `PrimitiveBlock::raw_group_bytes(index)` - raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` - raw StringTable bytes
- `PrimitiveBlock::block_scalars()` - granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs` - assemble
  PrimitiveBlock from raw components

### Write-path throughput

After raw group passthrough, `BlockBuilder` (`src/write/block_builder.rs`)
and `PbfWriter` (`src/write/writer.rs`) are the next bottleneck for commands
that must re-encode partial-match groups. Opportunities: SIMD varint encoding
in `src/write/wire.rs` (the write-side protobuf primitives), zlib compression
level tuning, and reducing per-element overhead in
`BlockBuilder::add_node/add_way/add_relation` (string table construction
is the hot path - FxHashMap lookup + Rc<str> alloc per unique string).
See [notes/SIMD.md](notes/SIMD.md) for the varint research.

**Zlib level tuning:** extremely low priority. Investigated multiple
times in the project's history with no actionable outcome. Default
level 6 matches osmium and is the right choice for interop. zstd is
better for internal pipelines but the production pipeline already
works. See [notes/zlib-level-tuning.md](notes/zlib-level-tuning.md).

**Zstd:1 vs zlib:6 for ALTW external** (measured 2026-04-14): for
pipelines that can opt out of osmium interop, `--compression zstd:1`
is a substantial wall win on the external join path. Europe ALTW
external: 419 s (zlib:6, UUID `f3c53a34`) → 379 s (zstd:1, UUID
`66e43a11`), **−40 s, −9.5 %**. Stage 4 wall drops 28 % (132 s →
95 s); `s4_send_ms` cumulative drops 81 % (270 s → 51 s) and
`s4_channel_high_water` falls far below capacity - confirming that
zlib compression throughput was the steady-state stage-4 ceiling
under the consumer-owned raw-passthrough pipeline. The wall win
comes entirely from relieving consumer/compression saturation
downstream of the decode workers, not from any change in the
encode/decode code path. Zstd is not safe as the library default
(osmium and most consumers still expect zlib-compressed blobs;
[wiki: PBF specifies zlib](https://wiki.openstreetmap.org/wiki/PBF_Format))
but the flag is right there for internal-pipeline users. Output
file size stays within a few percent of zlib:6 at zstd:1, so the
knob is pure wall/interop trade-off, not a size trade-off.

### Reviewer findings (2026-03-29)

**Do later:**

- [ ] **Tags-filter raw passthrough via lightweight ID scanner** - the
  `count_in_range >= blob_count` check was unsound (extraneous IDs from
  other blobs inflate count). The correct approach: a cheap wire-format
  ID-only scanner per blob that verifies every element ID is in the
  included set without full PrimitiveBlock decode. If all match, raw
  passthrough. Only worth implementing if broad filters (e.g.,
  `building=*`) are a common use case. Flagged by 3/6 reviewers.

- [ ] **`pread_execute` opens a new `Arc<File>` per call** - simple extract
  calls it 3 times for the same input file. Could share the file handle
  across phases. Minor (~1µs per open). Flagged by 1/10 reviewers.

- [ ] **Simple extract phase 3 relation classify is sequential** - "needs
  full PrimitiveBlock (member access)" comment at `extract.rs` ~line 1472.
  Could use `parallel_classify_phase` like complete/smart phase 3.
  Relations are ~2K blobs at Europe - small gain but inconsistent with
  other strategies. Flagged by 1/10 reviewers.

- [ ] **No `fadvise(DONTNEED)` after pread in `parallel_classify_phase`** -
  external join's stage 2 workers call fadvise per pread, classify
  workers don't. At Europe scale (~2 GB compressed) this is fine. At
  planet scale (~87 GB) could accumulate page cache. Low priority since
  current planet-scale paths don't use `parallel_classify_phase` for
  heavy scans. Flagged by 1/10 reviewers.

- [ ] **Simple extract node_scanner skips non-dense Node messages** -
  `node_scanner.rs` only parses DenseNodes (line 15, 43). On legacy
  PBFs with field-1 Node messages, `bbox_node_ids` would be incomplete,
  cascading into missing ways and relations. Not reachable in practice
  (all modern PBFs use DenseNodes). Flagged by 1/10 reviewers.

### Smaller items

- [ ] **getid include: pread skip for non-matching blobs** - the include
  path now skips decompression via ID-range filtering (planet 71.5s →
  32.5s), but still sequentially reads the entire file to check each
  blob's header. A header-only scan + pread of only matching blobs
  would reduce planet from 32.5s to under 1s (only 3-9 blobs need
  reading). Low priority - 32.5s is already fast for planet-scale.
- [ ] `tags_count.rs` parallel path - `parallel_classify_phase` with
  per-worker CountMap accumulation. Tag counting is order-independent,
  so the merge is straightforward. Would restore parallel decode for
  unfiltered `inspect tags` on planet. Low priority.
- [ ] ALTW dense pass 2 decode-all fallback (`write_output_decode_all` in
  `src/commands/add_locations_to_ways.rs` ~line 1045) - uses
  `into_blocks_pipelined` processing all blobs. Retention solved by
  DecompressPool. Only triggers with `--force` on non-indexed PBFs.
  Pipelined decode + par_iter justified (heaviest per-block work).
  See retention audit for details.

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

- [ ] **strip-4 verify failure** - `brokkr verify multi-extract --regions 5`
  on Denmark: strip-4 has 1 fewer node than sequential (41643 vs 41644).
  Passes with 3 and 4 regions. Only fails with 5 regions where strip
  boundaries fall at exact integer longitudes (8,9,10,11,12,13). Likely
  a floating-point rounding issue in brokkr's bbox strip generation,
  not a pbfhogg bug. Pre-existing since multi-extract shipped.

**v2 improvements:**
See [notes/multi-extract-optimization.md](notes/multi-extract-optimization.md)
for full analysis of 6 optimization opportunities.

- [x] **Parallel decode** - write phases converted from sequential
  BlobReader to pread-from-workers via `multi_extract_pread_write`.
  Workers decode blobs in parallel, classify against N regions, produce
  N × Vec<OwnedBlock>. Consumer routes to N sync-mode writers via
  ReorderBuffer. Denmark 5-region: 6.7s → 2.0s (3.4x). Japan 5-region:
  32.5s → 8.1s (4.0x). Single-pass now 2.7x faster than 5 sequential
  extracts at Japan scale (8.1s vs 22s).
- [ ] **Spatial index** - grid or R-tree over regions for O(1)
  per-element lookup instead of O(N). Required for 200+ regions where
  linear scan becomes the bottleneck. Simple grid (3600×1800 cells of
  0.1°, precompute overlapping regions per cell) is sufficient.
- [ ] **Complete/smart strategies** - per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [ ] **Raw passthrough** - infrastructure in place: `NodeBlobInfo`
  tracks per-region containment, `multi_extract_pread_write_nodes`
  handles passthrough via ReorderBuffer interleaving. Currently only
  fires when a blob is contained in ALL N regions (useful for N=1 or
  fully-overlapping regions). Per-region passthrough for disjoint
  strips needs hybrid decode+raw consumer path - decode once, write
  raw to contained regions, route elements to non-contained regions.

**Reviewer findings (2026-04-09):**

- [ ] **Raw passthrough unsafe for polygon regions** - `contained_in`
  is computed from each slot's bbox, not polygon geometry. For polygon
  or multipolygon extracts, "blob bbox contained in region bbox" does
  not prove every node is inside the polygon - can raw-copy
  out-of-polygon nodes. Pre-existing issue, not introduced by the
  allocation fixes. Flagged by sweep review (bugs/codex).
- [ ] **O(workers × regions) scaling for large N** - each worker
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

- [ ] Resolve or document known semantic differences in verify output.
  Three commands have known diffs: extract (relation inclusion criteria),
  diff (14-element version comparison), check-refs (occurrences vs unique).
  See `brokkr verify all` output and README cross-validation section.
- [ ] Auto-selection: `--index-type auto` exists (dense vs external).
  Extend to other decisions: sequential vs pread-from-workers based on
  available RAM and blob count; compression level based on output target;
  batch size based on core count. Config or heuristic, not manual flags.
- [ ] Migration guide from other tools - command mapping table, behavioral
  differences, indexdata workflow explanation. Build on existing
  `reference/osmium-parity.md`.
- [ ] **`renumber` - minor optimization (current: 194 s / 3m14s, planet).**
  Planet: 194 s, 3.3 GB peak anon, zero temp disk (commit `cb99106`).
  - [ ] **Varint encode lookup table.** 256-entry for single-byte varints
    in the reframe functions. Est. −2 to −3 s wall.
  - [ ] **Skip `way_id_set` if way rank derivable from schedule.** Sorted
    input means new way ID = `start_way_id + global_position`. Derive from
    schedule prefix sums instead of building a full IdSetDense. Saves ~160 MB.
  - [ ] **Finer stage 2d reframe breakdown.** Split `reframe_ms` into
    parse/lookup/encode/frame to identify which sub-step dominates.

- [ ] **`add-locations-to-ways --index-type external`.** Active backlog,
  dataset gates, prior-art / shelved-with-reasoning, and codebase patterns
  live in
  [`notes/altw-external-optimization-plan.md`](notes/altw-external-optimization-plan.md).
  Measurement history (prototype, integration measurement, Stage 6
  cleanup, NVMe-floor analysis that closed the structural-rearrangement
  family) is in
  [`notes/altw-optimization-history.md`](notes/altw-optimization-history.md).
  Latest baselines: planet `4f059b67` 867.7 s, Europe `ffdf5f69` 375.9 s.

  **Active backlog moved to the plan.** See
  [`notes/altw-external-optimization-plan.md`](notes/altw-external-optimization-plan.md)
  for ordered items, dataset gates, prior-art / shelved-with-reasoning,
  and the appendix of bench / verify / pattern references.

### Ecosystem

- [ ] crates.io version badge - `https://img.shields.io/crates/v/pbfhogg`
- [ ] docs.rs badge - `https://img.shields.io/docsrs/pbfhogg`
- [ ] CI status badge - `https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`
  (requires GitHub Actions CI workflow)
- [ ] Add GitHub Actions CI - clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline - build binaries on tag push, attach to GitHub release
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

- [ ] **Global allocator investigation** - jemalloc and mimalloc were
  previously benchmarked at <1% wall time difference on Denmark (483 MB)
  and removed as CLI features (they broke `--all-features` builds due to
  duplicate `#[global_allocator]` definitions). Re-investigate at planet
  scale where allocator behavior under cross-thread free patterns and
  high churn may differ. Meta/Facebook has restarted active jemalloc
  development - revisit `tikv-jemallocator` and `mimalloc` when the
  arena/scratch work is complete and the remaining alloc profile is
  clearer. Measure RSS and wall time on planet add-locations-to-ways,
  merge, and build-geocode-index.
    - **jemalloc 5.3.1 (released 2026-04)** - wait for `tikv-jemallocator`
      to tag a release pointing at 5.3.1, then rerun the bench.
      Specifically relevant to the pipelined reader's cross-thread free
      pattern (`src/read/pipeline.rs:70` - decode workers allocate
      `PrimitiveBlock`s dropped on the consumer thread, the exact reason
      the prior jemalloc bench only saved RSS and not wall time):
        - tcache for deallocation-only threads (most on-point)
        - locality-aware tcache GC (`experimental_tcache_gc`, default on)
        - `calloc_madvise_threshold`, `process_madvise_max_batch`,
          `tcache_ncached_max` for ~MB-sized block allocations
      Check tikv-jemallocator releases; when 5.3.1 lands, run planet read
      + ALTW external + merge.

- [ ] **1. Custom allocators (per-block arena)** - 4/6 reviewers ranked 1st.
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

- [x] **`scan_block_ids` / `scan_block_tags` groups Vec** - NOT FEASIBLE.
  `Vec<&[u8]>` borrows from function parameter `raw: &[u8]`, lifetime
  changes each call. Cannot pass scratch from outer scope. Typically
  1-3 entries - negligible allocation.

- [ ] **Geocode pass 3 stage A par_iter** - per-way `Vec::new()` inside
  `flat_map_iter` closure (`builder.rs` ~line 1226). Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** - 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work.

- [ ] **Borrowed XML writer Vec elimination** - `write_borrowed_way_xml`
  and `write_borrowed_relation_xml` in `elements_xml.rs` still collect
  refs and members into `Vec`s. Could use `.peekable()` like tags to
  iterate directly. Low priority (~8 refs/way, ~10 members/relation).

- [x] **2. Columnar batch processing** - shipped for extract node
  classification. `DenseNodeColumns` decodes IDs/lats/lons into
  contiguous arrays. `collect_matching_ids_multi_bbox` does single-pass
  N-region bbox test. Used in multi-extract and single-extract.
  Measured: multi-extract Japan node classify 1081ms → 748ms (-31%).
  See [notes/columnar-integration.md](notes/columnar-integration.md).

- [x] **Smart-extract planet memory blocker - CLOSED 2026-04-11, ship
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
  bbox isn't a real workload - use `cat` passthrough.

  See [notes/parallel-classify-regression.md](notes/parallel-classify-regression.md)
  for the full investigation history, mechanism analysis (cold-arena-page
  residency cascade), and the historical mitigation menu preserved
  as reviewer-context rather than outstanding work.

**Milestone B: vectorization (after columnar layout stabilizes)**

- [ ] **3. SIMD** - universal agreement: comes after columnar. Columnar
  now shipped for extract (single + multi-region). ASM inspection
  confirms LLVM does NOT autovectorize the bbox classify loop - the
  `push()` side effect prevents vectorization entirely.

  **Codegen finding:** explicit AVX2 intrinsics are the only path.
  The multi-bbox loop is a better SIMD target than single-bbox: N
  region tests per node amortizes setup (N=5 with AVX2 8-wide ≈ 1.6
  nodes of all 5 tests per vector op). Single-bbox is only 2.8% of
  total Europe extract time - not worth it alone.

  SIMD becomes worthwhile when:
  - The classify loop is a larger fraction of runtime (after write-path
    optimization makes classify the bottleneck)
  - Multiple consumers use columnar arrays (multi-region, polygon PIP)
  - Batch varint decode in protohoggr (different SIMD target, broader
    impact across all commands)

  Varint SIMD research (notes/SIMD.md) previously closed - scalar beats
  SIMD for individual LEB128 varints. Batch varint decode into contiguous
  arrays is a different problem (columnar enables this).

**Milestone C: hardware-level tuning (where perf counters justify it)**

- [ ] **4. Huge pages** - `MAP_HUGETLB` (2 MB pages) for large mmap'd
  structures. Dense ALTW index (128 GB virtual, ~16 GB touched): 4 KB
  pages cover 8 MB via TLB, 2 MB pages cover 4 GB. Geocode index mmap
  reader, external join temp files. 5-15% speedup for random-access
  patterns. Note: dense ALTW is deprecated at planet scale in favor of
  external join. Requires hugepage availability (`sysctl` config) or
  `madvise(MADV_HUGEPAGE)` for THP. Linux-only.

- [ ] **5. NUMA-aware memory placement** - last by unanimous agreement
  (6/6). Only matters on multi-socket servers. Current benchmark host
  (plantasjen) is single-socket. Pread-from-workers pattern already has
  natural NUMA affinity (thread-local allocations, first-touch policy).
  `set_mempolicy(MPOL_BIND)` / `mbind()` for explicit placement.
  Candidates: pipelined reader decode pool, dense ALTW index interleave,
  external join scatter buffers. 10-20% on dual-socket, 0% on
  single-socket. Requires per-host tuning and NUMA hardware to validate.

**Separate track (GPU, independent of milestones A-C):**

- [ ] **GPU-accelerated point-in-polygon for geocode builder** - Pass 2
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
- [ ] Incremental extract update (`extract --apply-changes` - base extract + OSC +
  region → updated extract without re-reading planet).
  Recommended: compose two existing commands - `apply-changes` on
  the region extract (with `--allow-missing` for new elements not in
  the base), then `extract` to re-filter to the bbox. ~10s vs 862s
  for the full-planet pipeline. Works for simple strategy immediately.
  Complete/smart strategies need planet access for newly referenced
  elements outside the bbox.
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
  See [notes/spatial-index-in-pbf.md](notes/spatial-index-in-pbf.md)
  and [notes/way-blob-bbox-speculation.md](notes/way-blob-bbox-speculation.md).
  Node blob header scan is already fast (~0.5s planet). Way blob spatial
  bboxes are limited by chronological ID ordering (~30% skip for Denmark,
  not 50-80%). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [x] Streaming pipeline composition - CLOSED, limited benefit.
  The codebase already does the most valuable composition (inline
  indexdata in all write paths). Multi-pass commands can't consume
  streams. See [notes/streaming-pipeline-composition.md](notes/streaming-pipeline-composition.md).
- [ ] Zstd as default compression for internal pipelines - extremely
  low priority. Investigated multiple times, production pipeline works.
- [ ] Dense ALTW compact rank-indexed array (same pattern as geocode builder -
  better locality on hosts where dense currently works, reviewers split 1/8).
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [ ] History-file support - decide in-scope or explicitly out-of-scope.

## Release prep

### Testing

See `reference/performance.md` for consolidated baselines.

- [ ] **Diff element_stream fallback path untested** - all test PBFs are
  indexed because `PbfWriter::write_primitive_block` unconditionally adds
  indexdata. The `diff_element_stream` fallback (non-indexed inputs) has
  no direct coverage. Needs a `write_test_pbf_non_indexed` helper that
  either strips indexdata post-write or uses `write_blob` directly.

- [ ] **Test fixture infrastructure** - current `write_test_pbf` /
  `write_test_pbf_sorted` helpers create minimal PBFs (1-3 elements per
  type, single block). Needed: (1) a sorted+indexed fixture generator
  for commands that require indexdata (merge, extract, diff, ALTW),
  (2) larger multi-block fixtures (~100 elements, 3-5 blocks) to exercise
  batch boundaries, blob classification, and passthrough coalescing,
  (3) a fixture with metadata (version, changeset, timestamp, uid, user)
  for CleanAttrs / time_filter / diff verbose testing.

- [ ] **Fuzz testing** - PBF parsing (`PrimitiveBlock::from_vec`), OSC
  parsing (`parse_osc_file`), and wire-format decoders (`Cursor`,
  `WireBlock`, `WireInfo`) accept untrusted input. `cargo-fuzz` targets
  for these entry points would catch panics, OOM, and logic errors on
  malformed data. Also fuzz the roundtrip path (write → read → compare).

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
- [ ] Audit pass fusion for `--add-referenced` / `--invert` flows - checked:
  cannot fuse (pass 2 needs complete dep_node_ids before deciding which nodes
  to emit). Two-pass structure is inherent to the data dependency.

**extract --smart** (verified - already optimized):
- [ ] Check for opportunities to reduce repeated full-file traversals in relation
  closure expansion. (Inherent to transitive closure - may not be reducible.)

**add-locations-to-ways** (verified - already optimized):
- [ ] Tag-first rejection in rewrite phase: ALTW processes all ways unconditionally
  (no tag-based filtering). Not applicable - every way gets location enrichment.
- [ ] Clone/allocation in batch processing: passthrough coalescing uses raw bytes,
  no cloning. Batch slot dispatch is enum-based. Already well optimized.

**check_refs** (verified - no action):
- Consumer-bound (RoaringTreemap insertions, decode workers idle at 1% CPU).
  Block-pipelined + skip_metadata would not reduce wall time.
- [ ] Re-evaluate if consumer bottleneck shifts after RoaringTreemap improvements.

**sort, cat** (no action):
- Already optimal - blob-level passthrough, single-pass, or need full metadata for output.
