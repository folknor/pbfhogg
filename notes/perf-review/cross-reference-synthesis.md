# Cross-Reference Synthesis: Performance Review Boxes 1-8

## Implementation Progress

**Last updated:** `eeff9c1` (2026-02-28)

| Tier | Total | Done | Open | Deferred |
|------|-------|------|------|----------|
| P0 | 2 | 2 | 0 | 0 |
| P1 | 3 | 2 | 1 | 0 |
| P2 | 8 | 6 | 1 | 1 |
| P3 | 11 | 0 | 11 | 0 |
| **Total** | **24** | **10** | **13** | **1** |

All per-blob allocation waste (the dominant waste pattern, §6.1) has been eliminated.
The BlockBuilder→PbfWriter API boundary information loss (§6.5) has been resolved.
Remaining open items are correctness (P1-5 panic recovery), algorithmic (P2-13 parallel extract),
and speculative/future (P3).

---

## 1. Duplicate Findings

### 1.1 `take()` returns `&[u8]`, forcing `to_vec()` copies in pipelined paths

**Root cause:** `BlockBuilder::take()` (block_builder.rs:772) returns a borrow of the internal `encode_buf`. Any caller that needs ownership (pipelined writer, parallel rewrite, `flush_local`) must copy ~130 KB per block. (box6-block-builder.md §8.1, box5-writer-pipeline.md §Finding 1)

**Boxes flagging it:**
- **Box 5** (Finding 1): Identifies the `to_vec()` at writer.rs:330 as "real but low-impact" and estimates ~75 seconds of cumulative memcpy at planet scale (325 GB). Proposes a double-buffer scheme. (box5-writer-pipeline.md §Finding 1, Verdict)
- **Box 6** (Finding 8.1): Calls this "the highest-impact finding in this investigation." Estimates ~155 GB of copy churn at planet scale for cat, ~143 GB for merge. Proposes `take_owned()` returning `Vec<u8>` via `std::mem::replace`. (box6-block-builder.md §8.1)
- **Box 8** (Finding 8A): Documents the `flush_local` double-copy pattern where blocks are copied once from `take()` into a local output Vec, then a second time inside `write_primitive_block`. Estimates ~17 GB of unnecessary copying at planet scale. (box8-commands.md §8A)

**Assessment:** Box 6 and Box 8 capture different facets of the same root cause. Box 5 understates impact by only counting the writer.rs copy, missing the `flush_local` first copy (box5-writer-pipeline.md §Finding 1, "Additional to_vec in flush_local"). Box 6's planet-scale estimate of ~155 GB (cat) and ~143 GB (merge) is the most complete (box6-block-builder.md §8.1, cost quantification table). Box 8 adds the insight that `flush_block` in `mod.rs` suffers only one copy (no `flush_local` intermediary), while parallel paths suffer two (box8-commands.md §8A).

The fix proposals are consistent: add `take_owned()` to BlockBuilder (Boxes 5, 6, 8 all converge on this). Box 5 additionally proposes `write_primitive_block_owned(Vec<u8>)` on the PbfWriter side. (box5-writer-pipeline.md §Recommended Action 4; box6-block-builder.md §10, P0; box8-commands.md §10, P1-1)

---

### 1.2 `scan_block_ids` redundancy in the write path

**Root cause:** `write_primitive_block` (writer.rs:333) rescans the serialized PrimitiveBlock wire format to extract element type, min/max ID, and count for the BlobIndex. BlockBuilder already knows all of this at `take()` time but does not expose it. (box5-writer-pipeline.md §Finding 4)

**Boxes flagging it:**
- **Box 5** (Finding 4): "A genuine wasted-work finding the reviewer missed." Estimates ~325 GB of data scanned at planet scale, ~50-125 seconds of wall time. Proposes adding min_id/max_id/count tracking to BlockBuilder and returning `BlobIndex` alongside the bytes from `take()`. (box5-writer-pipeline.md §Finding 4, Quantified waste)
- **Box 6** (Section 7, take() analysis): Implicitly acknowledges this by noting take()'s measured 468us/call for cat includes both serialization and the overhead of the subsequent scan. (box6-block-builder.md §7, take() timing from profiling)

