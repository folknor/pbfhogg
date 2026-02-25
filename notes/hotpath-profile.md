# Hotpath profiling notes

Denmark seq4704 (483 MB, 59.1M elements) + seq4705 OSC (300 KB, 9K changes).
Commit d5c8095, fat LTO, zlib-ng.

Run with: `scripts/run-hotpath.sh` and `scripts/run-hotpath-alloc.sh`

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

Same API path as nidhogg weekly planet refresh. Input PBF has no indexdata
(osmium-generated), so classify_blob must decompress every blob.
630 of 7396 blobs rewritten, rest passthrough.

### Timing

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

classify_blob at 93% is the no-indexdata penalty — every blob must be
decompressed to check if it contains affected IDs. With indexdata
(pbfhogg-generated PBFs), this drops to ~21% (see TODO.md old numbers:
603ms vs 3.26s). The indexdata optimization saves ~2.6s on Denmark.

rewrite_block at 57% is the decode+re-encode cost for the 630 affected blocks.
frame_blob (compression) at 163% is parallelized across rayon workers.

## Optimization targets

### Compression (57% of write time)
- zstd:3 is ~2x faster than zlib:6 for writes (10.8s vs 17.4s on Denmark)
- Pipelined writer (`to_path_pipelined`) would parallelize compression across cores
- nidhogg already uses pipelined writer in production

### BlockBuilder alloc churn (24% of write alloc)
- add_way allocates fresh tags + refs Vecs every call (4.1 GB total)
- Could reuse Vecs across calls with drain pattern
- Marked wontfix for now — would require API change or internal buffer reuse

### decompress_blob buffer reuse (33% of read time)
- DecompressPool already exists for pipelined path
- Sequential path (BlobReader) allocates fresh buffer every blob
- pipelined read already handles this well (3.4s vs 8.3s total)
