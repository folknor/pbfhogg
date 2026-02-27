pbfhogg
=======

Fast OpenStreetMap PBF reader and writer for Rust.

Originally a fork of [osmpbf](https://github.com/b-r-u/osmpbf/), extended with PBF writing, pipelined parallel decoding, memory-mapped reading, and blob passthrough for efficient merge workflows.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `HeaderBuilder`, `BlockBuilder`, and `PbfWriter` — dense node packing, delta encoding, configurable compression (none, zlib, zstd)
- **Memory-mapped reading** via `MmapBlobReader` for zero-copy blob iteration
- **Blob passthrough** (`write_raw` / `copy_file_range`) for copying unmodified blobs during merge/cat — kernel-space copy eliminates userspace buffer overhead
- **Blob indexdata** — embeds element type + ID range in BlobHeader for fast merge classification without decompression
- **Configurable compression** — zlib (default), zstd, or none; pure Rust zlib, system zlib, or zlib-ng via feature flags
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

Read throughput — count all 59M elements in Denmark extract (461 MB), best of 3 runs, `zlib-ng`, fat LTO (commit `90df51f`):

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

CLI commands — Denmark extract (483 MB, 59M elements, commit `1a3fcd3`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| cat --type way (indexdata) | **1.07s** | 2.22s | **2.1x** |
| tags-filter amenity=restaurant -R | **0.45s** | 1.19s | **2.6x** |
| getid (9 elements) | **0.38s** | 0.83s | **2.2x** |
| tags-count --type way (indexdata) | **0.36s** | 0.59s | **1.6x** |
| tags-filter w/highway=primary -R | **0.45s** | 0.56s | **1.2x** |
| add-locations-to-ways | **11.5s** | 12.1s | **1.1x** |
| merge (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| sort (sorted) | **2.3s** | 11.6s | **5.0x** |
| sort (unsorted) | **2.8s** | 21.3s | **7.6x** |
| extract (simple / complete / smart) | 4.1 / 8.6 / 11.2s | **1.7 / 2.8 / 3.5s** | 0.4x |

Filter commands (cat, tags-filter, tags-count, getid, removeid) use parallel element processing — each rayon thread owns a `BlockBuilder` and processes decoded blocks in parallel, then results are written sequentially. PBFs with blob-level indexdata get an additional boost by skipping decompression of irrelevant blob types. Merge uses blob passthrough (zero decode for unmodified blobs). Sort uses blob-level permutation. Extract is not yet parallelized.

Merge at scale — Germany (4.5 GB, 500M elements, daily diff with 146K changes, 18.4% blobs rewritten). Before = sequential rewrite (commit `d79f673`), after = parallel rewrite (commit `14034c1`):

| Config | before | after | change |
|--------|--------|-------|--------|
| indexdata + zlib | 49.9s | **35.1s** | -30% |
| indexdata + none | 52.3s | **46.4s** | -11% |

The improvement comes from parallel `rewrite_block` — rewriting touched blobs on the rayon pool instead of the main thread. Denmark's 8.5% rewrite fraction is too small to show the effect; at Germany's 18.4% (and planet's ~92%) the main-thread rewrite bottleneck dominates.

Merge with io_uring — North America (18.8 GB, 645K element diff, ~87% blobs passthrough, Linux 6.18, commit `7b65ab7`):

| Config | Buffered | io_uring | Change |
|--------|----------|----------|--------|
| zlib | 43.2s | **32.6s** | **-25%** |
| none | 36.4s | **25.5s** | **-30%** |

At this scale the file exceeds page cache (30 GB RAM), so O_DIRECT + io_uring's linked `ReadFixed` → `WriteFixed` SQE chains for passthrough blobs eliminate both page cache thrashing and per-blob `pread` syscalls. SQ polling (`--sqpoll`) adds no improvement (<1%) — the syscall elimination from the kernel polling thread doesn't matter at this throughput. At Denmark and Japan scale (≤2.3 GB), io_uring adds 3-5% overhead since page cache absorbs everything.

All CLI commands are cross-validated against osmium on Denmark (`verify/*.sh`). cat, tags-filter, add-locations-to-ways, and getid produce byte-identical output. derive-changes produces a correct roundtrip (apply derived OSC back to old = new, 59.1M elements identical) while osmium's derived OSC loses 1243 delete directives. extract has expected differences in relation inclusion criteria across all three strategies (99.99% node/way match; smart: pbfhogg includes more way-referenced nodes, osmium includes more relations). diff has a 14-element discrepancy out of 59.1M due to different version comparison semantics.

System: plantasjen — AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD, Linux 6.18.

Measured with `scripts/bench.sh`. Cross-validated with `verify/*.sh`.

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

With `Compression::None` on fast storage (e.g. erofs), the write pipeline becomes I/O-bound. The `linux-io-uring` feature replaces the synchronous writer thread with an io_uring submission loop using `WriteFixed` and pre-registered page-aligned buffers. Passthrough blobs use linked `ReadFixed` → `WriteFixed` SQE chains — fully async file-to-file copy through the ring with no userspace syscalls beyond `io_uring_enter`.

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

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