**Assessment:** Box 5 provides the definitive analysis (box5-writer-pipeline.md §Finding 4, Fix). Box 6 does not flag it independently but its profiling data (take() timing) would improve if the scan were eliminated (box6-block-builder.md §7). The fix is clean and low-risk: track 3 fields in BlockBuilder, change `take()` signature or add a companion method.

---

### 1.3 Per-blob allocation patterns across subsystems

**Root cause:** Multiple subsystems create and destroy per-blob state that could be pooled or reused.

**Boxes flagging it:**
- **Box 2** (Finding D1): ZlibDecoder allocates ~32 KB inflate state per blob. At planet scale: 2.5M * 32 KB = 80 GB of cumulative alloc/dealloc for decoder state. (box2-blob-decode.md §D1)
- **Box 2** (Finding D2): `BlobReader::next()` allocates a fresh Vec per blob for compressed data (~32 KB). 2.5M * 32 KB = 80 GB cumulative. (box2-blob-decode.md §D2)
- **Box 2** (Finding D3): `WireBlobHeader::parse` allocates a `String` for `blob_type` (~40 bytes) and a `Vec<u8>` for `indexdata` (~48 bytes) per blob. ~100-220 MB cumulative. (box2-blob-decode.md §D3)
- **Box 5** (Finding 5): Zstd encoder allocates ~512 KB internal state per blob. 2.5M * 512 KB = 1.28 TB cumulative (when using zstd). (box5-writer-pipeline.md §Finding 5)
- **Box 5** (Finding 6): `frame_blob_into` allocates a fresh output `Vec<u8>` (~32-64 KB) per blob. 2.5M * 50 KB = 125 GB cumulative. (box5-writer-pipeline.md §Finding 6)
- **Box 5** (Finding 9): `FrameScratch` thread-local correctly reuses zlib compressor state via `reset()`, proving the pattern works -- but this was not extended to zstd (see Finding 5). (box5-writer-pipeline.md §Finding 9)

**Assessment:** This is the dominant waste pattern in the codebase. Total cumulative allocation across all sources: ~80 GB (decoder state) + ~80 GB (blob read buffer) + ~1.28 TB (zstd, when active) + ~125 GB (frame output) + ~220 MB (header fields) = **~1.57 TB of allocator churn at planet scale**. The zstd encoder is the largest single contributor when active. The zlib decoder state is the largest on the read side.

---

### 1.4 SIMD varint decode/encode as a speculative optimization

**Boxes flagging it:**
- **Box 3** (Section 5): Estimates ~119B varint decodes for planet. SIMD could yield 2x on ~76B dense node varints, saving ~25s sequential. Verdict: "Low priority" because decompression dominates 5-20x. (box3-wire-parsing.md §5, SIMD varint decoding potential)
- **Box 6** (Finding 8.2): Estimates ~117B zigzag+varint encode operations for planet. ~351 seconds of irreducible encode floor. SIMD varint encoding "could help" but "complexity is high." Verdict: P4 "Research only." (box6-block-builder.md §8.2)

**Assessment:** Both boxes agree on the magnitude (~120B operations, ~350s) and priority (low/speculative). The read-side and write-side analyses independently confirm that varint processing is a ~10% floor, dominated by compression costs. Neither box considers this actionable without first exhausting compression optimization.

---

### 1.5 DecompressPool Mutex contention: not a concern

**Boxes flagging it:**
- **Box 1** (Section 6, Cross-Box): Estimates contention as minimal -- "Mutex held for <100ns per operation... decode work (200-500 us) dwarfs the lock hold time by 3 orders of magnitude." (box1-read-orchestration.md §6, Cross-Box Interactions, Box 2)
- **Box 2** (Finding 1): Full quantitative analysis. P(contention) = 0.005%. Expected contended acquisitions: ~230 out of 5M for planet. "Total contention cost: ~50 us over the entire planet file read." (box2-blob-decode.md §Finding 1, Contention math)

**Assessment:** Both boxes agree this is not a real issue. Box 2's analysis is definitive. No further discussion needed.

---

## 2. Contradictions

