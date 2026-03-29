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

- [ ] **Extract sorted pass1 (`37b7c19`): benchmark and clean up.** Parallelizes
  way/relation ID collection for sorted PBFs by batching blocks and using
  `par_iter` with thread-local Vecs. Algorithm is correct but has open issues:
  1. ~~**No benchmark data.**~~ Benchmarked (commit `1b10bfd`): Denmark simple
     2259ms (-14% from 2625ms baseline), Japan simple 11,643ms (-8% from
     12,619ms). Sorted pass1 optimization validated — single-pass eliminates
     second file read. Full results in `notes/performance.md`.
  2. ~~**~300 lines of duplication** between `collect_pass1` and `collect_pass1_smart`.~~
     Refactored into `collect_pass1_generic<H: RelationHandler>` with
     `CompleteRelationHandler` (no-op) and `SmartRelationHandler` (collects
     extra way/node IDs). Net -144 lines. Verified via `brokkr verify extract`.
  3. ~~**`Mixed | Empty` handler is a full sequential fallback.**~~
     Split Empty from Mixed (commit `ff29c1f`). Empty blocks now skip without
     flushing batches. Mixed blocks retain sequential fallback — rare in
     production PBFs, not worth parallelizing (perf reviewer consensus).
  4. ~~**Vec-per-block allocation in batch helpers.**~~ Closed. Alloc profiling
     showed 1.7 GB cumulative churn (52% of extract budget), but RSS stayed
     under 200 MB — the allocator recycles pages efficiently. Three fix
     attempts failed: IdSetDense fold+merge (15x wall time regression,
     merge is O(id_space)), Vec fold (rayon splits at same granularity,
     no reduction), map_init (can't return ownership without losing capacity).
     The original per-block Vec pattern is the correct tradeoff.
  5. **`decode_threads(1)` may under-utilize.** Reduces pipeline decode to one
     thread since the consumer does its own parallelism. Sensible tradeoff but
     may leave the I/O thread idle waiting for the single decoder.

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
  Full results in `notes/performance.md`.

## ~~BlobReader fadvise: gate on `target_os = "linux"` instead of `linux-direct-io`~~ DONE

Done in commit `7acbb1a`. libc now non-optional, fadvise gated on `target_os = "linux"`.

## Cross-pipeline optimization

PrimitiveBlock cross-thread alloc/free retention affects every command using
the pipelined reader at 400K+ blocks (Europe/planet scale). The geocode builder
is the predicted next victim (16 GB DenseMmapIndex + 25 GB retention = OOM).

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.

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

### Raw group passthrough (priority 1)

Copy raw PrimitiveGroup bytes for groups where all elements are selected.
Partial-match groups fall back to decode + re-encode. String table copied
whole. Applies to every re-encoding command: extract, cat --type,
tags-filter, sort, getid, renumber, time-filter.

Four primitives needed: `raw_group_bytes`, `raw_stringtable_bytes`,
`classify_group`, `frame_raw_block`. Independent of read-path work.
See [notes/raw-group-passthrough.md](notes/raw-group-passthrough.md).

### Write-path throughput

After raw group passthrough, `BlockBuilder` (`src/write/block_builder.rs`)
and `PbfWriter` (`src/write/writer.rs`) are the next bottleneck for commands
that must re-encode partial-match groups. Opportunities: SIMD varint encoding
in `src/write/wire.rs` (the write-side protobuf primitives), zlib compression
level tuning (currently hardcoded level 6), and reducing per-element overhead
in `BlockBuilder::add_node/add_way/add_relation` (string table construction
is the hot path — FxHashMap lookup + Rc<str> alloc per unique string).
See [notes/SIMD.md](notes/SIMD.md) for the varint research.

### Published benchmark matrix

Denmark/Japan/Europe/planet benchmarks for every command. Time, RSS,
temp disk, compression mode. Regression CI to prevent backsliding.

### Smaller items

- [ ] `merge --locations-on-ways` node scanner — `src/commands/merge.rs` ~line
  1460 decompresses passthrough node blobs into full PrimitiveBlock just to
  collect (id, lat, lon). Replace with `extract_node_tuples` from
  `src/commands/node_scanner.rs` (same pattern as ALTW pass 1).
- [ ] `node_stats.rs` — uses `for_each_pipelined` (cross-thread PrimitiveBlock).
  Only needs id/lat/lon. Convert to node-only scanner for planet safety.
- [ ] `getid::parse_ids_from_pbf` (`src/commands/getid.rs` ~line 132) —
  full PrimitiveBlock decode for an ID-only scan. Could use a lightweight
  wire-format scanner that extracts only element IDs.
- [ ] `tags_count.rs` — uses `for_each_primitive_block_batch` (pipelined).
  At planet scale with 520K+ blobs the retention pattern applies. Convert
  to sequential BlobReader if planet-scale tag analytics is needed.
- [ ] ALTW dense pass 2 decode-all fallback (`write_output_decode_all` in
  `src/commands/add_locations_to_ways.rs` ~line 1045) — uses
  `into_blocks_pipelined` processing all blobs. 25+ GB retention at planet.
  Only triggers with `--force` on non-indexed PBFs. Niche but the last
  unmitigated retention path.

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Multi-extract

Produce 200+ regional extracts from a single planet pass. The CLI already
has `extract --config` (`src/commands/extract.rs`, `ExtractConfig` struct)
for multiple bbox/polygon regions from a JSON config file. Current
implementation runs each extract sequentially. The optimization: read the
PBF once, classify each element against all N regions simultaneously, and
route to N `PbfWriter` instances. The pread-from-workers infrastructure
(`pread_write_pass`) could dispatch workers per-region or per-blob with
multi-region classification. At planet scale, this avoids N × 87 GB of
redundant I/O — critical for data distributors maintaining regional extracts.

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

### Command surface

- [ ] `show` — display a single element by ID with all metadata, tags, refs,
  members. Human-readable output (like `osmium show`). Needs indexed lookup
  via `IndexedReader` or sequential scan with early exit.
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
  `notes/osmium-parity.md`.

### Ecosystem

- [ ] crates.io release (protohoggr + pbfhogg + pbfhogg-cli).
- [ ] CI with benchmark regression guard.
- [ ] API documentation for library consumers.
- [ ] PyO3 Python bindings (read/write API for the Python ecosystem).
- [ ] Packaged "planet on 32 GB" reference pipeline (documented, runnable).

### Research / stretch ideas

- [ ] Incremental geocode index update (daily diff → index patch, no full rebuild).
- [ ] Incremental extract update (`extract --apply-changes` — base extract + OSC +
  region → updated extract without re-reading planet).
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
- [ ] Streaming pipeline composition (pipe commands without intermediate
  PBF encode/decode — library-level iterator API).
- [ ] Zstd as default compression for internal pipelines (3-5x faster
  decompress than zlib at equivalent ratios).
- [ ] Dense ALTW compact rank-indexed array (same pattern as geocode builder —
  better locality on hosts where dense currently works, reviewers split 1/8).
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [ ] History-file support — decide in-scope or explicitly out-of-scope.

## Release prep

### crates.io blockers

- [ ] **Publish `protohoggr` first** — currently `path = "../protohoggr"` only. Add `version = "0.2"` alongside the path dep so crates.io resolves it. Publish protohoggr before pbfhogg.
- [ ] **Add `version` to CLI path dep** — `cli/Cargo.toml` needs `version = "0.2"` on the `pbfhogg` dep if we publish pbfhogg-cli too (or skip publishing the CLI crate).
- [ ] **Clarify license** — README mentions MIT but only Apache-2.0 is declared. Pick one story.

### Testing

See `notes/test-plan.md` for the full pre-release test matrix (feature permutations,
I/O modes, CLI commands) and `notes/performance.md` for consolidated baselines.

### Cross-validation known diffs

Three `brokkr verify` commands show known differences vs osmium. These are semantic
disagreements, not bugs — but should be investigated and either fixed or documented
before release.

- [x] **Planet-scale merge on 32 GB host** — **762s (12.7 min), 1.8 GB RSS.** 86% rewrite, 3.4M diff entries. Validated.
- [ ] **`cat --type` OOM on planet (87 GB, 30 GB host)** — Two fixes landed:
  1. Batch-side (commit `abe2782`): `DECODE_BATCH_BYTE_BUDGET = 32 MiB` caps
     decompressed bytes per batch via `for_each_primitive_block_batch_budgeted`.
  2. Writer-side: compression moved into the `par_iter` parallel phase, then
     `write_raw_owned` feeds the writer thread's bounded `sync_channel(32)`.
     Eliminates the unbounded `rayon::spawn` queue that was the main OOM cause.
  Europe (33.6 GB) completes in 121s, 224/8200 batches byte-limited.
  **Planet validation still pending.** Strip `eprintln!` instrumentation
  in `cat_filtered` after planet run.

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
- [ ] `elements_skip_metadata()` in `--locations` without `--extended`: minor, deferred.
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

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Add a CHANGELOG.md before first tagged release
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

