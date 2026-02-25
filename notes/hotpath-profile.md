# Hotpath profiling notes

Denmark 483 MB, 59.1M elements (52.5M nodes, 6.6M ways, 46K rels).
Commit d5c8095, fat LTO, zlib-ng.

Run with: `scripts/run-hotpath.sh` and `scripts/run-hotpath-alloc.sh`

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
