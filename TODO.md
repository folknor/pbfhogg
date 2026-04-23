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

## Planet-scale validation coverage

README's planet table is the source of truth for "this command runs
cleanly on the 32 GB-RAM reference host". `overnight.sh` fills most
of the reachable gaps as bench runs (produces
`.brokkr/results.db` entries that get promoted into the README once
they land). This section tracks the remaining axes and dataset gaps
that are not currently driven by `overnight.sh`.

### Blocked on pbfhogg CLI changes

Need a pbfhogg CLI flag to exist before brokkr can forward it:

- [ ] **`merge-changes -j N`** - parallel-parse axis the
  [`notes/merge-changes.md`](notes/merge-changes.md) plan will
  eventually deliver. Not a gap today; will appear when the feature
  lands.

### Blocked on dataset / config

- [ ] **History PBF for `time-filter`**. pbfhogg supports per-element
  version history and visibility, but `brokkr.toml` has no history
  variant on any dataset. `time-filter` benches on a regular PBF
  record near-no-op walls (every timestamp compare decides keep).
  Configure a history PBF variant (planet history is ~120 GB; europe
  history is more realistic for iteration) to unlock the actual
  workload.
- [ ] **Additional planet snapshots** for `diff-snapshots`. Current
  `brokkr.toml` has only one alternate (`snapshot.20260411`), so the
  snapshot-range axis is a single pairing. Downloading another
  snapshot 2-4 weeks away would let us measure diff-wall vs
  snapshot-delta-size empirically.
- [x] ~~**Multi-OSC merge-changes at europe / germany**~~ - landed
  2026-04-22. europe now carries OSCs 4715..4722 (8 entries), germany
  carries 4705..4712 (8 entries). 7-OSC ranges (4716..4722 and
  4706..4712 respectively) match planet's 4914..4920 shape so the
  parallel-parse plan can iterate at smaller scales. `overnight.sh`
  queues both ranges (streaming + `--simplify` paths).

### Un-benched permutations (low priority)

Known to work, no performance question open, but not in the results DB:

- [ ] **Custom ID set distributions for `getid` / `getparents`**.
  brokkr's ID set is baked in; no way to test different distributions
  (sparse vs dense, forward vs spread across the ID range, cold-cache
  vs hot-cache). Add a CLI pass-through if ID-set shape becomes a
  perf question. Not needed for general validation.
- [ ] **`--direct-io` at planet for commands beyond apply-changes**.
  `apply-changes` has coverage. Every other command supporting
  `--direct-io` (cat, sort, extract, add-locations, merge-changes
  where applicable, ...) has no `--direct-io` planet number. Only
  matters if direct-io becomes a default on any of them.
- [ ] **`renumber` with non-default flags**. Has no non-default flags
  in pbfhogg today (just the one variant since the in-memory path
  was retired). If a future variant adds flags this reopens.
- [ ] **`bench-read` / `bench-write` / `bench-merge` at planet**.
  Synthetic benchmarks, intentionally excluded from the README user
  surface. Periodically-useful diagnostic tools; not a validation
  target.
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

- [x] ~~**Parallelise `assemble_osc` gzip**~~ - landed. New
  `ParallelGzipWriter` (`src/write/parallel_gzip.rs`) buffers 2 MB
  chunks, dispatches each to a worker-pool for independent gzip,
  writes concatenated RFC-1952 multi-member output. Three in-crate
  `.osc.gz` readers (`osc::parse`, `merge-changes`, `tags_filter
  --osc`) migrated to `MultiGzDecoder` in the same commit so
  cross-command composition stays intact. Planet `diff --format osc
  -j 16` wall to be remeasured (prior baseline UUID `9b3fc2b9`,
  313.8 s with a 32.8 s single-threaded `assemble_osc` tail).

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

- [x] ~~**Diff element_stream fallback path untested**~~ - landed
  2026-04-22. `PbfWriter::write_primitive_block_no_indexdata` added as
  public library API (bypasses the `scan_block_ids` / `scan_block_tags`
  path that populates `BlobHeader.indexdata` / `.tagdata`). New
  `write_test_pbf_non_indexed` helper in `tests/common/mod.rs` drives
  the three `diff_element_stream` pairings (old non-indexed, new
  non-indexed, both non-indexed) plus a parity test that asserts the
  fallback path produces identical text + stats to the optimized
  `diff_block_pair` on the same logical input. Verbose mode pinned
  separately.

- [x] ~~**Test fixture infrastructure**~~ - landed 2026-04-22.
  `TestNode` / `TestWay` / `TestRelation` extended with
  `meta: Option<TestMeta>` (default `None`); ~428 struct literals across
  14 test files migrated via a one-shot migration script (script
  deleted post-migration - the migration is idempotent and the
  literals are now the source of truth). New
  `write_multi_block_test_pbf(path, nodes, ways, rels, block_size)`
  forces block flushes every N elements to produce multi-blob fixtures
  without needing 8000+ elements per type. `generate_nodes` /
  `generate_ways` / `generate_relations` emit sequential id-sorted
  vectors straight into the writer. `assert_indexed` and
  `assert_non_indexed` expose the existing `has_indexdata` probe for
  blob-header assertions. All covered by
  [`tests/fixture_helpers.rs`](tests/fixture_helpers.rs) smoke tests.

### Tests enabled by the new fixtures - landed 2026-04-22

Eight gaps surfaced by parallel reviewer-agent sweeps after the
fixture work. All landed as integration tests across `tests/cat.rs`,
`tests/diff.rs`, `tests/extract.rs`, `tests/getid.rs`,
`tests/read_paths.rs`, `tests/inspect.rs`, `tests/tags_filter.rs`,
new files `tests/tags_count.rs` and `tests/non_indexed_parity.rs`.
Two of the eight landed with `#[ignore]`-gated tests pinning real
correctness bugs (see "Known correctness gaps" above).

- [ ] **Parallel classify correctness for `check --refs`.** The
  other three parallel-classify commands (`inspect --nodes`,
  `tags-filter` two-pass, `tags-count`) got `jobs=1` vs `jobs=4`
  parity tests via their `jobs: Option<usize>` / `jobs: usize`
  library APIs. `check_refs` has no equivalent override in its
  public signature (`src/commands/check/refs.rs:141`), so a parity
  test has to either exercise the CLI via `cli/tests/cli.rs` (hard
  to observe worker count from outside) or wait for a plumbed
  `jobs` argument. Not urgent - the worker-count-independent
  correctness is implicitly covered by the existing single-blob
  tests - but worth revisiting if check-refs ever grows a jobs
  flag.

### Next test-coverage batches (2026-04-22)

The first round of tests-enabled-by-fixtures surfaced two real
correctness bugs (see "Known correctness gaps"). Batches below
continue the same pattern; each is self-contained and can land on
its own commit.

- [x] ~~**Batch A: non-indexed `--force` parity for the rest of
  the command surface.**~~ - landed 2026-04-22. Added parity
  tests in `tests/non_indexed_parity.rs` for `derive_changes`,
  `renumber_external`, `check_refs`, `verify_ids`,
  `show_element`, `extract_multi --CompleteWays`, and `merge_pbf`.
  Passing: derive_changes, check_refs, show_element, merge_pbf
  (with disjoint inputs). Renumber pinned via error-message test
  (it explicitly rejects non-indexed). Three `#[ignore]`-gated
  tests added to pin new bugs: verify_ids spurious TypeOrder,
  extract_multi CompleteWays empty output, merge_pbf A+A drops
  ways/rels. All five bugs surfaced by parity testing are listed
  under "Known correctness gaps" above.
- [x] ~~**Batch B: structural roundtrip invariants.**~~ - landed
  2026-04-22 in `tests/roundtrip_invariants.rs`. Four tests pin
  sort idempotence, extract idempotence, derive/apply roundtrip,
  and tags_filter idempotence. `tags_filter` composability
  scoped down to idempotence because expression OR-combination
  semantics aren't identical to chained filtering (filtered
  output feeds later pass with already-thinned element set).
- [x] ~~**Batch C: blob-layout parity.**~~ - landed 2026-04-22 in
  the same file. Three tests pin read-path equivalence across
  block_size=1/5/100 layouts, `tags_filter` output equivalence
  across the same layouts, and `diff` stats parity across
  mixed-layout pairings.
- [x] ~~**Batch D: edge-case / boundary coverage.**~~ - landed
  2026-04-22 in `tests/edge_cases.rs`. Nine tests covering empty
  PBF (header only), zero-ref ways, zero-member relations,
  empty-string tag values, empty-string tag keys,
  relation-of-relation transitivity, large positive ids, and the
  8000-entity BlockBuilder capacity boundary. No bugs found;
  edge cases all behaved correctly.
- [x] ~~**Batch E: compression-level parity.**~~ - landed
  2026-04-22 in `tests/roundtrip_invariants.rs`. Three tests
  pin that `sort` and `tags_filter` outputs are element-
  equivalent across `Compression::None`, `Compression::Zlib(6)`,
  and `Compression::Zstd(3)`, plus a dedicated None-vs-Zlib
  read-back pin. No bugs found - codec is correctly isolated
  from element encoding. Byte-size comparisons deliberately
  omitted; on a 10-node fixture None often lands smaller than
  Zlib by virtue of DEFLATE framing overhead.

### Known correctness gaps surfaced by parity tests (2026-04-22)

Pinned as `#[ignore]` regression tests in
`tests/non_indexed_parity.rs`,
`tests/apply_changes_invariants.rs`, and
`tests/derive_changes.rs`, and
`tests/merge_pbf.rs` - remove the ignore attribute to reproduce.

