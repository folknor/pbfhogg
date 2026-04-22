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
- **diff**: shard-based parallel block-pair merge for both text and `--format osc` output, opt-in via new `-j/--jobs N` flag on `pbfhogg diff`. Planet two-snapshot diff (text) 2134s (35m34s) → **208.6s (3m28s)** at `-j 16` (**10.2×**); `--format osc` 2225s (37m06s) → **313.8s (5m13s)** at `-j 16` (**7.1×**). Germany text 103s → 17s at `-j 8` (6.3×); Germany osc 114s → 20s at `-j 8` (5.6×). Planner partitions the ID space into N shards at old-blob boundaries; straddling new blobs are read by both adjacent shards and each shard's element merge clips to its `(t_low, t_high]` window so every element is emitted exactly once. `std::thread::scope` workers pread/decode/merge each shard; main thread concatenates outputs in shard order (in-memory Vec<u8> buffers for text, per-shard scratch files for osc). `-j 0` auto-picks from `available_parallelism()`; `-j 1` (default) keeps the sequential path. Germany 8-shard balance within 1.03× max/min blobs per shard. Peak RSS 2.29 GB at planet text (shards buffer formatted output in memory), 663 MB at planet osc (shards stream XML to per-shard scratch temp files).
- **diff `--format osc` assembly parallelised via chunked gzip.** The final `assemble_osc_from_paths` stage - gzip + concat of ~45 GB of XML fragments - was a single-threaded 32.8 s tail at planet (~10 % of the full `-j 16` wall). Output now streams through a new `ParallelGzipWriter` (`src/write/parallel_gzip.rs`) that buffers into 2 MB chunks, dispatches each chunk to a worker pool for independent gzip compression, and emits compressed chunks in order as concatenated RFC-1952 gzip members. `MultiGzDecoder` (which `gunzip`, `zcat`, and the in-crate OSC readers all use) transparently stitches the members back into one logical stream. The three in-crate `.osc.gz` readers (`osc::parse::parse_osc_file_into`, `merge_changes` readers, `tags_filter --osc`) migrated to `MultiGzDecoder` in the same commit so cross-command composition (diff --format osc → apply-changes, etc.) still works. Output file bytes are no longer identical to the old single-stream format (concatenated members have per-chunk 18-byte framing and reset dictionaries); decompressed content is byte-identical. Compression ratio penalty measured at <2 % on the create/modify/delete fragment mix.
- **getid** include mode: pread-only header walk + on-demand blob-body pread via new shared `HeaderWalker` primitive. Planet `getid` 43.7s → **7.0s (6.2×)**; disk read 88 GB → 636 MB (140× I/O reduction). The walker opens the fd with `posix_fadvise(POSIX_FADV_RANDOM)` so the kernel no longer prefetches blob bodies behind the header scan, and only preads the data payload for blobs whose ID-range index actually matches. Invert mode (raw passthrough) is unchanged behaviourally; it still needs every frame, so it preads full frames via the walker too. Output byte-identical to osmium on `brokkr verify getid-removeid`.
- **inspect** default metadata (index-only fast path): migrated to the shared `HeaderWalker` primitive. Planet `inspect` 21.4s → **6.5s (3.3×)**; disk read 36.3 GB → 14.2 GB. The previous path used `FileReader::skip` (which delegates to `BufReader::seek_relative`), so blob-body bytes sitting inside the 256 KB buffer window got pulled into the page cache even though the command only needed headers. Full-decode fallback (triggered by `--locations`, `--extended`, or any blob lacking indexdata) still uses the sequential buffered reader; `--direct-io` is intentionally ignored on the fast path because O_DIRECT's page-alignment requirements are incompatible with the small per-header preads.
- **inspect --nodes** and **inspect --tags**: parallel workers via `parallel_classify_accumulate` / `parallel_classify_phase` replace the previous sequential scans. Germany `--nodes` 18.5s → **3.6s (5.1×)** at `-j 8`; Germany `--tags` 41.3s → **8.5s (4.9×)** at `-j 8`. Planet `--nodes -j 16` **56.8s** (UUID `c5edebe7`, peak 410 MB RSS - per-worker scalar accumulators merged at completion); Planet `--tags -j 16` **169.5s / 2m50s** (UUID `9d741341`, peak 17.5 GB RSS - per-blob tag maps merged on main thread; peak dominated by the global distinct-tag map + glibc anon-page retention, not per-worker accumulation). Both commands now accept `-j/--jobs N` on `pbfhogg inspect` (and on the `inspect tags` subcommand), with `0 = auto from available_parallelism()`. `parallel_classify_phase` and `parallel_classify_accumulate` both gained an optional `threads: Option<usize>` override parameter; all existing callers pass `None` so behaviour on other commands (ALTW, check --refs, geocode, extract, tags-filter) is unchanged.
- **build-geocode-index**: hard-errors instead of silently truncating when per-cell or per-way counts exceed `u16::MAX`. Error names the offending cell/way and points at the `u32` + `FORMAT_VERSION` bump path.
- **build-geocode-index**: parallel Pass 2 (nodes + ways) and parallel Pass 3 cell assignment; Pass 1.5 switched to shared-atomic `IdSetDense`. Planet 1,255s → 432.9s (7m12s, -65%); Europe 344s → 183.4s (-47%); Germany 71s → 31s (-57%). Pass 1.5 peak anon 29.5 GB → 3.0 GB (-90%); now fits comfortably on 27 GB hosts with governing peak at ~25 GB (Pass 3 Stage B).
- **scan-audit cross-command header-walk migration (2026-04-20).** Every schedule-building header walk in the crate now routes through the shared pread-only `HeaderWalker` + `posix_fadvise(RANDOM)` instead of `BlobReader::seekable_from_path` + `BufReader::seek_relative`. The prior buffered walks pulled blob bodies into the page cache as a side-effect of sequential seek-skips; the new path reads only the ~header bytes and leaves the cache alone. The shared primitive in `src/scan/classify.rs::build_classify_schedule{,_split}` ripples through 10+ downstream commands (extract all strategies, tags-filter, tags-count, check --refs / --ids, inspect --nodes, geocode Pass 2, apply-changes prefill, renumber, ALTW relation scan, multi-extract). Eight additional per-command schedule builders (extract common + smart, multi-extract, renumber, tags-filter single-pass + pass-2, geocode Pass 2, ALTW external `scan_blob_metadata`, apply-changes `scan_node_blob_schedule`) were migrated directly. Europe phase-only walls (`--stop SCHEDULE_SCAN_LOOP`): check-refs 24.7s → 0.4s (**57×**), tags-filter 25.1s → 1.0s (**25×**), extract 24.6s → 0.5s (**52×**). Planet full-command `--bench 1` wins: tags-filter `w/highway=primary` 147.5s → 119.9s (**-18.7%**), check-refs 72.6s → 62.7s (-13.7%), check-ids --full 72.5s → 63.2s (-12.8%), inspect --nodes 58.1s → 49.4s (-15.0%), multi-extract 1004.6s → 972.0s (-3.2%). Full-command wins shrink or disappear at europe scale because the old buffered walk's accidental prefetch helped the downstream decompression pass reuse warm pages; at planet the file is much larger than RAM so the prefetched pages would be evicted before reuse anyway, and header-walk savings dominate. Dense ALTW `collect_way_referenced_node_ids` and `collect_relation_member_node_ids` (pattern 2, sequential-to-parallel) also migrated onto `parallel_classify_phase` / `parallel_classify_accumulate`. Three now-dead methods removed: `BlobHeader::{index, tag_index}` and `BlobReader::next_header_with_data_offset`.
- **apply-changes descriptor-first streaming pipeline (2026-04-21).** Per-batch rayon barrier replaced with a three-stage scanner + worker-pool + drain shape. Scanner walks blob headers via `HeaderWalker` and emits per-blob descriptors; no-overlap indexed blobs route directly to the drain as `CopyRange` (bypassing the worker pool entirely); overlap candidates route to a long-lived worker pool that preads body, decompresses, precise-checks, and either rewrites inline on a persistent `BlockBuilder` or emits `CopyRange` (false positive). Workers inline-frame rewrite output via `frame_blob_pipelined` (P1.5); drain ships framed bytes through `write_raw_owned` without the writer's per-block `rayon::spawn` dispatch. `writer_pipeline_send_wait_ns` at planet `--compression none`: 859 s cumulative → 117 s (-86%). `NodeLocationIndex::prefill_from_base` deleted; coord extraction fuses into the worker pool's node phase and publishes via a node→way barrier. Planet `--compression none` 144.4 s → 135.5 s, zlib:6 ~170 s (est) → 143.7 s. Deletions: `parallel_reader.rs`, `classify.rs::classify_only`'s fast-path + legacy batch-slot types, `stream_output.rs::coalesce_passthrough`, `stats.rs::{PhaseTimers, ClassifyCounters, StallAccumulator, PhaseRss}`. Three new modules: `scanner.rs`, `streaming.rs`, `drain.rs`. Peak RSS 1.63 GB → 3.29 GB (+2.0×, inside 27 GB host envelope); involuntary context switches dropped 70%. 18/18 integration tests and 6/6 property tests pass; Denmark element counts byte-equal to pre-flip.
- **apply-changes `-j/--jobs N` worker-count override.** New CLI flag; `0` (default) keeps the existing `nproc - 2` heuristic, `N > 0` pins the descriptor-first pipeline's worker-pool size to exactly N. Enables scaling curves and bench reproduction on hosts where `nproc - 2` is a poor default, and lets benches isolate the `-j` axis from compression / writer-backend drift. Emits a `merge_worker_count` counter to the sidecar for post-hoc verification.
- **apply-changes parallel pwrite writer backend (2026-04-21).** New writer backend, now the default for `apply-changes`: one coordinator thread + `POOL_SIZE=16` pwrite workers on a shared file descriptor. Writer pops pipeline items in global-seq order via the existing `ReorderBuffer`, computes each item's final byte offset, and round-robins `WriteOp` (Write { offset, bytes } | CopyRange { out_offset, in_fd, src_offset, len }) across per-worker bounded channels. Workers run `pwrite` / `copy_file_range(out_offset)` concurrently; cross-device copy_file_range (EXDEV) falls back to pread+pwrite with explicit offsets (parallel-safe). `--io-uring` and `--direct-io` remain opt-in alternatives; the buffered fallback is removed from the `apply-changes` path. Planet bench matrix at `--compression zstd:1` cross-disk: buffered 87.1 s → io_uring 82.8 s → **parallel pwrite 80.9 s**. Pool-size sweep: 4 → 89.2 s, 8 → 83.4 s, 16 → 80.9 s, 32 → 82.2 s (NVMe queue saturated around 16). Writer-backend rule: io_uring wins same-disk (IOPS contention, queue-depth batching) and at `--compression none`; parallel pwrite wins cross-disk + zstd:1 (bandwidth headroom, parallel pwrite saturates disk).

