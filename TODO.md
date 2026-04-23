# pbfhogg TODO

## Active optimization plans (high priority)

Planet-scale command plan docs live in [notes/](notes/). Each has a current-state header, a ranked opportunity list, and cross-references. Read the plan before touching the command.

- [ ] **[notes/merge-changes.md](notes/merge-changes.md)** - `merge-changes` (squash N OSCs → 1). The serial-across-inputs CLI shape `merge_changes::write_streaming` parallelizes cleanly via `IdSet::set_atomic_if_new` newer-wins dedupe; estimated ~20-30 s saved at 7-OSC planet per reviewer Q7 speculation. No win at 1-OSC scale. **All prerequisites landed**: per-input `MERGECHANGES_PARSE_{START,END}` markers (commit `4e3c7ea`) on both the streaming and `--simplify` paths with `merge_changes_input_bytes` counter inside the span for per-OSC size distribution; overnight.sh already runs `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench 1` + `--hotpath` (overnight.sh:246-247) so the 2026-04-24 morning sidecar gives serial wall + per-OSC parse wall + hotpath breakdown. Library-level `osc::load_all_diffs` has no markers but is **unused in production** (only `#[cfg(test)]` call sites); apply-changes takes a single OSC via `parse_osc_file` so the "per-input" axis doesn't apply there. After tomorrow's baseline lands, size the parallel-parse win against the per-OSC share of serial wall and implement the rayon-scoped parse-fan-out if it's worth the complexity. Content factored out of `apply-changes-opportunities.md` 2026-04-21 - these items were filed under "weekly apply-changes" but apply scale-independently to any consumer that squashes N > 1 OSCs.

- [ ] **[notes/sort.md](notes/sort.md)** - `sort` (repair unsorted PBFs into `Sort.Type_then_ID`). Drafted 2026-04-23. **Production reality**: Geofabrik / planet input is already sorted, so the overlap-count is ~zero and pass 2 is pure raw passthrough. The headline opportunity that helps the production case is **`copy_file_range` coalescing for passthrough runs** (hours-scope, transplant from apply-changes drain, 1.1-1.5x via syscall reduction). The bigger theoretical wins - parallel overlap-rewrite in pass 2 (1.5-3x) and HeaderWalker-based pass 1 (1.2-2x on non-indexed input) - only fire on genuinely-unsorted input, which has no dataset configured in `brokkr.toml` today. Planet baseline scheduled for tonight's `overnight.sh:272-275`; lands 2026-04-24. Anti-conversion rule (pipelined → sequential) explicitly off the table per `reference/pipelined-reader-paths.md:138`.

