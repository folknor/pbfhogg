# Hotpath profiling notes

## Host

- **CPU:** AMD Ryzen 9 5900X 12-core / 24-thread, 3.7 GHz base / 4.95 GHz boost
- **RAM:** 30 GB DDR4
- **Storage:** Samsung 970 EVO Plus 1TB NVMe (project + data)
- **Kernel:** Linux 6.18.0-9-generic (x86_64)

## Datasets

- **Denmark:** seq4704 (465 MB, 59.1M elements) + seq4705 OSC (300 KB, 9K changes)
- **Germany:** seq4704 (4.5 GB, 500M elements) + seq4705 OSC (5.9 MB, 146K changes)

Indexdata variants generated via `pbfhogg cat --type node,way,relation`.
Build: fat LTO, zlib-ng. Run with: `scripts/run-hotpath.sh`,
`scripts/run-hotpath-alloc.sh`, `scripts/run-hotpath-germany.sh`

## Check-refs (pipelined read baseline)

Lightweight pipelined read — directly comparable to TODO.md old numbers.

### Timing

| Function                    | Calls | Avg    | Total  | % Total |
|-----------------------------|-------|--------|--------|---------|
| pbfhogg::main               | 1     | 6.94s  | 6.94s  | 100%    |
| check_refs::check_refs      | 1     | 6.94s  | 6.94s  | 100%    |
| pipeline::run_pipeline      | 1     | 6.93s  | 6.93s  | 100%    |
| reader::for_each_pipelined  | 1     | 6.93s  | 6.93s  | 100%    |
| blob::decompress_blob       | 7396  | 337 us | 2.49s  | 36%     |
| block::new                  | 7396  | 14 us  | 102 ms | 1.5%    |
| wire::parse                 | 14792 | 4.1 us | 60 ms  | 0.9%    |

RSS: 125 MB. Single-threaded (main thread 100% CPU, workers ~2% each).

vs TODO.md old: wall 7.51s -> 6.94s (-8%), decompress_blob 2.55s -> 2.49s,
RSS 143 MB -> 125 MB (-13%). Improvement from fat LTO + codegen-units=1.

## Pipelined read (tags-count)

Exercises `ElementReader::for_each_pipelined` — same path as elivagar/nidhogg ingest.

### Timing

| Function                    | Calls | Avg       | Total  | % Total |
|-----------------------------|-------|-----------|--------|---------|
| pbfhogg::main               | 1     | 8.30s     | 8.30s  | 100%    |
| tags_count::tags_count      | 1     | 5.08s     | 5.08s  | 61%     |
| pipeline::run_pipeline      | 1     | 3.40s     | 3.40s  | 41%     |
| reader::for_each_pipelined  | 1     | 3.40s     | 3.40s  | 41%     |
| blob::decompress_blob       | 7396  | 374 us    | 2.77s  | 33%     |
| block::new                  | 7396  | 14 us     | 103 ms | 1.2%    |
| wire::parse                 | 14792 | 3.6 us    | 54 ms  | 0.6%    |

RSS: 616 MB. Single-threaded (main thread 100% CPU).

tags_count itself (HashMap inserts) is 61% - 41% pipeline = ~20% of total.
Decompression is the dominant library cost at 33%.

### Allocations

| Function                    | Calls | Total    | % Total |
|-----------------------------|-------|----------|---------|
| blob::decompress_blob       | 7396  | 790 MB   | 106%*   |
| wire::parse                 | 14792 | 342 MB   | 46%     |
| block::new                  | 7396  | 171 MB   | 23%     |

*>100% because cumulative (nested calls counted multiple times).

Total alloc: 745 MB. Net RSS diff: 125 MB (most alloc/dealloc churn).
decompress_blob dominates because it allocates the decompression output buffer every call.
wire::parse allocates WireStringTable's Vec<(u32,u32)> offsets per block.

## Decode + write (cat --type node,way,relation)

Full decode of every element, rebuild through BlockBuilder + PbfWriter.
Same write path as nidhogg output. Compression: zlib (default).

### Timing