### 2.1 Impact of the `take()` `to_vec()` copy

**Box 5** describes the copy as "real but low-impact" (box5-writer-pipeline.md §Finding 1, Verdict):
> "The memcpy is L1/L2 hot (encode just wrote the data) and overlapped with rayon compression and is L1-hot. Peak memory impact is 4.2 MB."

**Box 6** describes it as "the highest-impact finding in this investigation" (box6-block-builder.md §8.1):
> "At planet scale with 1.19M blocks, this is ~155 GB of copy churn. This is the highest-impact finding."

**Box 8** is closer to Box 6 (box8-commands.md §8A):
> "~17 GB of unnecessary copying" (for the double-copy pattern specifically).

**Resolution:** Box 5 is analyzing only the writer.rs:330 copy in isolation and correctly notes it overlaps with compression (box5-writer-pipeline.md §Finding 1, Verdict). Box 6 is analyzing the total allocation churn including the flush_local double-copy and the allocation cost of the replacement buffer (box6-block-builder.md §8.1, cost quantification table). Both are technically correct about different aspects. However, Box 6's characterization is misleading in one way: it estimates ~194 seconds at planet scale assuming 0.8 GB/s memcpy throughput for "L2-miss copies" (box6-block-builder.md §8.1), but Box 5 correctly observes that the data is L1/L2-hot (just encoded by BlockBuilder) (box5-writer-pipeline.md §Finding 1). The real throughput is closer to 4 GB/s, putting the time at ~39-49 seconds at planet scale. Box 6's estimate is 4-5x too pessimistic on throughput.

The best synthesis: the copy is real, affects ~155 GB at planet scale, takes ~40-50 seconds of wall time (not ~194s), and is partially overlapped with compression. It is a meaningful optimization target but not the "highest-impact" item when compared to scan_block_ids elimination (~50-125s) or compression optimization.

---

### 2.2 Planet-scale estimate for merge rewrite block count

**Box 6** states (box6-block-builder.md §8.1, cost quantification table):
> "Planet merge at 92% rewrite = 1.1M blocks * 130 KB = 143 GB of copies."

**Box 8** states (box8-commands.md §8A):
> "~43K blocks * ~200 KB average = ~8.6 GB of serialized block data, this means ~17 GB of unnecessary copying."

**Resolution:** These are measuring different things. Box 6's "92% rewrite" is incorrect for the to_vec copy count -- the 92% figure from MEMORY.md refers to blob *overlap with diff*, meaning those blobs need rewriting. But the to_vec copy happens for ALL blocks written through `write_primitive_block`, not just rewritten ones. Box 8's "43K blocks" is closer for the full-write case (cat), while merge writes only rewritten blocks (~8% of planet = ~3.4K blocks) through `write_primitive_block`. Box 6 appears to have confused "rewrite fraction" (8%) with "passthrough fraction" (92%) when computing the copy volume for merge specifically. For cat, Box 6's estimate of ~155 GB (all 1.19M blocks) is correct. For merge, the correct number is ~3.4K blocks * 130 KB = ~442 MB (negligible), not 143 GB.

---

### 2.3 Queue depths: concern vs. non-concern

**Box 1** on WRITE_AHEAD/READ_AHEAD/DECODE_AHEAD (box1-read-orchestration.md §3, Finding 2):
> "Not needed at current scale... 51 MB pipeline overhead is negligible."

**Box 5** on WRITE_AHEAD=32 (box5-writer-pipeline.md §Finding 2, Verdict):
> "Not a real concern. 32 is a well-chosen value."

**Box 7** on ring depth 256 and 64 buffers (box7-direct-io-uring.md §9A, §5):
> "Ring depth is correctly sized... 64 is the right size."

**Assessment:** All three boxes independently confirm that queue/buffer depths are not concerns. No contradiction exists -- this is unanimous agreement across the read pipeline (Box 1), write pipeline (Box 5), and io_uring path (Box 7).

---

## 3. Priority Conflicts

### 3.1 `take_owned()` / double-copy elimination

