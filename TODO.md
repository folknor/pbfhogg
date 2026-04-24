# pbfhogg TODO

## Active work items (top priority, 2026-04-24)

Three new docs capturing a cross-cutting insight and two new commands that fall out of it. All scaffolding-level; details drift as work lands.

- [ ] **[reference/blob-density.md](reference/blob-density.md)** - the insight: Geofabrik-style PBFs (~8k elements/blob, ~522 k blobs on europe) scale very differently from `planet.openstreetmap.org`-style PBFs (~300k elements/blob, ~50 k blobs on planet). Every `HeaderWalker`-based command (`sort`, `getid`, `getparents`, `inspect`, `apply-changes::scanner`, `check --refs`, `extract --smart`, `tags-filter`, `build-geocode-index`, `renumber_external`) has an implicit blob-count scaling dependency silently shaped by the encoder on the producer side. README's "Planet scale" table and all `notes/*.md` "N seconds at planet" predictions are measured on the sparse-blob encoding. Needs same-corpus-different-encoding measurements once `repack` exists.

- [ ] **[notes/repack.md](notes/repack.md)** - new command: re-encode a PBF with a configurable `--elements-per-blob N` cap. Primary consumer: the measurement matrix in `blob-density.md`. Reuses `ElementReader` + `BlockBuilder` + `PbfWriter`. Small extension to `BlockBuilder` if its element cap isn't already caller-configurable. v1 scope: `--elements-per-blob`, `--compression`. 1-2 days.

- [ ] **[notes/degrade.md](notes/degrade.md)** - new command: adversarial-test tool for producing valid-but-harder PBFs. v1 flags: `--unsort` (exercises `sort`'s overlap-rewrite path, which landed in commit `68e1ba0` without a planet bench), `--strip-locations` (for `add-locations-to-ways`), `--strip-indexdata` (for `--force`/non-indexed fallbacks). Deferred: `--strip-tagdata`, `--strip-bbox`, `--recompress`, `--drop-ids`. Flags compose; order of effects documented in the doc. 1-2 days for v1.

**Open decision on `getparents`** (see [notes/getparents.md](notes/getparents.md) current state): uncommitted `HeaderWalker` path is +68 % on europe / -46 % on planet. Revert, threshold-dispatch, or accept? Deferred until `repack` produces an 8k-packed planet so the crossover point can be measured directly.

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

- [ ] **Fault injection for parallel pipelines.** Harness landed
  2026-04-24 for apply-changes streaming. Feature-gated `test-hooks`
  flag (zero production overhead; off by default, on under
  `--all-features`). `MergeOptions::panic_at_blob_seq` + a
  one-line check at the top of the worker loop. First canonical
  test (`fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean`
  in `tests/apply_changes_invariants.rs`) surfaced a real drain
  deadlock on worker panic - the drain loop only exited when the
  reorder buffer was empty, so stuck seqs spun the loop forever.
  Fix: break on channel-disconnect unconditionally; the post-loop
  "items stuck" error surfaces the real diagnostic. Also found:
  `jobs: Some(1)` worker-panic hangs the scanner (no surviving
  worker to drain `candidate_rx`); tracked below as a separate
  invariant gap.

  Remaining pipelines to cover by transplanting the same
  feature-gated hook shape + one test per pipeline:
  `write/parallel_writer.rs`, `write/uring_writer.rs`,
  `write/parallel_gzip.rs`, `diff/parallel.rs`, `derive_parallel.rs`,
  altw external stages 3/4, geocode Pass 3 Stage A. Harness
  template: see `MergeOptions::panic_at_blob_seq` +
  `streaming::worker_loop` + the canonical test. Scratch tracking
  helpers: `common::snapshot_dir` and `common::assert_scratch_unchanged`.

- [ ] **`jobs == 1` apply-changes worker-panic deadlock.** With a
  single worker and a worker panic, the scanner blocks on a full
  `candidate_rx` because no one is consuming, the drain blocks
  waiting on senders, and the command hangs forever. The fault-
  injection test currently uses `jobs: Some(2)` to sidestep this.
  Fix shape: plumb a "scope unwinding" shutdown signal that the
  scanner polls between sends (a single `Arc<AtomicBool>` would
  do), set it from the worker scope's drop path. Not fixed in the
  initial harness landing because it requires real engineering
  across scope boundaries; parked here as a follow-up the harness
  will catch again once re-armed with `jobs == 1`.

- [ ] **Lying-indexdata fixtures (extended coverage).** Partial
  progress 2026-04-24: cluster 2's landing (see
  [decisions/0004](decisions/0004-defensive-input-errors-and-fixtures.md))
  closed the runtime half of this test-shape - five fixes
  promoting `debug_assert` / unchecked arithmetic / silent-truncation
  to hard errors - and added `tests/cluster2_defensive_input.rs`
  with two tests covering the out-of-order-sort-claim and
  unsorted-header fixes. What remains is the byte-level fixture
  helper itself: `tests/common/adversarial.rs` with
  `mutate_blob_header_indexdata(pbf_bytes, blob_idx, f)` and
  `mutate_blob_payload(pbf_bytes, blob_idx, f)` primitives so a
  test can inject reversed / overshooting indexdata ranges,
  truncated varints in relation memids, and DenseNodes with
  adversarial granularity without hand-rolling wire-format
  manipulation per test. Three fixes landed in cluster 2 still
  lack direct regression tests (`scan_ids.rs` overflow,
  `wire_rewrite.rs::count_varints_strict`, `stage1.rs` reversed
  range) and will be covered once those primitives exist.
  Covers the fixes landed 2026-04-24 plus additional indexdata-
  trust sites not in cluster 2: `renumber/pass1.rs:179`,
  `renumber/wire_rewrite.rs:272`, `renumber/stage2.rs:226-231`,
  `altw/external/stage4.rs:438-478`,
  `apply_changes/scanner.rs:162,188`, `apply_changes/streaming.rs:496`,
  `commands/inspect/show_element.rs:53-57`.

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

