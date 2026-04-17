# Changelog

## Unreleased

### Renumber - external join rewrite

Complete rewrite of the `renumber` command using an external-join architecture. The in-memory path has been removed; external is now the only implementation.

- **Wire-format rewriters** for DenseNodes, ways, and relations - rewrite IDs directly in the protobuf wire format, avoiding full PrimitiveBlock decode/re-encode.
- **Fused scan passes** - R1+R2A+R2B merged into a single relation scan; way resolve fused into stage 2d; relation resolve fused into R2d.
- **Parallel workers** - 4-6 pread workers for pass 1 and stage 2d, parallel R2d relation rewriter, parallel stage 2b radix sort.
- **IdSetDense throughout** - replaced FxHashMap relation map and BTreeSet ID sets with chunked sparse bitsets. Rank-indexed lookup eliminates shard files and mmap scatter for way maps.
- **Shared blob schedules** - cached across all phases, eliminating redundant index scans.
- **zlib:1 output compression** - faster recompression with minimal size impact.
- Planet: **194s (3m14s)**, down from 3,456s at first measurement - **94% reduction** over the optimization arc.

### Commands

- **renumber**: dropped in-memory path; external is now the only implementation. `--mode` flag removed. Negative input IDs rejected with clear error.
- **renumber**: orphan-ref tracking - `RenumberStats.orphan_refs` counts way refs and relation members not found in the input. `print_summary()` warns when non-zero.
- **renumber**: comprehensive per-phase instrumentation (consumer drain-rate, sub-phase counters for all 4 phases).
- **add-locations-to-ways**: comprehensive per-stage instrumentation for external join (sub-phase counters for all 4 stages, consumer drain-rate, bucket load/scatter/write breakdown).
- **extract** (smart/complete): reuse PASS1 blob schedule in subsequent passes, reducing redundant index scans. PASS2 way deps converted to per-blob send.
- **derive-changes**: stream output to temp files instead of buffering all changes in memory.
- **renumber**: forward-ref relation bug fixed via two-pass structure. Negative ID guard added.

### Documentation

- **DEVIATIONS.md**: added renumber negative-ID rejection and orphan-ref handling sections. Synced to `docs/cli/deviations.md`.

### Library

- `IdSetDense`: rank-indexed lookup (`resolve()`), denser 64B rank blocks, atomic set support, negative-ID guard. Migrated all `IdSet` (BTreeSet) call sites to `IdSetDense` for O(1) lookups.
- `PbfWriter`: rayon dispatch bounded by permit pool to prevent unbounded in-flight blob accumulation.
- `external_radix`: shared `ScratchDir` + `BucketWriters` module extracted for reuse across external-join commands.
- `elements_xml`: borrowed element XML writers (zero-copy OSC output).
- `merge.rs` split into submodules: `classify`, `diff_ranges`, `node_locations`, `rewrite`, `stats`.
- Eliminated `Bytes→Vec` copies in all 16 sequential decode paths (Phase A + Phase B).
- Removed dead `Blob::decompress_pooled` method.
- Element-equivalence test helper for PBF cross-checks.

### Bug fixes

- 19 bugs fixed from post-release code review (F1-F22, F23-F32, F33-F40).
- `dp_count_range`: use clamped segment distance matching `dp_mark`.
- `decompress_blob_raw` lifetime bound, `RelMember` error type.
- Dead `loc_missing` increments removed from merge way writers.
- Geocode builder pass 2 extraction + merge locations pre-scan.
- Geocode `FORMAT_VERSION` bumped to 2, `cover_segment` steps capped.
- **Geocode builder: fail hard on u16 on-disk count overflow** instead of
  silently truncating. Stage B (street/addr/interp per-cell entries, admin
  entries) and per-way `StreetWay.node_count` / `InterpWay.node_count`
  previously used `.min(u16::MAX)` which could drop data without any error
  signal. Errors now include the offending cell/way and explicit guidance to
  bump the field to u32 + FORMAT_VERSION if the limit is ever hit in
  practice.
- **`IdSetDense::set_atomic` / `set_atomic_if_new`: diagnostic panic on
  out-of-range IDs.** The `.expect("not pre-allocated")` was opaque when
  indexdata under-reported `max_id` (rare but possible with corrupted
  inputs). New panic text names the offending ID, the pre-allocated upper
  bound, and the most likely root causes (indexdata mismatch, hard-coded
  cap overshoot, missing `pre_allocate`).

### Documentation

- **Null Island sentinel collision**: ALTW stage 2 and the geocode builder
  both use `(lat_e7 == 0, lon_e7 == 0)` as the unresolved-coordinate
  sentinel, colliding with the legitimate OSM node at 0°, 0° off the
  African coast. Flagged in both source files as a known limitation with
  pointers to the other site so a future fix (presence bitmap) covers both.
  Root `CORRECTNESS.md` Null Island section updated to list the geocode
  builder site explicitly alongside the three ALTW index types.
- **Interpolation unresolved sentinel**: `SlimInterpWay.start_number == 0
  && end_number == 0` doubles as "unresolved" and as a legitimate
  interpolation way that genuinely starts at house number 0. Documented at
  the struct definition, init site, and resolve site, and promoted to a
  new `CORRECTNESS.md` section.