- [ ] **[notes/getparents.md](notes/getparents.md)** - `getparents` (whole-file scan listing ways / relations referencing a given ID set). Drafted 2026-04-23. **Already uses modern primitives** (pipelined decode, `BlobFilter` node-only-blob skip per CHANGELOG's ~85 % claim, `IdSet` chunked sparse bitset); not "never optimized". Headline opportunity is a **`HeaderWalker` + `IdSet::any_in_range()` blob-level fast path** mirroring `getid`'s include mode (planet 44 s → 7 s, 6.2x); estimated 4-8x at planet, 1-2 days scope, requires indexdata with a fallback. `parallel_classify_phase` substitution is bench-gated (10-20 % or neutral; keeps the c912e4d Denmark 4.7× sequential-decode regression explicitly off the table - that rule targets sequential-decode conversions, not pread-worker substitutions). Planet baseline scheduled for tonight's `overnight.sh:276-278` + `--alloc`; lands 2026-04-24.

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

## Performance

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
- [ ] **CLI UX: scratch dir + mode naming, unified across the CLI** (raised
  2026-04-23, unresolved). Two related decisions, both of which should be
  applied uniformly across every command that carries the pattern, not
  one-off per command.

  (A) **Scratch-dir argument presence.** Today `add-locations-to-ways
  --index-type external` infers scratch as `output.parent()` with a `.`
  fallback (silent cwd footgun at 112-224 GB scale; see the `altw/external/mod.rs:191`
  bug-sweep entry). Dense/sparse follow the same pattern. Other large-scratch
  paths (extract complete/smart, geocode builder, renumber stage 2d) need
  auditing: do they infer scratch the same way, and would a unified policy
  apply to all of them?

  Three postures for the unified policy, from least to most strict:
  1. **Fail-on-unsafe-default.** Infer from output.parent(); error cleanly
     if the derivation falls back to `.`. Catches the footgun, no new flag,
     no friction for the common "output on big disk" case.
  2. **Balanced: add a `--scratch DIR` override everywhere.** Default to
     output.parent(). Error on bare filename without `--scratch`. Gives
     users who want scratch on a different disk than output an explicit
     lever. Same footgun protection as (1).
  3. **Strict: require `--scratch` on every large-scratch command.**
     Self-documenting; every invocation names the scratch dir. Script-
     breaking for existing users; friction even when the inference would
     have been right.

  Pick one posture, apply to altw (all three backends) + extract complete/smart
  + geocode builder + any other commands that land >1 GB of scratch. The
  per-command bug-sweep LOW ticket for altw folds in once the posture
  is picked.

  (B) **Rename `--index-type` to `--mode`, and unify mode-like flag naming
  across the CLI.** Today we have:
  - `add-locations-to-ways --index-type` (dense/sparse/external/auto)
  - `extract --strategy` (simple/complete/smart)
  - `bench-read --mode`, `bench-write --mode`, `bench-write --io-mode`
  - `diff --format` (text/osc) - semantically "output shape", different concept

  `--index-type` is misnamed (external isn't really an index), `--strategy`
  and `--mode` are synonyms picked inconsistently, and `bench-*` already
  uses `--mode`. Unifying on `--mode` across add-locations-to-ways,
  extract, and the bench subcommands would make the CLI more regular at
  the cost of a breaking rename on two user-facing commands. `--format`
  for output shape (diff, and potential future geojson/csv exports) is
  a different axis and should stay `--format`.

  Both decisions are breaking CLI changes; batch them into a single
  release note when we land them. No urgency - 0.3.0 ships with the
  current names.
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

- [x] ~~**altw external fd footprint**~~ - landed 2026-04-23.
  Stage 1 pass B was holding `num_workers * NUM_BUCKETS` rank-shard
  files open concurrently (~4352 fds at 17 workers). Linux default
  soft ulimit of 1024 (some distros cap hard at 4096) made
  `add-locations-to-ways --index-type external` fail with EMFILE
  on any fresh shell. Fix: self-raise `RLIMIT_NOFILE` soft to hard
  cap at the top of `stage1_way_pass` (unprivileged, free), then
  cap `num_workers` at `(fd_budget - 64_headroom) / NUM_BUCKETS`
  so the worker fleet never requests more shards than can be held
  open. If even 1 worker's 256-shard budget can't fit, fail clean
  with a `ulimit -n N` hint. New counters:
  `extjoin_nofile_soft_cap`, `extjoin_cpu_cap_workers`,
  `extjoin_fd_cap_workers`. The
  `backend_parity_dense_sparse_external_auto` test is un-ignored
  and passes under default ulimit in both feature sweeps.

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

Remaining open findings from a multi-agent Opus audit of 0.3.0 high-churn areas. Originally 88 findings (117 after consolidation), re-verified across two Opus passes after an outside reviewer flagged ~28% of an initial slice as mis-called. Completed items have been removed from this list as they land; see git log and CHANGELOG.md for the landed fixes. All six headline items landed 2026-04-23 (sort kind-boundary, header MAX size caps, apply-changes --force+LoW rejection, show_element is_sorted gate, renumber unconditional negative-ID checks, writer gap-at-close surfacing).

**Open counts by area:**
- apply-changes pipeline: 3 MEDIUM, 1 LOW
- write path: 2 MEDIUM, 2 LOW
- read path: 1 MEDIUM, 2 LOW
- renumber: 6 MEDIUM, 2 LOW
- altw external: 5 MEDIUM, 3 LOW
- diff / derive-parallel: 3 MEDIUM latent (mixed-sign numeric compare; production PBFs are positive-only), 2 LOW
- geocode: 3 MEDIUM, 3 LOW
- smaller commands: 1 MEDIUM, 2 LOW

**Cross-cutting patterns still present:**

1. **Indexdata-trust without defensive check.** Call sites read descriptor fields populated from `BlobHeader.indexdata` and act on them without verifying indexdata was present, tight, or correct. The --force headline fixes closed the worst cases; residue remains across apply-changes, renumber, altw external, and geocode.

2. **Silent data loss on Drop-swallows-error paths.** `ParallelGzipWriter::drop` and `PbfWriter::drop` still discard `io::Result` from final flushes/joins when callers forget to call `finish()` / `flush()`.

3. **Temp-file leaks on worker-error paths.** altw external, apply-changes, and geocode Pass 3 Stage A still leak scratch files on worker error (diff-parallel landed).

4. **Null Island (0,0) sentinel.** altw external uses `(lat==0 && lon==0)` as the missing-coord sentinel; documented-accepted.

5. **Negative-ID and signed-arithmetic hazards.** diff shard planner uses raw numeric compare while element merge uses canonical `osm_id_cmp`; latent for positive-only production PBFs.

**Planning note:** the ~28% first-round error rate from the original sweep was not tolerable for landing decisions. Any "fix" based on a finding's text without independent code-reading has a high probability of mis-shaping the change. The most insidious mis-calls pointed at the right file but proposed a wrong mechanism. Read the code path before writing a patch.

### apply-changes pipeline

- [ ] **`apply_changes/rewrite.rs:244` - MEDIUM (diagnostic-quality)** *(verified 2026-04-23)*. If the scanner errors out mid-stream after sending some items but before all candidates are dispatched, some seqs never get produced; the drain hits its "channel closed with items still in reorder buffer" check at drain.rs:332 and returns an error whose diagnostic (`next_seq` vs smallest remaining) misleads away from the real upstream failure. Not a correctness bug - error surfaces, just two errors for one fault with the misleading one first. The true scanner error is surfaced separately via the scope join at rewrite.rs:337. Trigger: corrupted PBF header mid-stream. NOTE: drain.rs:330-338 original finding is the same concern from another angle - deduped into this entry.

- [ ] **`apply_changes/streaming.rs:242` - MEDIUM.** If a worker panics mid-stream, its `drain_tx` clone is dropped and other workers keep running, but seqs from the panicked worker's in-flight candidate are lost; the drain trips the reorder-buffer-non-empty check at drain.rs:330 and the panic only propagates when `std::thread::scope` returns, so the user sees "drain: channel closed with N items" rather than the real panic message. Trigger: OOM or unwrap in worker code path.

- [ ] **`apply_changes/streaming.rs:420-445` - MEDIUM.** Modifications to existing base nodes (same ID, new coords) are covered only because `build_from_diff` in `node_locations.rs:51` is generous and inserts every diff node (create or modify) into `seeded_locations`; any future narrowing of `build_from_diff` would silently break coord freshness for way-refs to modified nodes. Trigger: latent invariant; regresses if the seeded-locations population is ever tightened.

- [ ] **`apply_changes/rewrite_block.rs:103` - LOW.** Upsert-create emission uses `osm_id_cmp(inline_upserts[cursor], elem_id).is_lt()`, but `inline_upserts` was pre-sliced using `osm_id_key` bounds in `streaming.rs::upsert_slice`; if the base block is not sorted in OSM order (malformed input), the cursor can skip past upserts that compare greater than one element but less than a later element, silently dropping creates. Trigger: malformed base PBF violating Sort.Type_then_ID (partially protected by `--locations-on-ways` requiring `is_sorted()`, but the general path doesn't).

### Write path (parallel-pwrite, io_uring, parallel gzip)

- [ ] **`write/parallel_gzip.rs:188-213` - LOW (downgraded 2026-04-23)** *(verified: not a live failure mode today)*. `compress_one` at :216 uses `GzEncoder::new(Vec::with_capacity(...), ...)`. `Vec<u8>` `Write` impl is infallible; flate2 doesn't return Err for OOM (it panics). So the `Err(_) => return` arm at :207 is effectively unreachable in current code. A worker panic unwinds rather than running the `return`, so this path does not reach the described hang via normal io::Error. The original trigger "flate2 OOM" is wrong. The hang is only reachable via (a) a future sink change to a fallible writer, or (b) a panic-caught-and-converted-to-Err path. Keep as a latent defensive-coding note; fix shape is still "on worker error/panic, poison the writer_loop channel or close it with a sentinel" but urgency drops with no live trigger.

- [ ] **`write/parallel_writer.rs:399-430` - MEDIUM.** `copy_range_fallback_pwrite` loops via pread+pwrite but does not handle pread EINTR: a signal-interrupted pread returns `-1`/EINTR and the function errors immediately instead of retrying. Trigger: SIGWINCH or other signal delivered during a cross-device passthrough copy.

- [ ] **`write/copy_range.rs:63-96` - MEDIUM (latent).** `copy_range_fallback` writes to `out_fd` via `out.write_all`, which uses position-based writes; parallel contexts that reuse this function (not current callers) would race with other position-based writers on the same fd. Documented latent constraint the parallel writer's EXDEV fallback silently avoids by using pwrite. Trigger: future code reusing `copy_range_fallback` in a parallel context.

- [ ] **`write/writer.rs:108-145` - LOW.** `to_path_uring` handles a failed `init_rx.recv()` by `drop(tx); handle.join()` - if the uring thread is hung (e.g. blocked forever in `register_buffers` on a buggy kernel), the join hangs the main thread with no timeout. Trigger: pathological kernel behavior on `register_buffers` or `register_files`.

### renumber

- [ ] **`renumber/wire_rewrite.rs:293-296` - MEDIUM.** Way ref orphan detection does both `resolve(old)` AND `get(old)` - two chunk lookups per ref (~1.5 B refs on planet). `rank_if_set(old)` combines them and matches `resolve`'s internals exactly. Code-quality rather than correctness; flagged for the optimization bar. Trigger: none.

- [ ] **`renumber/relations.rs:297-298` - MEDIUM.** `stats.relations_written` accumulates from `rels_written.fetch_add(blob_count, ...)` which fires inside the worker BEFORE `tx.send` (line 232); `stats.orphan_refs` accumulates from `r2d_orphans.fetch_add` on the consumer side AFTER reorder (line 281). On a mid-stream error, `rels_written` counts blobs that were never emitted to output while `r2d_orphans` only counts orphans from blobs the consumer actually received. Summary counters disagree with actual output on error path. Trigger: reframe error in a middle blob.

- [ ] **`renumber/wire_rewrite.rs:486-491` - MEDIUM.** `memids_count` / `types_count` are derived by counting varint-terminator bytes in raw field data; correct only for well-formed varints, and a malformed trailing varint (missing continuation) would miscount and cause misalignment in the decode loop rather than clean error. Trigger: truncated/corrupt memids field.

- [ ] **`renumber/mod.rs:240` - MEDIUM.** `max_node_id = pass1_schedule.last().map_or(0, |t| t.max_id)` assumes the last node blob has the global max node ID; true for `Sort.Type_then_ID` (enforced by `require_sorted`), but if the header advertises sorted and the content is not (lying header), a later blob's id could overshoot `max_node_id` and `set_atomic` panics. Trigger: mis-flagged unsorted PBF.

- [ ] **`renumber/wire_rewrite.rs:250,255,454,460` - MEDIUM.** `tag_start = val_start - 1` hard-codes a 1-byte field tag; correct for fields 1-15 (all low-numbered tags used here are <=15). If the PBF schema ever adds field >=16 that the rewriter needs to splice, `val_start - 1` would slice mid-varint and produce corrupt output silently. Add `debug_assert` tying tag byte count to field number. Trigger: future PBF schema addition.

- [ ] **`renumber/wire_rewrite.rs:519-524` - MEDIUM** *(verified 2026-04-23, mechanism nit)*. Real effect confirmed: negative `old_abs_id` flows through `get` (returns false via bounds-check early-return at idset.rs:216) and `resolve` (returns `id` unchanged via cid-out-of-bounds early-return at idset.rs:408, not a huge-cid chunk lookup as the finding stated). Output contains the negative value AND orphan count bumps. Fix shape unchanged: explicit negative-ID check before the orphan decision.

- [ ] **`renumber/mod.rs:256-262` - LOW.** `nodes_written != pass1_total_nodes` aborts AFTER pass1 output has been written and stage 2d is about to run; leaves a half-written file with no explicit output cleanup. Trigger: `task.element_count` sum mismatch.

- [ ] **`renumber/mod.rs:308-311` - LOW.** Way `id_sets` are merged by removing element 0 and folding the rest with `merge` (takes ownership); if `STAGE2D_WORKERS = 0` in a future tweak, `remove(0)` panics. Current constant is 6 but the shape is fragile - `merge_from` on a default-constructed set would be safer. Trigger: future refactor.

### altw external

- [ ] **`altw/external/stage1.rs:269-273` + `stage2.rs:459-493` - MEDIUM.** Stage 2's blob-local rank counter (`next_rank = blob.ref_rank_start`, incremented per referenced tuple) is correct only if indexdata `(min_id, max_id)` tightly brackets actual node IDs in the blob. A producer with loose bounds plus the `debug_assert_eq!` at stage2.rs:488 passes in release and silently produces skewed ranks, scrambling the join. Trigger: input PBF with sloppy indexdata ranges from a third-party writer.

- [ ] **`altw/external/mod.rs:225-273` - MEDIUM.** If stage 1 returns an error, the scope closure short-circuits via `??` on `s1_handle.join()` before joining the relation-scan handle; `thread::scope` waits for `rel_handle` to finish, delaying error reporting by up to the scan's wall time (~4s Europe, longer planet). Trigger: any stage-1 failure while relation scan is running.

- [ ] **`altw/external/stage2.rs:67-72` - MEDIUM (latent).** `bucket_rank_end = ((bucket_idx + 1) * rank_range_size).min(unique_nodes)`; with `div_ceil(unique_nodes, NUM_BUCKETS)` as `rank_range_size`, a middle bucket can have `bucket_rank_start > unique_nodes` when `unique_nodes < NUM_BUCKETS`, and the subtraction at line 72 would underflow. Masked today by the `rank_bucket_counts[bucket_idx] == 0` early-continue at stage2.rs:355. Trigger: pathologically small inputs (`unique_nodes < 256`) plus future removal of the early-continue.

- [ ] **`altw/external/coord_payloads.rs:104-153` - MEDIUM.** `straddler_partials` allocates one `Mutex<Option<StraddlerPartial>>` per way blob (~57K at planet, ~3 MB) even though only a few hundred ever hold a value. Committed resident memory the design doc calls "only hundreds" but the implementation sizes for N. Trigger: any planet-scale run.

- [ ] **`altw/external/stage4.rs:645` - MEDIUM (perf)** *(verified 2026-04-23)*. Passthrough path takes `frame_read_buf` via `std::mem::take`; next iteration's `frame_read_buf.resize(frame_size, 0)` on the now-empty Vec forces a fresh allocation. Buffer reuse intent is defeated. Fix: use `std::mem::replace(&mut frame_read_buf, Vec::with_capacity(frame_size))` or pass by reference.

- [ ] **`altw/external/mod.rs:191` - LOW.** `ScratchDir::new` uses `output.parent().unwrap_or(Path::new("."))` - if `output` is a bare filename with no parent component, scratch files land in the current working directory. A user running from `/` or a tmpfs cwd while outputting to a large disk can land ~224 GB of scratch on the wrong filesystem. The dense path has the same pattern. Trigger: running external-join from a small-fs cwd. See "CLI UX: scratch dir + mode naming" under Milestone 3 > Command surface for the broader design question this finding folds into.

- [x] ~~**`altw/external/stage2.rs:488-493`**~~ - landed 2026-04-23. Promoted `debug_assert_eq!` to an always-on `return Err(...)` with blob offset + expected/actual rank range in the error message. Negligible cost (once per blob, not per tuple).

- [ ] **`altw/external/stage4.rs:573-600` - LOW.** The consumer pre-seeds passthrough items with `reorder.push(desc.seq, ...)` for every passthrough descriptor before looping on decode results; if `passthrough_items` is very large (planet: ~5K relations + up to 40K nodes if `keep_untagged_nodes=true`), `ReorderBuffer` with initial capacity 32 grows to hold all of them plus the decode-in-flight set. Acceptable cost but the buffer is now effectively sized by `len(passthrough_items)`. Trigger: large relation/node-passthrough counts.

### diff / derive-changes shard-parallel

- [ ] **`diff/parallel.rs:138-142` / `derive_parallel.rs:136-142` - MEDIUM (latent - positive-only production PBFs)** *(verified 2026-04-23)*. `plan_shards` builds thresholds via raw `i64` compare while element-merge uses `osm_id_cmp` canonical order. Mechanism is correct; effect is only reachable on mixed-sign inputs, which production PBFs are not. Fix lives in this file but priority drops until a real negative-ID consumer surfaces. Same goes for findings #2 (single-sided emit) and #3 (merge_up_to .min()) in this area - all three share the raw-vs-canonical compare issue and are similarly latent.

- [ ] **`diff/parallel.rs:354-357` / `derive_parallel.rs:310-315, 324-329, 339-343, 354-358` - MEDIUM (latent)** *(verified 2026-04-23 - see the consolidated note on finding #1 above).*

- [ ] **`diff/parallel.rs:384` / `derive_parallel.rs:429` - MEDIUM (latent)** *(verified 2026-04-23 - same class as #1 above; collapses to correct bound for positive-only inputs).*

- [ ] **`diff/parallel_gzip.rs:170-184` - LOW.** `Drop` swallows flush error when `finish()` was not called (also noted in write path). Trigger: caller relies on RAII.

- [ ] **`diff/derive_parallel.rs:240-248` - LOW.** Per-shard scratch filenames are `derive-par-{creates|modifies|deletes}-{pid}-{kind_tag}-{shard_idx}.xml.tmp`; two `pbfhogg` processes with the same PID running concurrently in the same `scratch_dir` (container restart recycling PID) collide. Sequential `ChangeSink` has the same exposure - pre-existing latent class. Add random suffix for 0.4.0. Trigger: PID collision.

### geocode builder v2

- [ ] **`geocode_index/builder/pass3.rs:152-167` - MEDIUM.** `parse_bucket_file` silently truncates any trailing bytes that don't form a complete 15-byte record (`count = data.len() / BUCKET_RECORD_SIZE`); if a bucket-writer flush fails partway (ENOSPC), the partial tail is silently dropped at Stage B with no diagnostic. Trigger: ENOSPC during Stage A writes.

- [ ] **`geocode_index/builder/pass3.rs:229-231` - MEDIUM.** On entry to `bucketed_cell_assignment_fused`, bucket dirs are blown away with `remove_dir_all`, but on error mid-Stage-A the dirs and partial buckets are left behind (no Drop/ScratchDir guard). A crash leaves ~256 temp files per bucket dir; subsequent build succeeds only because of the unconditional remove at top. Trigger: panic or I/O error between `create_dir_all` and the `remove_dir_all(bucket_dir).ok()` at end of Stage B.

- [ ] **`geocode_index/builder/admin.rs:88-111` - MEDIUM.** Hole-in-outer containment check uses only `hole[0]` (first vertex) with `point_in_ring`; a hole whose first vertex happens to lie outside the outer ring (e.g. aggressive `simplify_ring` on outer) is discarded even though most of the hole is inside. Trigger: aggressive `simplify_ring` on outer reduces it past the hole's first vertex.

- [ ] **`geocode_index/reader.rs:1033-1040` - LOW.** `segment_length` returns `approx_distance_sq().sqrt()` in radians; `way_length` / `accumulated_length` look like meters/length but are radians. Interpolation ratio is dimensionless so correct, but names mislead. Trigger: none (latent confusion source).

- [ ] **`geocode_index/reader.rs:800-831` - LOW.** `search_admin_all`'s `seen: Vec<u32>` uses linear `contains()` dedup; for points inside many overlapping admin boundaries (national + regional + municipal), O(n^2) per query. Latent scaling issue at the query API. Trigger: query API usage with deeply-nested admin overlap.

- [ ] **`geocode_index/builder/pass2.rs:295-304` - LOW.** Building-centroid uses integer division `sum_lat / count` on `i64` decimicrodegree sums; for ways spanning the antimeridian, the "centroid" sits on the wrong hemisphere. Not realistic in OSM but no antimeridian-aware averaging. Trigger: building polygon crossing +/-180 degrees.

### Read path infrastructure

- [ ] **`blob_meta/scan_ids.rs:192-202` - MEDIUM.** The coordinate conversion multiplies `gran * min_raw_lat` as i64 without overflow checking; on adversarial or bitrot-corrupted `granularity` / `lat_offset` fields the result wraps silently in release builds, producing a bogus bbox that then gets serialized into indexdata and trusted by every spatial filter downstream. Trigger: a PBF whose `granularity` field is set to `i32::MAX` combined with extreme delta-coded coords.

- [ ] **`read/blob.rs:670-681` - LOW.** `BlobReader::seek_raw` sets `self.offset = Some(ByteOffset(offset))` and does not reset `last_blob_ok`; if the previous iteration left `last_blob_ok = false` (after HeaderTooBig or InvalidDataSize), `seek_raw` succeeds but subsequent `next()` still short-circuits to `None`. Trigger: call `seek_raw` on a reader that just returned `Err` - iteration stays dead even though the user recovered via seek.

- [ ] **`reorder_buffer.rs:21-33` - LOW (read-path side; also noted in write-path sweep).** `ReorderBuffer::push` asserts `seq >= self.next_seq` and `self.pending[slot_idx].is_none()`; both are panics-on-caller-bug rather than Result errors. In the pipeline the sequence comes from `enumerate()` so these only fire if a rayon worker sends duplicate `(seq, ...)` tuples. If a new caller retries a seq on a transient error and sends it twice, the panic kills the pipeline thread. Trigger: add a retry loop in `run_pipeline` that re-sends on transient decode errors without updating seq tracking.

### Smaller commands

- [ ] **`commands/getid/mod.rs:259` - MEDIUM.** `removeid` (invert mode) reaches `filter_by_id` without any `require_indexdata` / `--force` gate. On a non-indexed PBF, the raw-passthrough fast path at 332-360 is unreachable (branch is conditional on `meta.index.is_some()`), so every blob falls into the full-decode path at 364 with no user warning. Correct output but silently slow, and inconsistent with `getid` at line 238 which gates on indexdata. Trigger: `removeid` on a non-indexed PBF.

- [ ] **`commands/tags_filter/mod.rs:778-785` - LOW.** Pass-2 schedule skips the type filter when `meta.index` is None (the entire filter check lives behind `if let Some(idx)` at 779), so non-indexed blobs go through pass-2 decode regardless of `blob_filter`. With `has_included_way=false && has_included_relation=false && invert=false`, the type filter would skip way/relation blobs; non-indexed they still decode, but `filter_block_pass2` correctly drops them (empty `included_way_ids`). Correct, wasteful. Trigger: `tags-filter` on non-indexed input with a narrow type filter.

- [ ] **`commands/inspect/scan.rs:61-119` - LOW.** `try_index_only_scan` unconditionally ignores `direct_io` (doc at 57-60). Safe because any non-indexed blob returns `None` at 91-92, triggering a fallback; but the pattern of "silently ignore a user flag when a fast path applies" is not surfaced in user output. Trigger: `inspect --direct-io` on any indexed PBF.
