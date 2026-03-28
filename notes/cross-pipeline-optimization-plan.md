# Cross-pipeline optimization plan

Patterns discovered during the external join OOM investigation (commit `daaafc3`)
that apply across the pbfhogg codebase. Every item below is planned work.

See [external-join-oom-investigation.md](external-join-oom-investigation.md) for
the full investigation that produced these findings.

## Implementation order

Ordered by impact and dependency. Items within a group can be done in any order.

**Group 1: Infrastructure (unblocks everything else)** — ALL DONE
1. ~~Document PrimitiveBlock retention footgun in pipeline.rs~~ — Done (commit `a067759`)
2. ~~Move `extract_node_tuples` / `NodeTuple` to shared location~~ — Done (`commands/node_scanner.rs`, commit `b3e8bf7`)
3. ~~Move `read_rss_detail()` to shared command utilities~~ — Done (`debug.rs`, done by user)
4. ~~BlobReader fadvise: gate on `target_os = "linux"`~~ — Done (commit `7acbb1a`). libc now non-optional.

**Group 2: Planet blockers** — ALL DONE
5. ~~build-geocode-index: sequential reader for pass 2~~ — Done (commit `5776b67`). Sidecar: anon=325 MB.
6. ~~ALTW dense pass 1: node-only scanner~~ — Done (commit `b3e8bf7`). Japan 69s→42s (-39%).
7. ~~ALTW sparse pass 1: same~~ — Done (commit `a067759`).
8. ~~diff + derive-changes: sequential readers~~ — Done (commit `6d996f6`).
9. ~~check-refs: sequential reader~~ — Done (commit `fb8dd3c`). Sidecar: anon 2.9 GB→581 MB (5x).

**Group 3: External join next cycle (P2b/P2c)**
9. ~~P2b: parallel tuples for external join stage 2~~ — Done. P2b-v2 (commit `80e227b`): pread-from-workers, all alloc thread-local. Europe stage 2: 301s→216s (-28%), anon 20.4 GB→**1.4 GB** (-93%). Planet-safe (~3.9 GB extrapolated).
10. ~~External join stage 1: sequential reader~~ — Done (commit `4daf995`). Anon 11 GB→70 MB. Stage 1: 82s→128s (+54%).
11. ~~P2c: parallel assembly for external join stage 4~~ — Done (commit `6b09796`). Ref count sidecar from stage 1, pread-from-workers with dedicated thread pool. Europe stage 4: 432s→**136s** (-68%), 7.3 GB anon. Total: 866s→**577s** (-33%). 4.5x faster than dense.
12. Planet benchmark for external join

**Group 4: Remaining commands at Europe/planet scale**
12. extract pass 1: node-only scanner for bbox classification (sorted path)
13. check-refs: sequential reader or batch-bounded consumption
14. tags-filter: sequential reader for planet-scale runs
15. cat --type: sequential reader fallback
16. getid / getparents: sequential reader
17. merge --locations-on-ways: node-only scanner for coord mining

**Group 5: Polish**
18. Sparse deprecation: emit warning suggesting external on sorted PBFs
19. Auto-selection for --index-type (dense/external/sparse)
20. node-with-tags-light scanner for geocode builder address points

## Pattern 1: PrimitiveBlock cross-thread retention

**Problem:** The pipelined reader (`into_blocks_pipelined`, `for_each_block_pipelined`,
`for_each_pipelined`) allocates PrimitiveBlock on rayon decode threads
(WireStringTable entries `Box<[(u32,u32)]>` ~10 KB + group_ranges `Box<[(u32,u32)]>`
~8 bytes per block). Consumer thread drops them. Neither glibc nor jemalloc returns
the freed pages to the OS. At 400K+ blocks (Europe/planet), this accumulates 25+ GB
of anonymous heap that appears as RssAnon.

**Root cause:** Cross-thread alloc/free. Allocated on rayon decode thread pool,
freed on consumer thread. Allocator retains freed pages in per-thread arenas.

**Verified by:** jemalloc, MALLOC_ARENA_MAX=1/2, decode_threads(1/2) — all showed
same 25+ GB retention. Only sequential reader (all alloc/free on one thread) or
node-only scanner (zero per-block heap alloc) avoided it.

### Commands affected

Every command using pipelined PrimitiveBlock decode at Europe/planet scale:

| Command | Path | Block count (Europe) | Additional memory | Combined risk |
|---------|------|---------------------|-------------------|---------------|
| cat --type | `for_each_block_pipelined` | ~520K | Minimal | High |
| sort | `into_blocks_pipelined` | ~520K | O(file_size) elements | Already memory-bound |
| extract (all strategies) | `into_blocks_pipelined` (1-3 passes) | ~520K per pass | 4-6 IdSetDense (~9 GB) | High |
| check-refs | `for_each_pipelined` | ~520K | RoaringTreemap (~3 GB) | High |
| tags-filter | `into_blocks_pipelined` (up to 4 passes) | ~520K per pass | Up to 7 IdSetDense (~10.5 GB) | High |
| getid / getparents | `into_blocks_pipelined` | ~520K (often blob-filtered) | Minimal | Medium |
| build-geocode-index pass 1+2 | sequential BlobReader (commit `5776b67`) | ~520K | DenseMmapIndex (~16 GB) | **Fixed** — anon bounded at 325 MB. Mmap RSS remains. |
| diff / derive-changes | `into_blocks_pipelined` (two files) | ~520K x 2 | 2x retention | Very high |
| ALTW dense pass 0/1 | `into_blocks_pipelined` | ~520K (filtered) | 16 GB mmap | Medium (mmap dominates) |
| ALTW sparse pass 0/1 | `into_blocks_pipelined` | ~520K (filtered) | 540 MB + 16 GB values | Medium |

Not affected: merge (raw frames), cat passthrough (raw frames), sort pass 1
(custom sequential), ALTW external (sequential reader + node-only scanner).

### Fix strategy

**Infrastructure level:** Document as known footgun in pipeline.rs. Any consumer
doing lightweight work per block over 400K+ blocks WILL accumulate 25+ GB.

**Per-command fixes (two approaches):**
1. Sequential reader — eliminates cross-thread pattern. Sacrifices IO/decode overlap.
   Already used by external join stages 2 + 4.
2. Specialized scanners — bypass PrimitiveBlock entirely for passes that don't need
   full element access. Already used by external join stage 2 (node-only scanner).

**Long-term fix:** Parallel workers own full lifecycle (decompress -> parse -> extract ->
compact result). Only compact results cross thread boundaries. This is the P2b/P2c
pattern from the external join investigation.

### Implementation plan

| Priority | Command | Fix | Status |
|----------|---------|-----|--------|
| ~~1~~ | ~~pipeline.rs~~ | ~~Document footgun~~ | **Done** (commit `a067759`) |
| ~~2~~ | ~~build-geocode-index~~ | ~~Sequential reader for pass 2~~ | **Done** (commit `5776b67`). Sidecar: anon=325 MB flat. |
| ~~3~~ | ~~ALTW dense pass 1~~ | ~~Node-only scanner~~ | **Done** (commit `b3e8bf7`). Japan 69s→42s (-39%). |
| ~~4~~ | ~~ALTW sparse pass 1~~ | ~~Node-only scanner~~ | **Done** (commit `a067759`). |
| ~~5~~ | ~~check-refs~~ | ~~Sequential reader~~ | **Done** (commit `fb8dd3c`). Sidecar: anon 2.9 GB→581 MB (5x). |
| ~~6~~ | ~~diff~~ | ~~Sequential readers (2x)~~ | **Done** (commit `6d996f6`). |
| ~~7~~ | ~~derive-changes~~ | ~~Sequential readers (2x)~~ | **Done** (commit `6d996f6`). |
| 8 | extract pass 1 | Node-only scanner for bbox classification | **Deprioritized** — sidecar showed flat anon (batched, 2.2 GB). |
| 9 | tags-filter | Sequential reader | **Deprioritized** — sidecar showed flat anon (batched, 2.7 GB). |
| 10 | cat --type | Sequential reader | **Deprioritized** — sidecar showed flat anon (batched, 46 MB). |
| 11 | getid / getparents | Sequential reader | **Deprioritized** — blob-filtered, low block count. |
| 12 | merge --locations-on-ways | Node-only scanner | Niche — merge passthrough handles 92%+. |

## Pattern 2: Node-only wire scanner

**What:** Decompress blob + inline wire format parsing for dense node id/lat/lon.
Skips WireStringTable construction, group_ranges allocation, UTF-8 validation.
Zero per-block heap allocations beyond the reusable decompression buffer.

**Existing code:** `extract_node_tuples()` and `NodeTuple` in external_join.rs
(currently dead code, kept for P2b). Inline scanner in stage 2.

### Scanner family (from perf-Codex)

| Scanner type | Parses | Use case |
|-------------|--------|----------|
| node-id-only | id | ID collection without coordinates |
| node-coordinate | id, lat, lon | ALTW index build, extract bbox, external join |
| node-with-tags-light | id, lat, lon + lazy string table for tag matching | geocode builder address points |

