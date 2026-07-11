# Changelog

## Unreleased

### Breaking changes

- **Minimum supported Rust version raised 1.87 → 1.96** (library and CLI). Required by the move to `if let` chains and `as_chunks` in the read and command paths.

### Changed

- `getparents` and `getid` include mode now select between the pread
  header walk and a full-file scan from a bounded OSMData blob-count
  estimate. This preserves the walker's win on low-blob-count planet
  encodings while recovering full-scan performance on high-blob-count
  encodings such as Geofabrik extracts.

- The pipelined reader (`for_each_pipelined`,
  `for_each_block_pipelined`, `into_blocks_pipelined`) now bounds
  decode-in-flight memory at the `decode_ahead` knob (default 32 blocks).
  Previously the decode stage admitted the entire file at disk
  rate, so decoded-block memory grew with file size (21.5 GB peak observed
  on a 19 GB input). Dropping `PipelinedBlocks` early, or returning an
  error from the block closure, now stops the pipeline within about
  `decode_ahead` blobs instead of reading and decompressing the rest of the
  file in the background.

### Fixed

- **OSC output no longer corrupts multi-line tag values.** All OSC-emitting
  paths (`diff --format osc` / derive-changes, `merge-changes`,
  `tags-filter --input-kind osc`) wrote raw newline/tab/CR characters
  inside XML attribute values. XML attribute-value normalization replaces
  those with spaces at parse time, so applying a pbfhogg-derived OSC
  silently turned multi-line tag values (e.g. `inscription`) into
  space-separated ones. They are now escaped as character references
  (`&#10;` etc.), matching osmium's OSC writer. Affected attributes: tag
  keys/values, relation member roles, user names.
- **apply-changes now carries OSC element metadata into its output.**
  The OSC parser never read `version`/`timestamp`/`changeset`/`uid`/`user`
  attributes, so every OSC-sourced created or modified element in merged
  output silently carried version 0 and no timestamp (osmium's
  applychanges preserves them). The overlay now stores a metadata block
  per element and all nine apply-changes write paths pass it through.
  Elements whose OSC carries no metadata attributes are written without
  metadata, as before.
- **`diff --format osc` now emits full element metadata** (timestamp,
  changeset, uid, user - previously version only), matching osmium's OSC
  writer and making the derive -> apply roundtrip metadata-lossless
  end-to-end. Attributes absent from the source element are omitted.

### Dependencies

- `hotpath` 0.17.0 → 0.21.1, `quick-xml` 0.40.1 → 0.41.0, `io-uring` 0.7.12 → 0.7.13, `memmap2` → 0.9.11, `rustc-hash` → 2.1.3.
- **`s2` 0.0.13 → 0.1.0** (optional, behind `geocode-reader`/`commands`). Pinned with `default-features = false, features = ["float_extras"]` so the geocode path no longer pulls s2's `serde` default.

## 0.4.1 - 2026-06-22

### Dependencies

- **`flate2` 1.0 → 1.1.9, pulling `zlib-rs` 0.6.4** (the pure-Rust zlib backend used for all blob (de)compression). This is the headline change for the release.
- `hotpath` 0.15.0 → 0.17.0, `quick-xml` 0.39.2 → 0.40.1, `libc` 0.2.185 → 0.2.186, `bytes` → 1.12.0, `serde_json` → 1.0.150. The `quick-xml` minor bump deprecated `Attribute::unescape_value`; OSC attribute parsing migrated to `normalized_value(XmlVersion::Implicit1_0)` (same behavior).

## 0.4.0 - 2026-05-09

### Breaking changes

- **`add-locations-to-ways --index-type dense` removed.** Sparse beats dense at every measured scale (japan 51.6s → 11.9s, europe dense OOMed) with no regime where dense won. The flag now errors pointing at `--index-type sparse`. Default `--index-type` changed from `dense` to `sparse`. `--index-type auto` now resolves to `external if sorted+indexed, sparse otherwise` (was: dense fallback).

### Commands