- **Box 5** rates it as Priority 4 (lowest): "Consider double-buffer scheme in BlockBuilder." (box5-writer-pipeline.md §Recommended Action 4)
- **Box 6** rates it as **P0** (highest): "Add take_owned() to eliminate pipelined to_vec() copy." (box6-block-builder.md §10, P0)
- **Box 8** rates it as **P1**: "flush_local double-copy elimination." (box8-commands.md §10, P1-1)

**Resolution:** Box 5 understates the priority because it analyzes only the writer.rs side and considers the memcpy "overlapped with compression" (box5-writer-pipeline.md §Finding 1, Verdict). Box 6 overestimates it by using pessimistic throughput numbers (0.8 GB/s instead of ~4 GB/s) (box6-block-builder.md §8.1). Box 8's P1 is the most balanced: the fix is low-effort, eliminates measurable waste, but is not the single highest priority when scan_block_ids elimination saves more wall time (box8-commands.md §10, P1-1).

**Unified priority: P1.** Important, straightforward fix, but scan_block_ids elimination (P0) should come first since it saves more wall time and the fix is additive to the same `take()` API.

---

### 3.2 ZlibDecoder pooling

- **Box 2** rates it as Priority 1 (highest in that box): "Pool ZlibDecoder State (Medium effort, Medium impact)." (box2-blob-decode.md §Recommended Actions, Priority 1)
- **Box 3** does not mention it.
- **Box 5** does not mention it (despite covering the write-side compression reuse).

**Assessment:** Box 2 is the only box that identifies this. The estimated savings (80 GB allocator churn, 125-250 ms wall time at planet scale) are modest in wall-clock terms but significant for allocator pressure (box2-blob-decode.md §D1, §Recommended Actions Priority 1). Given that Box 5 already demonstrates the pattern works for zlib compression (FrameScratch reuses `Compress` via `reset()`) (box5-writer-pipeline.md §Finding 9), extending the same pattern to the read-side ZlibDecoder is consistent and low-risk. Unified priority: P2. Real savings but modest wall-clock impact.

---

### 3.3 Zstd encoder pooling

- **Box 5** rates it as Priority 2: "Add zstd compressor reuse to FrameScratch." (box5-writer-pipeline.md §Recommended Action 2)
- No other box mentions it.

**Assessment:** Zstd is not the default compression and has limited ecosystem adoption for PBF. The 1.28 TB cumulative allocation is large but only applies when zstd is explicitly selected (box5-writer-pipeline.md §Finding 5, Quantified waste). Unified priority: P2, conditional on zstd adoption. The fix is low-effort (~20 lines) (box5-writer-pipeline.md §Finding 5, Fix).

---

### 3.4 tags_filter memory blocker

- **Box 8** rates it as **P0**: "The most severe planet-scale blocker in the command layer." (box8-commands.md §4B, §10, P0-1)
- No other box mentions it.

**Assessment:** Box 8 is correct that this is a planet-scale blocker. A broad filter like `highway=*` would require ~20-40 GB for `way_dep_node_ids` as a sorted `Vec<i64>` (box8-commands.md §4B). Replacing with `IdSetDense` (already available from extract.rs) caps this at ~1.5 GB (box8-commands.md §10, P0-1; box8-commands.md §3A for IdSetDense sizing). This is the only finding that prevents a command from functioning at planet scale (vs. merely being slow). Unified priority: P0 for correctness.

---

### 3.5 extract smart pass 2 missing BlobFilter

- **Box 8** rates it as P1: "Eliminates decompression of ~80% of blobs." (box8-commands.md §3D, §10, P1-2)
- **Box 4** mentions it in section 6.6/7 but does not rate it. (box4-indexing-mmap.md §6.6, §7, Box 8)

**Assessment:** Straightforward one-line fix (add `.with_blob_filter(BlobFilter::only_ways())`) (box8-commands.md §3D). Saves ~60 seconds at planet scale for smart extract (box8-commands.md §10, P1-2). Box 8's P1 is appropriate.

---

## 4. Missed Connections

### 4.1 `scan_block_ids` elimination unlocks `take()` API redesign

