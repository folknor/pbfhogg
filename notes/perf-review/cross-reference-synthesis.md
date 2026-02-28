# Cross-Reference Synthesis: Performance Review Boxes 1-8

## 1. Duplicate Findings

### 1.1 `take()` returns `&[u8]`, forcing `to_vec()` copies in pipelined paths

**Root cause:** `BlockBuilder::take()` (block_builder.rs:772) returns a borrow of the internal `encode_buf`. Any caller that needs ownership (pipelined writer, parallel rewrite, `flush_local`) must copy ~130 KB per block.

**Boxes flagging it:**
- **Box 5** (Finding 1): Identifies the `to_vec()` at writer.rs:330 as "real but low-impact" and estimates ~75 seconds of cumulative memcpy at planet scale (325 GB). Proposes a double-buffer scheme.
- **Box 6** (Finding 8.1): Calls this "the highest-impact finding in this investigation." Estimates ~155 GB of copy churn at planet scale for cat, ~143 GB for merge. Proposes `take_owned()` returning `Vec<u8>` via `std::mem::replace`.
- **Box 8** (Finding 8A): Documents the `flush_local` double-copy pattern where blocks are copied once from `take()` into a local output Vec, then a second time inside `write_primitive_block`. Estimates ~17 GB of unnecessary copying at planet scale.

**Assessment:** Box 6 and Box 8 capture different facets of the same root cause. Box 5 understates impact by only counting the writer.rs copy, missing the `flush_local` first copy. Box 6's planet-scale estimate of ~155 GB (cat) and ~143 GB (merge) is the most complete. Box 8 adds the insight that `flush_block` in `mod.rs` suffers only one copy (no `flush_local` intermediary), while parallel paths suffer two.

The fix proposals are consistent: add `take_owned()` to BlockBuilder (Boxes 5, 6, 8 all converge on this). Box 5 additionally proposes `write_primitive_block_owned(Vec<u8>)` on the PbfWriter side.

---

### 1.2 `scan_block_ids` redundancy in the write path

**Root cause:** `write_primitive_block` (writer.rs:333) rescans the serialized PrimitiveBlock wire format to extract element type, min/max ID, and count for the BlobIndex. BlockBuilder already knows all of this at `take()` time but does not expose it.

**Boxes flagging it:**
- **Box 5** (Finding 4): "A genuine wasted-work finding the reviewer missed." Estimates ~325 GB of data scanned at planet scale, ~50-125 seconds of wall time. Proposes adding min_id/max_id/count tracking to BlockBuilder and returning `BlobIndex` alongside the bytes from `take()`.
- **Box 6** (Section 7, take() analysis): Implicitly acknowledges this by noting take()'s measured 468us/call for cat includes both serialization and the overhead of the subsequent scan.

**Assessment:** Box 5 provides the definitive analysis. Box 6 does not flag it independently but its profiling data (take() timing) would improve if the scan were eliminated. The fix is clean and low-risk: track 3 fields in BlockBuilder, change `take()` signature or add a companion method.

---

### 1.3 Per-blob allocation patterns across subsystems

**Root cause:** Multiple subsystems create and destroy per-blob state that could be pooled or reused.

**Boxes flagging it:**
- **Box 2** (Finding D1): ZlibDecoder allocates ~32 KB inflate state per blob. At planet scale: 2.5M * 32 KB = 80 GB of cumulative alloc/dealloc for decoder state.
- **Box 2** (Finding D2): `BlobReader::next()` allocates a fresh Vec per blob for compressed data (~32 KB). 2.5M * 32 KB = 80 GB cumulative.
- **Box 2** (Finding D3): `WireBlobHeader::parse` allocates a `String` for `blob_type` (~40 bytes) and a `Vec<u8>` for `indexdata` (~48 bytes) per blob. ~100-220 MB cumulative.
- **Box 5** (Finding 5): Zstd encoder allocates ~512 KB internal state per blob. 2.5M * 512 KB = 1.28 TB cumulative (when using zstd).
- **Box 5** (Finding 6): `frame_blob_into` allocates a fresh output `Vec<u8>` (~32-64 KB) per blob. 2.5M * 50 KB = 125 GB cumulative.
- **Box 5** (Finding 9): `FrameScratch` thread-local correctly reuses zlib compressor state via `reset()`, proving the pattern works -- but this was not extended to zstd (see Finding 5).