### Bug fixes

- **`has_indexdata` / `check_sorted_and_indexed` full-scan regression (shipped in v0.2.0).** Both probes were scanning every blob header instead of short-circuiting on the first OsmData blob, adding several seconds on small datasets and ~20s on planet. Restored to short-circuit. Affected 8 subcommands unconditionally (`sort`, `getid`, `add-locations-to-ways`, `build-geocode-index`, `diff` including `--format osc`, `inspect --nodes`, `cat --dedupe`, `check --ids`) plus 5 conditionally (`cat --type`, `extract` non-simple, `tags-filter` non-invert, `inspect --tags --type`). Planet `cat --type way`: 73.8s → 43.9s (-41%). Planet unfiltered `cat` (indexdata generation, the header-walk-dominated case): 497s → 86.5s (**5.8×**), combining this short-circuit fix with the `BlobReaderSource` seek-raw fix below. Planet `getid`: 66s → 44s (-33%).
- **`BlobReader::seek_raw` header-walk amplification.** `Seek::seek` on `BufReader<File>` discards the buffer on every blob-body skip, causing ~10× file-size read amplification on header-walking call sites. New `BlobReaderSource` trait routes `BufReader` sources through `seek_relative` instead. Extract --smart Europe 211.2s → 195.2s (-7.6%); ALTW external Europe 286.3s → 270.7s (-5.5%, META_SCAN phase -49%).
- **renumber forward-ref relations.** Fixed via two-pass structure.
- **Concurrent-process temp file collisions.** Scratch temp file names now include the PID, so two pbfhogg instances running against the same scratch directory no longer clash.
- **Temp file cleanup on error paths.** Assembly failures previously leaked temp files; cleanup now runs on every exit path via deferred sink cleanup.
- **Geocode `simplify_ring` Douglas-Peucker divergence.** `dp_count_range` was using unclamped perpendicular distance to the infinite line while `dp_mark` used clamped segment projection - the binary search could converge to a different epsilon and exceed `max_vertices`. Both now use the same clamped projection.
- **io_uring writer corrupted output on the `CopyRange` fast-path.** `apply-changes --io-uring` on indexed input produced structurally-broken PBFs: `apply-changes` exited Ok but the output failed to parse, with the header followed by a page of zeros where the next OSMData blob header should have been. Root cause: `handle_copy_range_uring` flushed a partial accumulator buffer via `submit_current` before each CopyRange run, padding the `WriteFixed` to the next page boundary on disk. `logical_size` only tracked real bytes, so the padding became zero-filled gaps mid-file that `set_len(logical_size)` could not remove. Fixed by routing CopyRange bytes through the normal accumulator: `pread` reads directly into the current registered buffer at `current_len`, and only full (BUF_SIZE = page-aligned) buffers are submitted mid-stream. Preserves the writer's core invariant that only the final `flush_final` SQE is partial. Denmark `apply-changes --io-uring` output is now byte-identical to the buffered/parallel writer.

