# Cross-Reference Synthesis: Performance Review Boxes 1-8

## Implementation Progress

**Last updated:** uncommitted (2026-03-01)

| Tier | Total | Done | Open | Deferred |
|------|-------|------|------|----------|
| P0 | 2 | 2 | 0 | 0 |
| P1 | 3 | 3 | 0 | 0 |
| P2 | 8 | 6 | 0 | 2 |
| P3 | 11 | 9 | 2 | 0 |
| **Total** | **24** | **20** | **2** | **2** |

All per-blob allocation waste (the dominant waste pattern, see "Per-blob allocation" below) has been eliminated.
The BlockBuilder→PbfWriter API boundary information loss (see "Information loss at API boundaries" below) has been resolved.
P0-P2 tiers complete (P2-12 sqpoll deferred, P2-13 reverted). P3 trivial items done.
P3-14 (spatial blob filter) delivers 5-14% extract improvement (not predicted 99%+): blob bboxes
in ID-sorted PBFs span most of the geographic area (87% cover >50% of Denmark), but ~20% of
node blobs are still filtered. Low overhead, free win. Kept.
Remaining open P3: SIMD varint (P3-20), diff streaming (P3-22).
Both are high effort and speculative.

---

## Missed Connections (still relevant)

### io_uring + write-path optimizations compound

Box 7's io_uring replaces only the I/O backend; all framing/compression optimizations (Box 5/6) apply equally. For North America merge: io_uring gave -25% to -30% on I/O. The now-completed scan_block_ids and `to_vec()` eliminations add further savings on top.

### `Compression::None` paradox spans write pipeline and io_uring

`Compression::None` makes the rayon pipeline wasteful (rayon does almost no work). io_uring resolves this: "uring+none is 30% faster than buffered+none." With `Compression::None`, non-compression per-blob work is a larger fraction of wall time, so eliminating overhead (now done) disproportionately benefits the uncompressed path.

### Raw-bytes passthrough could extend beyond merge

`add_way_raw_bytes()` is 12x faster than `add_way()` (17ns vs 210ns). Currently only merge uses this. For cat with type filters that keep all elements in a block, a block-level passthrough would bypass BlockBuilder entirely. This optimization is missed by all boxes as a concrete recommendation.

### Spatial blob filtering: measured reality vs prediction

The 99% estimate was wrong. OSM node IDs are chronological, not geographic. In ID-sorted PBFs, blob bboxes span most of the geographic area (87% of Denmark blobs cover >50% of country). Measured: 5-14% improvement for typical city bbox — modest but free win (negligible overhead). Would become dramatically more valuable with spatially-sorted inputs.

---

## Unified Priority List

### P0 -- Planet-Scale Blockers / Highest Impact

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Status |
|---|---|---|---|---|---|
| 1 | **tags_filter: replace `way_dep_node_ids` Vec\<i64\> with IdSetDense** (box8-commands.md §4B, §10, P0-1) | Box 8 | Prevents OOM: ~40 GB -> ~1.5 GB for broad filters. Enables planet-scale tags_filter two-pass mode. | Low | **DONE** `88f1a2d` — IdSetDense extracted to shared module `src/commands/id_set_dense.rs`, tags_filter updated |
| 2 | **Eliminate `scan_block_ids` in write path by exposing BlobIndex from BlockBuilder** (box5-writer-pipeline.md §Finding 4) | Box 5 | Saves ~50-125 seconds wall time at planet scale. Eliminates ~325 GB of redundant wire-format scanning. | Low-Medium | **DONE** (combined with P1-3) — `take_owned()` returns `Option<(Vec<u8>, BlobIndex)>`, `write_primitive_block_owned` accepts pre-computed BlobIndex |

### P1 -- Measurable Wall-Time Improvements

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Status |
|---|---|---|---|---|---|
| 3 | **Add `take_owned()` to BlockBuilder to eliminate `to_vec()` copies in pipelined paths** (box5-writer-pipeline.md §Finding 1; box6-block-builder.md §8.1, §10, P0; box8-commands.md §8A, §10, P1-1) | Box 5, Box 6, Box 8 | Eliminates ~155 GB copy churn (cat), ~40-50 seconds wall time. Eliminates flush_local double-copy (~17 GB for parallel rewrite paths). | Low | **DONE** (combined with P0-2) — `take_owned() -> Option<(Vec<u8>, BlobIndex)>` via `std::mem::replace`. All command `flush_local`/`flush_block` callers updated |
| 4 | **extract smart pass 2: add BlobFilter for ways-only** (box8-commands.md §3D, §10, P1-2; box4-indexing-mmap.md §6.6) | Box 4, Box 8 | Saves ~60 seconds at planet scale (skips ~80% of decompression in smart pass 2). | Trivial (one line) | **DONE** `b4f2998` |
| 5 | **Add panic recovery to decode pool tasks** (box1-read-orchestration.md §5.1, §7, Priority 1) | Box 1 | Prevents silent blob skipping on rayon task panic. Correctness fix, not performance. | Low (~15 lines) | **DONE** `d1edf45` — `catch_unwind` wraps decode closure, panics converted to Error |

