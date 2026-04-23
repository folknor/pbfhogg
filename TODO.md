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

**Cross-cutting patterns the sweep surfaced:**

1. **Indexdata-trust without defensive check.** Multiple call sites read descriptor fields populated from `BlobHeader.indexdata` and act on them without verifying indexdata was present, tight, or correct. The mid-cycle non-indexed `--force` bugs were the most prominent instance; the audit found more across apply-changes, renumber, altw external, and geocode.

2. **Silent data loss on error paths in parallel writers.** The parallel-pwrite writer, io_uring writer, and parallel-gzip writer all have variants where a single worker's panic/error either hangs the pipeline waiting for a seq that never arrives, or silently truncates output with no diagnostic.

3. **Temp-file leaks on worker-error paths.** diff-parallel, altw external, apply-changes, and geocode Pass 3 Stage A all leak scratch files when a worker errors mid-stream. Individually small, collectively scruffy.

4. **Null Island (0,0) sentinel.** altw external uses `(lat==0 && lon==0)` as the missing-coord sentinel; real OSM nodes at (0°, 0°) are miscounted. Documented as a known limitation but still ships.

5. **Negative-ID and signed-arithmetic hazards.** renumber's negative-ID guard is gated on indexdata; diff shard planner uses raw numeric compare while element merge uses canonical `osm_id_cmp`; geocode's `set_atomic` has no guard for negative IDs.

### apply-changes pipeline

- [ ] **`apply_changes/drain.rs:560` / `streaming.rs:633` - HIGH.** `DrainItem::Rewritten` uses `id_range: (min_id, max_id)` verbatim for cursor advancement; `infer_kind_and_range` returns the sentinel `(i64::MAX, i64::MIN)` for an empty block, and `blob_osm_last_key(i64::MAX, i64::MIN)` computes a key larger than every remaining upsert, so the cursor loop advances past every remaining upsert of that kind, swallowing them. Trigger: a `--force` non-indexed blob whose parsed contents are empty or whose walk finds no matching kind, reaching the rewrite path.

- [ ] **`apply_changes/scanner.rs:162,188` - HIGH.** Under `--force --locations-on-ways` on non-indexed input, the scanner sets `kind=ElemKind::Node` placeholder for every blob, so `low_node_must_decompress = locations_on_ways && matches!(kind, Node)` is true for every blob (even ways/relations); the node->way barrier tracking at scanner.rs:214 treats way/relation blobs as nodes and never buffers them, `last_node_seq` is set to the seq of an actual way blob, and the drain publishes an empty/partial `loc_map` to way workers before real node blobs have been processed. Trigger: `--force --locations-on-ways` on a non-indexed PBF (arguably this combination should be rejected at setup, but nothing in `merge()` rejects it).

- [ ] **`apply_changes/streaming.rs:496` - HIGH.** On the consumer build / `--direct-io` false-positive fallback path, `handle_owned_passthrough` is called with `id_range = desc.id_range.unwrap_or((0, 0))` and `kind = desc.kind` (scanner placeholder `Node` on non-indexed), so the drain receives `DrainItem::OwnedBytes { id_range: (0,0), kind: Node(placeholder!) }`; the drain uses those values for type-transition detection and gap-create decisions, breaking drain state ordering when the actual blob is Way/Relation. Trigger: `--force` + consumer build (or `--direct-io`) merging a non-indexed PBF whose first way-blob has no overlap.

- [ ] **`apply_changes/streaming.rs:357` - HIGH.** The comment claims "Scanner only emits Passthrough for indexed blobs", but the `--force` path under `--direct-io` + consumer-build false positives takes a different route (`handle_owned_passthrough` from within `handle_candidate` at line 496 with `desc.kind`/`desc.id_range` still bearing the scanner placeholder `(Node, None)`). Same surface as the finding above; the docs-vs-reality drift is the hazard. Trigger: same as above.

- [ ] **`apply_changes/rewrite.rs:244` - MEDIUM.** If the scanner errors out mid-stream after sending some items but before all candidates are dispatched, some seqs never get produced; the drain hits its "channel closed with items still in reorder buffer" check at drain.rs:332 and returns an error whose diagnostic (`next_seq` vs smallest remaining) misleads away from the real upstream failure. Trigger: corrupted PBF header mid-stream.

- [ ] **`apply_changes/streaming.rs:242` - MEDIUM.** If a worker panics mid-stream, its `drain_tx` clone is dropped and other workers keep running, but seqs from the panicked worker's in-flight candidate are lost; the drain trips the reorder-buffer-non-empty check at drain.rs:330 and the panic only propagates when `std::thread::scope` returns, so the user sees "drain: channel closed with N items" rather than the real panic message. Trigger: OOM or unwrap in worker code path.

- [ ] **`apply_changes/drain.rs:304` - MEDIUM.** `u64::MAX` is the "no nodes at all" sentinel that fires the barrier immediately; if there is ever a node blob whose seq happens to be `u64::MAX` (trillion-blob file), the sentinel collides with a real seq. Planet is 4+ orders of magnitude away, but nothing in the protocol enforces that seqs stay below `MAX-1`. Trigger: latent; requires a file-scale assumption change.

- [ ] **`apply_changes/streaming.rs:420-445` - MEDIUM.** Modifications to existing base nodes (same ID, new coords) are covered only because `build_from_diff` in `node_locations.rs:51` is generous and inserts every diff node (create or modify) into `seeded_locations`; any future narrowing of `build_from_diff` would silently break coord freshness for way-refs to modified nodes. Trigger: latent invariant; regresses if the seeded-locations population is ever tightened.

- [ ] **`apply_changes/drain.rs:330-338` - MEDIUM.** The "channel closed with items still in reorder buffer" diagnostic fires on both genuine seq gaps and legitimate mid-stream scanner failure; the error text says "Producer dropped a seq" which points away from the real failure, and the drain returns its generic error before the scanner/worker error surfaces on the `.join()` in `rewrite.rs:337`. Two errors for one fault, less-useful one reported first. Trigger: any upstream error during apply-changes.

- [ ] **`apply_changes/scanner.rs:252-257` - LOW.** The opportunistic `try_recv()` on `barrier_rx` after every emitted node sets `barrier_open=true`; on small datasets where `last_node_seq` is reported to the drain before the corresponding node worker finishes, the drain can publish the barrier early (via the idle-fire path at drain.rs:321) before all node-phase coord extraction has flushed, letting way workers race against node coord flushing. Trigger: tiny datasets with specific interleaving.

- [ ] **`apply_changes/drain.rs:754` - LOW.** `merged.extend(local.drain())` - if two workers somehow end up with the same node ID (impossible under `needed_set` filtering but not enforced by type), the later insertion wins with undefined last-writer-wins semantics across worker boundaries. Trigger: malformed input (duplicate ID in base PBF) plus a `needed_set` regression.

