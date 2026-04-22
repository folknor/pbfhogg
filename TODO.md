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
`tests/non_indexed_parity.rs` and
`tests/apply_changes_invariants.rs` - remove the ignore attribute to
reproduce.

- [ ] **`extract --strategy simple --force` on non-indexed input
  double-emits elements.** Parity test ran the same logical input
  through indexed + non-indexed twins with `force: true`, bbox
  clipping about half the 10-node fixture. Indexed output: 6 nodes
  (correct). Non-indexed output: 18 nodes - a 3x multiplier, which
  matches the "non-indexed blobs are decompressed up to 3 times"
  comment in `src/commands/extract/simple.rs::extract_simple_single_pass`.
  Leading hypothesis: the passthrough schedule silently no-ops on
  blobs without indexdata and then the same decoded block flows
  through the output three times via the type-split phases. First
  step: confirm hypothesis with a counter / trace, then gate the
  pass-2/pass-3 emits on "already emitted this blob".

- [ ] **`apply-changes --force` on non-indexed input off-by-one on
  delete.** Parity test ran a `modify n2, delete n3, create n100`
  OSC against indexed + non-indexed twins of the same 10-node base.
  Indexed output: 10 nodes (1,2,4..=10,100 - delete applied).
  Non-indexed output: 11 nodes (one extra, likely n3 survived the
  delete because the scanner's descriptor routing at
  `src/commands/apply_changes/scanner.rs:129-204` falls into the
  worker-pool-unconditional branch with placeholder `kind` and
  `id_range: None`, and the delete match by id doesn't fire in that
  path). First step: add a counter around the worker-pool branch to
  confirm it's the route taken, then teach the scanner to populate
  `id_range` from decoded block metadata on the non-indexed fallback.

- [ ] **`merge_pbf([A, A])` drops ways and relations.** Observed
  during Batch A parity-test development: merging an indexed PBF
  with itself via `merge_pbf` and `--force` produces a node-only
  output (original 10 nodes + 4 ways + 1 relation -> 10 nodes, 0
  ways, 0 relations after merge). Parity with non-indexed twin
  holds (both drop ways/rels identically), so the bug is in the
  indexed path. The `merge_pbf` implementation in
  `src/commands/cat/dedupe.rs` probably treats blob-identical ways
  across input files as "same blob already emitted" without
  considering that both blobs exist in the input list. The
  subsequent parity test in `non_indexed_parity.rs` uses disjoint
  inputs and passes. Real-world `cat --dedupe` on regional PBFs
  would be affected only if two inputs carry byte-identical way
  blobs - unlikely in practice but latent.

- [ ] **`extract --strategy complete-ways --force` on non-indexed
  input produces empty output.** Parity test: indexed fixture
  yields 6 nodes + dependent ways; non-indexed twin yields 0 nodes.
  `require_indexdata(.., force: true, ..)` lets the call proceed
  but `extract_complete_ways`'s pass-1 appears to rely on per-blob
  bboxes from indexdata to populate `bbox_node_ids`; without
  indexdata that set stays empty and propagates to 0 matched ways
  and 0 transitive refs. Affects `extract_multi` whenever it falls
  through to `extract_complete_ways` per slot. Fix: make pass-1
  scanner do element-level bbox testing when `blob.index()` is
  None. Same root cause likely affects `ExtractStrategy::Smart`.

- [ ] **`extract --strategy smart --force` on non-indexed input
  produces empty output.** Parity test used the existing smart
  multipolygon + boundary fixture: indexed output had the expected 4
  nodes, 3 ways, and 2 relations; non-indexed output was empty
  (0 nodes). This strongly suggests the same root cause as
  complete-ways - pass 1 relies on per-blob bbox/indexdata to seed
  `bbox_node_ids`, so matched ways never materialize and the smart
  relation-member expansion has nothing to build on. First fix to
  try: make the non-indexed pass-1 fallback do element-level bbox
  testing before relation handling.

- [ ] **`apply-changes -j N --locations-on-ways` consumer build trips
  the drain/copy-range invariant.** New jobs-parity test
  (`tests/apply_changes_invariants.rs::merge_jobs_parity_on_multiblob_input`)
  bootstraps a multi-blob indexed base through
  `add-locations-to-ways`, then compares `jobs=1` vs `jobs=4` on a
  create/modify/delete OSC. All-features sweep passes. Consumer
  sweep (`--no-default-features --features commands`) fails before
  parity assertions with `drain: received CopyRange item but
  use_copy_range is false`. That means the scanner/drain contract is
  inconsistent in that feature set: `CopyRange` items are still
  reaching the drain while copy-range output is disabled.

- [ ] **`merge` summary/stat counters diverge between all-features and
  consumer builds on the same fixture.** While running
  `tests/derive_changes.rs::derive_changes_jobs_parity_roundtrips_to_same_output`,
  both sweeps produced element-equivalent outputs, but the merge
  summaries differed sharply. All-features reported `34 elements
  written`, `Base: 22 nodes, 8 ways`; consumer reported `16 elements
  written`, `Base: 6 nodes, 6 ways` for the same logical roundtrip.
  This looks like stats/reporting drift or feature-gated counting
  semantics rather than data corruption, but if these counters are
  user-visible they need a parity pin. First step: add a dedicated
  stats-parity test around `MergeStats` / summary accounting.

- [ ] **`check --ids` (`verify_ids`) reports spurious TypeOrder
  violations on non-indexed input.** `check_type_order` in
  `src/commands/check/verify_ids.rs` validates that
  `max(node_offsets) < min(way_offsets) < min(relation_offsets)`
  using the schedule from `build_classify_schedules_split`. For
  non-indexed PBFs that schedule builder replicates every blob
  into all three per-kind schedules (`src/scan/classify.rs:140-149`),
  so the offset comparisons span replicated copies rather than
  per-type blob sets and produce spurious violations on correctly-
  ordered input. Parity test: 10-node fixture -> 0 violations
  indexed, 2 violations non-indexed. Fix options: gate the
  offset-based check on `indexed == true`, or swap to an
  element-kind-based ordering check that decodes blob contents.

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