No individual box explicitly connects these two findings as a single API change. Box 5 proposes changing `take()` to return `(& [u8], BlobIndex)` (box5-writer-pipeline.md §Finding 4, Fix). Box 6 proposes adding `take_owned() -> Vec<u8>` (box6-block-builder.md §8.1, §10, P0). These should be combined: `take_owned() -> Option<(Vec<u8>, BlobIndex)>`. This single API change eliminates both the redundant scan (box5-writer-pipeline.md §Finding 4, ~50-125s) AND the `to_vec()` copy for pipelined paths (box6-block-builder.md §8.1, ~40-50s adjusted). The combined savings are larger than either alone and the implementation touches the same code.

### 4.2 Per-blob allocation is the dominant waste pattern, not per-element

Across all 8 boxes, the recurring theme is per-blob allocation waste:
- Read side: ZlibDecoder state (80 GB) (box2-blob-decode.md §D1), compressed blob Vec (80 GB) (box2-blob-decode.md §D2), blob_type String (100 MB) (box2-blob-decode.md §D3), indexdata Vec (120 MB) (box2-blob-decode.md §D3)
- Write side: Zstd encoder (1.28 TB when active) (box5-writer-pipeline.md §Finding 5), `frame_blob_into` output Vec (125 GB) (box5-writer-pipeline.md §Finding 6), `to_vec()` copies (155 GB) (box6-block-builder.md §8.1)

No box explicitly totals these. The combined read-side allocation churn is **~260 GB** at planet scale (without zstd). The combined write-side churn is **~280 GB** (without zstd). With zstd on the write side, it balloons to **~1.56 TB**. By contrast, per-element costs (varint decode/encode at ~350s each, StringTable at ~222s) are irreducible computation, not allocation waste. The per-blob allocation pattern is addressable through pooling and API changes; the per-element costs are not.

### 4.3 Box 7's io_uring optimizations do not interact with Box 5/6 write-path waste

Box 7 thoroughly analyzes the io_uring I/O backend but notes (box7-direct-io-uring.md §10, Cross-Box Interactions, Box 5): "The uring writer does not change the framing/compression pipeline -- it only replaces the I/O backend. All findings about `to_vec`, `scan_block_ids`, and zstd encoder reuse apply equally to the uring path."

This means the write-path optimizations from Box 5/6 compound with Box 7's io_uring improvements. For North America merge: io_uring gave -25% to -30% on the I/O side. Eliminating scan_block_ids and `to_vec()` copies on the framing side would add further savings on top of io_uring's gains. No box quantifies this compound effect.

### 4.4 `Compression::None` paradox spans Box 5 and Box 7

Box 5 notes that `Compression::None` makes the rayon pipeline wasteful (rayon does almost no work, I/O thread is bottleneck) (box5-writer-pipeline.md §Finding 3). Box 7 notes that io_uring resolves this: "uring+none is 30% faster than buffered+none" (box7-direct-io-uring.md §9D). But neither box connects this to the `scan_block_ids` finding (box5-writer-pipeline.md §Finding 4): with `Compression::None`, the scan is a larger fraction of per-blob work because there is no compression to hide behind. Eliminating the scan would disproportionately benefit the `Compression::None` path.

### 4.5 `IdSetDense` should be a shared utility, not extract-private

Box 8 identifies that `tags_filter` needs `IdSetDense` (currently only in extract.rs) for its planet-scale memory fix (box8-commands.md §10, P0-1). Box 4 identifies that `IndexedReader` could also benefit from better ID set structures (box4-indexing-mmap.md §4, Finding 3). Neither box notes that `IdSetDense` should be extracted to a shared location (e.g., `src/commands/mod.rs` or its own module) to serve multiple commands. The current duplication risk is that tags_filter might roll its own solution instead of reusing the proven implementation.

**Status (as of `96ada44`):** Resolved. `IdSetDense` extracted to `src/commands/id_set_dense.rs` as a shared module (`88f1a2d`). Both `extract.rs` and `tags_filter.rs` import from it. All 4 ID sets in tags_filter `Pass2IdSets` converted to `IdSetDense` with O(1) `.get()` lookups (`96ada44`).

### 4.6 Raw-bytes passthrough pattern could extend beyond merge

