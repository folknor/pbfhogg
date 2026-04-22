# pbfhogg TODO

## Active optimization plans (high priority)

Five planet-scale command plans are in notes, each with a ranked set of opportunities and a plan of attack. Read the plan before touching the command.

- [ ] **[notes/merge-changes.md](notes/merge-changes.md)** - `merge-changes` (squash N OSCs → 1). **Unmeasured at planet scale.** Two serial-across-inputs shapes (`merge_changes::write_streaming` at CLI level, `osc::load_all_diffs` at library level) parallelize cleanly via `IdSetDense::set_atomic_if_new` newer-wins dedupe; estimated ~20-30 s saved at 7-OSC planet per reviewer Q7 speculation, unconfirmed until a baseline exists. No win at 1-OSC scale. Prerequisites before shipping: (1) run `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench 1` to store a wall + RSS baseline in `.brokkr/results.db`; (2) add per-input `MERGE_CHANGES_PARSE_START/END` markers so parallel-parse can be compared against the per-OSC share of serial wall; (3) confirm `load_all_diffs` call-site scope (only `merge-changes` + `apply-changes` today?). Content factored out of `apply-changes-opportunities.md` 2026-04-21 - these items were filed under "weekly apply-changes" but apply scale-independently to any consumer that squashes N > 1 OSCs.

- [x] ~~**altw-as-renumber (in-RAM coord-table thesis)**~~ - **EXPERIMENT FAILED (2026-04-16).** Implemented as `src/commands/altw_v2.rs`, OOM-killed at Europe. Measured unique-referenced count was 3.6 B → 29 GB coord table (plan estimated 2 B / 16 GB at planet; real planet ~10 B / ~80 GB). The in-RAM-coord-table thesis is disproven for Europe+; the existing 4-stage external-sort shape is load-bearing and correct. Post-mortem and numbers now live in [notes/altw-optimization-history.md](notes/altw-optimization-history.md). **Active ALTW work moves to** [notes/altw-external.md](notes/altw-external.md) **(live leads).**