### P2 -- Moderate Impact, Worth Doing

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Status |
|---|---|---|---|---|---|
| 6 | **Pool ZlibDecoder state in DecompressPool** (box2-blob-decode.md §D1, §Recommended Actions Priority 1) | Box 2 | Eliminates ~80 GB allocator churn on read side. Saves ~125-250 ms wall time directly, plus reduced fragmentation pressure. | Low-Medium | **DONE** `1194dc1` — thread-local `flate2::Decompress` with `reset(true)`, uses `decompress_vec` with `FlushDecompress::None` |
| 7 | **Add zstd compressor reuse to FrameScratch** (box5-writer-pipeline.md §Finding 5, §Recommended Action 2) | Box 5 | Eliminates ~1.28 TB allocator churn when zstd is used. Saves ~2.5-12.5 seconds wall time. | Low (~20 lines) | **DONE** `9957279` — `zstd::bulk::Compressor<'static>` stored in `FrameScratch`, CCtx reused across blobs |
| 8 | **tags_filter pass 2: use IdSetDense for O(1) lookups** (box8-commands.md §4C, §10, P2-2) | Box 8 | Reduces pass 2 node processing from ~20 min to ~3 min at planet scale. | Low | **DONE** `96ada44` — all 4 ID sets in `Pass2IdSets` converted to `IdSetDense`, binary_search → `.get()` |
| 9 | **Eliminate blob_type String allocation with enum** (box2-blob-decode.md §D3, §Recommended Actions Priority 2) | Box 2 | Eliminates ~100 MB cumulative alloc at planet scale. Modest wall-clock savings. | Low | **DONE** `387eaf6` — `BlobKind` enum (`OsmHeader`/`OsmData`/`Unknown(String)`) replaces String, parse matches raw bytes |
| 10 | **Use fixed-size array for indexdata** (box2-blob-decode.md §D3, §Recommended Actions Priority 3) | Box 2 | Eliminates ~120 MB cumulative alloc for indexed PBFs at planet scale. | Low | **DONE** `eeff9c1` — `Option<[u8; INDEX_SIZE]>` replaces `Option<Vec<u8>>` |
| 11 | **Use `write_raw_owned` in cat.rs passthrough** (box5-writer-pipeline.md §Finding 7, §Recommended Action 3) | Box 5 | Eliminates one `to_vec()` per passthrough blob in cat. Minor. | Trivial | **DONE** `e5bfa36` — `std::mem::take` moves Vec into writer channel |
| 12 | **Consider removing sqpoll code path** (box7-direct-io-uring.md §7, §11, Priority 2) | Box 7 | No performance gain (<1% across 3 scales). Removes ~30 lines and kernel 5.12+ dependency. Eliminates SQ overflow bug class. | Low | Deferred — needs planet-scale verification first |
| 13 | **extract pass 1: parallel fold+reduce for IdSetDense** (box8-commands.md §3C, §10, P2-1) | Box 8 | ~2-4x speedup for pass 1 (~170s -> ~50-85s at planet scale). | Medium | Reverted `b67aa96` — par_iter contends with pipeline decode pool (14x regression at Denmark). Needs shared thread pool architecture. |