Box 3, Box 6, and Box 8 all document the 12x speedup of `add_way_raw_bytes()` over `add_way()` (17ns vs 210ns per way) (box6-block-builder.md §6, raw bytes passthrough performance; box3-wire-parsing.md §9, Box 6 interaction). Currently only merge uses this. Box 6 notes: "A potential optimization for cat/sort: add a 'clone block' API that copies an entire PrimitiveBlock's wire-format bytes directly when no transformation is needed" (box6-block-builder.md §9, Box 8 interaction). Box 8 does not list this in its recommendations. For cat with type filters that keep all elements in a block (common case), a block-level passthrough would bypass BlockBuilder entirely, saving the entire StringTable + delta encoding cost. This optimization is missed by all boxes as a concrete recommendation.

### 4.7 Spatial blob filtering connects Box 4 to Box 8's extract analysis

Box 4 (section 6.6) identifies spatial blob filtering as "the single largest optimization opportunity identified in Box 4" -- 99%+ node decompression savings for small extracts from planet (box4-indexing-mmap.md §6.6). Box 8 (section 3C) identifies extract pass 1 as bottlenecked at ~170 seconds on the main thread for planet-scale IdSetDense inserts (box8-commands.md §3C). A spatial blob filter would reduce the number of elements processed in pass 1 by ~99% for city-level extracts, making the pass 1 bottleneck irrelevant. Neither box connects these findings to show that spatial filtering would effectively eliminate both the I/O bottleneck (decompression savings from box4-indexing-mmap.md §6.6) and the compute bottleneck (IdSetDense insertion from box8-commands.md §3C) simultaneously.

---

## 5. Unified Priority List

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
| 5 | **Add panic recovery to decode pool tasks** (box1-read-orchestration.md §5.1, §7, Priority 1) | Box 1 | Prevents silent blob skipping on rayon task panic. Correctness fix, not performance. | Low (~15 lines) | Open |

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
| 13 | **extract pass 1: parallel fold+reduce for IdSetDense** (box8-commands.md §3C, §10, P2-1) | Box 8 | ~2-4x speedup for pass 1 (~170s -> ~50-85s at planet scale). | Medium | Open |

### P3 -- Low Priority / Future / Speculative

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 14 | **Spatial blob filter for extract (BlobIndex v2 with bbox)** (box4-indexing-mmap.md §6.6, §8, Priority 3a) | Box 4 | 99%+ node decompression savings for small extracts from planet. Most impactful for city-level planet extracts. | High | BlobIndex format extension, scan_block_ids extension |
| 15 | **Extend io_uring to sort command** (box7-direct-io-uring.md §11, Priority 5; §10, Box 8) | Box 7 | ~25-30% improvement for planet-scale sort write path. | Medium | None |
| 16 | **Add `#[inline]` to hot iterators** (`WireMessageIter::next()`, `DenseNodeIter::next()`, `WireGroup::{nodes,ways,relations}()`) (box3-wire-parsing.md §8c, §10, Priority 2) | Box 3 | 0% with fat LTO (current build). 1-3% for library consumers without LTO. | Trivial | None |
| 17 | **BlobReader buffer reuse** (box2-blob-decode.md §D2, §Recommended Actions Priority 5) | Box 2 | Eliminates ~80 GB alloc churn for compressed blob data on read side. Low wall-clock impact due to allocator free-list efficiency. | Low-Medium | None |
| 18 | **Add `madvise(MADV_SEQUENTIAL)` to MmapBlobReader** (box4-indexing-mmap.md §6.2, §8, Priority 2a) | Box 4 | ~50-80 ms improvement for mmap path. Mmap not used by any command. | Trivial (one line) | None |
| 19 | **Add `/// # Memory` doc section to `par_map_reduce`** (box1-read-orchestration.md §2, Finding 1; §7, Priority 2) | Box 1 | Prevents library users from OOMing on planet files. Documentation only. | Trivial | None |
| 20 | **SIMD varint decode for packed arrays (protohoggr)** (box3-wire-parsing.md §5, SIMD varint decoding potential; box6-block-builder.md §8.2, §10, P4) | Box 3, Box 6 | ~25s saved on read side, ~175s on write side (2x on ~350s floor). Requires batch-decode API change. | High | Decompression must be optimized first |
| 21 | **sort overlap run streaming (priority-queue merge)** (box8-commands.md §5B, §10, P2-3) | Box 8 | Prevents OOM on unsorted planet files (worst case ~1 TB). No impact on sorted inputs (common case). | Medium | None |
| 22 | **derive_changes / diff streaming merge-join** (box8-commands.md §8B, §10, P3-1) | Box 8 | Enables planet-scale derive_changes/diff (currently OOM at ~680 GB per file). | High | Not a current use case |
| 23 | **Deprecate or document IndexedReader limitations** (box4-indexing-mmap.md §4, Finding 3; §8, Priority 3b) | Box 4 | Prevents library users from OOMing with `read_ways_and_deps` at planet scale. | Trivial | None |
| 24 | **Add opcode probe at io_uring init** (box7-direct-io-uring.md §2, Finding 1; §11, Priority 3) | Box 7 | Better error messages on old kernels. No performance impact. | Trivial | None |

