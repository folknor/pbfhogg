# Changelog

## Unreleased

### Breaking changes

- **`renumber --mode` flag removed.** External join is now the only implementation.
- **`BlobReader::new_seekable<R>` and `IndexedReader::new<R>` bounds widened** from `R: Read + Seek + Send` to `R: BlobReaderSource + Send`. Downstream users with custom reader types add `impl BlobReaderSource for MyReader {}`.
- **Geocode `FORMAT_VERSION` bumped to 2.** Indexes built with older versions must be rebuilt.
- **`IdSetDense` renamed to `IdSet`** and moved from `getid` to the top-level `pbfhogg::idset` module.
- **`pbfhogg::getid::IdSet` renamed to `pbfhogg::getid::ElementIds`.** The struct bundles three per-type ID sets and was misnamed as a single "set". Field accessors and method calls unchanged.

### Commands

- **renumber**: complete rewrite to an external-join architecture. Planet 3m25s, 3.3 GB peak RSS, zero temp disk. Negative input IDs are now rejected. Orphan refs (way refs / relation members absent from the input) are counted and surfaced in the summary.
- **apply-changes**: new descriptor-first streaming pipeline and new parallel-pwrite writer backend (now the default). `-j/--jobs N` worker-count override. Planet daily diff 762s → 81s with the default backend; 135s with `--compression none`. `io_uring` and `--direct-io` remain opt-in. The buffered writer is no longer used on this path.
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

- **`has_indexdata` / `check_sorted_and_indexed` no longer scan every blob header.** Both probes now short-circuit on the first OsmData blob. Affected many subcommands; headline: planet `cat` (indexdata generation) 497s → 86.5s combined with the `seek_raw` fix below.
- **`BlobReader::seek_raw` header-walk amplification.** `Seek::seek` on `BufReader<File>` discarded the buffer on every blob-body skip, causing ~10× file-size read amplification on header-walking paths. Fixed via the new `BlobReaderSource` trait routing `BufReader` sources through `seek_relative`.
- **Geocode `simplify_ring` Douglas-Peucker divergence** could exceed `max_vertices` because the cost and marking passes used different distance metrics. Both now use clamped segment projection.
- **io_uring writer corrupted output on the `CopyRange` fast-path.** `apply-changes --io-uring` on indexed input produced structurally-broken PBFs (zero-filled gap mid-file). Output is now byte-identical to the buffered and parallel-pwrite writers.
- **`cat --dedupe` silently dropped elements at kind boundaries** when same-kind overlap pairs sat adjacent to a kind transition.
- **`sort` silently dropped elements at kind boundaries**, twin of the `cat --dedupe` bug above. The pass-2 overlap-run walker grouped consecutive `overlaps[i]=true` entries without checking kind; a node/node overlap pair followed immediately by a way/way overlap pair merged into one run and `write_overlap_run`'s kind-gated sweep silently discarded the off-kind elements.
- **Adversarial blob-header length prefix caused allocation abort** on any command routing through `read_raw_frame`, `read_blob_header_only`, or `HeaderWalker::next_header` (affects cat passthrough, getid raw passthrough, `has_indexdata` / `check_sorted_and_indexed` probes, and the HeaderWalker-backed classify path used by apply-changes, altw, extract, inspect, tags-filter, geocode). `BlobReader::read_blob_header` has long had a `MAX_BLOB_HEADER_SIZE` (64 KiB) cap; the three newer primitives did not, so a malicious or corrupt 4-byte file with a `u32::MAX` length prefix would attempt a multi-GB `vec![0u8; header_len]` and abort the process. All three now return `BlobError::HeaderTooBig` cleanly.
- **`inspect --show` (show_element) returned "not found" on unsorted PBFs** when the target element lived in a later blob whose `min_id` was smaller than a preceding blob's. The blob-skip fast path used an `idx.min_id > target_id` early-exit without first checking whether the header declared `Sort.Type_then_ID`. The gate is now explicit: on unsorted input the scan keeps going until every blob has been considered.
- **`renumber` could panic or produce phantom orphans on PBFs with stale/lying blob indexdata containing negative IDs.** The `renumber requires non-negative input ids` check was gated on the per-blob `min_id < 0` read from indexdata; if indexdata understated the real range (hand-edited fixtures, third-party writers), a negative node id reached `IdSet::set_atomic` and panicked with an opaque "pre_allocate only covers..." diagnostic, while a negative way id reached the non-atomic `IdSet::set`, was silently dropped, and left phantom orphan refs in relations that still pointed at the old negative way id. The check is now unconditional at both call sites.
- **`apply-changes --force --locations-on-ways` on a non-indexed PBF silently stripped LocationsOnWays data from base ways.** Under `--force` the scanner tags every blob as a placeholder `Node` (the real kind is only known after decompress), which defeated the Node->Way barrier that would normally gate the worker pool until the node-coord `loc_map` is published. Way workers then ran without a `loc_map` and the per-block rewriter fell back to `write_base_way_local` (strips LoW) instead of `write_base_way_local_with_locations`. The combination is now rejected up front with a clear error pointing at the `pbfhogg cat` indexed-generation workflow.
- **Concurrent pbfhogg processes against the same scratch directory** could collide on temp file names; names now include the PID.
- **Temp files leaked on assembly error paths.** Cleanup now runs on every exit path.

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