**Assessment:** This is the dominant waste pattern in the codebase. Total cumulative allocation across all sources: ~80 GB (decoder state) + ~80 GB (blob read buffer) + ~1.28 TB (zstd, when active) + ~125 GB (frame output) + ~220 MB (header fields) = **~1.57 TB of allocator churn at planet scale**. The zstd encoder is the largest single contributor when active. The zlib decoder state is the largest on the read side.

---

### 1.4 SIMD varint decode/encode as a speculative optimization

**Boxes flagging it:**
- **Box 3** (Section 5): Estimates ~119B varint decodes for planet. SIMD could yield 2x on ~76B dense node varints, saving ~25s sequential. Verdict: "Low priority" because decompression dominates 5-20x.
- **Box 6** (Finding 8.2): Estimates ~117B zigzag+varint encode operations for planet. ~351 seconds of irreducible encode floor. SIMD varint encoding "could help" but "complexity is high." Verdict: P4 "Research only."

**Assessment:** Both boxes agree on the magnitude (~120B operations, ~350s) and priority (low/speculative). The read-side and write-side analyses independently confirm that varint processing is a ~10% floor, dominated by compression costs. Neither box considers this actionable without first exhausting compression optimization.

---

### 1.5 DecompressPool Mutex contention: not a concern

**Boxes flagging it:**
- **Box 1** (Section 6, Cross-Box): Estimates contention as minimal -- "Mutex held for <100ns per operation... decode work (200-500 us) dwarfs the lock hold time by 3 orders of magnitude."
- **Box 2** (Finding 1): Full quantitative analysis. P(contention) = 0.005%. Expected contended acquisitions: ~230 out of 5M for planet. "Total contention cost: ~50 us over the entire planet file read."

**Assessment:** Both boxes agree this is not a real issue. Box 2's analysis is definitive. No further discussion needed.

---

## 2. Contradictions

### 2.1 Impact of the `take()` `to_vec()` copy

**Box 5** describes the copy as "real but low-impact":
> "The memcpy is L1/L2 hot (encode just wrote the data) and overlapped with rayon compression and is L1-hot. Peak memory impact is 4.2 MB."

**Box 6** describes it as "the highest-impact finding in this investigation":
> "At planet scale with 1.19M blocks, this is ~155 GB of copy churn. This is the highest-impact finding."

**Box 8** is closer to Box 6:
> "~17 GB of unnecessary copying" (for the double-copy pattern specifically).

**Resolution:** Box 5 is analyzing only the writer.rs:330 copy in isolation and correctly notes it overlaps with compression. Box 6 is analyzing the total allocation churn including the flush_local double-copy and the allocation cost of the replacement buffer. Both are technically correct about different aspects. However, Box 6's characterization is misleading in one way: it estimates ~194 seconds at planet scale assuming 0.8 GB/s memcpy throughput for "L2-miss copies," but Box 5 correctly observes that the data is L1/L2-hot (just encoded by BlockBuilder). The real throughput is closer to 4 GB/s, putting the time at ~39-49 seconds at planet scale. Box 6's estimate is 4-5x too pessimistic on throughput.

The best synthesis: the copy is real, affects ~155 GB at planet scale, takes ~40-50 seconds of wall time (not ~194s), and is partially overlapped with compression. It is a meaningful optimization target but not the "highest-impact" item when compared to scan_block_ids elimination (~50-125s) or compression optimization.

---

### 2.2 Planet-scale estimate for merge rewrite block count

**Box 6** states:
> "Planet merge at 92% rewrite = 1.1M blocks * 130 KB = 143 GB of copies."

**Box 8** states:
> "~43K blocks * ~200 KB average = ~8.6 GB of serialized block data, this means ~17 GB of unnecessary copying."