- **add-locations-to-ways** sparse: rewritten as a rank-indexed flat layout with parallel pass 1 and wire-format reframe in pass 2. Japan 20.9s → 11.9s (-43%); europe OOM → 5m59s (now competitive with external). Temp disk shrinks 2.4-2.8× (japan 5.7 GB → 2.0 GB; europe 52 GB → 29 GB). The strictly-increasing-id precondition is gone.
- **merge-changes**: parallel-drain via multi-member gzip output. Planet 7-OSC `--osc-range 4914..4920`: **267s → 55s (5.0×)**; `--simplify`: **262s → 74s (3.6×)**. 1-OSC fast path unchanged. Output is within 1% of the serial path's bytes; OSC consumers (osmium, osmosis, gzip CLI, `MultiGzDecoder`) all accept multi-member gzip. New `-j/--jobs N` caps the worker pool.
- **repack**: new command. Re-encode a PBF with a configurable `--elements-per-blob N` cap. Tags, refs, members, metadata, and DenseNodes encoding round-trip; output is type-sorted and propagates `Sort.Type_then_ID`. Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement.
- **degrade**: new command. Produce a valid-but-adversarial PBF for benchmarking non-optimal code paths. Three composable transformations: `--unsort` (clear `Sort.Type_then_ID` and overlap adjacent same-kind blobs), `--strip-locations` (drop `LocationsOnWays`), `--strip-indexdata` (clear `BlobHeader.indexdata` on every OsmData blob). At least one required; `--strip-indexdata` alone is a blob-level passthrough (payload bit-identical). Planet-safe in all combinations.
- **time-filter** (snapshot path): now planet-safe. Planet 5× SIGKILL → 4m30s, 812 MB peak anon (was ~28 GB at kill). Europe peak anon 16.9 GB → 324 MB (-98%); europe wall regresses 1m32s → 2m27s - the cost of bounding cross-thread retention.
- **tags-filter** `--invert-match`: planet-safe. `-i w/highway=primary` peak anon 28.3 GB → 7.0 GB on planet; wall within noise (8m08s → 7m57s).

### Library API

- `BlockBuilder::with_element_cap(n)` constructor for callers that need
  a non-default per-block element cap. `BlockBuilder::new()` keeps the
  8000 default.

### Performance highlights

| Operation | Dataset | Time | vs 0.3.0 |
|-----------|---------|------|----------|
| merge-changes (7-OSC range) | Planet 87 GB | 55s | -79% (5.0×) |
| merge-changes `--simplify` (7-OSC range) | Planet 87 GB | 74s | -72% (3.6×) |
| time-filter (snapshot) | Planet 87 GB | 4m30s | OOM → planet-safe |
| tags-filter `-i` (way-deps phase) | Planet 87 GB | 7m57s | 28.3 GB → 7.0 GB peak anon |
| repack `--elements-per-blob 8000` | Planet 87 GB | 6m20s | new |
| degrade `--strip-indexdata` | Planet 87 GB | 79s | new |

## 0.3.0 - 2026-04-27

### Breaking changes

- **`pbfhogg diff --summary` renamed to `--osmium-summary`.** The flag flips the stderr stats line to osmium's `Summary: left=N right=N same=N different=N` format. Migration: replace `--summary` with `--osmium-summary`, or use the unchanged `-s` short form.
- **`renumber --mode` flag removed.** External join is the only implementation.
- **`BlobReader::new_seekable<R>` and `IndexedReader::new<R>` bounds widened** from `R: Read + Seek + Send` to `R: BlobReaderSource + Send`. Custom reader types add `impl BlobReaderSource for MyReader {}`.
- **Geocode `FORMAT_VERSION` bumped to 2.** Indexes built with older versions must be rebuilt.
- **`IdSetDense` renamed to `IdSet`** and moved from `getid` to the top-level `pbfhogg::idset` module.
- **`pbfhogg::getid::IdSet` renamed to `pbfhogg::getid::ElementIds`.** Field accessors and method calls unchanged.
- **`pbfhogg::getid::removeid` adds `force: bool`** (positional, between `direct_io` and `overrides`). Pass `false` to require indexdata, `true` to permit non-indexed input.
- **`apply-changes --jobs 1` rejected.** Pass `--jobs 2` or omit `--jobs` for the default. The default's minimum is also bumped to 2 on low-core hosts.

### Commands