- [ ] **`apply_changes/rewrite_block.rs:103` - LOW.** Upsert-create emission uses `osm_id_cmp(inline_upserts[cursor], elem_id).is_lt()`, but `inline_upserts` was pre-sliced using `osm_id_key` bounds in `streaming.rs::upsert_slice`; if the base block is not sorted in OSM order (malformed input), the cursor can skip past upserts that compare greater than one element but less than a later element, silently dropping creates. Trigger: malformed base PBF violating Sort.Type_then_ID (partially protected by `--locations-on-ways` requiring `is_sorted()`, but the general path doesn't).

- [ ] **`apply_changes/scanner.rs:268` - LOW.** At EOF under `--locations-on-ways` with zero way/relation blobs, the scanner blocks on `barrier_rx` waiting for the drain's signal; this works only because `drain.rs:321`'s idle-fire path eventually runs. No deadlock today but fragile. Trigger: refactor that removes the drain's idle-fire path.

- [ ] **`apply_changes/drain.rs:751` - LOW.** The swap-first-extend-rest heuristic for coord-map merging is fine; defensive note only. Trigger: none.

- [ ] **`apply_changes/streaming.rs:227` - LOW.** `Mutex<Receiver>` serializes worker `recv()` calls - fine at small N, but under `-j N` much larger than physical cores the lock becomes a real bottleneck (no fairness). No correctness issue. Trigger: adversarial `-j` setting.

### Write path (parallel-pwrite, io_uring, parallel gzip)

- [ ] **`write/parallel_writer.rs:148-156` - HIGH.** When `dispatch_loop` returns an error mid-stream or `rx.recv()` returns `Err` (sender drop/panic), the loop breaks and returns `Ok(())` without checking that `ReorderBuffer` is empty; seqs sent after a gap (framing task panicked before sending an earlier seq) are silently dropped and the file closes short. Trigger: rayon framing task panics on seq N before sending seqs N-1 or earlier.

- [ ] **`write/parallel_writer.rs:141-176` - HIGH.** The parallel writer never calls `file.set_len(...)`; correctness relies entirely on pwrite extending EOF. If one worker errors mid-stream and higher-offset workers have already completed pwrites, the file is left with holes (zero-filled by the kernel) inside - sparse state that `err_slot` surfaces as an error but with on-disk garbage up to the last dispatched offset. Trigger: worker hits EIO or EXDEV-fallback pread error while later-offset workers have already completed.

- [ ] **`write/parallel_writer.rs:180-207` + `uring_writer.rs:588-655` - HIGH.** Both loops break on `rx.recv() == Err` and return `Ok(())` without checking `pending.pending_len()`; sequencing gaps silently truncate output. Trigger: rayon framing-task panic on a middle seq causes tx drop; reorder buffer holds later seqs; writer exits cleanly with a truncated file.

- [ ] **`write/uring_writer.rs:400-410` - HIGH.** `handle_copy_range_uring` adds `remaining` to `state.logical_size` upfront (line 408) before doing any pread; if the pread loop errors mid-copy (short-read, EIO), `logical_size` is inflated beyond actual on-disk bytes. A subsequent Drop-path `flush_final` (or outer retry) calls `file.set_len(logical_size)`, extending the file past real data with kernel zeroes - exactly the "zero-filled gap" class this cycle was meant to fix, in the sibling error path. Trigger: pread short-read or errno during CopyRange pread, followed by any path reaching `set_len(logical_size)`.

- [ ] **`write/parallel_gzip.rs:188-213` - HIGH.** `worker_loop`'s `compress_one` error path does `return` silently; a worker that consumes a chunk from the mutex-receiver and then errors before sending compressed output permanently drops that seq, and the writer_loop stalls indefinitely on the BTreeMap gap until `raw_tx` closes (i.e. until the producer finishes), then errors with "N chunks missing". Hang in `finish()` waiting on the writer thread. Trigger: any compression error (flate2 OOM, etc.) in a single worker mid-stream.

- [ ] **`write/parallel_gzip.rs:78-103` - MEDIUM.** `Arc<Mutex<Receiver>>` is poisoned if any worker panics while holding the lock; all remaining workers then `Err` out of `raw_rx.lock()` and exit, collapsing the entire pool on one localized fault. Trigger: panic inside `guard.recv()` while the lock is held.

- [ ] **`write/parallel_gzip.rs:170-184` - MEDIUM.** `Drop` silently swallows both the final-chunk `flush_current` error and the `writer_handle` join `io::Result`; inner-writer I/O errors vanish when callers forget `finish()`. Documented as "best-effort" but the failure mode is silent data loss. Trigger: any caller using RAII for `ParallelGzipWriter` with a file sink whose last writes matter.

- [ ] **`write/uring_writer.rs:324-340` - MEDIUM.** On a CQE carrying `result < 0`, `in_flight` is decremented but `self.pool.release(buf_idx)` is skipped - that buffer slot is leaked for the remaining lifetime of the writer. Not catastrophic because the writer errors anyway, but violates the accounting invariant and can surface as "no free buffers" on any caller that tries to continue. Trigger: kernel returns short write or EIO on a `WriteFixed` completion.

- [ ] **`write/parallel_writer.rs:278-297` - MEDIUM.** `send_to_worker` returns `Err` if the target worker has exited, but the round-robin counter is not advanced and the dispatcher surfaces failure rather than rerouting to healthy workers; one dead worker takes down the whole dispatch loop even if 15 others are still functional. Trigger: one worker panics.

- [ ] **`write/writer.rs:736-746` - MEDIUM.** `Drop for PbfWriter` joins the writer thread and discards the `io::Result`; for `to_path_parallel` and `to_path_uring`, the writer thread does the actual `sync_all` and (for uring) `set_len` truncation - any I/O error from those operations is lost if the caller drops without calling `flush()`. Documented hazard. Trigger: library user uses `to_path_parallel` without calling `flush()` and sync/truncate fails.

- [ ] **`write/parallel_writer.rs:399-430` - MEDIUM.** `copy_range_fallback_pwrite` loops via pread+pwrite but does not handle pread EINTR: a signal-interrupted pread returns `-1`/EINTR and the function errors immediately instead of retrying. Trigger: SIGWINCH or other signal delivered during a cross-device passthrough copy.

- [ ] **`write/copy_range.rs:63-96` - MEDIUM (latent).** `copy_range_fallback` writes to `out_fd` via `out.write_all`, which uses position-based writes; parallel contexts that reuse this function (not current callers) would race with other position-based writers on the same fd. Documented latent constraint the parallel writer's EXDEV fallback silently avoids by using pwrite. Trigger: future code reusing `copy_range_fallback` in a parallel context.

- [ ] **`write/uring_writer.rs:262-277` - LOW.** `acquire_buffer` returns `io::Error::other("no free buffers and nothing in-flight")` as an invariant sanity check; only reachable after another bug (buffer leak from the CQE error path above) already corrupted state. Trigger: requires a prior bug.

- [ ] **`write/writer.rs:108-145` - LOW.** `to_path_uring` handles a failed `init_rx.recv()` by `drop(tx); handle.join()` - if the uring thread is hung (e.g. blocked forever in `register_buffers` on a buggy kernel), the join hangs the main thread with no timeout. Trigger: pathological kernel behavior on `register_buffers` or `register_files`.

- [ ] **`write/parallel_writer.rs:54-61` - LOW.** `POOL_SIZE=16` × `PER_WORKER_CAPACITY=8` max owned `Vec<u8>` per op - each `RawChunks` item splits into one `WriteOp` per chunk, so a single `RawChunks` with many tiny chunks saturates the pool with round-robin small writes, amplifying syscall count above the serial writer. Trigger: passthrough workload with many small framed blobs coalesced into `RawChunks`.

- [ ] **`reorder_buffer.rs:21-33` - LOW.** `push` asserts `seq >= self.next_seq` and panics on violation; a stale seq from a retried rayon task or a bug elsewhere aborts the writer thread rather than surfacing as `io::Error`. Panic propagates through `join().map_err(...)?` but in-flight data is lost. Trigger: retry or misuse of seq in a producer.

### renumber

- [ ] **`renumber/pass1.rs:179` - HIGH.** `check_negative_ids` is gated on `task.min_id < 0` from indexdata; if indexdata understates (stale/wrong index claims `min_id >= 0` when a negative ID is actually present), the negative-ID guard is skipped and `IdSet::set_atomic(negative_id)` casts to u64 -> chunk index at huge offset -> panics in `chunk_for_atomic`, OR silently corrupts the set if the offset lands inside `pre_allocate`'d range. Trigger: hand-crafted or edited-indexdata PBF with a negative node ID whose blob's indexdata reports `min_id >= 0`.

- [ ] **`renumber/wire_rewrite.rs:272` - HIGH.** Negative way_id only errors when `check_negative_ids=true` (per-blob `task.min_id < 0`); otherwise `way_id_set.set(old_way_id)` silently drops the negative (`IdSet::set` guards `id < 0`). The way survives to the output with a fresh new_id via `current_new_id` but is never recorded in `way_id_set`, so any relation member pointing to that way becomes a phantom orphan passing through with the stale negative memid. Trigger: negative way id in a blob whose indexdata `min_id` is non-negative.

- [ ] **`renumber/stage2.rs:226-231` - MEDIUM.** The per-blob `element_count` mismatch check fires after `nodes_written.fetch_add` etc., but before sending through the channel; bailing here leaves a partially-written file. Since `base_way_id` for subsequent blobs was precomputed from indexdata, retrying would produce gaps or overlaps - no clean recovery. Trigger: indexdata disagrees with blob content.

- [ ] **`renumber/wire_rewrite.rs:293-296` - MEDIUM.** Way ref orphan detection does both `resolve(old)` AND `get(old)` - two chunk lookups per ref (~1.5 B refs on planet). `rank_if_set(old)` combines them and matches `resolve`'s internals exactly. Code-quality rather than correctness; flagged for the optimization bar. Trigger: none.

- [ ] **`renumber/relations.rs:297-298` - MEDIUM.** `stats.relations_written` accumulates from `rels_written.fetch_add(blob_count, ...)` which fires inside the worker BEFORE `tx.send` (line 232); `stats.orphan_refs` accumulates from `r2d_orphans.fetch_add` on the consumer side AFTER reorder (line 281). On a mid-stream error, `rels_written` counts blobs that were never emitted to output while `r2d_orphans` only counts orphans from blobs the consumer actually received. Summary counters disagree with actual output on error path. Trigger: reframe error in a middle blob.

- [ ] **`renumber/wire_rewrite.rs:486-491` - MEDIUM.** `memids_count` / `types_count` are derived by counting varint-terminator bytes in raw field data; correct only for well-formed varints, and a malformed trailing varint (missing continuation) would miscount and cause misalignment in the decode loop rather than clean error. Trigger: truncated/corrupt memids field.

- [ ] **`renumber/mod.rs:240` - MEDIUM.** `max_node_id = pass1_schedule.last().map_or(0, |t| t.max_id)` assumes the last node blob has the global max node ID; true for `Sort.Type_then_ID` (enforced by `require_sorted`), but if the header advertises sorted and the content is not (lying header), a later blob's id could overshoot `max_node_id` and `set_atomic` panics. Trigger: mis-flagged unsorted PBF.

- [ ] **`renumber/wire_rewrite.rs:580-584` - MEDIUM.** Non-relation fields inside a relation blob's PrimitiveGroup are silently dropped (nodes/ways/changesets). Protected by `require_sorted` today but fragile to any sort-enforcement bypass. Trigger: non-spec PBF bypassing the sort gate.

- [ ] **`renumber/wire_rewrite.rs:250,255,454,460` - MEDIUM.** `tag_start = val_start - 1` hard-codes a 1-byte field tag; correct for fields 1-15 (all low-numbered tags used here are <=15). If the PBF schema ever adds field >=16 that the rewriter needs to splice, `val_start - 1` would slice mid-varint and produce corrupt output silently. Add `debug_assert` tying tag byte count to field number. Trigger: future PBF schema addition.

- [ ] **`renumber/wire_rewrite.rs:519-524` - MEDIUM.** For `member_type in {0,1,2}`, orphan is computed as `!id_set.get(old_abs_id)`; a negative `old_abs_id` from a broken delta stream flows to `get` which casts to u64 -> huge cid -> returns false -> counted as "orphan" and passed through with the negative value. Technically correct but conflates legitimate orphans with corrupt input. Trigger: corrupt memids delta.

- [ ] **`renumber/mod.rs:256-262` - LOW.** `nodes_written != pass1_total_nodes` aborts AFTER pass1 output has been written and stage 2d is about to run; leaves a half-written file with no explicit output cleanup. Trigger: `task.element_count` sum mismatch.

- [ ] **`renumber/pass1.rs:217-220` - LOW.** `tx.send(...).is_err()` break discards the blob error silently; if the consumer returned due to unrelated writer failure, workers exit cleanly but any in-flight worker error that hadn't yet sent is lost. First-observed-error propagation is not guaranteed. Trigger: concurrent worker-decompress-error + consumer-write-error.

- [ ] **`renumber/mod.rs:308-311` - LOW.** Way `id_sets` are merged by removing element 0 and folding the rest with `merge` (takes ownership); if `STAGE2D_WORKERS = 0` in a future tweak, `remove(0)` panics. Current constant is 6 but the shape is fragile - `merge_from` on a default-constructed set would be safer. Trigger: future refactor.

### altw external

- [ ] **`altw/external/stage2.rs:534` / `mod.rs:559` - HIGH.** Stage-2 resolved-count uses `(lat == 0 && lon == 0)` as the missing-coord sentinel; a real OSM node at Null Island (0°, 0°) is classified as unresolved, and `stats.missing_locations = total_slots - resolved_count` is derived from this. The external path reports higher `missing_locations` than the dense path for inputs containing any ref to a (0,0) node. Documented as known limitation, still ships. Trigger: any way that references a node whose coordinates are exactly zero.

- [ ] **`altw/external/blob_meta.rs:49-50` - HIGH.** `scan_blob_metadata` errors with "OsmData blob missing indexdata" for any blob without indexdata, but `external_join` calls `require_indexdata(..., force, ...)` first, which accepts `--force` and returns success without indexdata - so `--force` on non-indexed input fails later with a confusing indexdata error instead of the gated `require_indexdata` message. The external path is effectively incompatible with `--force` despite the CLI accepting the flag. Trigger: `pbfhogg add-locations-to-ways --index-type external --force` on a non-indexed PBF.

- [ ] **`altw/external/stage4.rs:438-478` - HIGH.** The way decode path assumes every way blob's indexdata kind is `Way`; any way element appearing inside a non-Way-indexed blob reaches `assemble_block` at stage4.rs:836-851 which hard-errors because `coord_payloads` is keyed by way-blob index only. Mixed-kind or mis-labeled blobs slipping past sort enforcement hard-error stage 4 rather than produce output. Trigger: input indexdata whose `kind` disagrees with blob contents (out-of-spec writer or relaxed `require_sorted`).

- [ ] **`altw/external/stage1.rs:269-273` + `stage2.rs:459-493` - MEDIUM.** Stage 2's blob-local rank counter (`next_rank = blob.ref_rank_start`, incremented per referenced tuple) is correct only if indexdata `(min_id, max_id)` tightly brackets actual node IDs in the blob. A producer with loose bounds plus the `debug_assert_eq!` at stage2.rs:488 passes in release and silently produces skewed ranks, scrambling the join. Trigger: input PBF with sloppy indexdata ranges from a third-party writer.

- [ ] **`altw/external/mod.rs:225-273` - MEDIUM.** If stage 1 returns an error, the scope closure short-circuits via `??` on `s1_handle.join()` before joining the relation-scan handle; `thread::scope` waits for `rel_handle` to finish, delaying error reporting by up to the scan's wall time (~4s Europe, longer planet). Trigger: any stage-1 failure while relation scan is running.

- [ ] **`altw/external/stage2.rs:67-72` - MEDIUM (latent).** `bucket_rank_end = ((bucket_idx + 1) * rank_range_size).min(unique_nodes)`; with `div_ceil(unique_nodes, NUM_BUCKETS)` as `rank_range_size`, a middle bucket can have `bucket_rank_start > unique_nodes` when `unique_nodes < NUM_BUCKETS`, and the subtraction at line 72 would underflow. Masked today by the `rank_bucket_counts[bucket_idx] == 0` early-continue at stage2.rs:355. Trigger: pathologically small inputs (`unique_nodes < 256`) plus future removal of the early-continue.

- [ ] **`altw/external/coord_payloads.rs:104-153` - MEDIUM.** `straddler_partials` allocates one `Mutex<Option<StraddlerPartial>>` per way blob (~57K at planet, ~3 MB) even though only a few hundred ever hold a value. Committed resident memory the design doc calls "only hundreds" but the implementation sizes for N. Trigger: any planet-scale run.

- [ ] **`altw/external/stage4.rs:645` - MEDIUM.** Passthrough path takes `frame_read_buf` via `std::mem::take` and passes into `writer.write_raw_owned`; the reuse buffer at line 601 is re-initialised via `frame_read_buf.resize(frame_size, 0)` each iteration, so take-and-discard forces a fresh allocation per passthrough blob rather than reusing. Accumulates small-allocator churn across thousands of relation blobs at planet scale. Trigger: any run with many passthrough blobs.

- [ ] **`altw/external/stage3.rs:289` - MEDIUM.** A stage-3 worker error calls `router.abort(...)` and stores the error in `err_ref`; the `expect` at stage4.rs:590 (`desc.kind.expect("passthrough eligibility requires a known blob kind")`) panics visibly if any blob reaches stage 4 with `kind = None` after a stage-3 abort. Shouldn't happen given `require_indexdata`, but the panic is user-visible if invariants slip. Trigger: indexdata invariant violation concurrent with a stage-3 abort.

- [ ] **`altw/external/stage2.rs:155-181` - LOW.** `SharedSlotBuckets::finish` flushes each writer behind its own mutex; writers stay open and are dropped implicitly at scope exit. Stage 3 opens files via `std::fs::File::open` after `finish()` runs (sequential structure), so no race today - but the ordering is not asserted anywhere beyond code structure. Trigger: refactor that moves stage 3 into a scope overlapping with stage 2.

- [ ] **`altw/external/mod.rs:191` - LOW.** `ScratchDir::new` uses `output.parent().unwrap_or(Path::new("."))` - if `output` is a bare filename with no parent component, scratch files land in the current working directory. A user running from `/` or a tmpfs cwd while outputting to a large disk can land ~224 GB of scratch on the wrong filesystem. The dense path has the same pattern. Trigger: running external-join from a small-fs cwd.

- [ ] **`altw/external/stage2.rs:488-493` - LOW.** The `debug_assert_eq!(next_rank, blob.ref_rank_end, ...)` runs only in debug builds; in release a drifted rank counter silently produces wrong coord slice assignments. Promote to an always-on `return Err(...)` at negligible cost (once per blob, not per tuple). Trigger: upstream indexdata or node-scan regression.

- [ ] **`altw/external/stage4.rs:573-600` - LOW.** The consumer pre-seeds passthrough items with `reorder.push(desc.seq, ...)` for every passthrough descriptor before looping on decode results; if `passthrough_items` is very large (planet: ~5K relations + up to 40K nodes if `keep_untagged_nodes=true`), `ReorderBuffer` with initial capacity 32 grows to hold all of them plus the decode-in-flight set. Acceptable cost but the buffer is now effectively sized by `len(passthrough_items)`. Trigger: large relation/node-passthrough counts.

### diff / derive-changes shard-parallel

- [ ] **`diff/parallel.rs:138-142` / `derive_parallel.rs:136-142` - MEDIUM.** `plan_shards` builds thresholds as raw `i64` from `old_descs[k].index.max_id`, but element-merge inside shards uses `osm_id_cmp` (canonical OSM order: `0, -1, -2, ..., 1, 2, ...`). If inputs contain negative IDs (synthetic extracts, test fixtures, some editor outputs), threshold ordering and element ordering disagree: a shard with `(t_low=MIN, t_high=numericPositive)` sees its iterator yield negatives first, then positives, and the `id <= t_low`/`id > t_high` clip is raw numeric. Trigger: any PBF with a negative element ID plus `-j >= 2`.

- [ ] **`diff/parallel.rs:354-357` / `derive_parallel.rs:310-315, 324-329, 339-343, 354-358` - MEDIUM.** Single-sided emit does `if id > t_high { break; }` / `if id <= t_low { continue; }` on raw numeric compare; sorted order inside a blob is canonical. On mixed-sign blobs (min_id < 0 < max_id), the `break` fires prematurely or late relative to canonical order, producing drops or double-emits under `-j >= 2`. Trigger: blob spans zero combined with shard window entirely on one sign side.

- [ ] **`diff/parallel.rs:384` / `derive_parallel.rs:429` - MEDIUM.** `let merge_up_to = os.index.max_id.min(ns.index.max_id).min(t_high);` uses numeric `.min()` on IDs, not `osm_id_cmp`; collapses to correct bound for all-positive inputs, but picks the wrong merge-horizon on mixed-sign inputs and can skip residual processing of a still-pending side. Trigger: same as above.

- [ ] **`diff/parallel.rs:735` / `derive_parallel.rs:854-856` - MEDIUM.** When `slot?` propagates a worker error in the main-thread concatenate loop, remaining already-successful `ShardOutput` slots are never visited, so their per-shard `.txt.tmp` / `.xml.tmp` files are never removed. The outer `derive_parallel` temp files have the same lifecycle gap. Trigger: any shard worker returning `Err` (decompression failure, short read) or phase error after earlier phase populated `scratch_dir`.

- [ ] **`diff/parallel.rs:700-730` / `derive_parallel.rs:817-850` - MEDIUM.** `std::thread::scope` panic handling: if a worker panics after creating scratch files, `h.join()` returns `Err` (converted to `io::Error::other("shard worker panicked")`) but the scratch files are never cleaned up - `scratch_dir` grows a `derive-par-*-{pid}-*` set on every failed run. Same class as above. Trigger: worker panic mid-shard.

- [ ] **`diff/parallel.rs:686` / `derive_parallel.rs:782` - LOW.** `if phase.old.is_empty() && phase.new.is_empty() { continue; }` silently skips emitting `DIFF_PHASE_<tag>_START/END` markers for empty phases; sequential `diff_block_pair` emits them unconditionally. Brokkr `--durations` / `--compare` will see misaligned phase sets between `-j 1` and `-j N` runs on datasets with one type kind missing. Trigger: synthetic / region-extract PBFs with zero ways or zero relations.

- [ ] **`diff/parallel.rs:137` / `derive_parallel.rs:135` - LOW.** `let n = target_count.min(old_descs.len()).max(1);` - when `old_descs.len() == 1`, `n = 1` and `-j 16` degenerates to one worker + zero parallelism with full thread/FIFO setup cost. Not a bug; worth confirming benchmark expectations. Trigger: single-blob kind at high `-j`.

- [ ] **`diff/parallel_gzip.rs:170-184` - LOW.** `Drop` swallows flush error when `finish()` was not called (also noted in write path). Trigger: caller relies on RAII.

- [ ] **`diff/parallel_gzip.rs:273-285` (test) - LOW (spec).** Truly-empty input yields zero bytes, not a valid gzip file; `assemble_osc_from_paths` always writes XML prologue + root element first, so production never hits this. Latent contract issue for future callers. Trigger: empty stream through `ParallelGzipWriter` from a future caller.

- [ ] **`diff/derive_parallel.rs:703` - LOW.** `let _ = element_version;` inside `write_delete_element` with comment claiming "silence unused helper"; the function is actually imported at module level and used transitively via `write_element_xml`. Comment/documentation mismatch, no correctness impact. Trigger: none.

- [ ] **`diff/derive_parallel.rs:537-540` - LOW (confirmed intentional).** `element_merge` Equal branch only calls `emit_create(modifies_w, &n, ...)` if `!borrowed_elements_equal(&o, &n)`; modifies emit new element verbatim via `write_element_xml`, ignoring `increment_version` and `update_timestamp` - matches sequential path's `write_modify` which also calls `write_element_xml` without applying those flags. Documented invariant: increment/update only apply to deletes. Trigger: none.

- [ ] **`diff/parallel.rs:501-502` - LOW.** `let _ = options.verbose;` with comment "Verbose details are not supported on the parallel path yet." If a user passes `-v -j 4`, verbose is silently dropped with no warning and no `modified` detail lines. Consider CLI-layer guard ("--verbose requires -j 1") over silent drop. Trigger: `diff -v -j 4`.

- [ ] **`diff/parallel.rs:142` / `derive_parallel.rs:142` - LOW.** `thresholds.dedup()` collapses only consecutive duplicates; because `old_descs` are sorted by id range (`max_id` monotone), thresholds are monotone non-decreasing and `dedup()` is sufficient. Undocumented invariant. Trigger: none.

- [ ] **`diff/derive_parallel.rs:240-248` - LOW.** Per-shard scratch filenames are `derive-par-{creates|modifies|deletes}-{pid}-{kind_tag}-{shard_idx}.xml.tmp`; two `pbfhogg` processes with the same PID running concurrently in the same `scratch_dir` (container restart recycling PID) collide. Sequential `ChangeSink` has the same exposure - pre-existing latent class. Add random suffix for 0.4.0. Trigger: PID collision.

- [ ] **`diff/parallel.rs:755` - LOW.** `append_and_cleanup` uses `io::copy` without `BufReader`; source file was just written by a `BufWriter<File>` that was flushed/dropped, so unbuffered reads work but do one syscall per `io::copy` kernel-default chunk. Sibling in `derive_parallel.rs:904` wraps in `BufReader` on open - inconsistent. Trigger: none.

### geocode builder v2

- [ ] **`geocode_index/builder/admin.rs:127-143` - HIGH.** `write_admin_data` accumulates `vertex_offset: u32` by adding `p.vertices.len() * NODE_COORD_SIZE` with no overflow check; past 4 GiB the offset silently wraps and subsequent polygons point to wrong vertices. No hard-error unlike the sibling u16::MAX overflows this cycle fixed. Trigger: planet-scale admin boundary geometry near the 4 GiB total-vertex-bytes boundary.

- [ ] **`geocode_index/builder/pass1_5.rs:102` - MEDIUM.** `set_atomic(r)` is called on raw way refs without filtering negative IDs; unlike `IdSet::set`, `set_atomic` does not guard `id < 0` and computes a chunk index from a huge u64 cast, panicking via `chunk_for_atomic`. Kills the whole parallel Pass 1.5 scan with a panic instead of a clean error. Trigger: corrupted PBF or test fixture containing a negative node ref in a way.

- [ ] **`geocode_index/builder/admin.rs:152-189` - MEDIUM.** `write_admin_index` tracks `byte_off: u32` for admin-entries file position with no overflow guard on `+= 2` / `+= 4` accumulators; past 4 GiB the offset wraps and cells after that point read garbage entries. Unlikely at today's scales but not rejected. Trigger: enough admin entries to exceed 4 GiB of entries data.

- [ ] **`geocode_index/builder/admin.rs:182` - MEDIUM.** `val = e.poly_index | INTERIOR_FLAG` corrupts `poly_index` silently when `poly_index >= 0x8000_0000`; interior-flagged entries lose their high bit and point to the wrong polygon. No guard on `admin_polygon_count`, `AdminPolygon` stored in `u32`. Trigger: more than 2,147,483,647 admin polygons (far future).

- [ ] **`geocode_index/builder/pass3.rs:152-167` - MEDIUM.** `parse_bucket_file` silently truncates any trailing bytes that don't form a complete 15-byte record (`count = data.len() / BUCKET_RECORD_SIZE`); if a bucket-writer flush fails partway (ENOSPC), the partial tail is silently dropped at Stage B with no diagnostic. Trigger: ENOSPC during Stage A writes.

- [ ] **`geocode_index/builder/pass3.rs:229-231` - MEDIUM.** On entry to `bucketed_cell_assignment_fused`, bucket dirs are blown away with `remove_dir_all`, but on error mid-Stage-A the dirs and partial buckets are left behind (no Drop/ScratchDir guard). A crash leaves ~256 temp files per bucket dir; subsequent build succeeds only because of the unconditional remove at top. Trigger: panic or I/O error between `create_dir_all` and the `remove_dir_all(bucket_dir).ok()` at end of Stage B.

- [ ] **`geocode_index/reader.rs:600-607` - MEDIUM.** `cell_neighborhood` silently drops neighbors beyond index 8 when `all_neighbors` returns more than `MAX_NEIGHBORHOOD - 1 = 8`. S2 normally yields at most 8 edge/corner neighbors, but face cells (level 0) or near face-corner topology can return a different count; the buffer truncates without warning. Trigger: query near an S2 face boundary where the cell's neighbor set is degenerate.

- [ ] **`geocode_index/reader.rs:765-801` - MEDIUM.** `search_admin_ranked` skips the PIP test on any entry with the interior hint (`is_interior || contains(...)`); a smaller interior-hinted polygon that doesn't actually contain the point (hint is cell-level, not point-level) can win over a correct larger containing polygon because the area check only gates on `poly.area < best_by_level[level].1`. Trigger: query near the cell-vs-polygon boundary for a small polygon marked interior-hint for a nearby cell.

- [ ] **`geocode_index/builder/admin.rs:88-111` - MEDIUM.** Hole-in-outer containment check uses only `hole[0]` (first vertex) with `point_in_ring`; a hole whose first vertex happens to lie outside the outer ring (e.g. aggressive `simplify_ring` on outer) is discarded even though most of the hole is inside. Trigger: aggressive `simplify_ring` on outer reduces it past the hole's first vertex.

- [ ] **`geocode_index/reader.rs:597` - LOW.** `cell_neighborhood` uses `.iter().take(8)` redundantly with `min(8)`; code smell, not a bug. Trigger: none.

- [ ] **`geocode_index/reader.rs:1033-1040` - LOW.** `segment_length` returns `approx_distance_sq().sqrt()` in radians; `way_length` / `accumulated_length` look like meters/length but are radians. Interpolation ratio is dimensionless so correct, but names mislead. Trigger: none (latent confusion source).

- [ ] **`geocode_index/builder/pass3.rs:119-120` - LOW.** `bucket_for_cell` uses top 8 bits; on any single face, all cells at level 17 share the same top 3 bits - only 32 bucket values per face exercised, heavily skewed bucket sizes. Not correctness, just uneven Stage B parallelism. Trigger: any build (perf-only note).

- [ ] **`geocode_index/reader.rs:800-831` - LOW.** `search_admin_all`'s `seen: Vec<u32>` uses linear `contains()` dedup; for points inside many overlapping admin boundaries (national + regional + municipal), O(n^2) per query. Latent scaling issue at the query API. Trigger: query API usage with deeply-nested admin overlap.

- [ ] **`geocode_index/format.rs:406-423` - LOW.** `parse_rings` drops rings with `<3` vertices silently (correct), but doesn't close unclosed rings; if an admin writer ever stops duplicating the closing vertex, the reader path still works but vertex-count becomes ambiguous. Currently internally consistent. Trigger: future writer change.

- [ ] **`geocode_index/builder/pass2.rs:295-304` - LOW.** Building-centroid uses integer division `sum_lat / count` on `i64` decimicrodegree sums; for ways spanning the antimeridian, the "centroid" sits on the wrong hemisphere. Not realistic in OSM but no antimeridian-aware averaging. Trigger: building polygon crossing +/-180 degrees.

### Read path infrastructure

- [ ] **`read/header_walker.rs:149-164` - HIGH.** `HeaderWalker::next_header` trusts the 4-byte length prefix without any `MAX_BLOB_HEADER_SIZE` cap; a corrupt or malicious file can force a multi-GB `Vec` resize on the fallback path (`self.header_buf.resize(header_end, 0)` on line 160 is unchecked). The old `BlobReader::read_blob_header` rejected this via `HeaderTooBig` but the new pread-only primitive lost the guard. Trigger: feed any PBF where the first four bytes of some blob frame encode `0x7FFFFFFF`.

- [ ] **`read/raw_frame.rs:65-67, 124-127` - HIGH.** `read_raw_frame` and `read_blob_header_only` both lack the `MAX_BLOB_HEADER_SIZE` guard before `vec![0u8; header_len]`; both are exercised by `has_indexdata`, `check_sorted_and_indexed`, and cat/diff/extract passthrough paths, so any command that probes an adversarial file gets an OOM-sized allocation instead of a clean `BlobError::HeaderTooBig`. Trigger: same as above, via any command touching these helpers instead of `BlobReader::read_blob_header`.

- [ ] **`read/pipeline.rs:148-219` - HIGH.** Stage 2 dispatcher builds a fresh `rayon::ThreadPoolBuilder` per pipeline invocation; if the build fails it emits a single `(0, Err(...))` via `dispatch_tx.send` but never consumes `raw_rx`, so the Stage 1 reader thread blocks forever on `raw_tx.send` once the 16-slot buffer fills and the scope never terminates. Trigger: exhausted thread budget / `rlimit(NPROC)` on the dispatcher spawn; also fires if any environment (cgroups, seccomp) denies rayon worker thread creation mid-run.

- [ ] **`scan/classify.rs:59-95, 110-163` - HIGH.** `build_classify_schedule` and `build_classify_schedules_split` do not check `data_offset + data_size <= file_size`; a truncated or partially-written PBF yields a schedule containing offsets past EOF, and workers only fail at `read_exact_at` after a lot of pread traffic has already happened (and in the `_split` case, the truncation is silently replicated across all three per-kind schedules). Trigger: any truncated upload or mid-write snapshot.

- [ ] **`read/decompress.rs:108-117, 74-84` - MEDIUM.** `pool_get` / `pool_get_pub` call `buf.reserve(...)` on the vec returned from the pool, but if the caller bails out with `?` before wrapping via `pool_wrap`, the vec is dropped - the 4 MB `MAX_RETAINED_CAPACITY` allocation does not return to the pool, and on a heavy decode-error run every error blob burns one recycled buffer. Trigger: a run of malformed zstd/zlib blobs (e.g. the `MessageTooBig` guard firing mid-decode) steadily drains the pool below steady state.

- [ ] **`read/header_walker.rs:74-105` - MEDIUM.** `HeaderWalker::open` always opens a plain buffered file and applies `posix_fadvise(RANDOM)`; the `--direct-io` CLI flag is silently dropped on every path that goes through `build_classify_schedule*` (apply-changes, altw, extract strategies, geocode classify) because the walker's `shared_file()` hands workers a non-O_DIRECT fd. Trigger: any classify-backed command invoked with `--direct-io` on a memory-constrained host - the page cache still fills from worker preads, defeating the flag.

- [ ] **`blob_meta/scan_ids.rs:192-202` - MEDIUM.** The coordinate conversion multiplies `gran * min_raw_lat` as i64 without overflow checking; on adversarial or bitrot-corrupted `granularity` / `lat_offset` fields the result wraps silently in release builds, producing a bogus bbox that then gets serialized into indexdata and trusted by every spatial filter downstream. Trigger: a PBF whose `granularity` field is set to `i32::MAX` combined with extreme delta-coded coords.

- [ ] **`read/indexed.rs:107-173` - MEDIUM.** `IndexedReader::new<R>` / `create_index` records every blob's offset with `SimpleBlobType::Primitive|Header|Unknown` but leaves `id_ranges: None` until the first full decode; `update_element_id_ranges` only inspects `group.nodes() / dense_nodes() / ways()`, so a blob containing only relations ends up with `{node_ids: None, way_ids: None}`. Future calls interpret this as `ElementsAvailable::No` - consistent but masks the fact that relation blobs were never indexed at all; any future relation-aware method added here will start silently skipping real data. Trigger: add a `for_each_relation` to `IndexedReader` and run it on any sorted PBF.

- [ ] **`read/block.rs:338-407` - MEDIUM.** `PrimitiveBlock { buffer: Bytes, block: WireBlock<'static> }` relies on field-declaration-order drop: `buffer` drops first (returning the `Vec` to `DecompressPool`), then `block` drops (trivial). If a future refactor reorders fields, adds a `Drop` impl to `WireBlock`, or introduces any access inside a `Drop`, the self-referential transmute silently becomes unsound. No `drop_check` or assert pins the invariant. Trigger: latent; any refactor touching either struct.

- [ ] **`read/direct_reader.rs:144-168` - MEDIUM.** `DirectReader::skip` uses `self.file.stream_position()` to compute the post-buffer absolute target, but `stream_position` returns the fd's position after the last `libc::read`, which is the page-aligned read end - not the logical read offset. The math happens to work when the buffer was fully consumed up to `len` (the common case) because `past_buf = n - buffered` equals `(current_pos - buffered) + n - current_pos` algebraically, but the comment on line 145 is wrong and the invariant is load-bearing. Trigger: latent; any edit that tweaks `past_buf` can break alignment.

- [ ] **`read/pipeline.rs:184-210` - MEDIUM.** `catch_unwind`/`AssertUnwindSafe` on rayon decode tasks converts panics into `io::Error::other("decode task panicked")`, losing the panic payload and the downstream stringtable-UTF8-err/wire-err detail. Stage 2 only catches panics inside the decode closure, not in the enclosing `decode_pool.spawn` or the dispatcher loop itself. Trigger: any bug that escapes `from_vec_pooled_with_scratch`'s Result path (e.g. a debug_assert panic) surfaces as an opaque "decode task panicked" with no blob offset.

- [ ] **`scan/classify.rs:36-43, 200, 304` - MEDIUM.** `resolve_thread_count(Some(0))` falls through to `available_parallelism() - 2`; the comment on line 34 says this is intentional for CLI flags that map `0 = auto` to pass through cleanly, but no current caller uses that mapping. If a new CLI flag lands with `--threads 0` intending "disable parallelism" (a common Linux convention), the scan will spawn `N-2` workers instead of 0 or 1. Trigger: add a new CLI `--threads 0` meaning serial - user silently gets a 14-thread run on a 16-core box.

- [ ] **`read/blob.rs:670-681` - LOW.** `BlobReader::seek_raw` sets `self.offset = Some(ByteOffset(offset))` and does not reset `last_blob_ok`; if the previous iteration left `last_blob_ok = false` (after HeaderTooBig or InvalidDataSize), `seek_raw` succeeds but subsequent `next()` still short-circuits to `None`. Trigger: call `seek_raw` on a reader that just returned `Err` - iteration stays dead even though the user recovered via seek.

- [ ] **`reorder_buffer.rs:21-33` - LOW (read-path side; also noted in write-path sweep).** `ReorderBuffer::push` asserts `seq >= self.next_seq` and `self.pending[slot_idx].is_none()`; both are panics-on-caller-bug rather than Result errors. In the pipeline the sequence comes from `enumerate()` so these only fire if a rayon worker sends duplicate `(seq, ...)` tuples. If a new caller retries a seq on a transient error and sends it twice, the panic kills the pipeline thread. Trigger: add a retry loop in `run_pipeline` that re-sends on transient decode errors without updating seq tracking.

- [ ] **`read/blob.rs:263-274` - LOW.** `BlobReaderSource` is implemented for `File`, `Cursor<T: AsRef<[u8]>>`, and `BufReader<R>`; the default `skip_relative` for any third type does `Seek::seek(SeekFrom::Current(n))`. Doc claims this is "correct but pays the discard cost" - true for `BufReader`-wrapping types, but a user-provided compressed-stream reader (e.g. gzip-on-the-fly) would be unimplemented or re-decode from scratch. Header walks on such sources now issue O(blob_count) expensive seeks instead of the O(blob_count) cheap `seek_relative` the doc promises. Trigger: wire a custom `Read + Seek` source into `BlobReader::new_seekable` that has expensive `seek`.

### Smaller commands

- [ ] **`commands/sort/mod.rs:178-181` - HIGH.** The overlap-run extension loop `while i < entries.len() && overlaps[i] { i += 1; }` is missing the same `&& entries[i].index.kind == run_kind` guard that was added to `cat::dedupe::merge_pbf` at `dedupe.rs:225` this cycle. Two adjacent same-kind overlap-runs at a type boundary (a node overlap-pair followed immediately by a way overlap-pair, both `overlaps[i]=true`) merge into a single `write_overlap_run` call; `write_overlap_run` uses `entries[0].index.kind` and the kind-gated extract closure silently drops every element whose kind doesn't match. **Direct parallel of the already-fixed dedupe bug, in its twin command.** Trigger: `sort` on a PBF where the last node blob overlaps its neighbor, the first way blob also has an overlap, and both overlap-runs are adjacent in sort order.

- [ ] **`commands/inspect/show_element.rs:53-57` - HIGH.** Early-exit `if idx.min_id > target_id { return Ok(false); }` assumes same-kind blobs are ID-sorted, but the function never checks `header().is_sorted()`. On unsorted or history PBFs the target can live in a later same-kind blob whose `min_id` is lower than a preceding blob with `min_id > target_id` - `show_element` returns "not found" for elements that exist. Trigger: `inspect --show n<id>` on a non-sorted PBF where the target node sits after a blob with a higher `min_id` in file order.

- [ ] **`commands/getid/mod.rs:259` - MEDIUM.** `removeid` (invert mode) reaches `filter_by_id` without any `require_indexdata` / `--force` gate. On a non-indexed PBF, the raw-passthrough fast path at 332-360 is unreachable (branch is conditional on `meta.index.is_some()`), so every blob falls into the full-decode path at 364 with no user warning. Correct output but silently slow, and inconsistent with `getid` at line 238 which gates on indexdata. Trigger: `removeid` on a non-indexed PBF.

- [ ] **`commands/extract/simple.rs:310-315` - MEDIUM.** `extract --simple` has no `require_indexdata` gate (`require_indexdata` at `mod.rs:629-633` only runs for CompleteWays/Smart). Pass 1 uses `blob.index()` behind `if let Some(idx)` at 176, silently skipping spatial filtering on non-indexed inputs, while Pass 2 has no equivalent. Correct but wasteful; also means users don't get the `--force` warning on non-indexed input for simple. Trigger: unsorted non-indexed PBF with simple extract strategy.

- [ ] **`commands/check/verify_ids.rs:534-536` - MEDIUM.** `check_type_order` is skipped in the `--full` path when `!indexed` (explicit comment: schedules are triplicated, would produce spurious violations). A user running `check --ids --full` on a non-indexed file with an actual type-order violation sees NonMonotonic and Duplicate violations but NO TypeOrder violations - silently downgrading the diagnosis vs the non-`--full` path which does element-level type-order checking. Trigger: `check --ids --full` on a non-indexed PBF with elements in wrong type order.

- [ ] **`commands/altw/passthrough.rs:285-286` - MEDIUM.** `is_passthrough = matches!(kind, Some(ElemKind::Relation)) || matches!(kind, Some(ElemKind::Node) if keep_untagged_nodes)`; evaluated only when `kind` comes from `header.index.as_ref().map(|idx| idx.kind)`. With `altw --force` on non-indexed input, `kind = None` and every blob decodes via `BatchSlot::Unknown(frame)`. Correctness is preserved (everything decodes in file order) but the "Flush pending decode batch before writing passthrough blobs" invariant at lines 289-307 is never triggered for non-indexed input, meaning the documented ordering-preservation mechanism silently doesn't apply. Trigger: `altw --force` on non-indexed input.

- [ ] **`commands/extract/multi.rs:158-168` - MEDIUM (latent).** Non-indexed blob fallback creates `NodeBlobInfo` entries with `frame_size: 0, count: 0, contained_in: Vec::new()`. At `multi.rs:540` the check `info.contained_in.len() == n` is false for empty `contained_in` when `n > 0` (so entries correctly route to `decode_items`), but when `n == 0` (caller bug; prevented by `parse_extract_config` at `mod.rs:373`), `contained_in.len() == 0 == n` would send the frame_size=0 entry to passthrough with a 0-byte `read_exact_at`. Latent bug behind a config validation. Trigger: bypass config validation to pass zero regions.

- [ ] **`commands/extract/mod.rs:518-524` - LOW.** `extract_multi` single-pass dispatch checks only `ExtractStrategy::Simple` and `!clean.any()`, never consults `force`. `try_extract_multi_single_pass` does not call `require_indexdata`, so multi-extract on non-indexed input with Simple silently uses the "no indexdata - include in all schedules (conservative)" branch at `multi.rs:158-168`. Not a correctness issue but the `--force` contract documented for single-region `extract()` is bypassed. Trigger: multi-extract on non-indexed PBF with Simple strategy.

- [ ] **`commands/time_filter/mod.rs:179-184` - LOW.** `group.latest = Some(clone_owned_element(&element))` unconditionally overwrites on every version with `timestamp <= cutoff`. Correct iff history versions within a `(kind, id)` group are timestamp-sorted; relies on PBF history convention without checking version numbers. A malformed history file with out-of-order versions produces "last version seen with `timestamp <= cutoff`" instead of "max timestamp <= cutoff". Trigger: malformed history PBF.

- [ ] **`commands/check/refs.rs:141-146` - LOW.** `check_refs` accepts `direct_io` and silently ignores it (`let _ = direct_io;`). The parallel pread workers open the input via internal `Arc<File>`. Users passing `--direct-io` on `check --refs` get no warning that it's a no-op. Same pattern as inspect's fast path but refs has no surfaced comment. Trigger: `check --refs --direct-io`.

- [ ] **`commands/cat/mod.rs:234-237` - LOW.** `cat_type_passthrough`'s `blob_filter.wants_index(idx)` drops blobs with the wrong kind only when indexdata is present. Non-indexed blobs always fully decode + filter. `cat --type node --force` on a non-indexed PBF decodes way+relation blobs unnecessarily, unlike the indexed fast path. Correct output, wasted work; partially covered by the `--force` warning. Trigger: `cat --type <kind> --force` on non-indexed input.

- [ ] **`commands/tags_filter/mod.rs:778-785` - LOW.** Pass-2 schedule skips the type filter when `meta.index` is None (the entire filter check lives behind `if let Some(idx)` at 779), so non-indexed blobs go through pass-2 decode regardless of `blob_filter`. With `has_included_way=false && has_included_relation=false && invert=false`, the type filter would skip way/relation blobs; non-indexed they still decode, but `filter_block_pass2` correctly drops them (empty `included_way_ids`). Correct, wasteful. Trigger: `tags-filter` on non-indexed input with a narrow type filter.

- [ ] **`commands/inspect/scan.rs:61-119` - LOW.** `try_index_only_scan` unconditionally ignores `direct_io` (doc at 57-60). Safe because any non-indexed blob returns `None` at 91-92, triggering a fallback; but the pattern of "silently ignore a user flag when a fast path applies" is not surfaced in user output. Trigger: `inspect --direct-io` on any indexed PBF.