**Resolution:** These are measuring different things. Box 6's "92% rewrite" is incorrect for the to_vec copy count -- the 92% figure from MEMORY.md refers to blob *overlap with diff*, meaning those blobs need rewriting. But the to_vec copy happens for ALL blocks written through `write_primitive_block`, not just rewritten ones. Box 8's "43K blocks" is closer for the full-write case (cat), while merge writes only rewritten blocks (~8% of planet = ~3.4K blocks) through `write_primitive_block`. Box 6 appears to have confused "rewrite fraction" (8%) with "passthrough fraction" (92%) when computing the copy volume for merge specifically. For cat, Box 6's estimate of ~155 GB (all 1.19M blocks) is correct. For merge, the correct number is ~3.4K blocks * 130 KB = ~442 MB (negligible), not 143 GB.

---

### 2.3 Queue depths: concern vs. non-concern

**Box 1** on WRITE_AHEAD/READ_AHEAD/DECODE_AHEAD:
> "Not needed at current scale... 51 MB pipeline overhead is negligible."

**Box 5** on WRITE_AHEAD=32:
> "Not a real concern. 32 is a well-chosen value."

**Box 7** on ring depth 256 and 64 buffers:
> "Ring depth is correctly sized... 64 is the right size."

**Assessment:** All three boxes independently confirm that queue/buffer depths are not concerns. No contradiction exists -- this is unanimous agreement across the read pipeline (Box 1), write pipeline (Box 5), and io_uring path (Box 7).

---

## 3. Priority Conflicts

### 3.1 `take_owned()` / double-copy elimination

- **Box 5** rates it as Priority 4 (lowest): "Consider double-buffer scheme in BlockBuilder."
- **Box 6** rates it as **P0** (highest): "Add take_owned() to eliminate pipelined to_vec() copy."
- **Box 8** rates it as **P1**: "flush_local double-copy elimination."

**Resolution:** Box 5 understates the priority because it analyzes only the writer.rs side and considers the memcpy "overlapped with compression." Box 6 overestimates it by using pessimistic throughput numbers (0.8 GB/s instead of ~4 GB/s). Box 8's P1 is the most balanced: the fix is low-effort, eliminates measurable waste, but is not the single highest priority when scan_block_ids elimination saves more wall time.

**Unified priority: P1.** Important, straightforward fix, but scan_block_ids elimination (P0) should come first since it saves more wall time and the fix is additive to the same `take()` API.

---

### 3.2 ZlibDecoder pooling

- **Box 2** rates it as Priority 1 (highest in that box): "Pool ZlibDecoder State (Medium effort, Medium impact)."
- **Box 3** does not mention it.
- **Box 5** does not mention it (despite covering the write-side compression reuse).

**Assessment:** Box 2 is the only box that identifies this. The estimated savings (80 GB allocator churn, 125-250 ms wall time at planet scale) are modest in wall-clock terms but significant for allocator pressure. Given that Box 5 already demonstrates the pattern works for zlib compression (FrameScratch reuses `Compress` via `reset()`), extending the same pattern to the read-side ZlibDecoder is consistent and low-risk. Unified priority: P2. Real savings but modest wall-clock impact.

---

### 3.3 Zstd encoder pooling

- **Box 5** rates it as Priority 2: "Add zstd compressor reuse to FrameScratch."
- No other box mentions it.

**Assessment:** Zstd is not the default compression and has limited ecosystem adoption for PBF. The 1.28 TB cumulative allocation is large but only applies when zstd is explicitly selected. Unified priority: P2, conditional on zstd adoption. The fix is low-effort (~20 lines).

---

### 3.4 tags_filter memory blocker

- **Box 8** rates it as **P0**: "The most severe planet-scale blocker in the command layer."
- No other box mentions it.

**Assessment:** Box 8 is correct that this is a planet-scale blocker. A broad filter like `highway=*` would require ~20-40 GB for `way_dep_node_ids` as a sorted `Vec<i64>`. Replacing with `IdSetDense` (already available from extract.rs) caps this at ~1.5 GB. This is the only finding that prevents a command from functioning at planet scale (vs. merely being slow). Unified priority: P0 for correctness.

---

### 3.5 extract smart pass 2 missing BlobFilter

- **Box 8** rates it as P1: "Eliminates decompression of ~80% of blobs."
- **Box 4** mentions it in section 6.6/7 but does not rate it.