- **Geocode u16 on-disk count caps**: new `CORRECTNESS.md` section
  documenting the per-cell and per-way u16 caps, the builder's hard-error
  contract on overflow, and the `FORMAT_VERSION` bump path if a real
  workload ever hits the limit.
- **`parallel_classify_accumulate` safety envelope**: clarified to describe
  three tiers (safe sparse, borderline, unsafe dense) with the geocode
  Pass 1.5 call site as the borderline exemplar. Pass 1.5 call site cross-
  links back to the contract and to the rewrite item in
  `notes/geocode-build-opportunities.md`.

### Testing

- Integration tests for 6 previously untested production pipeline surfaces.
- Tests for renumber remapping, `BlobFilter` composition, `merge_pbf` overlap.
- `IdSetDense` unit tests, sortedness tests for external renumber.

### Code quality

- 9 duplicated patterns consolidated (F76-F99).
- 36 minor cleanups (F101-F136).
- Shared OSC XML writers, unified getid filter, generic `sweep_merge`.
- `DenseNodeIter` batch `kv_pos` reverted after +8.7% regression.

### Dependencies

- `protohoggr` 0.2.1 → 0.4.0 (`read_raw_field` for wire-format rewriters).
- `hotpath` 0.14.1 → 0.15.

### Performance highlights

| Operation | Dataset | Time | vs 0.2.0 |
|-----------|---------|------|----------|
| renumber (external) | Planet 87 GB | 194s | new |
| derive-changes | - | streaming | constant memory |

## 0.2.0 - 2026-04-09

First public release.

### Commands

Full PBF processing pipeline validated at planet scale (87 GB, 11.6B elements, 30 GB host):

- **cat** - passthrough with indexdata generation, type filtering with raw frame passthrough. Planet: 497s buffered.
- **sort** - Sort.Type_then_ID ordering.
- **extract** - simple, complete-ways, and smart strategies. Parallel 3-phase classification via pread workers. Raw frame passthrough for fully-contained node blobs. Columnar dense node decode for bbox classification. Planet simple: ~100s.
- **multi-extract** - single-pass N-region extract with parallel decode workers. Denmark 5-region: 1.9s, Japan 5-region: 7.3s.
- **tags-filter** - two-pass with tag index filtering, parallel classification, relation closure with way/node dependency resolution.
- **getid** - ID-range blob skip, raw frame passthrough for `--invert`, `--add-referenced` with parallel way dependency scan.
- **add-locations-to-ways** - dense, sparse, and external join index types. External join: planet 1,462s (24.4 min), 3.9x faster than dense.
- **apply-changes** - 4-phase batch merge with passthrough coalescing. Planet daily diff: 762s, 1.8 GB RSS.
- **diff** - block-pair merge-join with compressed byte comparison (skip decode for unchanged blobs). Streaming constant-memory.
- **derive-changes** - OSC generation from two sorted PBFs.
- **merge** - merge-sort multiple PBFs.
- **inspect** - blob statistics, tag counting, `--show` for single element lookup by ID.
- **check** - reference integrity checking (`--refs`).
- **build-geocode-index** - 4-pass geocode index builder. Planet: 1,346s (22.4 min), 17.8 GB RSS.
- **renumber** - sequential ID renumbering.
- **time-filter** - timestamp-based element filtering.

### Library

- `ElementReader` - sequential, parallel (rayon), and pipelined iteration modes.
- `IndexedReader` - seekable reader with blob-level index for filtered queries.
- `PbfWriter` - sync, pipelined (rayon), O_DIRECT, and io_uring write modes.
- `BlockBuilder` - iterator-based tag API, dual-buffer single-pass encoding.
- `DenseNodeColumns` - columnar dense node decode for batch classification.
- `IdSetDense` - chunked sparse bitset with O(1) set/get, rank index, bitwise OR merge.
- `geocode_index::Reader` - reverse geocoding queries via S2 cell lookup (feature-gated).

### Architecture

- Pread-from-workers: parallel blob decode via `pread(2)` with shared file descriptor, eliminating cross-thread PrimitiveBlock retention.
- `parallel_classify_phase` / `parallel_classify_accumulate`: two-function API for planet-safe parallel classification. Per-blob streaming for dense paths, per-worker accumulation for sparse paths.
- Wire-format scanners (`node_scanner`, `way_scanner`): lightweight ID/coordinate extraction without PrimitiveBlock construction.
- Raw frame passthrough: skip decompress+recompress for fully-contained blobs.
- Blob-level indexdata (v2): element type, ID range, count, spatial bbox per blob.

### Performance highlights

| Operation | Dataset | Time |
|-----------|---------|------|
| Read (parallel) | North America 18.8 GB | 22s |
| cat (indexdata) | Planet 87 GB | 497s |
| add-locations-to-ways (external) | Planet | 1,462s |
| build-geocode-index | Planet | 1,346s |
| apply-changes (daily diff) | Planet | 762s |
| extract simple | Europe 35 GB | 113s |
| multi-extract 5-region | Japan 2.4 GB | 7.3s |

### License

Dual MIT/Apache-2.0.