- **renumber**: complete rewrite to an external-join architecture. Planet 3m25s, 3.3 GB peak RSS, zero temp disk. Negative input IDs are rejected. Orphan refs (way refs / relation members absent from the input) are counted and surfaced in the summary.
- **apply-changes**: new descriptor-first streaming pipeline and parallel-pwrite writer backend (now the default). `-j/--jobs N` worker-count override. Planet daily diff 762s → 81s with the default backend; 135s with `--compression none`. `io_uring` and `--direct-io` remain opt-in.
- **build-geocode-index**: parallel Pass 2 / Pass 3. Planet 21m → 7m (-65%). Pass 1.5 peak anon 29.5 GB → 3.0 GB.
- **build-geocode-index**: now hard-errors on three on-disk-format overflow classes (admin-vertex byte offset, admin-cell entries offset, per-cell / per-way `u16` counts) instead of silently producing wrong output. Error names the offending field and points at the `FORMAT_VERSION` bump path.
- **check --refs**: parallel three-phase scan. Planet 1,225s → 70s (17.5×); Europe 426s → 34s (12.7×). Peak RSS 2.17 GB.
- **check --ids**: parallel three-phase scan in both modes. Streaming (default) was previously OOM-killed at planet; now planet-safe at 57s wall, 504 MB peak. `--full` Europe 313s → 53s (5.9×). The element-level type-order check on non-indexed input is dropped (matches the existing `--full` semantics; PBFs with indexdata get the offset-based check from both modes).
- **cat --clean** / **cat --type X --clean**: parallel three-phase scan per kind, replacing a shape that was OOM-killed at planet. Now planet-safe at 5m48s wall, 835 MB peak. Output is type-sorted (nodes, ways, relations); already-sorted input keeps its structure, unsorted input is re-sorted, mixed-type blobs are split.
- **diff** / **diff --format osc**: new `-j/--jobs N` for shard-parallel merge. Planet text 35m → 3m30s at `-j 16` (10.2×); `--format osc` 37m → 5m13s (7.1×). `-j 1` (default) keeps the sequential path.
- **getid** include mode: header-walk fast path. Planet 44s → 7s (6.2×); disk read 88 GB → 636 MB.
- **inspect** default metadata: header-walk fast path. Planet 21s → 6.5s.
- **inspect --nodes** / **inspect --tags**: now parallel, with `-j/--jobs N`. Planet `--nodes -j 16` 57s; `--tags -j 16` 2m50s. Germany `--nodes -j 8` 18.5s → 3.6s.
- **add-locations-to-ways `--index-type external`**: rewrite plus a follow-up rankless reshape. Planet 24m → 10m4s (-58%); Europe 4m52s → 4m31s. The `IdSet` rank machinery is no longer used by this command.
- **extract** (all strategies): pass-1 blob schedule reused across subsequent passes. Europe smart 254s → 181s (-29%).
- **getparents**: skip node-only blobs via `BlobFilter` when the query doesn't need nodes (~85% of blobs at planet scale).
- **derive-changes**: streams output to temp files; constant memory regardless of diff size.
- **tags-filter**: new `-j/--jobs N` on the two-pass path.

### Bug fixes