**Assessment:** Straightforward one-line fix (add `.with_blob_filter(BlobFilter::only_ways())`). Saves ~60 seconds at planet scale for smart extract. Box 8's P1 is appropriate.

---

## 4. Missed Connections

### 4.1 `scan_block_ids` elimination unlocks `take()` API redesign

No individual box explicitly connects these two findings as a single API change. Box 5 proposes changing `take()` to return `(& [u8], BlobIndex)`. Box 6 proposes adding `take_owned() -> Vec<u8>`. These should be combined: `take_owned() -> Option<(Vec<u8>, BlobIndex)>`. This single API change eliminates both the redundant scan AND the `to_vec()` copy for pipelined paths. The combined savings (~50-125s from scan elimination + ~40-50s from copy elimination) are larger than either alone and the implementation touches the same code.

### 4.2 Per-blob allocation is the dominant waste pattern, not per-element

Across all 8 boxes, the recurring theme is per-blob allocation waste:
- Read side: ZlibDecoder state (80 GB), compressed blob Vec (80 GB), blob_type String (100 MB), indexdata Vec (120 MB)
- Write side: Zstd encoder (1.28 TB when active), `frame_blob_into` output Vec (125 GB), `to_vec()` copies (155 GB)

No box explicitly totals these. The combined read-side allocation churn is **~260 GB** at planet scale (without zstd). The combined write-side churn is **~280 GB** (without zstd). With zstd on the write side, it balloons to **~1.56 TB**. By contrast, per-element costs (varint decode/encode at ~350s each, StringTable at ~222s) are irreducible computation, not allocation waste. The per-blob allocation pattern is addressable through pooling and API changes; the per-element costs are not.

### 4.3 Box 7's io_uring optimizations do not interact with Box 5/6 write-path waste

Box 7 thoroughly analyzes the io_uring I/O backend but notes (Finding 10, Cross-Box): "The uring writer does not change the framing/compression pipeline -- it only replaces the I/O backend. All findings about `to_vec`, `scan_block_ids`, and zstd encoder reuse apply equally to the uring path."

This means the write-path optimizations from Box 5/6 compound with Box 7's io_uring improvements. For North America merge: io_uring gave -25% to -30% on the I/O side. Eliminating scan_block_ids and `to_vec()` copies on the framing side would add further savings on top of io_uring's gains. No box quantifies this compound effect.

### 4.4 `Compression::None` paradox spans Box 5 and Box 7

Box 5 notes that `Compression::None` makes the rayon pipeline wasteful (rayon does almost no work, I/O thread is bottleneck). Box 7 notes that io_uring resolves this: "uring+none is 30% faster than buffered+none." But neither box connects this to the `scan_block_ids` finding: with `Compression::None`, the scan is a larger fraction of per-blob work because there is no compression to hide behind. Eliminating the scan would disproportionately benefit the `Compression::None` path.

### 4.5 `IdSetDense` should be a shared utility, not extract-private

Box 8 identifies that `tags_filter` needs `IdSetDense` (currently only in extract.rs) for its planet-scale memory fix. Box 4 identifies that `IndexedReader` could also benefit from better ID set structures. Neither box notes that `IdSetDense` should be extracted to a shared location (e.g., `src/commands/mod.rs` or its own module) to serve multiple commands. The current duplication risk is that tags_filter might roll its own solution instead of reusing the proven implementation.

### 4.6 Raw-bytes passthrough pattern could extend beyond merge

Box 3, Box 6, and Box 8 all document the 12x speedup of `add_way_raw_bytes()` over `add_way()` (17ns vs 210ns per way). Currently only merge uses this. Box 6 notes: "A potential optimization for cat/sort: add a 'clone block' API that copies an entire PrimitiveBlock's wire-format bytes directly when no transformation is needed." Box 8 does not list this in its recommendations. For cat with type filters that keep all elements in a block (common case), a block-level passthrough would bypass BlockBuilder entirely, saving the entire StringTable + delta encoding cost. This optimization is missed by all boxes as a concrete recommendation.

### 4.7 Spatial blob filtering connects Box 4 to Box 8's extract analysis

