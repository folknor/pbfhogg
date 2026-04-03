# Performance research summary (April 2026)

Consolidated findings from the April 2026 research session. Links to
detailed analysis documents where available.

## Tier 1: High impact, ready to implement

### Block-pair merge-join for diff/derive_changes — SHIPPED

**Result:** Japan diff: 86.4s → 52.9s (39% faster), 80.7 GB → 40.6 GB
cumulative alloc (50% less). Commit `66990c3`.
**Document:** [fill-buffer-optimization.md](fill-buffer-optimization.md)
**What:** Replaced element-level merge-join with block-level comparison.
Non-overlapping blocks skip via indexdata min/max ID ranges.
Overlapping blocks use borrowed elements (no String allocation for the
98.8% Equal path). Falls back to existing `fill_buffer` path for
non-indexed PBFs. Remaining 24.1 GB alloc is protobuf parsing — v1
compressed byte comparison would skip decode for matching blocks.
**Affects:** `diff`, `derive_changes` (both use `stream_merge.rs`)

### Multi-extract parallel decode

**Impact:** Removes the single-threaded decode bottleneck in write phases.
**Document:** [multi-extract-optimization.md](multi-extract-optimization.md)
**What:** Convert sequential BlobReader to pread-from-workers in the
3 write phases. Workers decode in parallel, consumer routes to N writers.
Reuses existing `pread_execute` infrastructure from single-extract.
**Risk:** Low — same pattern already proven in single-extract.

## Tier 2: Moderate impact, benchmarks needed

### Zlib level tuning

**Impact:** 30-60% compression CPU reduction for internal pipelines.
**Document:** [zlib-level-tuning.md](zlib-level-tuning.md)
**What:** Default `zlib:6` may be overkill for pipeline-internal PBFs.
Level 1-3 trades modest ratio for significant speed. `zstd:3` is strictly
better for internal use (3-5x faster decompress).
**Action:** Benchmark `zlib:1` vs `zlib:6` vs `zstd:3` on Denmark/Europe
with `brokkr cat --compression <mode> --bench`.

### Multi-extract raw passthrough

**Impact:** Skip decode+re-encode for node blobs fully inside a region.
**Document:** [multi-extract-optimization.md](multi-extract-optimization.md)
**What:** Per-blob per-region containment check. Blobs inside a region's
bbox are written as raw compressed frames to that region's writer.
**Prerequisite:** Blob-level spatial index (bbox from indexdata).

## Tier 3: Research / low priority

### Columnar decode expansion

**Impact:** Cache-friendly classification, prerequisite for SIMD.
**Document:** [columnar-integration.md](columnar-integration.md)
**What:** Extend `DenseNodeColumns` to multi-extract (N-region bbox),
ALTW node scan, geocode builder pass 2.
**Status:** Prototype shipped for single-extract. LLVM doesn't
autovectorize (push() prevents it). Explicit SIMD would help but
the classify loop is only 2.8% of extract time.

### Pipelined reader retention cleanup

**Impact:** Prevents OOM on 30 GB hosts for specific commands.
**Document:** [pipelined-reader-retention.md](pipelined-reader-retention.md)
**What:** Convert `renumber` and `cat --type` to sequential BlobReader +
DecompressPool. Mechanical — same pattern as node_stats/tags_count.
6 remaining paths audited, 2 production-relevant.

### SIMD batch varint decode

**Impact:** Potentially 2x for varint-dominated paths at planet scale.
**Document:** [SIMD.md](SIMD.md) (individual SIMD closed)
**What:** Individual varint: scalar wins. Batch decode into contiguous
arrays (columnar) is a different problem — not yet benchmarked.
119B varints at planet scale, 64% from dense nodes.
**Prerequisite:** Columnar layout must be stabilized first so batch
decode has a consumer.

### GeoJSON export

**Impact:** New feature, not a performance optimization.
**Document:** [geojson-export-design.md](geojson-export-design.md)
**What:** Streaming PBF → GeoJSONSeq. v1 from ALTW-enriched PBFs.
Tag expression filtering, bbox filtering, property key selection.

## Completed in this session

### Code changes
- `inspect --show` — new feature for single element lookup
- `inspect`: `new_with_scratch` + `elements_skip_metadata()` (non-extended)
- `time_filter`: `take_owned` + `write_primitive_block_owned`
- `multi-extract`: `new_with_scratch` in 3 write phases
- `renumber`: `std::HashMap` → `FxHashMap` for ID maps
- `way_scanner`: `read_varint_i64()` consistency
- `extract`: duplicate comment, unused `seq` field, `BlobDesc` Copy derive
- `cli tests`: iterator-based tags API fix
- Dead code removal (260 lines): superseded raw passthrough scaffolding
- jemalloc/mimalloc features removed (fix CLI `--all-features` build)

### Research documents (7 new)
1. fill-buffer-optimization.md — block-pair merge-join design
2. zlib-level-tuning.md — write path compression analysis
3. multi-extract-optimization.md — 6 optimization opportunities
4. columnar-integration.md — expansion beyond extract
5. pipelined-reader-retention.md — cross-thread audit
6. geojson-export-design.md — export v1 design
7. performance-research-summary.md — this document