**Cross-cutting patterns still present:**

1. **Indexdata-trust without defensive check.** Call sites read descriptor fields populated from `BlobHeader.indexdata` and act on them without verifying indexdata was present, tight, or correct. The --force headline fixes closed the worst cases; residue remains across apply-changes, renumber, altw external, and geocode.

2. **Temp-file leaks on worker-error paths.** altw external, apply-changes, and geocode Pass 3 Stage A still leak scratch files on worker error (diff-parallel landed).

3. **Null Island (0,0) sentinel.** altw external uses `(lat==0 && lon==0)` as the missing-coord sentinel; documented-accepted.

4. **Negative-ID and signed-arithmetic hazards.** diff shard planner uses raw numeric compare while element merge uses canonical `osm_id_cmp`; latent for positive-only production PBFs.

**Planning note:** the ~28% first-round error rate from the original sweep was not tolerable for landing decisions. Any "fix" based on a finding's text without independent code-reading has a high probability of mis-shaping the change. The most insidious mis-calls pointed at the right file but proposed a wrong mechanism. Read the code path before writing a patch.

**Reclustered 2026-04-24** by policy question rather than subsystem. Landing a single decision on each cluster disposes of multiple items at once. Per-subsystem view is recoverable by grepping the file:line pins.

### Cluster 1: Negative-ID / mixed-sign handling policy

**Decision landed 2026-04-24: option (c) - document positive-only project-wide and gate latent paths with `debug_assert`.** Context from osmium audit: osmium supports negatives affirmatively via its canonical `id_order` (0 → negatives by abs value → positives by abs value) and documents JOSM interop as a designed feature. pbfhogg instead treats positive-only as a hard invariant because `IdSet` (the load-bearing data structure in renumber) is unsigned-indexed, and no user has asked for JOSM-staged input. See [decisions/0002](decisions/0002-negative-ids-rejected-project-wide.md) for the full decision record, alternatives considered, and migration path; see DEVIATIONS.md > "Negative input IDs rejected project-wide" for the behavior comparison with osmium.