| Function                    | Calls      | Avg    | Total  | % Total |
|-----------------------------|------------|--------|--------|---------|
| pbfhogg::main               | 1          | 42s    | 42s    | 100%    |
| cat::cat                    | 1          | 42s    | 42s    | 100%    |
| writer::frame_blob          | 7397       | 3.25ms | 24.0s  | 57%     |
| block_builder::take         | 7396       | 468 us | 3.46s  | 8.3%    |
| block_builder::add_node     | 52,489,653 | 43 ns  | 2.27s  | 5.4%    |
| blob::decompress_blob       | 7396       | 266 us | 1.96s  | 4.7%    |
| block_builder::add_way      | 6,616,526  | 219 ns | 1.45s  | 3.5%    |
| block::new                  | 7396       | 10 us  | 77 ms  | 0.2%    |
| wire::parse                 | 14792      | 2.3 us | 33 ms  | 0.1%    |
| block_builder::add_relation | 46,103     | 544 ns | 25 ms  | 0.06%   |

RSS: 19 MB. Single-threaded (main thread 100% CPU).

Compression (frame_blob) dominates at 57%. This is zlib:6 — the default.
BlockBuilder serialization (take) is 8%, node insertion 5%, way insertion 3.5%.
Read-side (decompress + parse) is only ~5% combined — write dominates completely.

### Allocations

| Function                    | Calls      | Total  | % Total |
|-----------------------------|------------|--------|---------|
| block_builder::take         | 7396       | 4.6 GB | 27%     |
| block_builder::add_way      | 6,616,526  | 4.1 GB | 24%     |
| writer::frame_blob          | 7397       | 4.0 GB | 24%     |
| block_builder::add_node     | 52,489,653 | 1.8 GB | 11%     |
| blob::decompress_blob       | 7396       | 1.6 GB | 10%     |
| wire::parse                 | 14792      | 342 MB | 2%      |
| block_builder::add_relation | 46,103     | 52 MB  | 0.3%    |

Total alloc: 16.8 GB (!). Net RSS: 10 MB (massive churn, tiny footprint).

add_way at 4.1 GB across 6.6M calls = 659 bytes/call avg.
This is from fresh Vec allocs for tags.collect() + refs.collect() on every element.
The drain-reuse optimization (reuse Vec across calls) would cut this significantly.

take allocates 4.6 GB — proto serialization buffers, rebuilt every flush.
frame_blob allocates 4.0 GB — compression output buffers.

## Merge (base PBF + 1 OSC diff)

Same API path as nidhogg weekly planet refresh.
630 of 7396 blobs rewritten, rest passthrough.

### Without indexdata (osmium-generated PBF, commit d5c8095)

Input PBF has no indexdata, so classify_blob must decompress every blob.

| Function                    | Calls     | Avg    | Total  | % Total |
|-----------------------------|-----------|--------|--------|---------|
| pbfhogg::main               | 1         | 3.50s  | 3.50s  | 100%    |
| merge::merge                | 1         | 3.50s  | 3.50s  | 100%    |
| writer::frame_blob          | 630       | 9.05ms | 5.70s  | 163%*   |
| merge::classify_blob        | 7383      | 442 us | 3.26s  | 93%     |
| merge::rewrite_block        | 630       | 3.16ms | 1.99s  | 57%     |
| block_builder::add_way      | 2,408,901 | 286 ns | 690 ms | 20%     |
| block_builder::take         | 7407      | 91 us  | 676 ms | 19%     |
| block_builder::add_node     | 2,573,619 | 48 ns  | 126 ms | 3.6%    |
| merge::read_raw_frame       | 7399      | 12 us  | 92 ms  | 2.6%    |
| block_builder::add_relation | 46,108    | 566 ns | 26 ms  | 0.7%    |

*>100% because frame_blob runs in parallel (pipelined writer).

RSS: 95 MB. Multi-threaded (main 95%, 3 workers 68-79%).

### With indexdata (pbfhogg-generated PBF, commit 2a1bfff)

Input PBF has 26-byte indexdata in every BlobHeader. classify_blob uses the
fast path (binary search, no decompression) for non-overlapping blobs.
6766 of 7396 blobs passthrough via index hit, 0 scan-only, 0 skip-decompress.

| Function                    | Calls     | Avg      | Total     | % Total |
|-----------------------------|-----------|----------|-----------|---------|
| pbfhogg::main               | 1         | 5.16s    | 5.16s     | 100%    |
| merge::merge                | 1         | 5.16s    | 5.16s     | 100%    |
| writer::frame_blob          | 631       | 8.99ms   | 5.67s     | 110%*   |
| merge::rewrite_block        | 630       | 1.57ms   | 989ms     | 19%     |
| merge::classify_blob        | 7389      | 85 us    | 630ms     | 12%     |
| block_builder::take         | 7407      | 83 us    | 618ms     | 12%     |
| merge::read_raw_frame       | 7399      | 10 us    | 76ms      | 1.5%    |
| block::new                  | 630       | 23 us    | 14ms      | 0.3%    |
| wire::parse                 | 1260      | 4.2 us   | 5.3ms     | 0.1%    |
| block_builder::add_way      | 1667      | 499 ns   | 833 us    | 0.01%   |

