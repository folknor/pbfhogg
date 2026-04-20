# Changelog

## Unreleased

### Breaking changes

- **`renumber --mode` flag removed.** External join is now the only implementation; the in-memory path is gone.
- **`BlobReader::new_seekable<R>` and `IndexedReader::new<R>` bounds widened** from `R: Read + Seek + Send` to `R: BlobReaderSource + Send`. Downstream users with custom reader types add `impl BlobReaderSource for MyReader {}` - one line, picks up the correct-but-slow default.
- **Geocode `FORMAT_VERSION` bumped to 2.** Indexes built with older versions must be rebuilt.
- **`pbfhogg::getid::IdSet` renamed to `pbfhogg::getid::ElementIds`.** The struct bundles three per-type ID sets (`node_ids`, `way_ids`, `relation_ids`) and was misnamed as a single "set". External consumers must update type annotations; field accessors and method calls are unchanged.

### Commands

- **renumber**: complete external-join rewrite. Planet 204.5s (3m25s), 3.3 GB peak anon RSS, zero temp disk. Negative input IDs now rejected with a clear error. Orphan refs - way refs and relation members absent from the input - are counted and surfaced in the summary.
- **add-locations-to-ways `--index-type external`**: major rewrite. `coord_payloads` direct emission replaces the `coord_slots` mmap, rank-bucketed counting sort replaces comparison sort, stage-4 raw passthrough for non-way blobs, file-backed `coords_by_rank` scatter. Planet 1,462s → 661s (-55%).
- **check --refs**: three-phase parallel scan via `parallel_classify_phase` after swapping `RoaringTreemap` → `IdSetDense`. Planet 1,225s → 70.2s (17.5×); Europe 426.2s → 33.6s (12.7×); Japan 56.7s → 2.1s (27×). Peak RSS 2.17 GB (pre-allocated `IdSetDense` for 14B node IDs).
- **check --ids --full**: parallel three-phase scan mirroring `check --refs`. Europe 312.6s → 52.7s (5.9×). Planet: 69.5s / 1m10s at `ef6ce09` (no pre-swap baseline). Streaming (non-`--full`) mode unchanged.
- **extract** (smart / complete / simple): reuse PASS1 blob schedule in subsequent passes. Europe smart 254s → 181s (-29%), complete -17%, simple -15%.
- **getparents**: skip node-only blobs via `BlobFilter` when `--add-self` doesn't need nodes (~85% of blobs at planet scale).
- **derive-changes**: streams output to temp files instead of buffering all changes in memory - constant memory regardless of diff size.
- **diff**: shard-based parallel block-pair merge, opt-in via new `-j/--jobs N` flag on `pbfhogg diff`. Planet two-snapshot diff 2134s (35m34s) → 219s (3m39s) at `-j 16` (**9.7×**); Germany 103s → 19s at `-j 8` (5.4×). Pre-pass walks both files' indexdata via `pread` (header-only; skips data bytes) to partition the ID space into N independent shards, then `std::thread::scope` workers pread/decode/merge each shard and buffer formatted output; main thread concatenates shard outputs in order. `-j 0` auto-picks from `available_parallelism()`; `-j 1` (default) keeps the sequential path. Peak RSS 2.4 GB at planet. Only the `diff` (human-readable) path is parallelised; `diff --format osc` still uses the sequential `derive_changes` (`-j > 1` rejected).
- **build-geocode-index**: hard-errors instead of silently truncating when per-cell or per-way counts exceed `u16::MAX`. Error names the offending cell/way and points at the `u32` + `FORMAT_VERSION` bump path.
- **build-geocode-index**: parallel Pass 2 (nodes + ways) and parallel Pass 3 cell assignment; Pass 1.5 switched to shared-atomic `IdSetDense`. Planet 1,255s → 432.9s (7m12s, -65%); Europe 344s → 183.4s (-47%); Germany 71s → 31s (-57%). Pass 1.5 peak anon 29.5 GB → 3.0 GB (-90%); now fits comfortably on 27 GB hosts with governing peak at ~25 GB (Pass 3 Stage B).

### Bug fixes

- **`has_indexdata` / `check_sorted_and_indexed` full-scan regression (shipped in v0.2.0).** Both probes were scanning every blob header instead of short-circuiting on the first OsmData blob, adding several seconds on small datasets and ~20s on planet. Restored to short-circuit. Affected 8 subcommands unconditionally (`sort`, `getid`, `add-locations-to-ways`, `build-geocode-index`, `diff` including `--format osc`, `inspect --nodes`, `cat --dedupe`, `check --ids`) plus 5 conditionally (`cat --type`, `extract` non-simple, `tags-filter` non-invert, `inspect --tags --type`). Planet `cat --type way`: 73.8s → 43.9s (-41%). Planet unfiltered `cat` (indexdata generation, the header-walk-dominated case): 497s → 86.5s (**5.8×**), combining this short-circuit fix with the `BlobReaderSource` seek-raw fix below. Planet `getid`: 66s → 44s (-33%).
- **`BlobReader::seek_raw` header-walk amplification.** `Seek::seek` on `BufReader<File>` discards the buffer on every blob-body skip, causing ~10× file-size read amplification on header-walking call sites. New `BlobReaderSource` trait routes `BufReader` sources through `seek_relative` instead. Extract --smart Europe 211.2s → 195.2s (-7.6%); ALTW external Europe 286.3s → 270.7s (-5.5%, META_SCAN phase -49%).
- **renumber forward-ref relations.** Fixed via two-pass structure.
- **Concurrent-process temp file collisions.** Scratch temp file names now include the PID, so two pbfhogg instances running against the same scratch directory no longer clash.
- **Temp file cleanup on error paths.** Assembly failures previously leaked temp files; cleanup now runs on every exit path via deferred sink cleanup.
- **Geocode `simplify_ring` Douglas-Peucker divergence.** `dp_count_range` was using unclamped perpendicular distance to the infinite line while `dp_mark` used clamped segment projection - the binary search could converge to a different epsilon and exceed `max_vertices`. Both now use the same clamped projection.

### Dependencies

- **`roaring` removed.** Both remaining consumers (`check --refs`, `check --ids --full`) migrated to `IdSetDense`. One fewer transitive dependency for library users.
- `protohoggr` 0.2.1 → 0.4.0 (`read_raw_field` for wire-format rewriters).
- `hotpath` 0.14.1 → 0.15.

### Performance highlights

| Operation | Dataset | Time | vs 0.2.0 |
|-----------|---------|------|----------|
| cat (indexdata generation) | Planet 87 GB | 86.5s | -83% (5.8×) |
| build-geocode-index | Planet 87 GB | 432.9s | -65% (2.9×) |
| renumber | Planet 87 GB | 204.5s | new architecture |
| add-locations-to-ways external | Planet 87 GB | 661s | -55% |
| check --refs | Planet 87 GB | 70.2s | -94% (17.5×) |
| check --ids --full | Europe 35 GB | 52.7s | -83% (5.9×) |
| extract --smart | Europe 35 GB | 181s | -29% |
| derive-changes | - | streaming | constant memory |
| diff (two independent planet snapshots) | Planet 87 GB | 2225s (37m) | 55 MB peak, streaming merge-join |

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