---

## 6. Thematic Patterns

### 6.1 Per-blob allocation is the dominant waste pattern; per-element costs are irreducible

The most striking cross-cutting finding is that nearly all actionable waste in the codebase occurs at blob granularity (once per ~32 KB compressed / ~1.4 MB decompressed unit), while per-element costs are dominated by irreducible computation (varint encode/decode, hash lookups, delta arithmetic).

**Read-side per-blob waste:** ZlibDecoder state (~32 KB) (box2-blob-decode.md §D1), BlobReader Vec (~32 KB) (box2-blob-decode.md §D2), WireBlobHeader String+Vec (~88 bytes) (box2-blob-decode.md §D3). Total per blob: ~64 KB of allocation churn. At 2.5M blobs: ~160 GB.

**Write-side per-blob waste:** `to_vec()` copy (~130 KB) (box6-block-builder.md §8.1), `scan_block_ids` scan (~130 KB read) (box5-writer-pipeline.md §Finding 4), `frame_blob_into` output Vec (~50 KB) (box5-writer-pipeline.md §Finding 6), zstd encoder (~512 KB when active) (box5-writer-pipeline.md §Finding 5). Total per blob: ~310 KB (zlib) or ~822 KB (zstd). At 2.5M blobs: ~775 GB (zlib) or ~2 TB (zstd).

**Per-element costs (irreducible):** varint decode (~3ns * 119B = 357s) (box3-wire-parsing.md §5), varint encode (~3ns * 117B = 351s) (box6-block-builder.md §8.2), StringTable hash (~10ns * 22.2B = 222s) (box6-block-builder.md §2, Finding 1). These sum to ~930 seconds of pure computation at planet scale and cannot be eliminated without protocol changes.

The implication is clear: **optimize the per-blob overhead first.** The three highest-impact items (scan_block_ids elimination, take_owned, ZlibDecoder pooling) are all per-blob optimizations. Per-element optimizations (SIMD varint, faster hashing) yield diminishing returns because they attack the irreducible floor.

**Status (as of `eeff9c1`):** All per-blob allocation waste has been addressed:
- **Eliminated:** `scan_block_ids` scan (P0-2), `to_vec()` copies via `take_owned()` (P1-3), ZlibDecoder state via thread-local `Decompress` reuse (P2-6), zstd CCtx via `FrameScratch` reuse (P2-7), `blob_type` String via `BlobKind` enum (P2-9), `indexdata` Vec via fixed `[u8; 26]` (P2-10), cat passthrough `to_vec` via `write_raw_owned` (P2-11).
- **Remaining:** BlobReader buffer reuse (P3-17) — low wall-clock impact due to allocator free-list efficiency.

### 6.2 The read path is well-optimized; the write path has systematic waste

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

### 6.3 The merge hot path is already near-optimal; other commands are not