- [x] ~~**[notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md)**~~ - `build-geocode-index`. **ARC LANDED 2026-04-18.** Planet 1,255 s (20.9 min, TAINTED baseline) -> **432.9 s (7m12s)**, -65 % / 2.9x. Pass 1.5 peak anon 29.5 GB -> 3.0 GB (-90 %); governing peak migrated to Pass 3 Stage B at ~25 GB, comfortable on 27 GB hosts. All 10 ranked items (#1 Phase 2a+2b, #2, #3, #4, #5, #6, #7, #8, plus header-walk consolidation and direct coord_mmap writes) shipped. Remaining follow-ups in the note: #4 "needs another pass" (fused Stage A delivered only 2.8 s at Europe vs the 40-60 s planet prediction), Pass 2 interp resolve still sequential at 30.6 s planet, interpolation endpoint CSR for RSS hygiene.

- [x] ~~**check --refs**~~ - landed 2026-04-17 across commits `8f0ccbb` (step #1: `RoaringTreemap` → `IdSetDense`), `053def6` + `fbf591c` (step #2: three-phase parallel scan + one-pass schedule walk). Japan 56.7 s → **2.1 s** (27×). Europe 426.2 s → **33.6 s** (12.7×). Planet **1225 s → 72.5 s** (16.9×, UUID `862547e4`), ~5-8× better than the 6-10 min plan floor. Peak RSS 2.17 GB. Step #3 (selective wire-format parser) was predicated on decompression and parse landing roughly co-equal; actual post-parallel split is ~162 s decompress vs ~2 s parse at Europe, putting the selective-parser ceiling at fractions of a second - so the next lever for check-refs perf is decompression throughput (zstd, io_uring, direct I/O), not selective parse. Load-bearing pin in `src/commands/check/refs.rs::check_refs` doc comment. Plan doc retired.

- [ ] **[notes/apply-changes-opportunities.md](notes/apply-changes-opportunities.md)** - `apply-changes --locations-on-ways`. **P1 + P1.5 landed 2026-04-21 (`719f306`)**; parallel writer made the default 2026-04-21 (buffered path removed, `--parallel-writer` flag deleted). **Planet best: 80.9 s cross-disk + zstd:1** (-44 % vs 144.4 s pre-flip baseline). Cross-disk `--compression none` + `--io-uring`: 93.0 s. Same-disk zstd:1 + `--io-uring`: 99.4 s. Germany + europe same/cross-disk matrix bench 2026-04-21 confirmed parallel-writer wins or ties the non-io-uring column at every scale; io-uring stays opt-in for users with `RLIMIT_MEMLOCK` raised. Remaining open items: splice-in-place (#11, deferred - doesn't reduce output bytes on compressed output), multi-file output / RAID-0 (unlanded and lower priority given 80.9 s is comfortably inside any realistic production budget).

- [x] ~~**getid include mode**~~ - landed 2026-04-20 via a shared `pread`-only `HeaderWalker` primitive (`src/read/header_walker.rs`). Planet **43.7 s → 6.1 s (7.2×, UUID `24362e36`)**, germany 200 ms, disk read 88 GB → 601 MB. Initial HeaderWalker landing hit 7.0 s with two preads per blob; the follow-up 1-pread probe walker (commit `d263d76`) trimmed a further 0.9 s (-13 %). Walker is syscall-bound; going lower would need io_uring batching - not pursued. Plan doc retired.

- [x] ~~**diff-snapshots (text and `--format osc`)**~~ - landed 2026-04-20. Planet baselines: text **2134 s / 35m34s**, osc **2225 s / 37m06s**. ID-range sharded parallel block-pair merge for both paths: text planet **227.5 s / 3m48s at `-j 16` (UUID `22a5eb55`, 9.5× speedup, temp-file shape, 586 MB peak anon)**, osc planet **313.8 s / 5m13s at `-j 16` (UUID `9b3fc2b9`, 7.1× speedup, 663 MB peak anon)**. CLI flag `-j/--jobs N` on `pbfhogg diff`. Germany text 16.5 s, germany osc 20.4 s at `-j 8`. Both paths now stream shard output to per-shard scratch temp files; an interim text-shape buffered each shard in a `Vec<u8>` (208.6 s, UUID `b02d86bc`, 2.29 GB peak anon) and was replaced with the temp-file shape for a 74 % RSS drop at a 10 % wall cost. Shard balance within 1.03× max/min. Both paths beat the 8-min aspirational target. OSC speedup is lower because the final `assemble_osc` (gzip + concat of ~45 GB of XML fragments) is single-threaded and runs 32.8 s. Remaining follow-ups (all small, below): auto-enable parallel by default, parallelise `assemble_osc` gzip. Plan doc retired.

- [ ] **[notes/altw-external.md](notes/altw-external.md)** - `add-locations-to-ways --index-type external`. Current planet baseline: **661.2 s `--bench 3`** (UUID `a406d77e`, commit `aee7727`, 2026-04-18 post-regression-fix). Europe **291.6 s** after metadata-driven relation scan (`6d71053`). Doc lists 20 live leads grouped by blocker (Tier 1 actionable now, Tier 2 speculative, Tier 3 hardware-gated, Tier 4 deep stretch) plus correctness invariants + implementation conventions. Failed attempts, measured numbers, physical floors, and meta-lessons live in [`notes/altw-optimization-history.md`](notes/altw-optimization-history.md). Dominant theme: the stage 2 → stage 3 → stage 4 disk-seam chain, with ~80 GB rank shards + ~112 GB slot buckets. Five ranked items landed this sprint (#4 stage-2 de-ranking, #8 BlobLocationRouter, #9 L1+L2 relation scan, #2 streaming stage 3 → 4); remaining seam-shaped items are blocked on RAM (~25 GB host) or a faster second NVMe.

  **Apply-changes work that might transfer (speculative, 2026-04-21, no deep ALTW research):**
  - **Worker-emits-framed-bytes (P1.5 pattern).** If ALTW Stage 4 still dispatches framing via `rayon::spawn` per output block and funnels through `write_primitive_block_owned`, moving framing inline into the worker (call `frame_blob_pipelined` directly, ship the framed `Vec<u8>` to the writer thread via `write_raw_owned`) would save the same `writer_pipeline_send_wait_ns` we shaved in apply-changes (-86% at planet `--compression none`). Pattern transfers cleanly; trigger is whether that counter is large in ALTW.
  - **Cross-disk scratch (no code, pure config).** Apply-changes planet dropped 31% just by moving bench output to a different physical NVMe (single-NVMe read+write contention removed). Worth a single `brokkr.toml` edit + bench to see if ALTW's 661 s shows similar shape - if so, it's an immediate runtime recommendation rather than code work.
  - **`zstd:1` for internal pipelines.** Already documented in apply-changes plan doc and in the ALTW notes (`notes/altw-optimization-history.md` mentions `--compression zstd:1` Europe 419 s → 379 s, -9.5%). Confirmed the same mechanism in apply-changes (workers parallelize zstd cheaply; smaller bytes → less writer wall). Should lift to ALTW Stage 4's writer config without changes.

  **Probably doesn't transfer:**
  - **Descriptor-first scanner + drain shape.** ALTW external is multi-pass external-sort; the design premise (reader/classify/rewrite in one pass) doesn't match.
  - **Node→way barrier + coord fusion.** Apply-changes-specific (coords needed mid-run to resolve OSC way refs); ALTW's whole job IS the coord scatter.
  - **BTreeMap seq reorder buffer at drain.** ALTW Stage 4 has its own output ordering.

  **Already shared:**
  - **HeaderWalker** (scan-audit round migrated both commands).
  - **`copy_file_range` coalescing** (apply-changes drain *ported from* `altw/passthrough.rs`, so the flow direction already ran).
  - **`IdSetDense::set_atomic_if_new`** primitive (used by both for parallel set-membership).

  Rough prioritization if a day were available: cross-disk bench first (10 min, tells us where the ALTW ceiling actually lives), then worker-framed-bytes if Stage 4 is writer-bound.

Measurement-first on every one: turn on `#[cfg(feature = "hotpath")]` counters (or add unconditional `*_ms` counters) to ground-truth the inferred per-phase breakdowns before committing to the order of landing items within a plan.

## Known issue: io_uring writer corrupts very small outputs

Surfaced 2026-04-21 when `RLIMIT_MEMLOCK` was raised to unlimited and
the previously-skipped `merge_basic_create_modify_delete_uring` test
actually ran. `merge()` returns `Ok`, but the resulting PBF file panics
on read with "failed to fill whole buffer" - looks like the writer
finalizes too early or truncates before the last frames flush. Confirmed
present at pre-flip commit `383a2eb`, so not introduced by P1+P1.5 (the
test had been silently skipping via the `is_uring_unavailable` helper).
Test marked `#[ignore]` with a comment pointing here. Planet-scale
io_uring runs (137-148 GB outputs) appear to produce valid PBFs - the
bug is specific to very small outputs. Likely candidate: `flush_final`
in `src/write/uring_writer.rs` interaction with the `set_len` truncate
when `logical_size < page_aligned`.

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

## Next up (2026-04-13)

- [x] ~~**`--allow-missing` for apply-changes**~~ - **not needed (2026-04-21).**
  Audit confirmed every missing-element case is already tolerated silently:
  modify-on-missing inserts, delete-on-missing is a no-op, create-on-existing
  overwrites, and way/relation refs to absent nodes get `(0, 0)` under
  `--locations-on-ways` with a `loc_missing` count in the summary.
  Independent-reader verification against the vendored
  `research/osmium-tool/src/command_apply_changes.cpp` confirmed osmium
  matches this permissive behaviour (not a deviation - positive parity),
  so the scenario table lives in
  [reference/osmium-parity.md](reference/osmium-parity.md#apply-changes-permissive-missing-element-semantics-parity)
  alongside osmium file:line anchors. Three new invariants in
  `tests/apply_changes_invariants.rs` pin the pbfhogg behaviour.
  Incremental extract works against current `apply-changes` with no flag.
  The related stretch item below ("Incremental extract update") is
  already unblocked.

## Performance

- [ ] **Parallelise `assemble_osc` gzip**. Final serial tail in
  `diff --format osc -j 16` is 32.8 s at planet (10 % of wall;
  gzip + concat of ~45 GB of per-shard XML fragment temp files).
  Pigz-style parallel gzip or switching to a concurrent compressor
  would recover most of that. Only matters if osc becomes a hot
  workload.

- [ ] **Consider auto-enabling diff `-j`**. Currently `pbfhogg diff`
  defaults to `-j 1` (sequential). `-j 0` maps to
  `available_parallelism()`. Evaluate flipping the default from 1
  to 0 once the parallel path has more field miles. Wait until
  Milestone 3.

- [ ] **Expose phase events as a proper Rust event/hook API** - wrap
  every instrumentation call in per-command `probes` modules, then swap
  the backend from the current FIFO sink to `tracing` spans/events so
  library consumers can subscribe. Full rollout (call-site shape,
  coverage sweep, brokkr `--probes`, backend migration) in
  [`notes/instrumentation-layering.md`](notes/instrumentation-layering.md).

- [ ] **Reclaim europe's lost prefetch win after scan-audit.** The
  2026-04-20 scan-audit swap to `HeaderWalker` gave up ~14 s of
  downstream decompression benefit at europe scale because the old
  buffered header walk was accidentally warming blob-body pages via
  the kernel's sequential readahead - pages the downstream phases
  then reused. `posix_fadvise(POSIX_FADV_RANDOM)` deliberately skips
  that. A deliberate `posix_fadvise(POSIX_FADV_WILLNEED)` over the
  exact blob ranges that the scan result flagged for later pread
  (`(data_offset, data_size)` for the schedule entries we're about
  to hit) would reclaim the prefetch without re-introducing the old
  walk's I/O waste. Only matters for mid-size workloads - planet is
  larger than RAM so prefetched pages evict before reuse, and
  germany is already fully cached. Measure on europe `check-refs` /
  `tags-filter` / `extract --simple` where the phase-level win is
  huge but the full-command wall is currently flat.

## Cross-pipeline optimization

Cross-thread buffer retention is **solved** - `DecompressPool` (commit
`8f6999b`) recycles decompression buffers in the pipelined reader. The
remaining architectural concern is thread oversubscription (two concurrent
rayon pools: decode + batch processing), not retention.

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [reference/pipelined-reader-paths.md](reference/pipelined-reader-paths.md)
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

The remaining re-encoding commands - tags-filter, renumber, time-filter -
still fully decode and re-encode via BlockBuilder. Of these:

- **tags-filter** is closed: blob-level raw passthrough was measured on
  2026-04-18 (shadow counter, commit `a5c6854` reverted in `0ef4107`,
  UUID `8c786794` at `w/highway=primary` on planet) and 0 / 50,364
  pass-2 blobs qualified. The load-bearing pin is the comment block
  at the pass-2 worker in `src/commands/tags_filter.rs`.
- **renumber / time-filter**: every element is modified, so raw
  passthrough does not apply - the win here is write-path throughput
  instead.

Four per-group raw passthrough primitives are committed as scaffolding
for partial-match blobs (e.g., extract boundary blobs where some groups
match and some don't). Currently unused - blob-level passthrough handles
the common case. Design tradeoffs and the measurement prerequisite live
in the module doc comment at `src/write/raw_passthrough.rs`. The
primitives themselves:

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

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Multi-extract

Single-pass multi-extract shipped for simple strategy on sorted input
(commit `542aad0`). Reads PBF once, classifies each element against N
regions, writes to N sync-mode PbfWriters. 3-phase barrier (nodes →
ways → relations) with per-region IdSet + BlockBuilder. Memory:
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

- [ ] **Spatial index** - grid or R-tree over regions for O(1)
  per-element lookup instead of O(N). Required for 200+ regions where
  linear scan becomes the bottleneck. Simple grid (3600×1800 cells of
  0.1°, precompute overlapping regions per cell) is sufficient.
- [ ] **Complete/smart strategies** - per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [x] ~~**Raw passthrough**~~ - CLOSED 2026-04-20 via shadow counter
  (planet 5-region `--config --simple` at commit `57b01f9`, UUID
  `dad573cb`): 0 / 32,835 node blobs qualify under any partial-passthrough
  gate. Same outcome as tags-filter's earlier 0 / 50,364. Structural:
  ID-sorted PBFs put chronologically-adjacent (geographically-scattered)
  nodes in each blob, so a blob's geographic bbox is ~planet-wide and
  cannot fit in a sub-planet region. The all-N-contained path stays
  for the N=1 / fully-overlapping niche. Load-bearing pin in
  `src/commands/extract/multi.rs::try_extract_multi_single_pass`.

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
- [ ] **`renumber` - maintenance polish** (current: 204.5 s / 3m25s planet at `aee7727`,
  historical 194 s at `cb99106`, 3.3 GB peak anon, zero temp disk).
  Three candidate items (varint fast path, `way_id_set` vs schedule, reframe
  breakdown instrumentation) captured in
  [`notes/renumber-optimization.md`](notes/renumber-optimization.md) with
  per-item regression analysis and disposition. Not today; revisit if
  renumber becomes critical path or the +10 s drift vs `cb99106` grows.

### Ecosystem

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

- [ ] **Geocode pass 3 stage A par_iter** - per-way `Vec::new()` inside
  `flat_map_iter` closure (`builder.rs` ~line 1226). Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** - 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work. Shape of the fix (for when /
  if it's ever needed): change `BlockBuilder::add_relation` from
  `members: &[MemberData<'_>]` to `impl IntoIterator<Item = MemberData<'_>>`,
  add three parallel packed scratches on the builder
  (`member_roles_scratch`, `member_ids_scratch`, `member_types_scratch`)
  so the single-pass iteration can write all three protobuf member
  fields without re-scanning. Most callers already reuse a buffer
  (`members_buf.clear(); members_buf.extend(...)`) so the saving is
  small; the concentrated win is `apply-changes`
  (`rewrite_block.rs`, `element_writes.rs`) which builds fresh
  `Vec<MemberData>` per relation from OSC input with no reuse.

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

  Mechanism: cold-arena-page residency cascade. Post-PASS1 header
  scans touched glibc's bloated free-list pages that were previously
  reserved but not resident; the fix (commits `d4ea760`, `0b085b1`)
  plumbs the PASS1 schedule forward so PASS2/PASS3 don't rescan.

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
  the region extract (current apply-changes already tolerates OSC ops
  referencing elements outside the region; see reference/osmium-parity.md), then
  `extract` to re-filter to the bbox. ~10s vs 862s
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
- [ ] Dense ALTW compact rank-indexed array (same pattern as geocode builder -
  better locality on hosts where dense currently works, reviewers split 1/8).
  **Still relevant 2026-04-19**: dense is a fallback path (auto-select picks
  External for sorted+indexed, dense only fires for non-canonical inputs),
  but the rank-indexed layout is isomorphic to the geocode pass 2 pattern
  (~16 GB contiguous vs scattered across 128 GB virtual) and would help
  users with non-canonical PBFs. No current dense planet bench to measure
  against; build and bench on a non-indexed/non-sorted planet variant if
  prioritised.
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