Box 4 (section 6.6) identifies spatial blob filtering as "the single largest optimization opportunity identified in Box 4" -- 99%+ node decompression savings for small extracts from planet. Box 8 (section 3C) identifies extract pass 1 as bottlenecked at ~170 seconds on the main thread for planet-scale IdSetDense inserts. A spatial blob filter would reduce the number of elements processed in pass 1 by ~99% for city-level extracts, making the pass 1 bottleneck irrelevant. Neither box connects these findings to show that spatial filtering would effectively eliminate both the I/O bottleneck (decompression savings from Box 4) and the compute bottleneck (IdSetDense insertion from Box 8) simultaneously.

---

## 5. Unified Priority List

### P0 -- Planet-Scale Blockers / Highest Impact

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 1 | **tags_filter: replace `way_dep_node_ids` Vec\<i64\> with IdSetDense** | Box 8 | Prevents OOM: ~40 GB -> ~1.5 GB for broad filters. Enables planet-scale tags_filter two-pass mode. | Low | Extract IdSetDense to shared module |
| 2 | **Eliminate `scan_block_ids` in write path by exposing BlobIndex from BlockBuilder** | Box 5 | Saves ~50-125 seconds wall time at planet scale. Eliminates ~325 GB of redundant wire-format scanning. | Low-Medium | None |

### P1 -- Measurable Wall-Time Improvements

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 3 | **Add `take_owned()` to BlockBuilder to eliminate `to_vec()` copies in pipelined paths** | Box 5, Box 6, Box 8 | Eliminates ~155 GB copy churn (cat), ~40-50 seconds wall time. Eliminates flush_local double-copy (~17 GB for parallel rewrite paths). | Low | Combine with P0-2 (same API change point) |
| 4 | **extract smart pass 2: add BlobFilter for ways-only** | Box 4, Box 8 | Saves ~60 seconds at planet scale (skips ~80% of decompression in smart pass 2). | Trivial (one line) | None |
| 5 | **Add panic recovery to decode pool tasks** | Box 1 | Prevents silent blob skipping on rayon task panic. Correctness fix, not performance. | Low (~15 lines) | None |

### P2 -- Moderate Impact, Worth Doing

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 6 | **Pool ZlibDecoder state in DecompressPool** | Box 2 | Eliminates ~80 GB allocator churn on read side. Saves ~125-250 ms wall time directly, plus reduced fragmentation pressure. | Low-Medium | None |
| 7 | **Add zstd compressor reuse to FrameScratch** | Box 5 | Eliminates ~1.28 TB allocator churn when zstd is used. Saves ~2.5-12.5 seconds wall time. | Low (~20 lines) | None (only matters when zstd selected) |
| 8 | **tags_filter pass 2: use IdSetDense for O(1) lookups** | Box 8 | Reduces pass 2 node processing from ~20 min to ~3 min at planet scale. | Low | Depends on P0-1 |
| 9 | **Eliminate blob_type String allocation with enum** | Box 2 | Eliminates ~100 MB cumulative alloc at planet scale. Modest wall-clock savings. | Low | None |
| 10 | **Use fixed-size array for indexdata** | Box 2 | Eliminates ~120 MB cumulative alloc for indexed PBFs at planet scale. | Low | None |
| 11 | **Use `write_raw_owned` in cat.rs passthrough** | Box 5 | Eliminates one `to_vec()` per passthrough blob in cat. Minor. | Trivial | None |
| 12 | **Consider removing sqpoll code path** | Box 7 | No performance gain (<1% across 3 scales). Removes ~30 lines and kernel 5.12+ dependency. Eliminates SQ overflow bug class. | Low | Verify at planet scale first |
| 13 | **extract pass 1: parallel fold+reduce for IdSetDense** | Box 8 | ~2-4x speedup for pass 1 (~170s -> ~50-85s at planet scale). | Medium | None (merge() method exists) |

### P3 -- Low Priority / Future / Speculative