*>100% because frame_blob runs in parallel (pipelined writer).

RSS: 90 MB. Multi-threaded (main peaks 94%, 4 workers 65-85%).

### Analysis

classify_blob improved exactly as predicted: 3.26s → 630ms (5.2×). With
indexdata, the 6766 non-overlapping blobs skip decompression entirely via
binary search on the 26-byte index record. The 630ms residual is the 630
overlapping blobs that still need decompression + scan + full parse.

rewrite_block improved from 1.99s to 989ms (2.0×). This reflects the
StringTable fast-path and pre-seed optimizations added between commits
d5c8095 and 2a1bfff.

**However, wall time went UP: 3.50s → 5.16s (+47%).** The new bottleneck
is `frame_blob` (zlib compression) at 5.67s. In the old profile, the
main thread spent 3.26s on classify_blob, which gave the rayon workers
time to drain the compression pipeline in parallel. With indexdata, the
main thread finishes classification near-instantly and races ahead to
produce all 630 rewrite blobs faster than the workers can compress them.
The main thread then blocks waiting for the compression pipeline to drain.

This reveals that for the zlib path, classify_blob was "useful" overhead:
it throttled main-thread throughput to match the compression pipeline's
drain rate. With indexdata, the compression pipeline becomes the
wallclock bottleneck.

### With indexdata + Compression::None (nidhogg production path, commit 2a1bfff)