### P3 -- Low Priority / Future / Speculative

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 14 | **Spatial blob filter for extract (BlobIndex v2 with bbox)** (box4-indexing-mmap.md §6.6, §8, Priority 3a) | Box 4 | ~~99%+ node decompression savings~~ **5-14% measured** — blob bboxes span most of geographic area in ID-sorted PBFs (87% of Denmark blobs cover >50% of country, median bbox: 2.45° lat × 4.40° lon). ~20% of node blobs filtered for typical city bbox. OSM node IDs are chronological, not geographic. Still a free win: low overhead (4 i32s tracked, 16 bytes/header, 4-comparison AABB test). | High | **DONE** `40959b8` — BlobIndex v2 (42 bytes) with per-node-blob bbox. Measured: Denmark/Copenhagen 10%, Denmark/tiny 14%, Japan/Tokyo 5.5%. |
| 15 | **Extend io_uring to sort command** (box7-direct-io-uring.md §11, Priority 5; §10, Box 8) | Box 7 | ~25-30% improvement for planet-scale sort write path. | Medium | **DONE** `b293c96` — `--io-uring` and `--sqpoll` flags, same writer selection as merge |
| 16 | **Add `#[inline]` to hot iterators** (`WireMessageIter::next()`, `DenseNodeIter::next()`, `WireGroup::{nodes,ways,relations}()`) (box3-wire-parsing.md §8c, §10, Priority 2) | Box 3 | 0% with fat LTO (current build). 1-3% for library consumers without LTO. | Trivial | **DONE** `3c95704` — `#[inline]` on 5 hot iterator methods |
| 17 | **BlobReader buffer reuse** (box2-blob-decode.md §D2, §Recommended Actions Priority 5) | Box 2 | Eliminates ~80 GB alloc churn for compressed blob data on read side. Low wall-clock impact due to allocator free-list efficiency. | Low-Medium | **DONE** (partial) `4040239` — header buffer reused; blob data buffer consumed by `Bytes::from()`, cannot reuse |
| 18 | **Add `madvise(MADV_SEQUENTIAL)` to MmapBlobReader** (box4-indexing-mmap.md §6.2, §8, Priority 2a) | Box 4 | ~50-80 ms improvement for mmap path. Mmap not used by any command. | Trivial (one line) | **DONE** `3c95704` — `advise(Sequential)` in `MmapBlobReader::new()` |
| 19 | **Add `/// # Memory` doc section to `par_map_reduce`** (box1-read-orchestration.md §2, Finding 1; §7, Priority 2) | Box 1 | Prevents library users from OOMing on planet files. Documentation only. | Trivial | **DONE** `4564a61` — documents ~80 GB RAM for planet, recommends `for_each_pipelined` |
| 20 | **SIMD varint decode for packed arrays (protohoggr)** (box3-wire-parsing.md §5, SIMD varint decoding potential; box6-block-builder.md §8.2, §10, P4) | Box 3, Box 6 | ~25s saved on read side, ~175s on write side (2x on ~350s floor). Requires batch-decode API change. | High | Decompression must be optimized first |
| 21 | **sort overlap run streaming (priority-queue merge)** (box8-commands.md §5B, §10, P2-3) | Box 8 | Prevents OOM on unsorted planet files (worst case ~1 TB). No impact on sorted inputs (common case). | Medium | **DONE** — streaming sweep merge: min-heap flushes elements when ID < next blob's min_id, O(overlap_depth) memory |
| 22 | **derive_changes / diff streaming merge-join** (box8-commands.md §8B, §10, P3-1) | Box 8 | Enables planet-scale derive_changes/diff (currently OOM at ~680 GB per file). | High | Not a current use case |
| 23 | **Deprecate or document IndexedReader limitations** (box4-indexing-mmap.md §4, Finding 3; §8, Priority 3b) | Box 4 | Prevents library users from OOMing with `read_ways_and_deps` at planet scale. | Trivial | **DONE** `4564a61` — `# Memory` doc section on `read_ways_and_deps` |
| 24 | **Add opcode probe at io_uring init** (box7-direct-io-uring.md §2, Finding 1; §11, Priority 3) | Box 7 | Better error messages on old kernels. No performance impact. | Trivial | **DONE** `4704513` — probes WriteFixed/ReadFixed opcodes, returns clear error |

---

## Thematic Patterns

### Per-blob allocation is the dominant waste pattern; per-element costs are irreducible

The most striking cross-cutting finding is that nearly all actionable waste in the codebase occurs at blob granularity (once per ~32 KB compressed / ~1.4 MB decompressed unit), while per-element costs are dominated by irreducible computation (varint encode/decode, hash lookups, delta arithmetic).

**Read-side per-blob waste:** ZlibDecoder state (~32 KB) (box2-blob-decode.md §D1), BlobReader Vec (~32 KB) (box2-blob-decode.md §D2), WireBlobHeader String+Vec (~88 bytes) (box2-blob-decode.md §D3). Total per blob: ~64 KB of allocation churn. At 2.5M blobs: ~160 GB.