- [x] ~~**`renumber/wire_rewrite.rs:519-524`**~~ - landed 2026-04-24. Added unconditional `old_abs_id < 0` reject at the relation-member-ref path, mirroring the node and way entry points from `ab01438`. Error names the enclosing relation id plus the offending member ref.

- [x] ~~**`diff/parallel.rs:138-142` / `derive_parallel.rs:115-125`**~~ - landed 2026-04-24. Added `debug_assert!(all descriptors have min_id >= 0)` at the top of `plan_shards` in both files, with a comment pointing at DEVIATIONS.md for the policy rationale. Covers the threshold-build site.

- [x] ~~**`diff/parallel.rs:354-357` / `derive_parallel.rs:310-315, 324-329, 339-343, 354-358`**~~ - landed 2026-04-24. Downstream of the `plan_shards` invariant: if all descriptor `min_id >= 0`, all thresholds and all ids inside `emit_side` / single-sided emit are non-negative, so the raw `i64` compares agree with `osm_id_cmp`. The planner-entry assert covers all four sites in one check.

- [x] ~~**`diff/parallel.rs:384` / `derive_parallel.rs:429`**~~ - landed 2026-04-24. Same argument as #3: the `merge_up_to(.min())` bound is correct for positive-only ids via the `plan_shards` invariant.

### Cluster 2: Defensive handling of adversarial or malformed input

**Decision landed 2026-04-24: option (d) - promote the five findings to hard errors at once-per-blob or once-per-transition checkpoints, AND build lying-indexdata test fixtures so future sites are caught by CI rather than individual read-audits.** Zero perf cost on happy-path inputs (checks are all at boundary transitions, not per-element hot paths). See [decisions/0004](decisions/0004-defensive-input-errors-and-fixtures.md) for the full decision record. The fixture infrastructure landed in a narrow form (two tests covering two of the five fixes); extended coverage for byte-level malformations tracked below under "Lying-indexdata fixtures (extended coverage)".

- [x] ~~**`renumber/mod.rs:240`**~~ - landed 2026-04-24. Replaced `max_node_id = pass1_schedule.last().max_id` with `pass1_schedule.iter().map(|t| t.max_id).max()`. O(N) once at startup; correct regardless of blob ordering. Regression test: `tests/cluster2_defensive_input.rs::renumber_survives_lying_sorted_header_out_of_order_blobs`.

- [x] ~~**`altw/external/stage1.rs:269-273` + `stage2.rs:459-493`**~~ - landed 2026-04-24. Added `max_id < min_id` sanity check at stage 1 blob-mapping entry (`build_node_blob_mapping`); the pre-existing stage 2 tail check promoted to hard `Err` on 2026-04-23 (`ab01438`) already covered the loose-bounds case.

- [x] ~~**`blob_meta/scan_ids.rs:192-202`**~~ - landed 2026-04-24. Replaced the unchecked `gran * raw` chain with `checked_mul` + `checked_add` throughout. On overflow the blob's bbox is dropped (rather than silently wrapping into indexdata); id-range coverage is retained so spatial filters fall back to full-decode for that blob only.

- [x] ~~**`renumber/wire_rewrite.rs:486-491`**~~ - landed 2026-04-24. Replaced terminator-byte counting with a `count_varints_strict` helper that walks `read_varint()` over the data and errors on truncated varints mid-stream or at the tail. Cost bump (~2-3x per byte in the count phase) is negligible: counts run once per relation, not per element.

- [x] ~~**`apply_changes/rewrite_block.rs:103`**~~ - landed 2026-04-24. Promoted the `header.is_sorted()` check in `rewrite.rs::build_header_bytes` from the `--locations-on-ways` branch to unconditional. The general path now also rejects unsorted base PBFs with a specific error. Regression test: `tests/cluster2_defensive_input.rs::apply_changes_rejects_unsorted_header`. Merge tests updated to use `write_test_pbf_sorted`.

