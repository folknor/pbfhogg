pbfhogg
=======

Fast OpenStreetMap PBF reader and writer for Rust.

Originally a fork of [osmpbf](https://github.com/b-r-u/osmpbf/), extended with PBF writing, pipelined parallel decoding, memory-mapped reading, and blob passthrough for efficient merge workflows.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `PbfWriter` and `BlockBuilder` — dense node packing, delta encoding, configurable compression (none, zlib, zstd)
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

## Read modes

| Method | Order | Sorted guarantee | Use case |
|--------|-------|-----------------|----------|
| `for_each` | File order | Yes — nodes arrive in ascending ID order when `header().is_sorted()` is `true` | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Yes — same guarantee, with parallel decompression overlapping I/O | Fastest ordered read (production hot path) |
| `for_each_block_pipelined` | File order | Yes — blocks arrive in file order, consumer iterates elements | Consumers that need parallel processing per block (owned `PrimitiveBlock`) |
| `par_map_reduce` | Arbitrary | No — elements are distributed across rayon workers in unspecified order | Aggregation (counts, statistics) where order doesn't matter |

`for_each_block_pipelined` delivers owned `PrimitiveBlock`s instead of individual elements. The consumer can send blocks to other threads for parallel processing, enabling overlapped I/O + decode + consumer parallelism without blocking the pipeline.

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
```

Extract supports three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), and `--smart` (three passes, completes multipolygon/boundary relations — all member ways and their nodes are included even if outside the region).

Add-locations-to-ways supports `--index-type hash` (default, HashMap) and `--index-type dense` (anonymous mmap, 8 bytes/slot, for planet-scale — ~68 GB physical for 8.5B nodes vs HashMap's ~192 GB).

All write commands accept `--compression` to control blob compression: `none`, `zlib` (default), `zstd`, or with explicit level (`zlib:9`, `zstd:19`).

## Performance

Read throughput — count all 59M elements in Denmark extract (461 MB), best of 3 runs, `zlib-ng`, fat LTO:

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

Write throughput — decode all 59M elements then write through `BlockBuilder` + `PbfWriter` to `/dev/null`:

| Compression | Sync | Pipelined | Notes |
|-------------|------|-----------|-------|
| none | 9.0s | 9.0s | decode + BlockBuilder floor |
| zstd:3 | 11.0s | **9.1s** | pipelined hides compression cost |
| zlib:6 | 17.5s | **9.1s** | 1.9x speedup from parallel compression |

With pipelined writes, all compression modes converge to ~9s — the decode + `BlockBuilder` serialization floor. `Compression::None` on erofs is the target production config.

CLI commands — Denmark extract (483 MB, 59M elements):

| Tool | merge | sort | sort (unsorted) | diff | extract | add-locs |
|------|-------|------|-----------------|------|---------|----------|
| **pbfhogg** | **2.7s** | **2.3s** | **2.8s** | **24s** | 9 / 16 / 21s | 67s |
| osmium 1.19 | 7.2s | 11.6s | 21.3s | 46s | **2 / 3 / 4s** | **13s** |

Merge applies an OSC diff (294 KB, ~4700 changesets). Sort (sorted) reorders an already-sorted PBF (7396 blobs, 100% passthrough). Sort (unsorted) reorders a PBF with ways before nodes (7390 blobs). Extract shows simple / complete-ways / smart strategy. Add-locs is add-locations-to-ways (10.2M output elements, byte-identical output). osmium uses multi-threaded compression; pbfhogg extract and add-locations-to-ways are single-threaded.

All CLI commands are cross-validated against osmium on Denmark (`verify/*.sh`). cat, tags-filter, add-locations-to-ways, and getid produce byte-identical output. derive-changes produces a correct roundtrip (apply derived OSC back to old = new, 59.1M elements identical) while osmium's derived OSC loses 1243 delete directives. extract has expected differences in relation inclusion criteria across all three strategies (99.99% node/way match; smart: pbfhogg includes more way-referenced nodes, osmium includes more relations). diff has a 14-element discrepancy out of 59.1M due to different version comparison semantics.

System: Linux 6.18, Ryzen 9 7950X.

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

With `Compression::None` on fast storage (e.g. erofs), the write pipeline becomes I/O-bound. The `linux-io-uring` feature replaces the synchronous writer thread with an io_uring submission loop using `WriteFixed` and pre-registered page-aligned buffers.

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