**Write-side per-blob waste:** `to_vec()` copy (~130 KB) (box6-block-builder.md §8.1), `scan_block_ids` scan (~130 KB read) (box5-writer-pipeline.md §Finding 4), `frame_blob_into` output Vec (~50 KB) (box5-writer-pipeline.md §Finding 6), zstd encoder (~512 KB when active) (box5-writer-pipeline.md §Finding 5). Total per blob: ~310 KB (zlib) or ~822 KB (zstd). At 2.5M blobs: ~775 GB (zlib) or ~2 TB (zstd).

**Per-element costs (irreducible):** varint decode (~3ns * 119B = 357s) (box3-wire-parsing.md §5), varint encode (~3ns * 117B = 351s) (box6-block-builder.md §8.2), StringTable hash (~10ns * 22.2B = 222s) (box6-block-builder.md §2, Finding 1). These sum to ~930 seconds of pure computation at planet scale and cannot be eliminated without protocol changes.

The implication is clear: **optimize the per-blob overhead first.** The three highest-impact items (scan_block_ids elimination, take_owned, ZlibDecoder pooling) are all per-blob optimizations. Per-element optimizations (SIMD varint, faster hashing) yield diminishing returns because they attack the irreducible floor.

**Status (as of `eeff9c1`):** All per-blob allocation waste has been addressed:
- **Eliminated:** `scan_block_ids` scan (P0-2), `to_vec()` copies via `take_owned()` (P1-3), ZlibDecoder state via thread-local `Decompress` reuse (P2-6), zstd CCtx via `FrameScratch` reuse (P2-7), `blob_type` String via `BlobKind` enum (P2-9), `indexdata` Vec via fixed `[u8; 26]` (P2-10), cat passthrough `to_vec` via `write_raw_owned` (P2-11).
- **Remaining:** BlobReader buffer reuse (P3-17) — low wall-clock impact due to allocator free-list efficiency.

### The read path is well-optimized; the write path has systematic waste

Across all 8 boxes, the read path receives consistently positive assessments:
- Box 1: "No code-level changes are needed urgently." (box1-read-orchestration.md §1, Executive Summary)
- Box 2: "The current design is correct and near-optimal" (DecompressPool). (box2-blob-decode.md §Finding 1, Recommendation)
- Box 3: "The wire parsing layer is not the bottleneck for any measured workload." (box3-wire-parsing.md §10, Priority 1)
- Box 4: "None required" (all three findings assessed as correct/mitigated). (box4-indexing-mmap.md §8, Priority 1)

The write path, by contrast, accumulates findings:
- Box 5: scan_block_ids redundancy (P0), zstd encoder waste (P2), to_vec copy (P4). (box5-writer-pipeline.md §Finding 4, §Finding 5, §Finding 1)
- Box 6: take() return type (P0), encode_packed cost floor (informational). (box6-block-builder.md §8.1, §8.2)
- Box 8: flush_local double-copy (P1). (box8-commands.md §8A)

This asymmetry reflects the project's history: the read path has been optimized through multiple rounds (DecompressPool, wire-format parser, raw-bytes passthrough), while the write path's `BlockBuilder -> PbfWriter` interface originally retained the borrow-based `take()` design that made sense for sync mode but created waste in pipelined mode.

**Status (as of `eeff9c1`):** The write-path systematic waste has been resolved. `take_owned()` returns `(Vec<u8>, BlobIndex)`, eliminating both the `to_vec()` copy and the redundant `scan_block_ids` rescan. Zstd compressor reuse is in `FrameScratch`. Read-side ZlibDecoder state is now pooled via thread-local. The read/write asymmetry has been largely closed.

### The merge hot path is already near-optimal; other commands are not

The merge command has received intensive optimization:
- Raw-bytes passthrough (add_way_raw_bytes, 12x faster than add_way) (box6-block-builder.md §6, raw bytes passthrough performance)
- pre_seed_string_table (eliminates string re-interning) (box6-block-builder.md §2, Finding 1; box3-wire-parsing.md §3, Finding 2)
- Passthrough coalescing (reduces channel sends) (box8-commands.md §2F)
- io_uring for I/O-bound workloads (box7-direct-io-uring.md §9D)
- BlobIndex fast classification (100ns vs 160us per blob) (box4-indexing-mmap.md §5; box8-commands.md §2C)

Other write commands (cat, sort, extract, tags_filter) still use the standard `add_way()` / `add_node()` path with full string interning and delta encoding. Box 6 quantifies this: `add_way_raw_bytes` costs 17ns vs `add_way()` at 210ns (box6-block-builder.md §6, raw bytes passthrough performance). For cat (which decoded elements are re-encoded identically), a block-level passthrough or raw-index API could bring similar gains. This gap is noted in Box 6 (box6-block-builder.md §9, Box 8 interaction) but not elevated to a recommendation by any box.

