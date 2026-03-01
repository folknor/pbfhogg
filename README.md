pbfhogg
=======

Fast OpenStreetMap PBF reader and writer for Rust.

Originally a fork of [osmpbf](https://github.com/b-r-u/osmpbf/), extended with PBF writing, pipelined parallel decoding, memory-mapped reading, and blob passthrough for efficient merge workflows.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `HeaderBuilder`, `BlockBuilder`, and `PbfWriter` — dense node packing, delta encoding, configurable compression (none, zlib, zstd)
- **Memory-mapped reading** via `MmapBlobReader` for zero-copy blob iteration
- **Blob passthrough** (`write_raw` / `copy_file_range`) for copying unmodified blobs during merge/cat — kernel-space copy eliminates userspace buffer overhead
- **Blob indexdata** — embeds element type + ID range + spatial bbox in BlobHeader for fast merge classification and spatial filtering without decompression
- **Blob tag index** — embeds per-blob tag key metadata in BlobHeader field 4; the pipeline skips decompression of blobs that provably lack required tag keys (e.g. `tags-filter highway=primary` skips all blobs without a `highway` key)
- **Configurable compression** — zlib (default), zstd, or none; zlib-rs for fast pure-Rust decompression and compression
- **O_DIRECT I/O** — optional `linux-direct-io` feature bypasses the page cache for planet-scale (80 GB+) reads and writes, preventing cache pollution on the host
- **io_uring writes** — optional `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and registered buffers for maximum throughput when I/O-bound

## Usage

```toml
[dependencies]
pbfhogg = "0.1"
```

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;

// Check if the PBF declares sorted elements
if reader.header().is_sorted() {
    println!("PBF is sorted by type then ID");
}

reader.for_each(|element| {
    if let Element::Way(way) = element {
        // process way
    }
})?;
# Ok::<(), std::io::Error>(())
```

### Writing

```rust
use pbfhogg::block_builder::{HeaderBuilder, BlockBuilder};
use pbfhogg::writer::{PbfWriter, Compression};

let mut writer = PbfWriter::to_path("output.osm.pbf".as_ref(), Compression::default())?;

// Build a sorted header with bounding box
let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
writer.write_header(&header_bytes)?;

// Add elements via BlockBuilder
let mut bb = BlockBuilder::new();
bb.add_dense_node(1, 100_000_000, 200_000_000, &[("name", "Test")], None);
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

`HeaderBuilder::from_header(&existing_header)` copies bbox and replication metadata from an existing PBF header — useful for commands that transform data while preserving metadata.

## Read modes

| Method | Order | Sorted guarantee | Use case |
|--------|-------|-----------------|----------|
| `for_each` | File order | Yes — nodes arrive in ascending ID order when `header().is_sorted()` is `true` | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Yes — same guarantee, with parallel decompression overlapping I/O | Fastest ordered read (production hot path) |
| `for_each_block_pipelined` | File order | Yes — blocks arrive in file order, consumer iterates elements | Consumers that need parallel processing per block (owned `PrimitiveBlock`) |
| `into_blocks_pipelined` | File order | Yes — same as above, but returns an `Iterator` | Loop control: early exit, zipping two files, interleaving work |
| `par_map_reduce` | Arbitrary | No — elements are distributed across rayon workers in unspecified order | Aggregation (counts, statistics) where order doesn't matter |

`for_each_block_pipelined` and `into_blocks_pipelined` deliver owned `PrimitiveBlock`s instead of individual elements. The consumer can send blocks to other threads for parallel processing, enabling overlapped I/O + decode + consumer parallelism without blocking the pipeline. `into_blocks_pipelined` runs the pipeline in a background thread and requires `R: 'static` (`ElementReader<FileReader>` satisfies this).

In debug builds, `for_each` and `for_each_pipelined` assert that node IDs are monotonically increasing when the sorted flag is set.

These methods are the `ElementReader` API for library consumers. The CLI commands mostly use `BlobReader` directly for blob-level operations (raw passthrough, file seeking, multi-pass scanning) where element-level decoding is unnecessary.

## CLI

pbfhogg includes a command-line toolkit for common OSM PBF operations:

```
pbfhogg fileinfo <file>                   Show PBF metadata (--extended for element counts)
pbfhogg check-refs <file>                 Validate referential integrity
pbfhogg cat <files...> -o <out>           Concatenate PBF files (-t node,way,relation to filter)
pbfhogg sort <file> -o <out>              Sort into standard order (nodes → ways → relations, by ID)
pbfhogg extract <file> -o <out> -b <bbox> Extract by bounding box (minlon,minlat,maxlon,maxlat)
pbfhogg extract <file> -o <out> -p <geo>  Extract by GeoJSON polygon
pbfhogg add-locations-to-ways <f> -o <o>  Embed node coordinates in ways
pbfhogg merge <base> <changes> -o <out>   Apply OSC diff to a PBF file
pbfhogg derive-changes <old> <new> -o <f> Generate OSC diff from two PBF snapshots
pbfhogg diff <old> <new>                 Compare two PBF files (-v for verbose, -c to hide common)
pbfhogg tags-count <file>                 Count tag key=value frequencies
pbfhogg tags-filter <file> -o <out> <exp> Filter elements by tag expressions
pbfhogg getid <file> -o <out> <ids>       Extract elements by ID (e.g. n123 w456 r789)
pbfhogg removeid <file> -o <out> <ids>    Remove elements by ID
pbfhogg node-stats <file>                 Analyze node coordinate statistics for FOR compression sizing
```

Extract supports three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), and `--smart` (three passes, completes multipolygon/boundary relations — all member ways and their nodes are included even if outside the region).