Same PBF as above, but `--compression none` (nidhogg's erofs config).
Eliminates the compression bottleneck entirely.

| Function                    | Calls     | Avg      | Total     | % Total |
|-----------------------------|-----------|----------|-----------|---------|
| pbfhogg::main               | 1         | 1.90s    | 1.90s     | 100%    |
| merge::merge                | 1         | 1.90s    | 1.90s     | 100%    |
| merge::rewrite_block        | 630       | 1.49ms   | 936ms     | 49%     |
| merge::classify_blob        | 7384      | 83 us    | 609ms     | 32%     |
| block_builder::take         | 7407      | 81 us    | 597ms     | 31%     |
| merge::read_raw_frame       | 7399      | 11 us    | 85ms      | 4.4%    |
| writer::frame_blob          | 627       | 39 us    | 25ms      | 1.3%    |
| block::new                  | 630       | 22 us    | 14ms      | 0.7%    |
| wire::parse                 | 1260      | 4.2 us   | 5.3ms     | 0.3%    |
| block_builder::add_way      | 1667      | 494 ns   | 824 us    | 0.04%   |

RSS: 85 MB. Effectively single-threaded (main 93%, one worker for I/O at 0%).

frame_blob drops from 5.67s to 25ms — Compression::None eliminates zlib
entirely, turning frame_blob into pure protobuf framing (39μs/blob).

The three remaining bottlenecks are nearly equal:
- rewrite_block: 936ms (49%) — decode + re-encode the 630 affected blocks
- classify_blob: 609ms (32%) — decompress + scan the 630 overlapping blobs
- block_builder::take: 597ms (31%) — protobuf serialization of rewritten blocks

### With passthrough I/O optimizations (indexdata + zlib, same host)

Eliminated unnecessary copies in the merge passthrough path:
1. `RawBlobFrame` stores `blob_offset` instead of duplicate `blob_bytes` Vec
2. `write_raw_owned(Vec<u8>)` moves Vec into channel (no `.to_vec()` copy)
3. `decompress_blob_data_into` decodes directly from `&[u8]` (no `Bytes::copy_from_slice`)
4. `parse_blob_header_with_index` decodes directly from `&[u8]`

| Function                    | Calls     | Avg      | Total     | % Total |
|-----------------------------|-----------|----------|-----------|---------|
| writer::frame_blob          | 628       | 9.85ms   | 6.19s     | 184%*   |
| pbfhogg::main               | 1         | 3.36s    | 3.36s     | 100%    |
| merge::merge                | 1         | 3.36s    | 3.36s     | 100%    |
| merge::classify_blob        | 7361      | 83 us    | 607ms     | 18%     |
| merge::rewrite_block        | 630       | 940 us   | 592ms     | 18%     |
| block_builder::take         | 7407      | 19 us    | 143ms     | 4.3%    |
| merge::read_raw_frame       | 7399      | 10 us    | 74ms      | 2.2%    |

*>100% because frame_blob runs in parallel (pipelined writer).

RSS: 74 MB. Alloc: read_raw_frame 465 MB (was ~795 MB), total merge 931 MB.

### Summary: merge progression

| Configuration                        | Wall time | Bottleneck              |
|--------------------------------------|-----------|-------------------------|
| No indexdata, zlib (old baseline)    | 3.50s     | classify_blob (93%)     |
| Indexdata, zlib (pre-passthrough-IO) | 5.16s     | frame_blob/zlib (110%)  |
| **Indexdata, zlib (passthrough-IO)** | **3.36s** | **frame_blob/zlib (184%)** |
| **Indexdata, none (nidhogg prod)**   | **1.90s** | **rewrite_block (49%)** |

The passthrough I/O optimizations recovered the indexdata+zlib regression:
5.16s → 3.36s (-35%). The main-thread alloc reduction (~330 MB less in
read_raw_frame, ~360 MB less in write_raw) freed CPU cycles that were
previously spent in allocator overhead, allowing the main thread to feed
the compression pipeline faster.

The nidhogg production path (indexdata + Compression::None) is **1.84× faster**
than the old baseline. With compression eliminated and classification mostly
free, the irreducible cost is rewrite_block for the ~630 blocks that overlap
the diff (~4.4M elements decoded + re-encoded).

### Planet-scale extrapolation (75 GB, daily diff)

Denmark has 8.5% blob rewrite ratio (630 / 7396). At planet scale (~1.19M
blobs) with a daily diff (~4M changes), the rewrite fraction explodes.
With ~3M changed node IDs across ~1.06M node blobs (~8000 nodes each):

    P(blob overlaps diff) = 1 - (1 - 3M/9B)^8000 ≈ 1 - e^(-2.67) ≈ 93%

Ways: ~99.8%. Relations: ~100%. Overall: **~92% of blobs need rewriting.**

Extrapolated merge time (indexdata + Compression::None, daily planet diff):

| Component        | Denmark    | Planet (92% rewrite) | Notes                     |
|------------------|------------|----------------------|---------------------------|
| read_raw_frame   | 85ms       | ~14s                 | 161× (I/O, sequential)    |
| classify_blob    | 609ms      | ~2 min               | 1.1M slow-path, ÷8 cores |
| rewrite_block    | 936ms      | **~27 min**          | 1.1M × 1.49ms, sequential |
| take             | 597ms      | ~1.5 min             | 1.1M rewritten blocks     |
| frame_blob (none)| 25ms       | ~43s                 | 1.1M × 39μs, parallel    |
| **Wall time**    | **1.90s**  | **~30 min**          | near-full-rewrite         |

For comparison, a full `cat` (decode + rewrite everything) at planet scale
with Compression::None is ~24 min. At 92% rewrite, merge has no advantage.

**Key insight:** The indexdata and per-blob micro-optimizations (StringTable
fast path, pre-seed, raw packed bytes) each save <10% of rewrite_block's
per-call cost — at planet scale they shave ~1-3 min off a 27-min bottleneck.
The structural optimization is **parallelizing rewrite_block**. At 8 cores,
this could reduce planet merge from ~30 min to ~5 min.

Allocation at planet scale: ~3.2 TB churn (alloc+dealloc), RSS bounded at
~200-500 MB (dominated by DiffRanges ~32 MB + working buffers).

## Germany merge (4.5 GB, 500M elements, 62K blobs, daily diff 146K changes)

Scale test: ~10× Denmark. Rewrite fraction: 18.4% (11,480 / 62,461).

### Summary table

| Config                    | Wall  | classify_blob | rewrite_block | frame_blob    | RSS    |
|---------------------------|-------|---------------|---------------|---------------|--------|
| No indexdata, zlib        | 50.0s | 33.8s (67%)   | 17.6s (35%)   | 109.9s (220%) | 364 MB |
| Indexdata, zlib           | 49.9s | 11.7s (23%)   | 17.7s (36%)   | 109.8s (220%) | 374 MB |
| Indexdata, none           | 52.3s | 11.7s (22%)   | 16.4s (31%)   | 415ms (0.8%)  | 338 MB |

### Key findings

**Rewrite fraction scaling:** Denmark 8.5% → Germany 18.4%. Germany has 16×
more daily changes for 10× more data, so higher change density per blob.
Validates the planet extrapolation model (92% rewrite at planet scale).

**Indexdata benefit hidden by compression:** classify_blob improved 2.9×
(33.8s → 11.7s), saving 22s. But wall time barely changed (50.0s → 49.9s)
because the zlib compression pipeline (110s on rayon workers) is the true
bottleneck. Main thread classify_blob work is completely overlapped by
worker compression time.

**Compression::None is SLOWER:** 52.3s vs 49.9s for zlib. Without parallel
compression work, there's nothing to overlap main-thread work with.
rewrite_block + classify_blob + take run purely sequentially on the main
thread (16.4 + 11.7 + 10.8 = 38.9s), and the total wall time reflects this.
The zlib path effectively hides ~30s of main-thread work behind 110s of
parallel compression.

**Thread utilization contrast:**
- Zlib: main 98%, 4 workers 83-92% — full utilization, compression-bound
- None: main 98%, workers idle (0-11%) — single-threaded, main-thread-bound

**Implication:** At moderate rewrite fractions (>10%), Compression::None only
wins when rewrite_block is also parallelized. Without parallel rewrite, the
zlib path accidentally provides better wall time by overlapping main-thread
serial work with compression. The optimization order matters: parallelize
rewrite_block FIRST, then Compression::None becomes the clear winner.

## Write benchmark: sync vs pipelined (bench_write)

Denmark 483 MB, best of 3, decode + write to /dev/null:

**Current (direct wire encoding):**

| Compression | Sync   | Pipelined | Speedup |
|-------------|--------|-----------|---------|
| none        | 7.1s   | 7.1s      | 1.0x    |
| zstd:3      | 9.1s   | 7.0s      | 1.3x    |
| zlib:6      | 15.5s  | 7.1s      | 2.2x    |

**Previous (prost-based proto::Way/Relation):**

| Compression | Sync   | Pipelined | Speedup |
|-------------|--------|-----------|---------|
| none        | 9.0s   | 9.0s      | 1.0x    |
| zstd:3      | 11.0s  | 9.1s      | 1.2x    |
| zlib:6      | 17.5s  | 9.1s      | 1.9x    |

Direct wire encoding reduced the pipelined floor from ~9s to ~7s (**22% faster**).
Ways and relations are encoded directly to protobuf wire format using 4 reusable
`Vec<u8>` scratch buffers, eliminating per-element `proto::Way`/`proto::Relation`
allocation (~580 bytes/call). Dense nodes stay on prost (already optimized with
pre-allocated Vecs).

Pipelined writer parallelizes compression across rayon workers. All modes
converge to ~7s — the decode + wire-format serialization floor. With
Compression::None (nidhogg production config on erofs), there's nothing
to parallelize so sync = pipelined.

The previous 9s floor broke down as (from hotpath data, pre-wire-encoding):
- block_builder::add_node: 2.3s (5.4%)
- block_builder::add_way: 1.5s (3.5%)
- block_builder::take: 3.5s (8.3%)
- blob::decompress_blob: 2.0s (4.7%)
- wire::parse + block::new: 0.1s
Total: ~9.4s. Direct wire encoding eliminated the add_way/add_relation
allocation overhead and take's prost two-pass encode (encoded_len + encode_raw).

## Optimization targets

### Write floor (~7s, decode + wire-format serialization)
- Compression is solved — pipelined writer hides it completely
- Direct wire encoding reduced floor from ~9s to ~7s (22% faster)
- Remaining cost is decode (4.7%) + BlockBuilder insertion + wire-format serialization
- With Compression::None the write path is I/O-bound at planet scale

### ~~BlockBuilder alloc churn (24% of write alloc)~~ — RESOLVED
- ~~add_way allocates fresh tags + refs Vecs every call (4.1 GB total)~~
- Direct wire encoding eliminated all per-element Vec allocations for ways/relations.
  4 reusable `Vec<u8>` scratch buffers replace `Vec<proto::Way>` / `Vec<proto::Relation>`.
  `StringTable::clear()` reuses HashMap/Vec capacity across blocks.

### decompress_blob buffer reuse (33% of read time)
- DecompressPool already exists for pipelined path
- Sequential path (BlobReader) allocates fresh buffer every blob
- pipelined read already handles this well (3.4s vs 8.3s total)