- [x] ~~**`altw/external/stage2.rs:488-493`**~~ - landed 2026-04-23. Promoted `debug_assert_eq!` to an always-on `return Err(...)` with blob offset + expected/actual rank range in the error message. Negligible cost (once per blob, not per tuple).

### Cluster 3: Error path hygiene - counter accuracy and partial output

**Decision landed 2026-04-24: option (a) - shared primitive + "bump counters after write" rule.** Added `src/path_guard.rs` with a `PathGuard` that removes a file or directory on Drop unless `commit()` is called. Applied at the two partial-output sites (renumber output, geocode Pass 3 bucket dirs). Counter drift site moved the `rels_written` bump from worker pre-send to consumer post-write so both it and `orphan_refs` reflect only actually-emitted work on the error path. Silent-truncation site promoted to a hard error with a diagnostic. See [decisions/0003](decisions/0003-error-path-hygiene-via-pathguard.md) for the full decision record, alternatives considered, and the checklist for adding a new command.

- [x] ~~**`renumber/relations.rs:297-298`**~~ - landed 2026-04-24. Removed the worker-side `rw.fetch_add(blob_count, ...)`; `rels_written` and `r2d_orphans` now both bump in the consumer loop AFTER the successful `writer.write_primitive_block_owned`. Empty blobs contribute zero either way. No perf effect: same number of atomics, one extra on the consumer side that's already doing atomic stats.

- [x] ~~**`geocode_index/builder/pass3.rs:229-231`**~~ - landed 2026-04-24. Wrapped `fine_bucket_dir` and `coarse_bucket_dir` in `PathGuard::dir()` immediately after `create_dir_all`; the guards are `drop`ped explicitly after each `run_stage_b` success to release disk ASAP, and cover every error path via implicit Drop. Retained the entry-time stale-dir sweep for crash-recovery (SIGKILL with no Drop). Removed the redundant `remove_dir_all` at the tail of `run_stage_b`.

- [x] ~~**`geocode_index/builder/pass3.rs:152-167`**~~ - landed 2026-04-24. `parse_bucket_file` now returns `io::Result<Vec<...>>` and errors on `data.len() % BUCKET_RECORD_SIZE != 0` with a diagnostic naming the incomplete file length. A Stage A ENOSPC mid-flush is now a loud Stage B failure rather than silent data loss.

- [x] ~~**`renumber/mod.rs:256-262`**~~ - landed 2026-04-24. Wrapped the output path in `PathGuard::file()` right after `writer_from_header` succeeds; `output_guard.commit()` fires at the end of the success path (after final `writer.flush()`). On any mid-stream error (pass1 count mismatch, stage 2d failure, relation rewrite failure, final flush) the partial output file is now removed.

### Cluster 7: Latent-only-on-future-refactor or pathological-input items

**Decision landed 2026-04-24: triage per site - `debug_assert` + invariant comment / doc-only / drop-from-cluster - no blanket sweep.** See [decisions/0005](decisions/0005-latent-invariant-debug-asserts.md) for the full decision record, alternatives considered, and the triage rule for future findings of the same shape. Net code changes: six `debug_assert` additions across three files, one real fix in `read/blob.rs` (not latent - live iteration-stays-dead bug with regression test), two doc-only safety comments, two items rehomed (perf / pathological-input) outside the cluster.

- [x] ~~**`write/copy_range.rs:63-96`**~~ - landed 2026-04-24. Added module-level safety doc to `copy_range_fallback` identifying it as single-thread-output only and pointing parallel callers at `parallel_writer::copy_range_fallback_pwrite`.

- [x] ~~**`renumber/wire_rewrite.rs:271,276,480,486`**~~ - landed 2026-04-24. Added `debug_assert!(bytes[val_start - 1] < 0x80)` at all four `val_start - 1` splice sites in `reframe_ways` (fields 1 and 8) and `reframe_relations` (fields 1 and 9). A future PBF schema extension introducing field >= 16 that the rewriter has to splice would trip the assertion in test rather than produce corrupt output silently.