Add-locations-to-ways supports `--index-type hash` (default, HashMap) and `--index-type dense` (anonymous mmap, 8 bytes/slot, for planet-scale — ~68 GB physical for 8.5B nodes vs HashMap's ~192 GB).

Node-stats streams all nodes and reports coordinate value ranges, FOR (Frame of Reference) block bit-width distributions, and estimated compressed size. Designed to evaluate whether FOR compression (128-value blocks, per-block min + bitpacked offsets) is viable for in-RAM sorted node stores at planet scale. Runs in constant memory using the pipelined reader.

All write commands accept `--compression` to control blob compression: `none`, `zlib` (default), `zstd`, or with explicit level (`zlib:9`, `zstd:19`).

## Performance

Read throughput — count all 59M elements in Denmark extract (461 MB), best of 3 runs, fat LTO (commit `90df51f`):

<!-- BENCH:START -->
| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.31s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| **pbfhogg** | pipelined | **1.3s** | `for_each_pipelined`, preserves file order |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | sequential | 2.8s | `for_each` |
| **pbfhogg** | mmap | 2.9s | `MmapBlobReader` sequential decode |
| **pbfhogg** | blobreader | 2.9s | `BlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat → opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |
<!-- BENCH:END -->

Write throughput — decode all 59M elements then write through `BlockBuilder` + `PbfWriter` to `/dev/null` (commit `def80d9`):

| Compression | Sync | Pipelined | Notes |
|-------------|------|-----------|-------|
| none | 6.2s | 6.2s | decode + wire-format serialization floor |
| zstd:3 | 8.1s | **6.2s** | pipelined hides compression cost |
| zlib:6 | 14.5s | **6.3s** | 2.3x speedup from parallel compression |

With pipelined writes, all compression modes converge to ~6.2s — the decode + wire-format serialization floor. All element types are encoded directly to protobuf wire format using reusable scratch buffers (no per-element allocation, no external protobuf dependencies). `Compression::None` on erofs is the target production config.

CLI commands — Denmark (487 MB, 59M elements, commit `23862d1`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| merge (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| tags-filter amenity=restaurant -R | **0.58s** | 1.19s | **2.1x** |
| cat --type way (indexdata) | **1.1s** | 2.22s | **2.0x** |
| tags-count --type way (indexdata) | **0.34s** | 0.59s | **1.7x** |
| getid (9 elements) | **0.53s** | 0.83s | **1.6x** |
| add-locations-to-ways | **11.5s** | 12.1s | **1.1x** |

Filter commands (cat, tags-filter, tags-count, getid, removeid) use parallel element processing — each rayon thread owns a `BlockBuilder` and processes decoded blocks in parallel, then results are written sequentially. PBFs with blob-level indexdata skip decompression of irrelevant blob types; PBFs with tagdata additionally skip blobs that provably lack required tag keys. Merge uses blob passthrough (zero decode for unmodified blobs). Sort uses streaming sweep merge — for sorted inputs with indexdata, blobs pass through as raw bytes; unsorted inputs use blob-level permutation.

Extract — Japan (2.4 GB, 344M elements, Tokyo bbox, commit `23862d1`):

| Strategy | pbfhogg | osmium | ratio |
|----------|---------|--------|-------|
| simple | 12.9s | **7.2s** | 1.79x |
| complete-ways | 13.3s | **11.0s** | 1.21x |
| smart | 14.9s | **13.4s** | 1.11x |

Extract uses pipelined parallel decoding with metadata skipping in scan-only passes. Smart Pass 2 (way dependency resolution) iterates only way groups, skipping all node and relation blocks. Complete-ways and smart are within 10-16% of osmium; simple's gap is structural (two passes vs osmium's single pass — the extra file read costs ~5s at this scale).

Merge at scale — single-pass 4-phase batch pipeline with O(log n) inline upsert assignment, reader thread read-ahead, and passthrough coalescing (commit `d6a9b55`):

| Dataset | Config | Time | vs osmium |
|---------|--------|------|-----------|
| Japan (2.4 GB, 43K diff) | indexdata + zlib | **3.0s** | **15x** faster |
| Germany (4.5 GB, 146K diff) | indexdata + zlib | **10.1s** | — |
| Germany (4.5 GB, 146K diff) | indexdata + none | **5.9s** | — |
| N. America (18.8 GB, 645K diff) | indexdata + zlib | **24.4s** | — |
| N. America (18.8 GB, 645K diff) | indexdata + none | **13.3s** | — |

At Japan scale, osmium takes 36.6s for the same operation (9-15x slower) because it decodes and re-encodes every element. pbfhogg passes ~92% of blobs through as raw bytes without decompression, using blob-level indexdata for O(1) classification. The single-pass pipeline overlaps reader I/O, parallel classification, parallel rewrite of touched blobs, and pipelined writes — passthrough blobs are coalesced into large buffers and moved into the writer channel with zero copy.

All CLI commands are cross-validated against osmium on Denmark (`dev verify`). cat, tags-filter, add-locations-to-ways, and getid produce byte-identical output. derive-changes produces a correct roundtrip (apply derived OSC back to old = new, 59.1M elements identical) while osmium's derived OSC loses 1243 delete directives. extract has expected differences in relation inclusion criteria across all three strategies (99.99% node/way match; smart: pbfhogg includes more way-referenced nodes, osmium includes more relations). diff has a 14-element discrepancy out of 59.1M due to different version comparison semantics.

System: plantasjen — AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD (input/output) + HDD (build artifacts), Linux 6.18.

Measured with `dev bench`. Cross-validated with `dev verify`.

## O_DIRECT I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache and evicting useful data from co-resident processes. The `linux-direct-io` feature adds O_DIRECT read and write paths that bypass the page cache entirely.

```toml
[dependencies]
pbfhogg = { version = "0.1", features = ["linux-direct-io"] }
```

CLI usage (all commands support `--direct-io`):

```
pbfhogg merge base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
pbfhogg extract input.osm.pbf -o output.osm.pbf -b 12.4,55.6,12.7,55.8 --direct-io
pbfhogg sort input.osm.pbf -o output.osm.pbf --direct-io
```

Library usage:

```rust
use pbfhogg::writer::{PbfWriter, Compression};
use pbfhogg::{BlobReader, ElementReader};

// O_DIRECT reads
let reader = BlobReader::open("input.osm.pbf", true)?;
let reader = ElementReader::open("input.osm.pbf", true)?;

// O_DIRECT writes (sync)
let writer = PbfWriter::to_path_direct(path, Compression::default())?;

// O_DIRECT writes (pipelined, parallel compression)
let writer = PbfWriter::to_path_pipelined_direct(path, compression, &header_bytes)?;
```

O_DIRECT requires a real filesystem (not tmpfs). Wall time is unchanged at country scale (CPU-bound on zlib compression) — the benefit is cache hygiene at planet scale.

## io_uring I/O

The `linux-io-uring` feature replaces the synchronous writer thread with an io_uring submission loop using `WriteFixed` and pre-registered page-aligned buffers.

```toml
[dependencies]
pbfhogg = { version = "0.1", features = ["linux-io-uring"] }
```

CLI usage:

```
pbfhogg merge base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

Library usage:

```rust
use pbfhogg::writer::{PbfWriter, Compression};

let writer = PbfWriter::to_path_pipelined_uring(path, Compression::None, &header_bytes)?;
```

Requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK` (16 MB for the default 64-buffer pool). If the limit is too low, the error message will suggest `ulimit -l unlimited`.

Note: with the current single-pass merge pipeline (reader thread + passthrough coalescing), the buffered writer keeps up with io_uring at all tested scales. io_uring may still help at planet scale (75 GB+) where the file far exceeds page cache.

## Compression

All write commands support `--compression` to control blob compression:

```
pbfhogg merge base.osm.pbf changes.osc.gz -o output.osm.pbf --compression none
pbfhogg sort input.osm.pbf -o output.osm.pbf --compression zstd
pbfhogg cat input.osm.pbf -o output.osm.pbf --compression zlib:9
```

| Value | Description |
|-------|-------------|
| `none` | No compression. Fastest writes, largest files. Ideal for intermediate files or erofs storage (where the filesystem handles compression). |
| `zlib` | Zlib level 6 (default). Standard PBF compression, compatible with all tools. |
| `zlib:LEVEL` | Zlib with explicit level (0-9). Higher = smaller + slower. |
| `zstd` | Zstandard level 3. Better compression ratio and faster decompression than zlib. |
| `zstd:LEVEL` | Zstandard with explicit level. |

## BlobHeader extensions

pbfhogg embeds additional metadata in BlobHeader fields that standard PBF readers silently skip (per protobuf wire format rules for unknown fields).

**Field 2 (indexdata)**: 42-byte fixed-size blob containing element type (`ElemKind`), ID range (min/max), and spatial bounding box (decimicrodegree `i32` coordinates). Used by merge and sort for O(1) blob classification without decompression.

**Field 4 (tagdata)**: Variable-length blob containing the set of unique tag key strings present in the blob. Wire format: version byte (`0x01`) + key count (`u16` LE) + repeated `[key_len (u16 LE) + key_bytes]`. Used by `tags-filter` and any filtered read to skip decompression of blobs that provably lack required tag keys. Blobs without tagdata (files from other tools) always pass the filter (conservative).

Both fields are written automatically by `PbfWriter` and preserved during sort/merge passthrough. No `optional_features` header declaration is added — these are transparent extensions.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