### Dependencies

- **`roaring` removed.** Both remaining consumers (`check --refs`, `check --ids --full`) migrated to `IdSetDense`. One fewer transitive dependency for library users.
- `protohoggr` 0.2.1 → 0.4.0 (`read_raw_field` for wire-format rewriters).
- `hotpath` 0.14.1 → 0.15.
- `io-uring` 0.7.11 → 0.7.12, `libc` 0.2.184 → 0.2.185, `rayon` 1.11 → 1.12, `clap` 4.6.0 → 4.6.1 (patch/minor maintenance bumps, no API-visible effect).

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
| diff `-j 16` (two independent planet snapshots, text) | Planet 87 GB | 228s (3m48s) | -89% (9.5×) |
| diff `--format osc -j 16` (two independent planet snapshots) | Planet 87 GB | 314s (5m13s) | -86% (7.1×) |
| getid (include mode) | Planet 87 GB | 6.1s | -86% (7.2×) |
| inspect (default metadata, index-only) | Planet 87 GB | 6.5s | -70% (3.3×) |
| inspect `--nodes -j 16` | Planet 87 GB | 57s | parallel (new) |
| inspect `--tags -j 16` | Planet 87 GB | 2m50s | parallel (new) |
| apply-changes --locations-on-ways (daily diff, same-disk, `--compression none`) | Planet 87 GB altw | 135.5 s | -82% (5.6×) vs 762 s in 0.2.0 |
| apply-changes --locations-on-ways (daily diff, default parallel pwrite + zstd:1, separate-NVMe scratch) | Planet 87 GB altw | 80.9 s | -89% (9.4×) vs 762 s in 0.2.0 |

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