- [x] ~~**`apply_changes/streaming.rs:420-445`**~~ - no code change landed 2026-04-24. The load-bearing invariant ("`get_node` returns creates AND modifies") is already documented at `apply_changes/node_locations.rs:50-55`. Locking it with a test (modify a node in OSC, verify way refs pick up fresh coords) is better than a `debug_assert` here because the invariant lives at a different call site; deferred to the `-j N` parity / coord-freshness test shape in "Release prep > Test-shape gaps".

- [x] ~~**`altw/external/stage2.rs:67-72`**~~ - landed 2026-04-24. Replaced the raw subtraction with `bucket_rank_end.saturating_sub(bucket_rank_start)` and added a `debug_assert!(bucket_rank_start <= bucket_rank_end)` documenting the invariant the `rank_bucket_counts[bucket_idx] == 0` early-continue upholds in the caller. Safe on pathologically small inputs (`unique_nodes < NUM_BUCKETS`) even if that guard is ever removed.

- [x] ~~**`renumber/mod.rs:325-328`**~~ - landed 2026-04-24. Added `debug_assert!(STAGE2D_WORKERS >= 1 && !way_id_sets.is_empty())` at the `way_id_sets.remove(0)` site plus a doc comment on the `STAGE2D_WORKERS` constant tying the 0-is-invalid invariant to the panic it would produce. Future tweak to 0 trips the assertion with a clear message.

- [x] ~~**`reorder_buffer.rs:21-33`**~~ - no change landed 2026-04-24. The design-doc comment at `reorder_buffer.rs:21-28` already explains why `push`'s asserts are panics: seqs originate from `enumerate()`, a stale/duplicate seq is a programming error, and the panic surfaces loudly via `join().map_err(...)?`. The invariant is already loudly enforced and documented.

- [x] ~~**`read/blob.rs:670-681`**~~ - landed 2026-04-24 (real fix, not latent). `seek_raw` now sets `self.last_blob_ok = true` on a successful seek, so callers that recover from a parse error by seeking past the bad blob can resume iteration. Regression test in `tests/read_paths.rs::blobreader_seek_raw_clears_error_state`.

- [x] ~~**`write/writer.rs:108-145`**~~ - landed 2026-04-24. Added a "Latent blocking scenario" section to `to_path_uring`'s doc comment describing the wedged-uring-init case (blocks on `init_rx.recv()`, not `handle.join()` as the original TODO said) and why a `recv_timeout` is not the right fix without a real reproducer.

- [ ] **`geocode_index/reader.rs:800-831` - LOW (rehomed).** Per ADR-0005: not a latent invariant; the `Vec::contains` dedup is O(n^2) on `n` admin polygons containing a single point, where `n < 20` for any realistic query. Documented perf footgun, not a correctness bug. Tracked here as a future query-API perf note if admin overlap depth ever grows.

- [ ] **`geocode_index/builder/pass2.rs:295-304` - LOW (rehomed).** Per ADR-0005: not a latent invariant; integer-divide centroid for buildings crossing ±180 is a documented coordinate-math limitation (no such building exists in OSM). Left as-is; move to `DEVIATIONS.md` only if the surface matters.

### Straight fixes (no policy discussion needed)

- [x] ~~**`altw/external/stage4.rs:645`**~~ - landed 2026-04-24. `std::mem::take` swapped for `std::mem::replace(&mut frame_read_buf, Vec::with_capacity(frame_size))` so the next iteration's `resize(frame_size, 0)` sees a correctly-sized capacity.

- [x] ~~**`altw/external/coord_payloads.rs:104-153`**~~ - landed 2026-04-24. Replaced the `Vec<Mutex<Option<StraddlerPartial>>>` sized to `num_way_blobs` with a sparse `Mutex<FxHashMap<usize, StraddlerPartial>>`. Single global lock is fine: publish rate is O(straddlers) = ~hundreds at planet, not O(way_blobs) = ~57K.