- [x] ~~**`extract --strategy simple --force` on non-indexed input
  double-emits elements.**~~ - landed 2026-04-23. Root cause: simple's
  sorted single-pass writer runs three phases (nodes, ways, relations)
  and every non-indexed blob is in all three per-kind schedules (kind
  unknown until decompress). Each phase called the kind-agnostic
  `extract_block_pass2`, which emits elements whose ids are in the
  monotonically-growing id sets - so one non-indexed blob emitted
  nodes 3x, ways 2x, relations 1x (matching the 18 vs 6 observation).
  Fix: added `phase_kind: Option<ElemKind>` to `extract_block_pass2`
  (`src/commands/extract/common.rs`); simple passes `Some(Node)` /
  `Some(Way)` / `Some(Relation)` for the three phase writes and
  `None` for its unsorted-fallback batch; complete passes `None`
  (single-pass write). For indexed PBFs the filter is a no-op
  (blobs are homogeneous and pre-routed to their phase).
  `extract_simple_non_indexed_parity` unignored.

- [x] ~~**`apply-changes --force` on non-indexed input off-by-one on
  delete.**~~ - landed 2026-04-23. Misdiagnosed in the original
  pin - node 3 was actually being deleted correctly; the extra node
  was node 2 (the modify) being re-emitted once as a trailing create.
  Root cause: the scanner assigned non-indexed blobs a placeholder
  `kind=Node` + `id_range=None` (`scanner.rs:129-132`) and the
  worker passed those unchanged into `DrainItem::Rewritten`
  (`id_range: (0, 0)` on the wire). The drain's cursor-advance rule
  at `drain.rs:644-649` uses `blob_osm_last_key(0, 0) = (0, 0)`
  which is less than every real upsert key, so the per-kind upsert
  cursor never advanced. The trailing-creates loop at end-of-stream
  then re-emitted every modify that the worker had already handled
  inline via `diff.get_node/way/relation`. Fix: new
  `streaming.rs::infer_kind_and_range` walks the already-parsed
  block to recover the true `(kind, min_id, max_id)` for non-indexed
  blobs, and the worker uses those recovered values for both
  `upsert_slice` and the `DrainItem::Rewritten { kind, id_range }`
  emission. Indexed blobs skip the walk (trust indexdata).
  `apply_changes_non_indexed_parity` unignored.

- [x] ~~**`merge_pbf([A, A])` drops ways and relations.**~~ - landed
  2026-04-23. Original diagnosis was off. The bug wasn't in how
  blob-identical inputs were deduplicated; it was in how the
  pass-2 loop *grouped* blobs into overlap runs. `detect_overlaps`
  correctly sets `overlaps[j]=true` only between same-kind
  adjacent blobs (nodes with nodes, ways with ways). But the outer
  loop that walks consecutive `overlaps[i]=true` entries didn't
  check kind, so when a node overlap-pair sat immediately before a
  way overlap-pair in file order, both pairs merged into one
  `write_overlap_run` call. That call took `entries[0].index.kind`
  and handed it to `sweep_merge_dedup`'s kind-gated extract
  closure, which silently dropped every element whose kind didn't
  match - i.e. every way and relation when the first entry was a
  node. Fix: add `entries[i].index.kind == run_kind` to the
  overlap-run walker in `cat/dedupe.rs`.
  `merge_same_input_preserves_ways_and_relations` unignored.
  Real-world exposure was broader than the original pin suggested:
  any `cat --dedupe` or `merge_pbf` run where same-kind overlap
  pairs sat adjacent across kind boundaries would have silently
  lost one side of the boundary.

- [x] ~~**`extract --strategy complete-ways --force` on non-indexed
  input produces empty output.**~~ - landed 2026-04-23. Same fix
  closes the smart case below. Root cause: the sorted-path header
  walker in `smart.rs::collect_pass1_generic` only populated
  `node_schedule` / `way_schedule` / `relation_schedule` /
  `full_way_schedule` from blobs with indexdata, so on a fully
  non-indexed input all four schedules stayed empty - pass-1
  produced empty id sets and the pass-2 writer emitted nothing.
  Fix: replicate non-indexed blobs (`idx.is_none()`) into every
  per-kind schedule; the classify closures already kind-filter via
  `match element`, so mismatched blobs no-op at the element level.
  Mirrors the pattern in `scan::classify::build_classify_schedules_split`
  and `multi::try_extract_multi_single_pass`.
  `extract_multi_complete_ways_non_indexed_parity` unignored.

- [x] ~~**`extract --strategy smart --force` on non-indexed input
  produces empty output.**~~ - landed 2026-04-23. Closed by the
  same `smart.rs::collect_pass1_generic` change as the complete-ways
  case above; smart's relation-member expansion rides on the same
  id sets that pass 1 now populates correctly for non-indexed input.
  `extract_smart_non_indexed_parity` unignored.

- [x] ~~**`apply-changes -j N --locations-on-ways` consumer build trips
  the drain/copy-range invariant.**~~ - landed 2026-04-23 alongside
  the non-indexed delete fix. Root cause was wider than the original
  pin suggested: the worker's false-positive path
  (`streaming.rs::handle_candidate`'s `!overlaps` branch) emits
  `WorkerOutput::FalsePositive` which unconditionally becomes
  `DrainItem::CopyRange` via `WorkerOutput::into_drain_item()`. The
  consumer build compiles out the `linux-direct-io` feature and
  forces `use_copy_range=false` (`rewrite.rs:220-221`), so the drain
  rejects every such item with "drain: received CopyRange item but
  use_copy_range is false" - affecting any consumer-build merge with
  a false-positive blob, not just `-j N --locations-on-ways`. Fix:
  thread `use_copy_range` through `StreamingConfig` → worker; when
  false, route the false-positive through the same path used for
  `--direct-io` (`handle_owned_passthrough` - pread the full frame,
  emit `DrainItem::OwnedBytes`). `merge_jobs_parity_on_multiblob_input`
  now passes in both feature sets. The three `merge.rs` stats tests
  that were also blocked by this panic
  (`merge_gap_creates_between_blobs`, `merge_stats_accuracy`,
  `merge_type_transition_node_to_relation_skipping_ways`) now run to
  completion in consumer; they still fail stats assertions because
  `DrainItem::OwnedBytes` does not credit per-kind `base_*` counts
  (only `CopyRange` does) - that's squarely gap #7 (merge stats
  drift) and tracked separately.

- [x] ~~**`merge` summary/stat counters diverge between all-features
  and consumer builds on the same fixture.**~~ - landed 2026-04-23.
  Root cause was narrow: the drain's `DrainItem::OwnedBytes` arm
  (`drain.rs::dispatch_variant`) did not credit per-kind `base_*`
  counters, only bumping `blobs_passthrough` / `bytes_passthrough`.
  Only the `CopyRange` arm bumped `base_<kind> += index.count`.
  Consumer builds force `use_copy_range=false` (no `linux-direct-io`
  feature), so every passthrough flowed through `OwnedBytes` and the
  per-kind counters stayed at zero. `--direct-io` runs were
  similarly affected. Fix: add `count: u64` to
  `DrainItem::OwnedBytes`; drain's `OwnedBytes` arm now mirrors
  `CopyRange`'s per-kind match. Both producers thread the count
  through: the scanner-passthrough worker branch reads
  `desc.index.count`; the worker false-positive branch reads
  `desc.index.count` (indexed) or walks the already-parsed block
  via a new `count_block_elements` helper (non-indexed `--force`).
  `WorkerOutput::into_drain_item` gained a matching `fallback_count`
  parameter for the non-indexed `--force` CopyRange fast-path. The
  pinned `merge_stats_match_output_counts_after_roundtrip` is
  unignored; `merge_stats_accuracy`,
  `merge_gap_creates_between_blobs`, and
  `merge_type_transition_node_to_relation_skipping_ways` now pass
  in both sweeps.

- [x] ~~**`check --ids` (`verify_ids`) reports spurious TypeOrder
  violations on non-indexed input.**~~ - landed 2026-04-23. Fixed
  by gating the offset-based `check_type_order` in
  `verify_ids_full_parallel` on `indexed`. Non-indexed `--full`
  runs lose the offset-based pre-check; the sequential (non-
  `--full`) path already has an element-level type-order check
  that works correctly on any input, so users who need actual
  type-ordering verification on non-indexed input have a path.
  `check_ids_non_indexed_parity` unignored. A richer solution
  (emit per-blob kind from the phase decoders and reconstruct
  per-kind offset ranges) is possible but not worth the
  complexity for the `--force` path today.

### Test-shape gaps surfaced by the 0.3.0 bug sweep (2026-04-23)

The 117-finding sweep clusters into ~6 test-shape gaps, not 117
individual tests. The pattern is already validated: Batch A's
non-indexed parity shape surfaced 7+ real bugs from one structured
test idea, and the `merge_jobs_parity_on_multiblob_input` /
cross-feature parity shape surfaced the consumer-build drain panic
plus the `base_*` counter drift. Same approach scales here. Items
below are the shapes that would catch the remaining findings; each
is self-contained and can land independently.

Ordered by value-to-cost (most bugs caught per line of test
infrastructure first).