| # | Description | Source Boxes | Planet-Scale Impact | Effort | Dependencies |
|---|---|---|---|---|---|
| 14 | **Spatial blob filter for extract (BlobIndex v2 with bbox)** | Box 4 | 99%+ node decompression savings for small extracts from planet. Most impactful for city-level planet extracts. | High | BlobIndex format extension, scan_block_ids extension |
| 15 | **Extend io_uring to sort command** | Box 7 | ~25-30% improvement for planet-scale sort write path. | Medium | None |
| 16 | **Add `#[inline]` to hot iterators** (`WireMessageIter::next()`, `DenseNodeIter::next()`, `WireGroup::{nodes,ways,relations}()`) | Box 3 | 0% with fat LTO (current build). 1-3% for library consumers without LTO. | Trivial | None |
| 17 | **BlobReader buffer reuse** | Box 2 | Eliminates ~80 GB alloc churn for compressed blob data on read side. Low wall-clock impact due to allocator free-list efficiency. | Low-Medium | None |
| 18 | **Add `madvise(MADV_SEQUENTIAL)` to MmapBlobReader** | Box 4 | ~50-80 ms improvement for mmap path. Mmap not used by any command. | Trivial (one line) | None |
| 19 | **Add `/// # Memory` doc section to `par_map_reduce`** | Box 1 | Prevents library users from OOMing on planet files. Documentation only. | Trivial | None |
| 20 | **SIMD varint decode for packed arrays (protohoggr)** | Box 3, Box 6 | ~25s saved on read side, ~175s on write side (2x on ~350s floor). Requires batch-decode API change. | High | Decompression must be optimized first |
| 21 | **sort overlap run streaming (priority-queue merge)** | Box 8 | Prevents OOM on unsorted planet files (worst case ~1 TB). No impact on sorted inputs (common case). | Medium | None |
| 22 | **derive_changes / diff streaming merge-join** | Box 8 | Enables planet-scale derive_changes/diff (currently OOM at ~680 GB per file). | High | Not a current use case |
| 23 | **Deprecate or document IndexedReader limitations** | Box 4 | Prevents library users from OOMing with `read_ways_and_deps` at planet scale. | Trivial | None |
| 24 | **Add opcode probe at io_uring init** | Box 7 | Better error messages on old kernels. No performance impact. | Trivial | None |

---

## 6. Thematic Patterns

### 6.1 Per-blob allocation is the dominant waste pattern; per-element costs are irreducible

The most striking cross-cutting finding is that nearly all actionable waste in the codebase occurs at blob granularity (once per ~32 KB compressed / ~1.4 MB decompressed unit), while per-element costs are dominated by irreducible computation (varint encode/decode, hash lookups, delta arithmetic).

**Read-side per-blob waste:** ZlibDecoder state (~32 KB), BlobReader Vec (~32 KB), WireBlobHeader String+Vec (~88 bytes). Total per blob: ~64 KB of allocation churn. At 2.5M blobs: ~160 GB.

**Write-side per-blob waste:** `to_vec()` copy (~130 KB), `scan_block_ids` scan (~130 KB read), `frame_blob_into` output Vec (~50 KB), zstd encoder (~512 KB when active). Total per blob: ~310 KB (zlib) or ~822 KB (zstd). At 2.5M blobs: ~775 GB (zlib) or ~2 TB (zstd).

**Per-element costs (irreducible):** varint decode (~3ns * 119B = 357s), varint encode (~3ns * 117B = 351s), StringTable hash (~10ns * 22.2B = 222s). These sum to ~930 seconds of pure computation at planet scale and cannot be eliminated without protocol changes.

The implication is clear: **optimize the per-blob overhead first.** The three highest-impact items (scan_block_ids elimination, take_owned, ZlibDecoder pooling) are all per-blob optimizations. Per-element optimizations (SIMD varint, faster hashing) yield diminishing returns because they attack the irreducible floor.

### 6.2 The read path is well-optimized; the write path has systematic waste

Across all 8 boxes, the read path receives consistently positive assessments:
- Box 1: "No code-level changes are needed urgently."
- Box 2: "The current design is correct and near-optimal" (DecompressPool).
- Box 3: "The wire parsing layer is not the bottleneck for any measured workload."
- Box 4: "None required" (all three findings assessed as correct/mitigated).

The write path, by contrast, accumulates findings:
- Box 5: scan_block_ids redundancy (P0), zstd encoder waste (P2), to_vec copy (P4).
- Box 6: take() return type (P0), encode_packed cost floor (informational).
- Box 8: flush_local double-copy (P1).

