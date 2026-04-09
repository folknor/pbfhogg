# Changelog

## 0.2.0 — 2026-04-09

First public release.

### Commands

Full PBF processing pipeline validated at planet scale (87 GB, 11.6B elements, 30 GB host):

- **cat** — passthrough with indexdata generation, type filtering with raw frame passthrough. Planet: 497s buffered.
- **sort** — Sort.Type_then_ID ordering.
- **extract** — simple, complete-ways, and smart strategies. Parallel 3-phase classification via pread workers. Raw frame passthrough for fully-contained node blobs. Columnar dense node decode for bbox classification. Planet simple: ~100s.
- **multi-extract** — single-pass N-region extract with parallel decode workers. Denmark 5-region: 1.9s, Japan 5-region: 7.3s.
- **tags-filter** — two-pass with tag index filtering, parallel classification, relation closure with way/node dependency resolution.
- **getid** — ID-range blob skip, raw frame passthrough for `--invert`, `--add-referenced` with parallel way dependency scan.
- **add-locations-to-ways** — dense, sparse, and external join index types. External join: planet 1,462s (24.4 min), 3.9x faster than dense.
- **apply-changes** — 4-phase batch merge with passthrough coalescing. Planet daily diff: 762s, 1.8 GB RSS.
- **diff** — block-pair merge-join with compressed byte comparison (skip decode for unchanged blobs). Streaming constant-memory.
- **derive-changes** — OSC generation from two sorted PBFs.
- **merge** — merge-sort multiple PBFs.
- **inspect** — blob statistics, tag counting, `--show` for single element lookup by ID.
- **check** — reference integrity checking (`--refs`).
- **build-geocode-index** — 4-pass geocode index builder. Planet: 1,346s (22.4 min), 17.8 GB RSS.
- **renumber** — sequential ID renumbering.
- **time-filter** — timestamp-based element filtering.

### Library

- `ElementReader` — sequential, parallel (rayon), and pipelined iteration modes.
- `IndexedReader` — seekable reader with blob-level index for filtered queries.
- `PbfWriter` — sync, pipelined (rayon), O_DIRECT, and io_uring write modes.
- `BlockBuilder` — iterator-based tag API, dual-buffer single-pass encoding.
- `DenseNodeColumns` — columnar dense node decode for batch classification.
- `IdSetDense` — chunked sparse bitset with O(1) set/get, rank index, bitwise OR merge.
- `geocode_index::Reader` — reverse geocoding queries via S2 cell lookup (feature-gated).

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