- [ ] **Fault injection for parallel pipelines.** Biggest hole -
  ~30 findings are "worker errors mid-stream -> silent truncation /
  hang / temp-file leak" across `write/parallel_writer.rs`,
  `write/uring_writer.rs`, `write/parallel_gzip.rs`,
  `apply_changes/drain.rs`, `apply_changes/rewrite.rs`,
  `diff/parallel.rs`, `derive_parallel.rs`, altw external
  stages 3/4, geocode Pass 3 Stage A. Zero tests today inject a
  worker panic or mid-stream I/O error and assert the output is
  either correct-or-errored, never silently short. Minimum shape:
  a thin `FaultSink` wrapper around `Write` that errors at a
  configurable byte offset, plus a `panic_at_seq(N)` hook on the
  worker closure (feature-gated behind `#[cfg(test)]` in the
  pipeline modules so it doesn't bloat release). Then one test
  per pipeline: "worker N panics at seq K -> command returns
  `Err`, scratch dir is clean, output file is either absent or
  truncated-to-zero (never a silent short file with zero-filled
  holes)." Catches the seven HIGH items in "Write path" plus the
  MEDIUM scanner/worker-panic items in "apply-changes pipeline"
  and the shard-worker-error items in "diff / derive-changes
  shard-parallel".

- [ ] **Lying-indexdata fixtures.** ~15 findings trust
  `BlobHeader.indexdata` without verification:
  `min_id`/`max_id` overstating or understating actual contents,
  `kind` disagreeing with blob contents, `element_count` drifting,
  phantom-sorted flag on unsorted content. Every existing fixture
  has either honest indexdata or none at all. Add a
  `write_pbf_with_custom_indexdata(path, blobs, |idx| { ... })`
  helper to `tests/common/mod.rs` that lets each test override
  the indexdata fields independently of blob contents. Then run
  the command surface ("indexdata says `min_id=0` but blob
  contains `id=-1`", "indexdata `kind=Node` but blob contains
  ways", etc.) and assert each command either ignores the
  indexdata, defends itself, or produces a clean error - never
  panics and never silently produces wrong output. Catches:
  `renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272`,
  `renumber/stage2.rs:226-231`, `renumber/mod.rs:240`,
  `altw/external/stage2.rs:488`, `altw/external/stage4.rs:438-478`,
  `apply_changes/scanner.rs:162,188`, `apply_changes/streaming.rs:496`,
  `commands/inspect/show_element.rs:53-57`,
  `blob_meta/scan_ids.rs:192-202`.

- [ ] **Negative-ID / signed-arithmetic matrix.** ~8 findings
  mishandle negative element IDs because guards are gated on
  indexdata or shard planners use raw numeric compare instead of
  `osm_id_cmp`. Today every fixture uses non-negative IDs.
  Add `generate_nodes_with_negatives(start_neg, start_pos, n)`
  plus equivalents for ways/relations to `tests/common/mod.rs`
  (canonical OSM order: `..., -3, -2, -1, 0, 1, 2, ...`). Run
  them through every command, including `-j N` variants. The
  `renumber` deviation in DEVIATIONS.md says "negative inputs
  rejected" - we don't currently test that the rejection fires
  across all paths (only the happy path with indexdata present).
  Catches: `renumber/pass1.rs:179`, `renumber/wire_rewrite.rs:272,519-524`,
  `diff/parallel.rs:138-142,354-357,384`, `derive_parallel.rs:136-142`
  and its sibling emit/merge sites, `geocode_index/builder/pass1_5.rs:102`.

- [ ] **Adversarial / truncated-input tests.** ~10 findings
  accept untrusted input without bounds-checking: missing
  `MAX_BLOB_HEADER_SIZE` guards in the new pread primitives,
  schedule offsets past EOF from truncated files, varint
  miscount on malformed fields. Two shapes cover the class:
  (a) the proptest item below, and (b) a "truncation sweep"
  integration test that takes a known-good PBF and truncates to
  every blob/frame/field boundary, asserting every command
  returns a clean `Err` without panic or multi-GB allocation.
  Catches `read/header_walker.rs:149-164`,
  `read/raw_frame.rs:65-67,124-127`, `scan/classify.rs:59-95,110-163`,
  `renumber/wire_rewrite.rs:486-491`, and the two geocode
  bucket-file truncation findings.

- [ ] **Complete the `-j N` vs `-j 1` parity matrix.** We have
  parity for `inspect --nodes`, `tags-filter` two-pass,
  `tags-count`, and the brand-new `merge_jobs_parity_on_multiblob_input`.
  Missing: `diff -j N`, `derive-changes -j N`, `apply-changes -j N`
  (beyond the single merge fixture), altw external stage 4
  worker count (currently hard-coded, would need a library arg),
  geocode Pass 1.5 / Pass 3 Stage A parallel degree,
  `check --refs` (blocked on a jobs arg on `check_refs`, see
  existing entry above). Same shape as the existing tests:
  element-equivalent output + matching summary counters across
  worker counts. Specifically pins regression of the diff/derive
  shard numeric-compare family, the `OwnedBytes` counter bug we
  just landed, and would catch any future worker-count-dependent
  drift. Pair with the negative-ID fixtures above for maximum
  coverage (the shard-parallel bugs only surface on mixed-sign
  inputs).

- [ ] **Scratch-dir / temp-file cleanup invariants.** ~8 findings
  leak scratch files on worker-error paths. One generic helper
  in `tests/common/mod.rs`:
  `with_tracked_scratch_dir(|scratch| { run_command(...); })`
  snapshots the directory before + after and asserts equality
  (or asserts a specific known-empty state). Combined with the
  fault-injection shape above, covers every leak in the sweep:
  altw external stages, diff-parallel, derive-parallel,
  apply-changes (the `rewrite.rs:244` mid-stream-abort path),
  geocode Pass 3 Stage A.

- [ ] **Boundary-twin scan across modules.** Lowest-effort lever.
  Several findings are direct cross-module twins of bugs already
  fixed: `commands/sort/mod.rs:178-181` is the same
  overlap-run kind-boundary bug as the just-fixed
  `cat/dedupe.rs:225`; `write/parallel_writer.rs` and
  `write/parallel_gzip.rs` both silently swallow `Drop`-path
  errors; the kind-placeholder-on-non-indexed pattern from
  apply-changes recurs in altw, extract-multi, getid, cat,
  tags-filter. When landing a fix in one module, add one test
  per twin site in the same commit. Cheaper than chasing each
  finding individually and prevents the next regression of the
  same pattern.

Meta-observation: bug density skews toward the three newest / biggest
subsystems - apply-changes pipeline, altw external, diff/derive-parallel.
A policy that every new parallel pipeline must ship with a
worker-panic test + a `-j N` parity test + a scratch-leak test
would change the shape going forward. Worth considering as a CI gate
once the fault-injection harness exists.

- [ ] **Property-based testing via `proptest`** *(recommended first
  pass before any `cargo-fuzz` investment)*. Same class of bugs the
  fuzz targets below would catch - parse crashes, boundary
  violations, roundtrip asymmetries - but runs inside `cargo test`
  in seconds, no corpus directory to gitignore, no long-running
  campaigns. Shrinks failing inputs to minimal reproducers. Rough
  targets (one `#[proptest]` fn each):
    - `PrimitiveBlock::from_vec(bytes)` over arbitrary `Vec<u8>` -
      must return `Err` or `Ok`, never panic. Same shape for
      `parse_osc_file(bytes)`, `Cursor::parse_*`, `WireBlock::parse`,
      `WireInfo::parse`.
    - `generate_nodes(n, start)` / `generate_ways` / etc -> write
      -> read -> `assert_elements_equivalent` over arbitrary
      element counts and start ids.
    - `apply_changes(base, derive_changes(base, modified))
      element-equivalent to modified` over arbitrary-shape
      modifications to a baseline fixture (add/remove/modify N
      elements for arbitrary N).
    - Header flag combinations: `sorted`, `bbox`, writing program,
      `required_features` - round-trip equality.
  ~100-200 lines across one new `tests/proptests.rs` file. Add
  `proptest = "1"` to `[dev-dependencies]`. Runs in the normal
  `brokkr check` sweep; no separate workflow.

- [ ] **Fuzz testing via `cargo-fuzz`** *(optional follow-up to
  proptest above; only worth the setup if someone wants to run
  weekend campaigns)*. PBF parsing (`PrimitiveBlock::from_vec`),
  OSC parsing (`parse_osc_file`), and wire-format decoders
  (`Cursor`, `WireBlock`, `WireInfo`) accept untrusted input.
  `cargo-fuzz` targets for these entry points would catch panics,
  OOM, and logic errors on malformed data. Also fuzz the roundtrip
  path (write → read → compare). **Cost to manage:** `fuzz/corpus/`
  grows to hundreds of MB-low GB per target over long campaigns
  and `fuzz/target/` is ~500 MB-1 GB of build artifacts - both
  must be gitignored, and a developer running the fuzzer locally
  needs that space. **Schedule:** smoke runs (60 s) only verify
  the harness; real bug-hunting needs hours to days per target
  ("weekend campaign" cadence). Skip until proptest exposes a gap
  that only coverage-guided fuzzing can fill.

## 0.3.0 pre-release bug sweep (2026-04-23)

Findings from a multi-agent Opus audit of 0.3.0 high-churn areas before the release. Six agents ran in parallel, scoped by area: apply-changes pipeline, write path, renumber, altw external, diff/derive-changes shard-parallel, geocode builder v2. 88 findings total. All are listed below - HIGH, MEDIUM, and LOW - with file:line, severity, description, and trigger. LOW items are kept deliberately so we have the full record even if we defer them.

**Verification status (2026-04-23):** All ~117 findings were re-verified across two Opus passes after an outside reviewer flagged ~28% of an initial slice as mis-called. Final tally: ~51 findings eliminated as false positives / documented-intentional / stated-invariant / unreachable-sentinel-math; ~6 mechanism rewrites (real bug, wrong described mechanism); ~2 downgrades. Strikethrough annotations below carry a one-line verification note; kept items retain the original description, with mechanism corrections inline where needed.

**What survived, by area (approximate real-bug counts after verification):**
- apply-changes pipeline: 2 HIGH (scanner.rs:162/188, streaming.rs:496), 2 MEDIUM diagnostic-quality, 0 LOW
- write path: 1 HIGH (truncation on framer panic in parallel_writer + uring_writer), 3 MEDIUM (Drop-swallows class, uring CQE buffer leak)
- read path: 3 HIGH (header_walker + raw_frame MAX_BLOB_HEADER_SIZE gaps, classify schedule past-EOF edge), 1 MEDIUM narrow (scan_ids coord overflow)
- renumber: 2 HIGH (negative-ID via stale indexdata, phantom way orphans), 4 MEDIUM (consistency + hard-panic on lying header)
- altw external: 1 HIGH (blob_meta --force confusion), 4 MEDIUM latent/perf; Null Island is documented-accepted per CORRECTNESS.md
- diff / derive-parallel: 0 HIGH live bugs (mixed-sign numeric-compare family is latent - production PBFs are positive-only), 2 MEDIUM scratch-leak on error/panic
- geocode: 1 HIGH (admin.rs u32 vertex_offset overflow), several MEDIUM latent overflow/correctness
- smaller commands: 2 HIGH (sort/mod.rs:178 direct parallel of dedupe fix, show_element.rs:53 missing is_sorted check), 1 MEDIUM (getid removeid missing require_indexdata gate)

**Headline items worth landing first (cross-verified, real-effect, clear fix shape):**
1. ~~`commands/sort/mod.rs:178-181` - direct parallel of the already-fixed `cat::dedupe` kind-boundary bug. Add `&& entries[i].index.kind == run_kind` guard.~~ **LANDED.** Regression `sort_overlap_runs_scoped_to_single_kind` in `tests/sort.rs` pins the fix.
2. ~~`read/header_walker.rs:149-164` + `read/raw_frame.rs:65-67,124-127` - missing MAX_BLOB_HEADER_SIZE caps. Real DoS/OOM vector on adversarial input; add the same guard `BlobReader::read_blob_header` has.~~ **LANDED.** Three regression tests in `tests/corrupt_input.rs` pin the guards via the `has_indexdata`, `cat`, and `inspect` entry points.
3. ~~`apply_changes/scanner.rs:162,188` - under `--force --locations-on-ways` non-indexed, LocationsOnWays is silently stripped from base ways. Gate the combination at setup, or fix the barrier to recover from placeholder kinds.~~ **LANDED.** The combination is now rejected up front in `apply_changes::merge()` with a clear error that points at the `pbfhogg cat` indexed-generation workflow. Regression `merge_rejects_force_with_locations_on_ways_on_non_indexed` in `tests/merge.rs` pins the rejection. Barrier-recovery from placeholder kinds was not attempted - the combination has no performance argument (non-indexed already disqualifies the splice fast-path) and the cleaner failure mode is worth more than a half-working execution path.
4. ~~`commands/inspect/show_element.rs:53-57` - missing `is_sorted()` check produces false negatives on history/unsorted PBFs.~~ **LANDED.** Regression `show_element_unsorted_pbf_finds_target_in_later_blob` in `tests/inspect.rs` pins the fix on an unsorted fixture where the target lives in a later blob with a smaller `min_id`.
5. ~~`renumber/pass1.rs:179` + `wire_rewrite.rs:272` - negative-ID guards bypassed by stale indexdata; results in loud panic (pass1) or phantom orphans (wire_rewrite).~~ **LANDED.** The `check_negative_ids` parameter has been removed; the check is now unconditional at both sites. Existing `renumber_rejects_negative_node_id` and `renumber_rejects_negative_way_ref` regressions cover honest-indexdata; the fix also closes the lying-indexdata path that infrastructure we don't yet have would need to exercise directly.
6. `write/parallel_writer.rs:191-206` + `uring_writer.rs:588-655` - silent truncation when an upstream framer panics with items still held in the reorder buffer.

**Implication for planning:** the ~28% first-round error rate is not tolerable for landing decisions. Any "fix" based on the original finding text without independent code-reading has a high probability of mis-shaping the change. The most insidious mis-calls point at the right file but propose a wrong mechanism (877, 1, 10 in renumber, 16 in altw). Read the code path before writing a patch.

**Cross-cutting patterns the sweep surfaced:**

1. **Indexdata-trust without defensive check.** Multiple call sites read descriptor fields populated from `BlobHeader.indexdata` and act on them without verifying indexdata was present, tight, or correct. The mid-cycle non-indexed `--force` bugs were the most prominent instance; the audit found more across apply-changes, renumber, altw external, and geocode.

2. **Silent data loss on error paths in parallel writers.** The parallel-pwrite writer, io_uring writer, and parallel-gzip writer all have variants where a single worker's panic/error either hangs the pipeline waiting for a seq that never arrives, or silently truncates output with no diagnostic.

3. **Temp-file leaks on worker-error paths.** diff-parallel, altw external, apply-changes, and geocode Pass 3 Stage A all leak scratch files when a worker errors mid-stream. Individually small, collectively scruffy.

4. **Null Island (0,0) sentinel.** altw external uses `(lat==0 && lon==0)` as the missing-coord sentinel; real OSM nodes at (0°, 0°) are miscounted. Documented as a known limitation but still ships.

5. **Negative-ID and signed-arithmetic hazards.** renumber's negative-ID guard is gated on indexdata; diff shard planner uses raw numeric compare while element merge uses canonical `osm_id_cmp`; geocode's `set_atomic` has no guard for negative IDs.

### apply-changes pipeline

- [ ] **`apply_changes/drain.rs:537` / `streaming.rs:633` - HIGH** *(verified 2026-04-23, mechanism rewritten - original cursor-swallow claim was wrong)*. `infer_kind_and_range` returns the sentinel `(i64::MAX, i64::MIN)` for an empty block. When `process_item` unpacks that into `blob_osm_first_id(min_id, max_id)` at drain.rs:537, the `min_id >= 0` branch (osm_id.rs:70-78) returns `i64::MAX`, driving `handle_gap_creates` to emit every remaining upsert of that kind as a gap-create - severe wrong-output rather than the cursor-swallow the original finding described. (The cursor-swallow mechanism is inverted: `blob_osm_last_key(i64::MAX, i64::MIN)` is `(1, i64::MAX)` per osm_id.rs:20-28, strictly LESS than `(2, positive_id)`, so the cursor does NOT advance past remaining positive upserts.) Trigger: a `--force` non-indexed blob whose parsed contents are empty or whose walk finds no matching kind, reaching the rewrite path. Fix shape: detect the empty-block sentinel before drain handoff and either skip the item entirely or use a neutral range; do NOT propagate `(i64::MAX, i64::MIN)` through `blob_osm_first_id`.

- [x] ~~**`apply_changes/scanner.rs:162,188` - HIGH.**~~ *(landed 2026-04-23 by gating the combination at setup in `apply_changes::merge()` rather than trying to recover the barrier from placeholder kinds. Regression `merge_rejects_force_with_locations_on_ways_on_non_indexed` in `tests/merge.rs` pins the rejection.)*

- [ ] **`apply_changes/streaming.rs:496` - HIGH** *(verified 2026-04-23, mechanism narrowed)*. On the consumer build / `--direct-io` false-positive fallback path, `handle_owned_passthrough` is called with `id_range = desc.id_range.unwrap_or((0, 0))` and `kind = desc.kind` (scanner placeholder `Node` on non-indexed), so the drain receives `DrainItem::OwnedBytes { id_range: (0,0), kind: Node(placeholder!) }`. Confirmed effects: drain uses `item_kind` at drain.rs:527-535 for `handle_type_transition` and at :538 for `handle_gap_creates`; per-kind `base_*` stats at drain.rs:619-623 are miscredited. Correction: the original claim that "gap-create decisions" are broken via the upsert cursor is wrong - `OwnedBytes` arm does NOT advance the upsert cursor (drain.rs:656 comment). Real bug surface is type-transition + stats miscrediting, not cursor drift. Trigger: `--force` + consumer build (or `--direct-io`) merging a non-indexed PBF whose first way-blob has no overlap.

- [x] ~~**`apply_changes/streaming.rs:357` - HIGH.**~~ *(verified 2026-04-23 - deleted as dupe of streaming.rs:496 finding above.)* The comment at streaming.rs:350-358 is lexically inside the `ScannedBlob::Passthrough(desc)` arm and is correct in that scope (scanner fast-path requires indexed). The non-indexed false-positive path enters `handle_owned_passthrough` from `handle_candidate` at streaming.rs:496, which is a separate call site explicitly documented at its function-level doc (streaming.rs:637-641). The real bug surface is the `desc.id_range.unwrap_or((0, 0))` + scanner-placeholder `kind` from the already-filed streaming.rs:496 item; no additional docs-drift bug exists.

- [ ] **`apply_changes/rewrite.rs:244` - MEDIUM (diagnostic-quality)** *(verified 2026-04-23)*. If the scanner errors out mid-stream after sending some items but before all candidates are dispatched, some seqs never get produced; the drain hits its "channel closed with items still in reorder buffer" check at drain.rs:332 and returns an error whose diagnostic (`next_seq` vs smallest remaining) misleads away from the real upstream failure. Not a correctness bug - error surfaces, just two errors for one fault with the misleading one first. The true scanner error is surfaced separately via the scope join at rewrite.rs:337. Trigger: corrupted PBF header mid-stream. NOTE: drain.rs:330-338 original finding is the same concern from another angle - deduped into this entry.

- [ ] **`apply_changes/streaming.rs:242` - MEDIUM.** If a worker panics mid-stream, its `drain_tx` clone is dropped and other workers keep running, but seqs from the panicked worker's in-flight candidate are lost; the drain trips the reorder-buffer-non-empty check at drain.rs:330 and the panic only propagates when `std::thread::scope` returns, so the user sees "drain: channel closed with N items" rather than the real panic message. Trigger: OOM or unwrap in worker code path.

- [x] ~~**`apply_changes/drain.rs:304` - MEDIUM.**~~ *(verified 2026-04-23 - sentinel collision requires >18 quintillion blobs; planet is 4+ orders of magnitude away. Not a realistic bug.)*

- [ ] **`apply_changes/streaming.rs:420-445` - MEDIUM.** Modifications to existing base nodes (same ID, new coords) are covered only because `build_from_diff` in `node_locations.rs:51` is generous and inserts every diff node (create or modify) into `seeded_locations`; any future narrowing of `build_from_diff` would silently break coord freshness for way-refs to modified nodes. Trigger: latent invariant; regresses if the seeded-locations population is ever tightened.

- [x] ~~**`apply_changes/drain.rs:330-338` - MEDIUM.**~~ *(verified 2026-04-23 - duplicate of the `rewrite.rs:244` diagnostic-quality item above; same concern from a different angle. Deleted to dedupe.)*

- [x] ~~**`apply_changes/scanner.rs:252-257` - LOW.**~~ *(verified 2026-04-23 - no race.)* Workers flush `local_coords` into the shared `CoordSlot` at streaming.rs:467-471 BEFORE sending the DrainItem for the node blob. The drain only calls `barrier_publish_loc_map` after `state.next_seq > last_node_seq` (drain.rs:301-307, 316-322) - i.e., after every node DrainItem through `last_node_seq` has been dequeued and processed. Items are processed in seq order and workers flush before emitting, so by the time drain publishes the barrier, every node worker's coords are already in the shared slot. Scanner's try_recv cannot observe an open barrier before flushes complete. Deleted.

- [x] ~~**`apply_changes/drain.rs:754` - LOW.**~~ *(verified 2026-04-23 - `needed_set` filtering + per-blob single-worker ownership prevents collision by construction. Defensive note only.)*

- [ ] **`apply_changes/rewrite_block.rs:103` - LOW.** Upsert-create emission uses `osm_id_cmp(inline_upserts[cursor], elem_id).is_lt()`, but `inline_upserts` was pre-sliced using `osm_id_key` bounds in `streaming.rs::upsert_slice`; if the base block is not sorted in OSM order (malformed input), the cursor can skip past upserts that compare greater than one element but less than a later element, silently dropping creates. Trigger: malformed base PBF violating Sort.Type_then_ID (partially protected by `--locations-on-ways` requiring `is_sorted()`, but the general path doesn't).

- [x] ~~**`apply_changes/scanner.rs:268` - LOW.**~~ *(verified 2026-04-23 - documented protocol, not a latent hazard. Scanner blocks on barrier_rx is the intended wait; drain.rs:321's idle-fire is the designed counterpart. Deleted.)*

- [x] ~~**`apply_changes/drain.rs:751` - LOW.**~~ *(verified 2026-04-23 - defensive note only, no bug.)*

- [x] ~~**`apply_changes/streaming.rs:227` - LOW.**~~ *(verified 2026-04-23 - acknowledged trade-off; inline comment at streaming.rs:223-226 already calls this out.)*

### Write path (parallel-pwrite, io_uring, parallel gzip)

- [ ] **`write/parallel_writer.rs:191-206` - HIGH** *(verified 2026-04-23, narrowed from 148-156)*. The mid-stream error path is covered: `dispatch_loop` propagates via `let chunk = result?;` at :195 and `dispatch_chunk(...)?;` at :203, with the outer caller re-raising at :156. Real bug is narrower: the EOF path `while let Ok(item) = rx.recv()` at :191 returns `Ok(())` at :206 without asserting `pending` is empty. When the sender closes cleanly with a seq gap (an earlier framing task panicked/dropped without sending its seq), the gap chunks never pop from the `ReorderBuffer` and the file closes short silently. Trigger: rayon framing task panics on seq N (unwind drops tx half without surfacing an error through the dispatch channel) before sending seqs N-1 or earlier.

- [x] ~~**`write/parallel_writer.rs:141-176` - HIGH.**~~ *(verified 2026-04-23 - no observable bug. On error `dispatch_loop` returns via `result?`/err_slot and the operation is reported as failed; the file is not claimed as successful. `file.set_len` is intentionally unused because pwrite extends EOF on success paths. Deleted.)*

- [ ] **`write/parallel_writer.rs:180-207` + `uring_writer.rs:588-655` - HIGH.** Both loops break on `rx.recv() == Err` and return `Ok(())` without checking `pending.pending_len()`; sequencing gaps silently truncate output. Trigger: rayon framing-task panic on a middle seq causes tx drop; reorder buffer holds later seqs; writer exits cleanly with a truncated file.

- [x] ~~**`write/uring_writer.rs:400-410` - HIGH.**~~ *(verified 2026-04-23 - mechanism wrong. On pread error, `handle_copy_range_uring` returns Err and `uring_main_loop` propagates via `?`; `flush_final` is never reached on the error path. `UringState` has no Drop impl that calls `file.set_len`, so the described "Drop-path flush_final" does not exist. Inflation is real but unreachable. Deleted.)*

- [ ] **`write/parallel_gzip.rs:188-213` - LOW (downgraded 2026-04-23)** *(verified: not a live failure mode today)*. `compress_one` at :216 uses `GzEncoder::new(Vec::with_capacity(...), ...)`. `Vec<u8>` `Write` impl is infallible; flate2 doesn't return Err for OOM (it panics). So the `Err(_) => return` arm at :207 is effectively unreachable in current code. A worker panic unwinds rather than running the `return`, so this path does not reach the described hang via normal io::Error. The original trigger "flate2 OOM" is wrong. The hang is only reachable via (a) a future sink change to a fallible writer, or (b) a panic-caught-and-converted-to-Err path. Keep as a latent defensive-coding note; fix shape is still "on worker error/panic, poison the writer_loop channel or close it with a sentinel" but urgency drops with no live trigger.

- [x] ~~**`write/parallel_gzip.rs:78-103` - MEDIUM.**~~ *(verified 2026-04-23 - no in-lock panic site.)* At :193-200 the lock scope is a tight block: `raw_rx.lock()` then `guard.recv()`, then the block ends and the guard drops before the `match item` at :201. `guard.recv()` is `mpsc::Receiver::recv`, which returns Err on channel close rather than panicking. `compress_one` at :205 and `compressed_tx.send` at :209 run after guard drop. No realistic in-lock panic site exists with current code; mutex poisoning is not a live failure mode. Deleted.

- [ ] **`write/parallel_gzip.rs:170-184` - MEDIUM.** `Drop` silently swallows both the final-chunk `flush_current` error and the `writer_handle` join `io::Result`; inner-writer I/O errors vanish when callers forget `finish()`. Documented as "best-effort" but the failure mode is silent data loss. Trigger: any caller using RAII for `ParallelGzipWriter` with a file sink whose last writes matter.

- [ ] **`write/uring_writer.rs:324-340` - MEDIUM.** On a CQE carrying `result < 0`, `in_flight` is decremented but `self.pool.release(buf_idx)` is skipped - that buffer slot is leaked for the remaining lifetime of the writer. Not catastrophic because the writer errors anyway, but violates the accounting invariant and can surface as "no free buffers" on any caller that tries to continue. Trigger: kernel returns short write or EIO on a `WriteFixed` completion.

- [x] ~~**`write/parallel_writer.rs:278-297` - MEDIUM.**~~ *(verified 2026-04-23 - surfacing the error is correct; rerouting to healthy workers would produce file holes at the dead worker's queued offsets. Deleted.)*

- [ ] **`write/writer.rs:736-746` - MEDIUM.** `Drop for PbfWriter` joins the writer thread and discards the `io::Result`; for `to_path_parallel` and `to_path_uring`, the writer thread does the actual `sync_all` and (for uring) `set_len` truncation - any I/O error from those operations is lost if the caller drops without calling `flush()`. Documented hazard. Trigger: library user uses `to_path_parallel` without calling `flush()` and sync/truncate fails.

- [ ] **`write/parallel_writer.rs:399-430` - MEDIUM.** `copy_range_fallback_pwrite` loops via pread+pwrite but does not handle pread EINTR: a signal-interrupted pread returns `-1`/EINTR and the function errors immediately instead of retrying. Trigger: SIGWINCH or other signal delivered during a cross-device passthrough copy.

- [ ] **`write/copy_range.rs:63-96` - MEDIUM (latent).** `copy_range_fallback` writes to `out_fd` via `out.write_all`, which uses position-based writes; parallel contexts that reuse this function (not current callers) would race with other position-based writers on the same fd. Documented latent constraint the parallel writer's EXDEV fallback silently avoids by using pwrite. Trigger: future code reusing `copy_range_fallback` in a parallel context.

- [x] ~~**`write/uring_writer.rs:262-277` - LOW.**~~ *(verified 2026-04-23 - invariant sanity check, not a bug. Reachable only via a prior bug elsewhere. Deleted.)*

- [ ] **`write/writer.rs:108-145` - LOW.** `to_path_uring` handles a failed `init_rx.recv()` by `drop(tx); handle.join()` - if the uring thread is hung (e.g. blocked forever in `register_buffers` on a buggy kernel), the join hangs the main thread with no timeout. Trigger: pathological kernel behavior on `register_buffers` or `register_files`.

- [x] ~~**`write/parallel_writer.rs:54-61` - LOW.**~~ *(verified 2026-04-23 - pool saturation just means dispatch blocks, which is correct backpressure rather than a bug. Deleted.)*

- [x] ~~**`reorder_buffer.rs:21-33` - LOW.**~~ *(verified 2026-04-23 - intentional defensive panic on invariant violation. Stale/duplicate seq is an upstream programming error, not a runtime input. Deleted.)*

### renumber

- [x] ~~**`renumber/pass1.rs:179` - HIGH.**~~ *(landed 2026-04-23; `check_negative_ids` parameter removed, check is now unconditional in `reframe_dense_with_new_ids`. Node-side regression `renumber_rejects_negative_node_id` in `tests/renumber_external.rs` pins the clean error surface.)*

- [x] ~~**`renumber/wire_rewrite.rs:272` - HIGH.**~~ *(landed 2026-04-23; `check_negative_ids` parameter removed, check is now unconditional in `reframe_ways_with_new_ids` ahead of the `way_id_set.set(...)` call that would otherwise silently drop the id. Way-side regression `renumber_rejects_negative_way_ref` covers the clean error surface.)*

- [x] ~~**`renumber/stage2.rs:226-231` - MEDIUM.**~~ *(verified 2026-04-23 - finding's ordering is inverted; check fires BEFORE the fetch_add and tx.send at stage2.rs:233,241. Partial-write concern exists by a different mechanism but this specific claim is a false positive. Deleted.)*

- [ ] **`renumber/wire_rewrite.rs:293-296` - MEDIUM.** Way ref orphan detection does both `resolve(old)` AND `get(old)` - two chunk lookups per ref (~1.5 B refs on planet). `rank_if_set(old)` combines them and matches `resolve`'s internals exactly. Code-quality rather than correctness; flagged for the optimization bar. Trigger: none.

- [ ] **`renumber/relations.rs:297-298` - MEDIUM.** `stats.relations_written` accumulates from `rels_written.fetch_add(blob_count, ...)` which fires inside the worker BEFORE `tx.send` (line 232); `stats.orphan_refs` accumulates from `r2d_orphans.fetch_add` on the consumer side AFTER reorder (line 281). On a mid-stream error, `rels_written` counts blobs that were never emitted to output while `r2d_orphans` only counts orphans from blobs the consumer actually received. Summary counters disagree with actual output on error path. Trigger: reframe error in a middle blob.

- [ ] **`renumber/wire_rewrite.rs:486-491` - MEDIUM.** `memids_count` / `types_count` are derived by counting varint-terminator bytes in raw field data; correct only for well-formed varints, and a malformed trailing varint (missing continuation) would miscount and cause misalignment in the decode loop rather than clean error. Trigger: truncated/corrupt memids field.

- [ ] **`renumber/mod.rs:240` - MEDIUM.** `max_node_id = pass1_schedule.last().map_or(0, |t| t.max_id)` assumes the last node blob has the global max node ID; true for `Sort.Type_then_ID` (enforced by `require_sorted`), but if the header advertises sorted and the content is not (lying header), a later blob's id could overshoot `max_node_id` and `set_atomic` panics. Trigger: mis-flagged unsorted PBF.

- [x] ~~**`renumber/wire_rewrite.rs:580-584` - MEDIUM.**~~ *(verified 2026-04-23 - explicitly intentional and documented with an inline comment. By design. Deleted.)*

- [ ] **`renumber/wire_rewrite.rs:250,255,454,460` - MEDIUM.** `tag_start = val_start - 1` hard-codes a 1-byte field tag; correct for fields 1-15 (all low-numbered tags used here are <=15). If the PBF schema ever adds field >=16 that the rewriter needs to splice, `val_start - 1` would slice mid-varint and produce corrupt output silently. Add `debug_assert` tying tag byte count to field number. Trigger: future PBF schema addition.

- [ ] **`renumber/wire_rewrite.rs:519-524` - MEDIUM** *(verified 2026-04-23, mechanism nit)*. Real effect confirmed: negative `old_abs_id` flows through `get` (returns false via bounds-check early-return at idset.rs:216) and `resolve` (returns `id` unchanged via cid-out-of-bounds early-return at idset.rs:408, not a huge-cid chunk lookup as the finding stated). Output contains the negative value AND orphan count bumps. Fix shape unchanged: explicit negative-ID check before the orphan decision.

- [ ] **`renumber/mod.rs:256-262` - LOW.** `nodes_written != pass1_total_nodes` aborts AFTER pass1 output has been written and stage 2d is about to run; leaves a half-written file with no explicit output cleanup. Trigger: `task.element_count` sum mismatch.

- [x] ~~**`renumber/pass1.rs:217-220` - LOW.**~~ *(verified 2026-04-23 - finding conflates send-on-closed-channel with error-discard. If receiver is dropped, there's nothing to send to; the blob error is already packaged in `result`. False positive. Deleted.)*

- [ ] **`renumber/mod.rs:308-311` - LOW.** Way `id_sets` are merged by removing element 0 and folding the rest with `merge` (takes ownership); if `STAGE2D_WORKERS = 0` in a future tweak, `remove(0)` panics. Current constant is 6 but the shape is fragile - `merge_from` on a default-constructed set would be safer. Trigger: future refactor.

### altw external

- [ ] **`altw/external/stage2.rs:534` / `mod.rs:559` - HIGH (documented-accepted)** *(verified 2026-04-23, matches CORRECTNESS.md)*. Stage-2 uses `(lat == 0 && lon == 0)` as the missing-coord sentinel. Confirmed: sentinel still lives at stage2.rs:534 with an explicit block-comment at :522-533 cross-linking to the geocode counterpart. Already documented-accepted per CORRECTNESS.md "Null Island ambiguity in dense mmap index" - affects zero real-world nodes. Keep as-is until a real-world case surfaces; fix shape is a separate occupancy bitmap or explicit valid-bit track.

- [ ] **`altw/external/blob_meta.rs:49-50` - HIGH.** `scan_blob_metadata` errors with "OsmData blob missing indexdata" for any blob without indexdata, but `external_join` calls `require_indexdata(..., force, ...)` first, which accepts `--force` and returns success without indexdata - so `--force` on non-indexed input fails later with a confusing indexdata error instead of the gated `require_indexdata` message. The external path is effectively incompatible with `--force` despite the CLI accepting the flag. Trigger: `pbfhogg add-locations-to-ways --index-type external --force` on a non-indexed PBF.

- [x] ~~**`altw/external/stage4.rs:438-478` - HIGH.**~~ *(verified 2026-04-23 - the `assemble_block` path at stage4.rs:836-851 is an **explicit defensive hard error with a detailed diagnostic message**, not silent corruption. A hard error on out-of-spec input is correct defensive behaviour, not a defect. Deleted.)*

- [ ] **`altw/external/stage1.rs:269-273` + `stage2.rs:459-493` - MEDIUM.** Stage 2's blob-local rank counter (`next_rank = blob.ref_rank_start`, incremented per referenced tuple) is correct only if indexdata `(min_id, max_id)` tightly brackets actual node IDs in the blob. A producer with loose bounds plus the `debug_assert_eq!` at stage2.rs:488 passes in release and silently produces skewed ranks, scrambling the join. Trigger: input PBF with sloppy indexdata ranges from a third-party writer.

- [ ] **`altw/external/mod.rs:225-273` - MEDIUM.** If stage 1 returns an error, the scope closure short-circuits via `??` on `s1_handle.join()` before joining the relation-scan handle; `thread::scope` waits for `rel_handle` to finish, delaying error reporting by up to the scan's wall time (~4s Europe, longer planet). Trigger: any stage-1 failure while relation scan is running.

- [ ] **`altw/external/stage2.rs:67-72` - MEDIUM (latent).** `bucket_rank_end = ((bucket_idx + 1) * rank_range_size).min(unique_nodes)`; with `div_ceil(unique_nodes, NUM_BUCKETS)` as `rank_range_size`, a middle bucket can have `bucket_rank_start > unique_nodes` when `unique_nodes < NUM_BUCKETS`, and the subtraction at line 72 would underflow. Masked today by the `rank_bucket_counts[bucket_idx] == 0` early-continue at stage2.rs:355. Trigger: pathologically small inputs (`unique_nodes < 256`) plus future removal of the early-continue.

- [ ] **`altw/external/coord_payloads.rs:104-153` - MEDIUM.** `straddler_partials` allocates one `Mutex<Option<StraddlerPartial>>` per way blob (~57K at planet, ~3 MB) even though only a few hundred ever hold a value. Committed resident memory the design doc calls "only hundreds" but the implementation sizes for N. Trigger: any planet-scale run.

- [ ] **`altw/external/stage4.rs:645` - MEDIUM (perf)** *(verified 2026-04-23)*. Passthrough path takes `frame_read_buf` via `std::mem::take`; next iteration's `frame_read_buf.resize(frame_size, 0)` on the now-empty Vec forces a fresh allocation. Buffer reuse intent is defeated. Fix: use `std::mem::replace(&mut frame_read_buf, Vec::with_capacity(frame_size))` or pass by reference.

- [x] ~~**`altw/external/stage3.rs:289` - MEDIUM.**~~ *(verified 2026-04-23 - passthrough descriptors always set `kind: Some(meta.kind)` at stage4.rs:208. No path produces `kind=None` for a passthrough desc. Theoretical panic only. Deleted.)*

- [x] ~~**`altw/external/stage2.rs:155-181` - LOW.**~~ *(verified 2026-04-23 - description of current structure, not a bug. Deleted.)*

- [ ] **`altw/external/mod.rs:191` - LOW.** `ScratchDir::new` uses `output.parent().unwrap_or(Path::new("."))` - if `output` is a bare filename with no parent component, scratch files land in the current working directory. A user running from `/` or a tmpfs cwd while outputting to a large disk can land ~224 GB of scratch on the wrong filesystem. The dense path has the same pattern. Trigger: running external-join from a small-fs cwd.

- [ ] **`altw/external/stage2.rs:488-493` - LOW.** The `debug_assert_eq!(next_rank, blob.ref_rank_end, ...)` runs only in debug builds; in release a drifted rank counter silently produces wrong coord slice assignments. Promote to an always-on `return Err(...)` at negligible cost (once per blob, not per tuple). Trigger: upstream indexdata or node-scan regression.

- [ ] **`altw/external/stage4.rs:573-600` - LOW.** The consumer pre-seeds passthrough items with `reorder.push(desc.seq, ...)` for every passthrough descriptor before looping on decode results; if `passthrough_items` is very large (planet: ~5K relations + up to 40K nodes if `keep_untagged_nodes=true`), `ReorderBuffer` with initial capacity 32 grows to hold all of them plus the decode-in-flight set. Acceptable cost but the buffer is now effectively sized by `len(passthrough_items)`. Trigger: large relation/node-passthrough counts.

### diff / derive-changes shard-parallel

- [ ] **`diff/parallel.rs:138-142` / `derive_parallel.rs:136-142` - MEDIUM (latent - positive-only production PBFs)** *(verified 2026-04-23)*. `plan_shards` builds thresholds via raw `i64` compare while element-merge uses `osm_id_cmp` canonical order. Mechanism is correct; effect is only reachable on mixed-sign inputs, which production PBFs are not. Fix lives in this file but priority drops until a real negative-ID consumer surfaces. Same goes for findings #2 (single-sided emit) and #3 (merge_up_to .min()) in this area - all three share the raw-vs-canonical compare issue and are similarly latent.

- [ ] **`diff/parallel.rs:354-357` / `derive_parallel.rs:310-315, 324-329, 339-343, 354-358` - MEDIUM (latent)** *(verified 2026-04-23 - see the consolidated note on finding #1 above).*

- [ ] **`diff/parallel.rs:384` / `derive_parallel.rs:429` - MEDIUM (latent)** *(verified 2026-04-23 - same class as #1 above; collapses to correct bound for positive-only inputs).*

- [ ] **`diff/parallel.rs:735` / `derive_parallel.rs:854-856` - MEDIUM.** When `slot?` propagates a worker error in the main-thread concatenate loop, remaining already-successful `ShardOutput` slots are never visited, so their per-shard `.txt.tmp` / `.xml.tmp` files are never removed. The outer `derive_parallel` temp files have the same lifecycle gap. Trigger: any shard worker returning `Err` (decompression failure, short read) or phase error after earlier phase populated `scratch_dir`.

- [ ] **`diff/parallel.rs:700-730` / `derive_parallel.rs:817-850` - MEDIUM.** `std::thread::scope` panic handling: if a worker panics after creating scratch files, `h.join()` returns `Err` (converted to `io::Error::other("shard worker panicked")`) but the scratch files are never cleaned up - `scratch_dir` grows a `derive-par-*-{pid}-*` set on every failed run. Same class as above. Trigger: worker panic mid-shard.

- [x] ~~**`diff/parallel.rs:686` / `derive_parallel.rs:782` - LOW.**~~ *(verified 2026-04-23 - marker skip is observability-only, not correctness. Deleted.)*

- [x] ~~**`diff/parallel.rs:137` / `derive_parallel.rs:135` - LOW.**~~ *(verified 2026-04-23 - correct degenerate behaviour for single-blob input. Deleted.)*

- [ ] **`diff/parallel_gzip.rs:170-184` - LOW.** `Drop` swallows flush error when `finish()` was not called (also noted in write path). Trigger: caller relies on RAII.

- [x] ~~**`diff/parallel_gzip.rs:273-285` (test) - LOW (spec).**~~ *(verified 2026-04-23 - documented & asserted behaviour; sole consumer never emits empty streams. Deleted.)*

- [x] ~~**`diff/derive_parallel.rs:703` - LOW.**~~ *(verified 2026-04-23 - future-proofing line, not a bug. Deleted.)*

- [x] ~~**`diff/derive_parallel.rs:537-540` - LOW.**~~ *(verified 2026-04-23 - finding itself labels this confirmed intentional. Deleted.)*

- [x] ~~**`diff/parallel.rs:501-502` - LOW.**~~ *(verified 2026-04-23 - finding acknowledges "not supported yet" comment; intentional. Consider a CLI guard if it becomes a real user complaint. Deleted.)*

- [x] ~~**`diff/parallel.rs:142` / `derive_parallel.rs:142` - LOW.**~~ *(verified 2026-04-23 - source sequence is monotone non-decreasing, so consecutive dedup is sufficient. Not a bug. Deleted.)*

- [ ] **`diff/derive_parallel.rs:240-248` - LOW.** Per-shard scratch filenames are `derive-par-{creates|modifies|deletes}-{pid}-{kind_tag}-{shard_idx}.xml.tmp`; two `pbfhogg` processes with the same PID running concurrently in the same `scratch_dir` (container restart recycling PID) collide. Sequential `ChangeSink` has the same exposure - pre-existing latent class. Add random suffix for 0.4.0. Trigger: PID collision.

- [x] ~~**`diff/parallel.rs:755` - LOW.**~~ *(verified 2026-04-23 - marginal micro-perf, not a bug. Deleted.)*

### geocode builder v2

- [ ] **`geocode_index/builder/admin.rs:127-143` - HIGH.** `write_admin_data` accumulates `vertex_offset: u32` by adding `p.vertices.len() * NODE_COORD_SIZE` with no overflow check; past 4 GiB the offset silently wraps and subsequent polygons point to wrong vertices. No hard-error unlike the sibling u16::MAX overflows this cycle fixed. Trigger: planet-scale admin boundary geometry near the 4 GiB total-vertex-bytes boundary.

- [ ] **`geocode_index/builder/pass1_5.rs:102` - MEDIUM.** `set_atomic(r)` is called on raw way refs without filtering negative IDs; unlike `IdSet::set`, `set_atomic` does not guard `id < 0` and computes a chunk index from a huge u64 cast, panicking via `chunk_for_atomic`. Kills the whole parallel Pass 1.5 scan with a panic instead of a clean error. Trigger: corrupted PBF or test fixture containing a negative node ref in a way.

- [ ] **`geocode_index/builder/admin.rs:152-189` - MEDIUM.** `write_admin_index` tracks `byte_off: u32` for admin-entries file position with no overflow guard on `+= 2` / `+= 4` accumulators; past 4 GiB the offset wraps and cells after that point read garbage entries. Unlikely at today's scales but not rejected. Trigger: enough admin entries to exceed 4 GiB of entries data.

- [ ] **`geocode_index/builder/admin.rs:182` - MEDIUM.** `val = e.poly_index | INTERIOR_FLAG` corrupts `poly_index` silently when `poly_index >= 0x8000_0000`; interior-flagged entries lose their high bit and point to the wrong polygon. No guard on `admin_polygon_count`, `AdminPolygon` stored in `u32`. Trigger: more than 2,147,483,647 admin polygons (far future).

- [ ] **`geocode_index/builder/pass3.rs:152-167` - MEDIUM.** `parse_bucket_file` silently truncates any trailing bytes that don't form a complete 15-byte record (`count = data.len() / BUCKET_RECORD_SIZE`); if a bucket-writer flush fails partway (ENOSPC), the partial tail is silently dropped at Stage B with no diagnostic. Trigger: ENOSPC during Stage A writes.

- [ ] **`geocode_index/builder/pass3.rs:229-231` - MEDIUM.** On entry to `bucketed_cell_assignment_fused`, bucket dirs are blown away with `remove_dir_all`, but on error mid-Stage-A the dirs and partial buckets are left behind (no Drop/ScratchDir guard). A crash leaves ~256 temp files per bucket dir; subsequent build succeeds only because of the unconditional remove at top. Trigger: panic or I/O error between `create_dir_all` and the `remove_dir_all(bucket_dir).ok()` at end of Stage B.

- [x] ~~**`geocode_index/reader.rs:600-607` - MEDIUM.**~~ *(verified 2026-04-23 - defensive clamp, not silent drop; S2 returns exactly 8 neighbors at any non-face level. Not a bug. Deleted.)*

- [x] ~~**`geocode_index/reader.rs:765-801` - MEDIUM.**~~ *(verified 2026-04-23 - documented intentional design (inline comment: "Interior hint: skip PIP test (accepted approximation per spec)"). Deleted.)*

- [ ] **`geocode_index/builder/admin.rs:88-111` - MEDIUM.** Hole-in-outer containment check uses only `hole[0]` (first vertex) with `point_in_ring`; a hole whose first vertex happens to lie outside the outer ring (e.g. aggressive `simplify_ring` on outer) is discarded even though most of the hole is inside. Trigger: aggressive `simplify_ring` on outer reduces it past the hole's first vertex.

- [x] ~~**`geocode_index/reader.rs:597` - LOW.**~~ *(verified 2026-04-23 - harmless code smell, not a bug. Deleted.)*

- [ ] **`geocode_index/reader.rs:1033-1040` - LOW.** `segment_length` returns `approx_distance_sq().sqrt()` in radians; `way_length` / `accumulated_length` look like meters/length but are radians. Interpolation ratio is dimensionless so correct, but names mislead. Trigger: none (latent confusion source).

- [x] ~~**`geocode_index/builder/pass3.rs:119-120` - LOW.**~~ *(verified 2026-04-23 - mechanism claim wrong. S2 cell IDs encode face in top 3 bits + level/pos in the rest; at level 17 on one face, bits 4-8 vary substantially. Not the claimed skew. Deleted.)*

- [ ] **`geocode_index/reader.rs:800-831` - LOW.** `search_admin_all`'s `seen: Vec<u32>` uses linear `contains()` dedup; for points inside many overlapping admin boundaries (national + regional + municipal), O(n^2) per query. Latent scaling issue at the query API. Trigger: query API usage with deeply-nested admin overlap.

- [x] ~~**`geocode_index/format.rs:406-423` - LOW.**~~ *(verified 2026-04-23 - defensive behaviour, currently internally consistent. Not a bug. Deleted.)*

- [ ] **`geocode_index/builder/pass2.rs:295-304` - LOW.** Building-centroid uses integer division `sum_lat / count` on `i64` decimicrodegree sums; for ways spanning the antimeridian, the "centroid" sits on the wrong hemisphere. Not realistic in OSM but no antimeridian-aware averaging. Trigger: building polygon crossing +/-180 degrees.

### Read path infrastructure

- [x] ~~**`read/header_walker.rs:149-164` - HIGH.**~~ *(landed 2026-04-23; `MAX_BLOB_HEADER_SIZE` cap added to `HeaderWalker::next_header`, returns `BlobError::HeaderTooBig` cleanly. Regression `inspect_rejects_oversized_header_length_via_walker` pins it.)*

- [x] ~~**`read/raw_frame.rs:65-67, 124-127` - HIGH.**~~ *(landed 2026-04-23; `MAX_BLOB_HEADER_SIZE` caps added to both `read_raw_frame` and `read_blob_header_only`. Regressions `has_indexdata_rejects_oversized_header_length` + `cat_rejects_oversized_header_length` in `tests/corrupt_input.rs` pin them.)*

- [x] ~~**`read/pipeline.rs:148-219` - HIGH.**~~ *(verified 2026-04-23 - no deadlock.)* Stage 2 is spawned with `move` at pipeline.rs:149, taking ownership of `raw_rx` (captured in the `for ... in raw_rx` loop at :171). On pool-build failure, stage 2 sends the error via `dispatch_tx` at :166 and `return`s at :167, exiting the closure and dropping `raw_rx` as the closure's locals drop. Stage 1 at :119-125 loops over `blob_reader.enumerate()` calling `raw_tx.send(...)`; when `raw_rx` drops, `sync_channel::send` wakes blocked senders with `Err`, the `if ... .is_err() { break; }` at :121-123 exits the loop, stage 1 terminates cleanly, and stage 3 receives the `(0, Err)` and propagates via :246. No path exists where stage 2 holds `raw_rx` alive after the error return. Deleted.

- [ ] **`scan/classify.rs:59-95, 110-163` - HIGH (narrowed 2026-04-23)** *(verified)*. `build_classify_schedule` / `_split` don't explicitly check `data_offset + data_size <= file_size`. The walker's own `offset >= file_size` guard at `header_walker.rs:127` stops iteration before producing past-EOF entries as long as offsets stay monotonic, so the truncation case mostly surfaces as a clean error from the walker. Narrow residual: a corrupt header advertising `data_size` that reaches beyond EOF on the last blob still produces a bogus schedule entry; workers only fail at `read_exact_at`. Fix: one explicit bounds check in the schedule builder.

- [x] ~~**`read/decompress.rs:108-117, 74-84` - MEDIUM.**~~ *(verified 2026-04-23 - pool is an opportunistic cache, not a resource pool. Dropped Vec frees its allocation but the pool refills from fresh allocations as needed. Not a bug. Deleted.)*

- [x] ~~**`read/header_walker.rs:74-105` - MEDIUM.**~~ *(verified 2026-04-23 - intentional design. Header walker is explicitly buffered for tiny-read patterns; `--direct-io` concerns the data-path, which the workers' separate fds may still honour. Not a bug. Deleted.)*

- [ ] **`blob_meta/scan_ids.rs:192-202` - MEDIUM.** The coordinate conversion multiplies `gran * min_raw_lat` as i64 without overflow checking; on adversarial or bitrot-corrupted `granularity` / `lat_offset` fields the result wraps silently in release builds, producing a bogus bbox that then gets serialized into indexdata and trusted by every spatial filter downstream. Trigger: a PBF whose `granularity` field is set to `i32::MAX` combined with extreme delta-coded coords.

- [x] ~~**`read/indexed.rs:107-173` - MEDIUM.**~~ *(verified 2026-04-23 - `None` means "unknown", not "no elements". No current consumer treats it otherwise. Not a bug. Deleted.)*

- [x] ~~**`read/block.rs:338-407` - MEDIUM.**~~ *(verified 2026-04-23 - Rust drop order is language-guaranteed by field declaration order, and the safety comment at block.rs:344-352 pins the invariant. Not a bug. Deleted.)*

- [x] ~~**`read/direct_reader.rs:144-168` - MEDIUM.**~~ *(verified 2026-04-23 - math works correctly; doc-comment concern only. Not a bug. Deleted.)*

- [x] ~~**`read/pipeline.rs:184-210` - MEDIUM.**~~ *(verified 2026-04-23 - converting panic to io::Error is the idiomatic bridge to the non-panic-propagating channel architecture. Design choice, not a bug. Deleted.)*

- [x] ~~**`scan/classify.rs:36-43, 200, 304` - MEDIUM.**~~ *(verified 2026-04-23 - explicitly documented intentional behaviour at classify.rs:33-35. Not a bug. Deleted.)*

- [ ] **`read/blob.rs:670-681` - LOW.** `BlobReader::seek_raw` sets `self.offset = Some(ByteOffset(offset))` and does not reset `last_blob_ok`; if the previous iteration left `last_blob_ok = false` (after HeaderTooBig or InvalidDataSize), `seek_raw` succeeds but subsequent `next()` still short-circuits to `None`. Trigger: call `seek_raw` on a reader that just returned `Err` - iteration stays dead even though the user recovered via seek.

- [ ] **`reorder_buffer.rs:21-33` - LOW (read-path side; also noted in write-path sweep).** `ReorderBuffer::push` asserts `seq >= self.next_seq` and `self.pending[slot_idx].is_none()`; both are panics-on-caller-bug rather than Result errors. In the pipeline the sequence comes from `enumerate()` so these only fire if a rayon worker sends duplicate `(seq, ...)` tuples. If a new caller retries a seq on a transient error and sends it twice, the panic kills the pipeline thread. Trigger: add a retry loop in `run_pipeline` that re-sends on transient decode errors without updating seq tracking.

- [x] ~~**`read/blob.rs:263-274` - LOW.**~~ *(verified 2026-04-23 - working as designed; BufReader overrides default appropriately. Not a bug. Deleted.)*

### Smaller commands

- [x] ~~**`commands/sort/mod.rs:178-181` - HIGH.**~~ *(landed 2026-04-23; same fix shape as `cat::dedupe` commit `486d4d1`, regression `sort_overlap_runs_scoped_to_single_kind` in `tests/sort.rs` pre-fix 0 ways out of 6 → post-fix 10/10 nodes + 6/6 ways preserved in both all-features and consumer sweeps.)*

- [x] ~~**`commands/inspect/show_element.rs:53-57` - HIGH.**~~ *(landed 2026-04-23; gated the min_id early-exit on `HeaderBlock::is_sorted()` by decoding the OsmHeader blob up front. Regression `show_element_unsorted_pbf_finds_target_in_later_blob` in `tests/inspect.rs` passes in both feature sweeps.)*

- [ ] **`commands/getid/mod.rs:259` - MEDIUM.** `removeid` (invert mode) reaches `filter_by_id` without any `require_indexdata` / `--force` gate. On a non-indexed PBF, the raw-passthrough fast path at 332-360 is unreachable (branch is conditional on `meta.index.is_some()`), so every blob falls into the full-decode path at 364 with no user warning. Correct output but silently slow, and inconsistent with `getid` at line 238 which gates on indexdata. Trigger: `removeid` on a non-indexed PBF.

- [x] ~~**`commands/extract/simple.rs:310-315` - MEDIUM.**~~ *(verified 2026-04-23 - extract --simple is explicitly exempt from `require_indexdata` at mod.rs:629 by design (bbox scan on decoded elements doesn't need indexdata). Not a bug. Deleted.)*

- [x] ~~**`commands/check/verify_ids.rs:534-536` - MEDIUM.**~~ *(verified 2026-04-23 - explicitly documented at verify_ids.rs:524-533. Skip is correct: non-indexed schedule triplication would produce spurious violations. Non-`--full` path still provides element-level type-order checking. Deleted.)*

- [x] ~~**`commands/altw/passthrough.rs:285-286` - MEDIUM.**~~ *(verified 2026-04-23 - with kind=None, no passthrough happens, so the flush invariant is vacuously satisfied. Not a bug. Deleted.)*

- [x] ~~**`commands/extract/multi.rs:158-168` - MEDIUM (latent).**~~ *(verified 2026-04-23 - extract-to-zero-regions is a degenerate case prevented by config validation; code path unreachable. Deleted.)*

- [x] ~~**`commands/extract/mod.rs:518-524` - LOW.**~~ *(verified 2026-04-23 - minor inefficiency at most; Simple doesn't need indexdata. Deleted.)*

- [x] ~~**`commands/time_filter/mod.rs:179-184` - LOW.**~~ *(verified 2026-04-23 - OSM history-file convention is ascending-version order, documented at time_filter/mod.rs:73. Standard. Deleted.)*

- [x] ~~**`commands/check/refs.rs:141-146` - LOW.**~~ *(verified 2026-04-23 - explicit doc-comment at refs.rs:142-146 acknowledges and explains. Deleted.)*

- [x] ~~**`commands/cat/mod.rs:234-237` - LOW.**~~ *(verified 2026-04-23 - conservative inclusion of indexdata-less blobs is correct; can't tell the kind without decoding. Not a bug. Deleted.)*

- [ ] **`commands/tags_filter/mod.rs:778-785` - LOW.** Pass-2 schedule skips the type filter when `meta.index` is None (the entire filter check lives behind `if let Some(idx)` at 779), so non-indexed blobs go through pass-2 decode regardless of `blob_filter`. With `has_included_way=false && has_included_relation=false && invert=false`, the type filter would skip way/relation blobs; non-indexed they still decode, but `filter_block_pass2` correctly drops them (empty `included_way_ids`). Correct, wasteful. Trigger: `tags-filter` on non-indexed input with a narrow type filter.

- [ ] **`commands/inspect/scan.rs:61-119` - LOW.** `try_index_only_scan` unconditionally ignores `direct_io` (doc at 57-60). Safe because any non-indexed blob returns `None` at 91-92, triggering a fallback; but the pattern of "silently ignore a user flag when a fast path applies" is not surfaced in user output. Trigger: `inspect --direct-io` on any indexed PBF.