### Retrofit targets

| Target | Scanner type | Effort | Notes |
|--------|-------------|--------|-------|
| ALTW dense pass 1 (node index build) | node-coordinate | Medium | Only needs (id, lat, lon) + `referenced` check. Strongest candidate — all 8 reviewers agree. |
| ALTW sparse pass 1 | node-coordinate | Medium | Same as dense. |
| build-geocode-index pass 2 (nodes) | node-with-tags-light | Medium-High | Needs addr tag matching. Requires lazy string table or split fused pass. Mixed reviewer consensus. |
| extract pass 1 (simple sorted) | node-coordinate | Medium | Fused with way/relation dependency logic. Lower priority. |
| merge --locations-on-ways node extraction | node-coordinate | Low | Currently decompresses node blobs just to mine coordinates (merge.rs:1454). |

### Shared infrastructure

Move `extract_node_tuples` and `NodeTuple` from external_join.rs to a shared
location (e.g., `commands/mod.rs` or a new `commands/node_scanner.rs`). Add the
`node-with-tags-light` variant for geocode builder.

## Pattern 3: Scatter buffer

**What:** For radix-bucketed workflows where bucket partitioning defines output
order. Allocate zeroed buffer for each bucket's range, scatter entries by position,
write_all once. Eliminates sort + reduces syscalls (15x speedup in external join
stage 3).

### Applicability

Narrow — only applies to radix-bucketed workflows:
- External join stage 3 (already done)
- Geocode builder bucketed cell assignment (already uses similar approach)
- Any future radix-partitioned output

## Pattern 4: DecompressPool single-thread reuse

**What:** The DecompressPool works correctly when alloc and free happen on the same
thread. Cross-thread usage (pipelined reader) causes retention because pool buffers
are allocated on rayon threads and returned to the pool from the consumer thread.

**Fix:** When using sequential reader, create a local DecompressPool. Already done
in external join stages 2 + 4. Apply to any command switching to sequential reader.

## Pattern 5: RssAnon/RssFile diagnostic

**What:** Read `/proc/self/status` for RssAnon vs RssFile breakdown. Essential for
diagnosing whether memory growth is heap (anon) or page cache (file). Without this,
we chased page cache fixes for hours when the problem was heap.

### Implementation plan

- Add `read_rss_detail()` to shared command utilities (currently duplicated in
  external_join.rs)
- Gate behind `debug-logging` feature
- Add to any command's periodic logging when investigating memory issues

## Pattern 6: BlobReader fadvise(DONTNEED)

Already shipped (commit `4ab6976`). Evicts page cache behind read head after each
blob. Benefits all pipelined reader consumers automatically.

Done (commit `7acbb1a`). Gated on `target_os = "linux"`, libc now non-optional.

## Sparse deprecation plan

5/8 reviewers say don't remove yet. Consensus: demote, don't deprecate.

**Reasons to keep:**
- Works on unsorted PBFs (external requires Sort.Type_then_ID)
- No temp disk needed (external uses 112-224 GB)
- Planet-tested (external hasn't been validated at planet yet)

**Plan:**
1. Update CLI help to recommend external over sparse for sorted indexed PBFs (done)
2. Validate external at planet scale
3. Emit warning when sparse is selected on a sorted PBF suggesting external
4. Long-term: auto-selection (dense when fits, external when sorted+indexed+disk, sparse otherwise)

## Additional items

### merge --locations-on-ways node extraction
Currently decompresses node blobs to mine coordinates (merge.rs:1454). Node-only
scanner could avoid full PrimitiveBlock construction. Low priority — merge's
passthrough path already handles 92%+ of blobs without decode.

### diff concurrent pipelines
diff runs two pipelined readers simultaneously. At Europe scale that's 2x the
PrimitiveBlock retention (~50 GB). Highest risk command for the retention issue.
Fix: sequential readers for both, or node-only scanners where applicable.

### auto-selection for --index-type
Long-term: detect available RAM, sorted flag, indexdata presence, and temp disk
space. Choose dense/external/sparse automatically. From arch-Codex.

### Document PrimitiveBlock retention footgun
Add warning comment to pipeline.rs explaining that any consumer doing lightweight
per-block work over 400K+ blocks will accumulate 25+ GB of anonymous heap from
cross-thread WireStringTable/group_ranges alloc/free. Point to the investigation
doc and the sequential reader / node-only scanner patterns as mitigations.