This asymmetry reflects the project's history: the read path has been optimized through multiple rounds (DecompressPool, wire-format parser, raw-bytes passthrough), while the write path's `BlockBuilder -> PbfWriter` interface retains the original borrow-based `take()` design that made sense for sync mode but creates waste in pipelined mode.

### 6.3 The merge hot path is already near-optimal; other commands are not

The merge command has received intensive optimization:
- Raw-bytes passthrough (add_way_raw_bytes, 12x faster than add_way)
- pre_seed_string_table (eliminates string re-interning)
- Passthrough coalescing (reduces channel sends)
- io_uring for I/O-bound workloads
- BlobIndex fast classification (100ns vs 160us per blob)

Other write commands (cat, sort, extract, tags_filter) still use the standard `add_way()` / `add_node()` path with full string interning and delta encoding. Box 6 quantifies this: `add_way_raw_bytes` costs 17ns vs `add_way()` at 210ns. For cat (which decoded elements are re-encoded identically), a block-level passthrough or raw-index API could bring similar gains. This gap is noted in Box 6 (section 9) but not elevated to a recommendation by any box.

### 6.4 Planet-scale memory feasibility separates P0 from P1

The clearest priority distinction is between items that prevent a command from functioning at planet scale (memory blockers) and items that merely make it slower (performance waste).

**Memory blockers (P0):**
- tags_filter `way_dep_node_ids`: ~40 GB for broad filters (Box 8)
- derive_changes / diff full-memory load: ~680 GB per file (Box 8, but acknowledged as out-of-scope)
- sort overlap run materialization: ~1 TB for fully unsorted planet (Box 8, but rare in practice)

**Performance waste (P1-P2):**
- scan_block_ids: ~50-125s wall time (Box 5)
- to_vec copies: ~40-50s wall time (Box 5, 6, 8)
- ZlibDecoder pooling: ~125-250ms wall time (Box 2)

Only tags_filter's P0 is both common (two-pass mode with broad filters is a normal use case) and severe (OOM). The other memory blockers involve edge cases (unsorted planet files) or acknowledged limitations (derive_changes). This makes tags_filter the single highest-priority fix.

### 6.5 Information loss at API boundaries is the root cause of multiple inefficiencies

Three separate findings trace back to the same architectural pattern: information available at one layer is lost at an API boundary and must be reconstructed at the next layer.

1. **BlockBuilder -> PbfWriter:** BlockBuilder knows element type, min/max ID, count, but `take()` returns only `&[u8]`. PbfWriter must re-derive this via `scan_block_ids`. (Box 5)

2. **BlockBuilder -> pipelined writer:** BlockBuilder owns the encoded bytes in `encode_buf`, but `take()` returns a borrow. The pipelined writer must copy to obtain ownership. (Box 5, 6, 8)

3. **Read-side WireBlock -> commands:** `PrimitiveBlock::new()` validates UTF-8 for all string table entries, but this validation result is not exposed. `str_from_stringtable` uses `unsafe from_utf8_unchecked` relying on the validation having occurred. This is correct but the information (validated vs. unvalidated) is implicit in the type system rather than explicit. (Box 3)

The pattern suggests that tightening the `take()` API to return richer types -- `(Vec<u8>, BlobIndex)` instead of `&[u8]` -- would eliminate two of the three issues simultaneously.

### 6.6 Compression dominates everything

The single most consistent finding across all boxes is that zlib/zstd compression dominates wall time for every workload:

- Box 2: decompression is 33-61% of pipelined read time
- Box 3: decompression is 5-20x more expensive than parsing
- Box 5: compression is 57% of cat wall time
- Box 6: "write-only commands are dominated by compression (57% of wall time)"
- Box 8: merge classification without indexdata costs ~500us for decompression vs ~50us for scanning

This means that all non-compression optimizations operate in the ~40-50% of wall time that remains. A hypothetical 2x speedup in parsing, encoding, or I/O would improve total wall time by at most 20-25%. The only way to fundamentally shift the performance profile is to either (a) improve compression throughput (libdeflater already does this for zlib), (b) bypass compression entirely for passthrough blobs (already implemented in merge), or (c) reduce the number of blobs that need compression (spatial blob filtering for extract, BlobFilter for type-filtered commands).