- [x] ~~**`write/parallel_writer.rs:399-430`**~~ - landed 2026-04-24. Wrapped the `libc::pread` call in `copy_range_fallback_pwrite` with a loop that retries on `EINTR` and returns other errors unchanged.

- [x] ~~**`diff/derive_parallel.rs:240-248`**~~ - landed 2026-04-24. Added a `process_scratch_tag()` returning a `OnceLock<String>` holding the low 32 bits of process-start-nanos in hex; every `shard_xml_paths` call now splices `{pid}-{tag}-...` into the filename.

- [x] ~~**`geocode_index/reader.rs:1033-1040`**~~ - landed 2026-04-24. Renamed `segment_length`, `way_length`, and `accumulated_length` to `*_radians` suffix so the unit is explicit at every call site.

- [x] ~~**`altw/external/stage4.rs:573-600`**~~ - landed 2026-04-24. `ReorderBuffer::with_capacity(passthrough_items.len() + decode_threads)` replaces the hardcoded 32, so the pre-seed no longer forces growth.

- [ ] **`altw/external/mod.rs:191` - LOW.** `ScratchDir::new` uses `output.parent().unwrap_or(Path::new("."))` - if `output` is a bare filename with no parent component, scratch files land in the current working directory. A user running from `/` or a tmpfs cwd while outputting to a large disk can land ~224 GB of scratch on the wrong filesystem. The dense path has the same pattern. Trigger: running external-join from a small-fs cwd. Folds into Milestone 3 > Command surface > "CLI UX: scratch dir + mode naming" - pending the unified posture decision there.

### Per-site items (standalone, each needs individual review)

- [x] ~~**`renumber/wire_rewrite.rs:293-296`**~~ - landed 2026-04-24. Collapsed `resolve(id) + get(id)` double chunk lookup to a single `rank_if_set(id)` via a new `resolve_with_orphan` helper at the top of the file. Applied at the way-ref loop and all three relation-member-ref branches (node/way/relation). Matches `resolve`'s internal orphan-passthrough semantics; no behavior change, one chunk lookup saved per ref (~1.5 B refs at planet).

- [x] ~~**`geocode_index/builder/admin.rs:88-111`**~~ - landed 2026-04-24. Hole-in-outer containment now tests `point_in_ring(hp, &outer_f64)` against the **original** outer polygon rather than the Douglas-Peucker-`simplified` one. A hole whose first vertex lands near the outer boundary no longer gets dropped when aggressive simplification shifts `simplified` past it. "Which outer owns this hole" is topology from the original geometry; rendered geometry stored in `vertices` stays simplified.

- [x] ~~**`commands/getid/mod.rs:259`**~~ - landed 2026-04-24. `removeid` now takes a `force: bool` param and calls `require_indexdata` on entry, matching `getid`'s include-mode gate. On a non-indexed PBF without `--force`, the command errors cleanly up front instead of silently full-decoding every blob. CLI wiring updated and the previous `Warning: --force has no effect with --invert` message (which was incorrect - `--force` IS the gate on this path) removed.

- [x] ~~**`commands/tags_filter/mod.rs:778-785`**~~ - no code change landed 2026-04-24, comment added. The wasteful-decode path is only reachable when the upstream `require_indexdata` at :177 is waived by `opts.force` or `opts.invert` (both explicit slow-path opt-ins), so the current shape is correct by construction. A multi-line comment at the filter site documents the load-bearing invariant and lists the tightening shape needed if the upstream gate is ever relaxed.

- [x] ~~**`commands/inspect/scan.rs:61-119`**~~ - landed 2026-04-24. `try_index_only_scan` now prints a one-line stderr notice when `--direct-io` was requested, explaining that `HeaderWalker`'s `posix_fadvise(POSIX_FADV_RANDOM)` provides the cache-pollution avoidance `--direct-io` would have given, and that the full-decode fallback (triggered by any non-indexed blob) still honours `--direct-io` normally.
