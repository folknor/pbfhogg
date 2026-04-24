# Changelog

## Unreleased

### Breaking changes

- **`renumber --mode` flag removed.** External join is now the only implementation.
- **`BlobReader::new_seekable<R>` and `IndexedReader::new<R>` bounds widened** from `R: Read + Seek + Send` to `R: BlobReaderSource + Send`. Downstream users with custom reader types add `impl BlobReaderSource for MyReader {}`.
- **Geocode `FORMAT_VERSION` bumped to 2.** Indexes built with older versions must be rebuilt.
- **`IdSetDense` renamed to `IdSet`** and moved from `getid` to the top-level `pbfhogg::idset` module.
- **`pbfhogg::getid::IdSet` renamed to `pbfhogg::getid::ElementIds`.** The struct bundles three per-type ID sets and was misnamed as a single "set". Field accessors and method calls unchanged.
- **`pbfhogg::getid::removeid` signature adds `force: bool`** (positional, between `direct_io` and `overrides`). Library callers must pass `false` to require indexdata (matches CLI default), or `true` to permit non-indexed input.
- **`apply-changes --jobs 1` (`MergeOptions::jobs = Some(1)`) now rejected** with a clear error at setup. A single worker has a deadlock hazard on mid-stream worker panic and no production use case (2+ workers is strictly faster on every host). Pass `--jobs 2` or omit `--jobs` for the default. The default's minimum is also bumped to 2 for low-core hosts.

### Commands

- **renumber**: complete rewrite to an external-join architecture. Planet 3m25s, 3.3 GB peak RSS, zero temp disk. Negative input IDs are now rejected. Orphan refs (way refs / relation members absent from the input) are counted and surfaced in the summary.
- **apply-changes**: new descriptor-first streaming pipeline and new parallel-pwrite writer backend (now the default). `-j/--jobs N` worker-count override. Planet daily diff 762s → 81s with the default backend; 135s with `--compression none`. `io_uring` and `--direct-io` remain opt-in. The buffered writer is no longer used on this path.
- **build-geocode-index**: hard-errors when the cumulative admin-vertex byte offset would overflow `AdminPolygon.vertex_offset` (u32, ~4 GiB of admin-vertex data). Previously the accumulator silently wrapped, making every subsequent polygon's `vertex_offset` point at garbage. Error names the current offset and the step that would overflow, with a pointer to bump `vertex_offset` to u64 and increment `FORMAT_VERSION`. Sibling per-polygon `vertex_count` guard also added.
- **build-geocode-index**: hard-errors on the same overflow class at `write_admin_index` (the `AdminCell.entries_offset` u32 accumulator) and on a pre-emptive `INTERIOR_FLAG` bit collision when `poly_index >= 2^31`. Both would silently produce wrong on-disk entries before; both now carry the same widen-and-bump diagnostic.
- **io_uring writer: buffer-pool accounting under CQE error.** On a negative-result or short-write CQE, `reap_cqes` returned the error without releasing the registered buffer slot, leaking one slot per error CQE for the writer's lifetime. The writer typically tears down on error anyway (no observable user-visible bug in production), but the accounting inconsistency could surface as "no free buffers" in any code path that tried to observe pool state or continue past a soft error. Release is now unconditional.
- **check --refs**: parallel three-phase scan. Planet 1,225s → 70s (17.5×); Europe 426s → 34s (12.7×). Peak RSS 2.17 GB.
- **check --ids --full**: parallel three-phase scan. Europe 313s → 53s (5.9×). Streaming (non-`--full`) mode unchanged.
- **diff** / **diff --format osc**: new `-j/--jobs N` for shard-parallel merge. Planet text 35m → 3m30s at `-j 16` (10.2×); `--format osc` 37m → 5m13s (7.1×). `-j 1` (default) keeps the sequential path.
- **getid** include mode: header-walk fast path. Planet 44s → 7s (6.2×); disk read 88 GB → 636 MB.
- **inspect** default metadata: header-walk fast path. Planet 21s → 6.5s.
- **inspect --nodes** / **inspect --tags**: now parallel, with `-j/--jobs N`. Planet `--nodes -j 16` 57s (new); `--tags -j 16` 2m50s (new). Germany `--nodes -j 8` 18.5s → 3.6s.
- **build-geocode-index**: parallel Pass 2 / Pass 3. Planet 21m → 7m (-65%). Pass 1.5 peak anon 29.5 GB → 3.0 GB.
- **build-geocode-index**: hard-errors when per-cell or per-way counts exceed `u16::MAX` (previously silently truncated). Error names the offending cell/way.
- **add-locations-to-ways `--index-type external`**: rewrite. Planet 24m → 11m (-55%).
- **extract** (all strategies): pass-1 blob schedule reused across subsequent passes. Europe smart 254s → 181s (-29%).
- **getparents**: skip node-only blobs via `BlobFilter` when the query doesn't need nodes (~85% of blobs at planet scale).
- **derive-changes**: streams output to temp files; constant memory regardless of diff size.
- **tags-filter**: new `-j/--jobs N` on the two-pass path.