The merge command has received intensive optimization:
- Raw-bytes passthrough (add_way_raw_bytes, 12x faster than add_way) (box6-block-builder.md §6, raw bytes passthrough performance)
- pre_seed_string_table (eliminates string re-interning) (box6-block-builder.md §2, Finding 1; box3-wire-parsing.md §3, Finding 2)
- Passthrough coalescing (reduces channel sends) (box8-commands.md §2F)
- io_uring for I/O-bound workloads (box7-direct-io-uring.md §9D)
- BlobIndex fast classification (100ns vs 160us per blob) (box4-indexing-mmap.md §5; box8-commands.md §2C)

Other write commands (cat, sort, extract, tags_filter) still use the standard `add_way()` / `add_node()` path with full string interning and delta encoding. Box 6 quantifies this: `add_way_raw_bytes` costs 17ns vs `add_way()` at 210ns (box6-block-builder.md §6, raw bytes passthrough performance). For cat (which decoded elements are re-encoded identically), a block-level passthrough or raw-index API could bring similar gains. This gap is noted in Box 6 (box6-block-builder.md §9, Box 8 interaction) but not elevated to a recommendation by any box.

### 6.4 Planet-scale memory feasibility separates P0 from P1

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

### 6.5 Information loss at API boundaries is the root cause of multiple inefficiencies

Three separate findings trace back to the same architectural pattern: information available at one layer is lost at an API boundary and must be reconstructed at the next layer.

1. **BlockBuilder -> PbfWriter:** BlockBuilder knows element type, min/max ID, count, but `take()` returns only `&[u8]`. PbfWriter must re-derive this via `scan_block_ids`. (box5-writer-pipeline.md §Finding 4)

2. **BlockBuilder -> pipelined writer:** BlockBuilder owns the encoded bytes in `encode_buf`, but `take()` returns a borrow. The pipelined writer must copy to obtain ownership. (box5-writer-pipeline.md §Finding 1; box6-block-builder.md §8.1; box8-commands.md §8A)

3. **Read-side WireBlock -> commands:** `PrimitiveBlock::new()` validates UTF-8 for all string table entries, but this validation result is not exposed. `str_from_stringtable` uses `unsafe from_utf8_unchecked` relying on the validation having occurred. This is correct but the information (validated vs. unvalidated) is implicit in the type system rather than explicit. (box3-wire-parsing.md §6, StringTable Analysis, "The from_utf8_unchecked in str_from_stringtable")

The pattern suggests that tightening the `take()` API to return richer types -- `(Vec<u8>, BlobIndex)` instead of `&[u8]` -- would eliminate two of the three issues simultaneously.

**Status (as of `eeff9c1`):** Issues 1 and 2 are resolved. `take_owned() -> Option<(Vec<u8>, BlobIndex)>` returns both ownership and metadata in a single call, eliminating the information loss at the BlockBuilder→PbfWriter boundary.

### 6.6 Compression dominates everything

The single most consistent finding across all boxes is that zlib/zstd compression dominates wall time for every workload:

- Box 2: decompression is 33-61% of pipelined read time (box2-blob-decode.md §D1; box3-wire-parsing.md §2, Finding 1)
- Box 3: decompression is 5-20x more expensive than parsing (box3-wire-parsing.md §2, Finding 1, Verdict)
- Box 5: compression is 57% of cat wall time (box5-writer-pipeline.md §Finding 3)
- Box 6: "write-only commands are dominated by compression (57% of wall time)" (box6-block-builder.md §2, Finding 1, Verdict)
- Box 8: merge classification without indexdata costs ~500us for decompression vs ~50us for scanning (box8-commands.md §2C)

This means that all non-compression optimizations operate in the ~40-50% of wall time that remains. A hypothetical 2x speedup in parsing, encoding, or I/O would improve total wall time by at most 20-25%. The only way to fundamentally shift the performance profile is to either (a) improve compression throughput (libdeflater already does this for zlib) (box5-writer-pipeline.md §Finding 11), (b) bypass compression entirely for passthrough blobs (already implemented in merge) (box8-commands.md §2C; box4-indexing-mmap.md §5), or (c) reduce the number of blobs that need compression (spatial blob filtering for extract (box4-indexing-mmap.md §6.6), BlobFilter for type-filtered commands (box4-indexing-mmap.md §7, Box 8)).