### Planet-scale memory feasibility separates P0 from P1

The clearest priority distinction is between items that prevent a command from functioning at planet scale (memory blockers) and items that merely make it slower (performance waste).

**Memory blockers (P0):**
- tags_filter `way_dep_node_ids`: ~40 GB for broad filters (box8-commands.md §4B)
- derive_changes / diff full-memory load: ~680 GB per file (box8-commands.md §8B, but acknowledged as out-of-scope)
- sort overlap run materialization: ~1 TB for fully unsorted planet (box8-commands.md §5B, but rare in practice)

**Performance waste (P1-P2):**
- scan_block_ids: ~50-125s wall time (box5-writer-pipeline.md §Finding 4)
- to_vec copies: ~40-50s wall time (box5-writer-pipeline.md §Finding 1; box6-block-builder.md §8.1; box8-commands.md §8A)
- ZlibDecoder pooling: ~125-250ms wall time (box2-blob-decode.md §D1)

Only tags_filter's P0 is both common (two-pass mode with broad filters is a normal use case) and severe (OOM). The other memory blockers involve edge cases (unsorted planet files) or acknowledged limitations (derive_changes). This makes tags_filter the single highest-priority fix.

### Information loss at API boundaries is the root cause of multiple inefficiencies

Three separate findings trace back to the same architectural pattern: information available at one layer is lost at an API boundary and must be reconstructed at the next layer.

1. **BlockBuilder -> PbfWriter:** BlockBuilder knows element type, min/max ID, count, but `take()` returns only `&[u8]`. PbfWriter must re-derive this via `scan_block_ids`. (box5-writer-pipeline.md §Finding 4)

2. **BlockBuilder -> pipelined writer:** BlockBuilder owns the encoded bytes in `encode_buf`, but `take()` returns a borrow. The pipelined writer must copy to obtain ownership. (box5-writer-pipeline.md §Finding 1; box6-block-builder.md §8.1; box8-commands.md §8A)

3. **Read-side WireBlock -> commands:** `PrimitiveBlock::new()` validates UTF-8 for all string table entries, but this validation result is not exposed. `str_from_stringtable` uses `unsafe from_utf8_unchecked` relying on the validation having occurred. This is correct but the information (validated vs. unvalidated) is implicit in the type system rather than explicit. (box3-wire-parsing.md §6, StringTable Analysis, "The from_utf8_unchecked in str_from_stringtable")

The pattern suggests that tightening the `take()` API to return richer types -- `(Vec<u8>, BlobIndex)` instead of `&[u8]` -- would eliminate two of the three issues simultaneously.

**Status (as of `eeff9c1`):** Issues 1 and 2 are resolved. `take_owned() -> Option<(Vec<u8>, BlobIndex)>` returns both ownership and metadata in a single call, eliminating the information loss at the BlockBuilder→PbfWriter boundary.

### Compression dominates everything

The single most consistent finding across all boxes is that zlib/zstd compression dominates wall time for every workload:

- Box 2: decompression is 33-61% of pipelined read time (box2-blob-decode.md §D1; box3-wire-parsing.md §2, Finding 1)
- Box 3: decompression is 5-20x more expensive than parsing (box3-wire-parsing.md §2, Finding 1, Verdict)
- Box 5: compression is 57% of cat wall time (box5-writer-pipeline.md §Finding 3)
- Box 6: "write-only commands are dominated by compression (57% of wall time)" (box6-block-builder.md §2, Finding 1, Verdict)
- Box 8: merge classification without indexdata costs ~500us for decompression vs ~50us for scanning (box8-commands.md §2C)

This means that all non-compression optimizations operate in the ~40-50% of wall time that remains. A hypothetical 2x speedup in parsing, encoding, or I/O would improve total wall time by at most 20-25%. The only way to fundamentally shift the performance profile is to either (a) improve compression throughput (libdeflater already does this for zlib) (box5-writer-pipeline.md §Finding 11), (b) bypass compression entirely for passthrough blobs (already implemented in merge) (box8-commands.md §2C; box4-indexing-mmap.md §5), or (c) reduce the number of blobs that need compression (spatial blob filtering for extract (box4-indexing-mmap.md §6.6), BlobFilter for type-filtered commands (box4-indexing-mmap.md §7, Box 8)).