### Bug fixes

- `has_indexdata` / `check_sorted_and_indexed` scanned every blob header instead of short-circuiting on the first OsmData blob. Fixed.
- `BlobReader::seek_raw` caused ~10x file-size read amplification on header-walking paths via buffered `BufReader<File>`. Fixed.
- Geocode `simplify_ring` Douglas-Peucker could exceed `max_vertices` due to metric divergence between cost and marking passes. Fixed.
- `apply-changes --io-uring` on indexed input produced structurally-broken PBFs on the `CopyRange` fast-path. Fixed.
- `cat --dedupe` silently dropped elements at kind boundaries. Fixed.
- `sort` silently dropped elements at kind boundaries. Fixed.
- `apply-changes --force` on a malformed non-indexed PBF could emit every remaining upsert of a kind as gap-creates before a single blob. Fixed.
- `apply-changes --force` on non-indexed input could silently drop upsert modifications. Fixed.
- Adversarial or truncated blob-header length prefix could abort the process via a multi-GB allocation. Fixed.
- `inspect --show` returned "not found" on unsorted PBFs when the target element lived in a later blob with a smaller `min_id` than a preceding blob. Fixed.
- `renumber` could panic or produce phantom orphans on PBFs containing negative IDs (via stale indexdata understating the range, or via inconsistent input whose relation member refs carry negatives while nodes/ways do not). Now hard-rejects at every `IdSet` entry point, with an error naming the offending element and the enclosing context.
- `renumber` left a partial output file on disk when an error surfaced mid-stream (pass1 count mismatch, stage 2d failure, relation rewrite failure, final flush). The output file is now removed on any error path.
- Geocode builder Pass 3 leaked ~256 temp files per bucket directory on a mid-Stage-A panic or I/O error; the next build would sweep them via an unconditional remove at entry. Now cleaned up on every error path as well.
- Geocode builder Pass 3 Stage B silently truncated trailing partial records when a Stage A bucket file write was interrupted (ENOSPC, SIGKILL during write), dropping real cell assignments with no diagnostic. Now errors with a message naming the incomplete file length.
- `apply-changes` (and `merge`) now requires a base PBF with `Sort.Type_then_ID` in the header regardless of `--locations-on-ways`. Previously the general path accepted unsorted headers and could silently drop upsert creates on non-canonical input. If you hit this error on a previously-accepted file, re-sort the base with `pbfhogg sort`.
- `renumber` no longer panics in `IdSet::set_atomic` when the PBF header advertises `Sort.Type_then_ID` but the node blobs are actually out of order. The per-command max-node-id bound now scans the full blob schedule rather than trusting "last blob's max_id == global max".
- Multiple read sites in `renumber`, `apply-changes`, and the ALTW external pipeline now reject malformed input at the boundary instead of surfacing as a panic or silently scrambled output: a relation with truncated `memids` varints, a DenseNodes block with a `granularity * offset` product that would overflow `i64`, and an ALTW node blob whose indexdata advertises `max_id < min_id`.
- `apply-changes --force --locations-on-ways` on a non-indexed PBF silently stripped LocationsOnWays data from base ways. Now rejected up front with a migration hint.
- Parallel and io_uring writers could silently truncate output on upstream framer panic. Fixed.
- `add-locations-to-ways --index-type external --force` on a non-indexed PBF failed with a confusing error deep inside the metadata scan. Now rejected up front with a migration hint.
- Parallel `diff` / `derive-changes` leaked per-shard scratch temp files on worker error or panic. Fixed.
- Truncated or corrupt-header PBFs produced past-EOF entries in classify schedules and failed much later inside pread workers. Fixed.
- Concurrent pbfhogg processes against the same scratch directory could collide on temp file names. Fixed.
- Temp files leaked on assembly error paths. Fixed.
- `add-locations-to-ways --index-type external` could silently produce wrong coordinates on PBFs with loose blob indexdata. Now hard-errors naming the offending blob.
- `add-locations-to-ways --index-type external` hit `EMFILE` on a default-ulimit shell. Fixed.
- Parallel-pwrite cross-device passthrough copy could fail on a signal-interrupted `pread` instead of retrying, surfacing as a spurious I/O error when a signal (e.g. SIGWINCH) arrived during an EXDEV fallback. Fixed.
- Parallel `derive-changes` scratch filenames could collide across concurrent `pbfhogg` processes when PIDs recycled in the same scratch directory (container restart). Process-lifetime random tag now included in the path.
- `BlobReader::seek_raw` left iteration permanently stuck after a prior `next()` returned an error: the sticky `last_blob_ok` flag was never reset on successful seek, so `next()` returned `None` even though the user had recovered by seeking past the bad bytes. A successful `seek_raw` now clears the flag.
- `removeid` (`getid --invert`) on a non-indexed PBF silently fell through to full decode + re-encode for every blob instead of using the raw-passthrough fast path (which is unreachable without indexdata). Now errors up front with a specific message unless `--force` is passed, matching `getid` include-mode behaviour. The previous `Warning: --force has no effect with --invert` message was incorrect - `--force` is the gate on this path - and has been removed.
- `build-geocode-index` could drop admin inner rings (holes) on multi-outer relations when aggressive Douglas-Peucker simplification on the outer ring shifted the simplified boundary past the hole's first vertex. Hole-in-outer containment now tests against the original (unsimplified) outer polygon; rendered geometry stored on disk is unchanged (still simplified).
- `inspect --direct-io` on an indexed PBF silently ignored the flag when the header-only fast path applied. Now prints a one-line stderr notice explaining that `HeaderWalker`'s `posix_fadvise(POSIX_FADV_RANDOM)` provides equivalent cache-avoidance on the fast path, and that the full-decode fallback still honours `--direct-io`.
- `apply-changes` drain loop could spin indefinitely if a worker thread panicked mid-stream (e.g. OOM or an internal `unwrap`). The drain only exited once its reorder buffer was empty, but a panicking worker leaves seqs stuck ahead of the dropped seq so the buffer never drains. Loop now breaks on channel-disconnect unconditionally; the post-loop "channel closed with items in reorder buffer" error surfaces the real diagnostic and the outer `thread::scope` propagates the worker's panic. Note: the narrow `-j 1` worker-panic scenario still deadlocks at the scanner (no surviving worker to drain the candidate channel); that's tracked as a separate invariant gap.
- `derive-changes -j N` (parallel OSC assembly, invoked via `diff --format osc`) leaked three outer aggregate XML temp files (`derive-par-{creates,modifies,deletes}-{pid}.xml.tmp`) on any error-path early-return (shard panic, sweep failure, flush failure). Fixed by wrapping each in `PathGuard::file()` per ADR-0003, so the files are removed on any `?`-bailout or panic unwind. Happy-path behavior unchanged.

