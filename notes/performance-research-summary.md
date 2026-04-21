# Performance research summary (April 2026)

Consolidated findings from the April 2026 research session. Links to
detailed analysis documents where available.

## Tier 1: High impact, ready to implement

### Block-pair merge-join for diff/derive_changes - SHIPPED

**Result:** Japan diff: 86.4s → 52.9s (39% faster), 80.7 GB → 40.6 GB
cumulative alloc (50% less). Commit `66990c3`.
**Documents:** fill-buffer-optimization.md and block-pair-merge-join-plan.md
(deleted - v1+v2 shipped, v3 tracked in TODO.md)
**What:** Replaced element-level merge-join with block-level comparison.
Non-overlapping blocks skip via indexdata min/max ID ranges.
Overlapping blocks use borrowed elements (no String allocation for the
98.8% Equal path). Falls back to existing `fill_buffer` path for
non-indexed PBFs. Remaining 24.1 GB alloc is protobuf parsing - v1
compressed byte comparison would skip decode for matching blocks.
**Affects:** `diff`, `derive_changes` (both use `stream_merge.rs`)

### Multi-extract parallel decode - LANDED

Shipped in commit `9f72bcf`: `multi_extract_pread_write` replaces the
sequential BlobReader in all three write phases. Denmark 5-region
6.7 s → 2.1 s (3.2x); Japan 5-region 32.5 s → 8.1 s (4.0x).

## Tier 2: Moderate impact, benchmarks needed

### Zlib level tuning

**Impact:** 30-60% compression CPU reduction for internal pipelines.
**Document:** [zlib-level-tuning.md](zlib-level-tuning.md)
**What:** Default `zlib:6` may be overkill for pipeline-internal PBFs.
Level 1-3 trades modest ratio for significant speed. `zstd:3` is strictly
better for internal use (3-5x faster decompress).
**Action:** Benchmark `zlib:1` vs `zlib:6` vs `zstd:3` on Denmark/Europe
with `brokkr cat --compression <mode> --bench`.

### Multi-extract raw passthrough - CLOSED 2026-04-20

Disproven at planet 5-region via shadow counter (UUID `dad573cb`):
0 / 32,835 node blobs qualify for any containment gate. PBFs are
ID-sorted and OSM IDs are chronological, so an 8,000-element blob
scatters across the planet geographically and can never fit in a
sub-planet region. Load-bearing pin at
`src/commands/extract/multi.rs::try_extract_multi_single_pass` (right
after the `MULTI_SCHEDULE_SCAN_END` marker). Sister disproof:
tags-filter, 0 / 50,364 at `w/highway=primary` on planet.

## Tier 3: Research / low priority

### Columnar decode expansion

**Impact:** Cache-friendly classification, prerequisite for SIMD.
**Document:** [columnar-integration.md](columnar-integration.md)
**What:** Extend `DenseNodeColumns` to multi-extract (N-region bbox),
ALTW node scan, geocode builder pass 2.
**Status:** Prototype shipped for single-extract. LLVM doesn't
autovectorize (push() prevents it). Explicit SIMD would help but
the classify loop is only 2.8% of extract time.

### Pipelined reader retention / oversubscription

**Impact:** Originally framed as an OOM risk. The retention problem
was solved by `DecompressPool` (commit `8f6999b`), which recycles
decompression buffers instead of cross-thread-freeing them. The
oversubscription concern (decode pool + global pool both running)
remains but is not worth attacking: a sequential conversion of
`getparents` (`c912e4d`) regressed 4.7× on Denmark and was reverted.
Decompression dominates, not per-block work.
**Reference:** [reference/pipelined-reader-paths.md](../reference/pipelined-reader-paths.md)
(per-caller breakdown; the conversion rule is "don't").

### SIMD batch varint decode

**Impact:** Potentially 2x for varint-dominated paths at planet scale.
**Document:** [SIMD.md](SIMD.md) (individual SIMD closed)
**What:** Individual varint: scalar wins. Batch decode into contiguous
arrays (columnar) is a different problem - not yet benchmarked.
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
- `inspect --show` - new feature for single element lookup
- `inspect`: `new_with_scratch` + `elements_skip_metadata()` (non-extended)
- `time_filter`: `take_owned` + `write_primitive_block_owned`
- `multi-extract`: `new_with_scratch` in 3 write phases
- `renumber`: `std::HashMap` → `FxHashMap` for ID maps
- `way_scanner`: `read_varint_i64()` consistency
- `extract`: duplicate comment, unused `seq` field, `BlobDesc` Copy derive
- `cli tests`: iterator-based tags API fix
- Dead code removal (260 lines): superseded raw passthrough scaffolding
- jemalloc/mimalloc features removed (fix CLI `--all-features` build)

### Research documents
1. fill-buffer-optimization.md - block-pair merge-join design
2. zlib-level-tuning.md - write path compression analysis
3. ~~multi-extract-optimization.md~~ - retired 2026-04-21; all six ranked items DONE or CLOSED (raw passthrough disproven at planet 5-region). Load-bearing pin in `src/commands/extract/multi.rs`.
4. columnar-integration.md - expansion beyond extract
5. reference/pipelined-reader-paths.md - per-caller reference (was notes/pipelined-reader-retention.md, moved + rewritten)
6. geojson-export-design.md - export v1 design
7. performance-research-summary.md - this document