- **Negative input IDs are now rejected at the input boundary** in `renumber` (every `IdSet` entry point) and in `getid` (parse-time, matching osmium's getid). Previously `renumber` could panic or produce phantom orphans, and `getid` silently dropped negative-id queries with a misleading `"no IDs specified"` message. See `DEVIATIONS.md > "Negative input IDs rejected project-wide"` for the full project-wide stance.
- **Truncated and adversarial PBF inputs now hard-error** instead of silently producing partial output. The contract covers `BlobReader::next`, `BlobReader::skip_blob_body`, `HeaderWalker::next_header`, and `FileReader::skip` (the caller-side payload-skip used by `has_indexdata`, `diff`, `cat --dedupe`, and `altw::passthrough`). Adversarial blob-header length prefixes can no longer trigger multi-GB allocations or process aborts. Truncated headers no longer produce past-EOF schedule entries that fail later inside pread workers. See `reference/truncation-handling.md`.
- **Multiple read sites** in `renumber`, `apply-changes`, and the ALTW external pipeline now reject malformed input at the boundary instead of panicking or scrambling output: truncated relation `memids` varints, DenseNodes blocks whose `granularity * offset` would overflow `i64`, and ALTW node blobs whose indexdata advertises `max_id < min_id`.
- **`apply-changes --io-uring`** on indexed input produced structurally-broken PBFs on the `CopyRange` fast-path. Fixed.
- **`apply-changes` (and `merge`)** now require a base PBF with `Sort.Type_then_ID` in the header regardless of `--locations-on-ways`. The general path previously accepted unsorted headers and could silently drop upsert creates. Migration: re-sort with `pbfhogg sort`.
- **`apply-changes --force` on non-indexed input** could silently drop upsert modifications, or emit every remaining upsert of a kind as gap-creates before a single blob. Fixed.
- **`apply-changes --force --locations-on-ways` on a non-indexed PBF** silently stripped LocationsOnWays data from base ways. Now rejected up front with a migration hint.
- **`apply-changes` drain loop** could spin indefinitely if a worker thread panicked mid-stream. The loop now breaks on channel-disconnect; the worker's panic propagates with a clear diagnostic.
- **`cat --dedupe`** silently dropped elements at kind boundaries. Fixed.
- **`sort`** silently dropped elements at kind boundaries. Fixed.
- **`inspect --show`** returned "not found" on unsorted PBFs when the target element lived in a later blob with a smaller `min_id` than a preceding blob. Fixed.
- **`inspect --direct-io`** on the indexed-PBF header-only fast path silently ignored the flag (the path uses `posix_fadvise(POSIX_FADV_RANDOM)` for equivalent cache-avoidance). Now prints a one-line stderr notice; the full-decode fallback honours the flag as before.
- **`renumber`** no longer panics in `IdSet::set_atomic` when the header advertises `Sort.Type_then_ID` but the node blobs are out of order.
- **`renumber`** removes its partial output file on every error path.
- **`removeid`** (`getid --invert`) on a non-indexed PBF silently fell through to full decode + re-encode for every blob instead of using the (unreachable-without-indexdata) raw-passthrough fast path. Now errors up front unless `--force` is passed.
- **`add-locations-to-ways --index-type external`** could silently produce wrong coordinates on PBFs with loose blob indexdata. Now hard-errors naming the offending blob.
- **`add-locations-to-ways --index-type external --force` on a non-indexed PBF** failed with a confusing error deep inside the metadata scan. Now rejected up front with a migration hint.
- **`add-locations-to-ways --index-type external`** hit `EMFILE` on a default-ulimit shell. Fixed.
- **Geocode builder** could drop admin inner rings (holes) on multi-outer relations when aggressive Douglas-Peucker simplification on the outer ring shifted the simplified boundary past the hole's first vertex. Hole containment now tests against the original (unsimplified) outer polygon; rendered geometry on disk is unchanged.
- **Geocode builder Pass 3 Stage B** silently truncated trailing partial records when a Stage A bucket file write was interrupted (ENOSPC, SIGKILL during write). Now errors with a message naming the incomplete file length.
- **Geocode `simplify_ring`** Douglas-Peucker could exceed `max_vertices` due to metric divergence between cost and marking passes. Fixed.
- **Parallel and io_uring writers** could silently truncate output on upstream framer panic. Fixed.
- **Parallel-pwrite cross-device passthrough copy** could fail on a signal-interrupted `pread` instead of retrying. Fixed.
- **`IdSet::any_in_range` blob prefilter** silently dropped any blob whose indexdata range straddles zero (negative `min`, positive `max`). Production input never triggers this; mixed-sign input (JOSM-staged data, hand-crafted PBFs) lost legitimate `getid` and ALTW external matches. Fixed.
- **Scratch / temp file leaks** on error paths in parallel `diff`, parallel `derive-changes`, geocode Pass 3 buckets, and OSC assembly aggregates are now cleaned up by `PathGuard` RAII. Concurrent `pbfhogg` processes against the same scratch directory no longer collide on temp file names (PID + process-lifetime random tag in the path).
- **`BlobReader::seek_raw`** left iteration permanently stuck after a prior `next()` returned an error. A successful `seek_raw` now clears the sticky error flag.

### Library

- **`PbfWriter::write_primitive_block_no_indexdata`.** New public method that writes an `OSMData` blob without the `BlobHeader.indexdata` / `BlobHeader.tagdata` fields, for producing byte-for-byte fixtures matching third-party tools.

### Dependencies

- **`roaring` removed.** Both remaining consumers (`check --refs`, `check --ids --full`) migrated to `IdSet`.
- `protohoggr` 0.2.1 → 0.4.0.

### Performance highlights

| Operation | Dataset | Time | vs 0.2.0 |
|-----------|---------|------|----------|
| cat (indexdata generation) | Planet 87 GB | 86.5s | -83% (5.8×) |
| cat --clean | Planet 87 GB | 5m48s | OOM → planet-safe |
| apply-changes (daily diff, default backend) | Planet 87 GB | 81s | -89% (9.4×) |
| renumber | Planet 87 GB | 3m25s | new architecture |
| build-geocode-index | Planet 87 GB | 7m12s | -65% (2.9×) |
| check --refs | Planet 87 GB | 70s | -94% (17.5×) |
| check --ids (streaming) | Planet 87 GB | 57s | OOM → planet-safe |
| diff `-j 16` (text) | Planet 87 GB | 3m30s | -90% (10.2×) |
| diff `--format osc -j 16` | Planet 87 GB | 5m13s | -86% (7.1×) |
| getid (include mode) | Planet 87 GB | 7s | -86% (6.2×) |
| inspect (default) | Planet 87 GB | 6.5s | -70% (3.3×) |
| inspect --nodes `-j 16` | Planet 87 GB | 57s | new |
| inspect --tags `-j 16` | Planet 87 GB | 2m50s | new |
| add-locations-to-ways external | Planet 87 GB | 10m4s | -58% |
| extract --smart | Europe 35 GB | 3m01s | -29% |

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