### Library

- **`PbfWriter::write_primitive_block_no_indexdata`.** New public method that writes an `OSMData` blob without the `BlobHeader.indexdata` / `BlobHeader.tagdata` fields, for producing byte-for-byte fixtures matching third-party tools.

### Dependencies

- **`roaring` removed.** Both remaining consumers (`check --refs`, `check --ids --full`) migrated to `IdSet`.
- `protohoggr` 0.2.1 → 0.4.0.
- `hotpath` 0.14.1 → 0.15.
- `io-uring`, `libc`, `rayon`, `clap`: patch/minor bumps.

### Performance highlights

| Operation | Dataset | Time | vs 0.2.0 |
|-----------|---------|------|----------|
| cat (indexdata generation) | Planet 87 GB | 86.5s | -83% (5.8×) |
| apply-changes (daily diff, default backend) | Planet 87 GB | 81s | -89% (9.4×) |
| renumber | Planet 87 GB | 3m25s | new architecture |
| build-geocode-index | Planet 87 GB | 7m12s | -65% (2.9×) |
| check --refs | Planet 87 GB | 70s | -94% (17.5×) |
| diff `-j 16` (text) | Planet 87 GB | 3m30s | -90% (10.2×) |
| diff `--format osc -j 16` | Planet 87 GB | 5m13s | -86% (7.1×) |
| getid (include mode) | Planet 87 GB | 7s | -86% (6.2×) |
| inspect (default) | Planet 87 GB | 6.5s | -70% (3.3×) |
| add-locations-to-ways external | Planet 87 GB | 11m | -55% |
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
