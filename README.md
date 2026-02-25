pbfhogg
=======

Fast OpenStreetMap PBF reader and writer for Rust.

Originally a fork of [osmpbf](https://github.com/b-r-u/osmpbf/), extended with PBF writing, pipelined parallel decoding, memory-mapped reading, and blob passthrough for efficient merge workflows.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `PbfWriter` and `BlockBuilder` — dense node packing, delta encoding, zlib compression
- **Memory-mapped reading** via `MmapBlobReader` for zero-copy blob iteration
- **Blob passthrough** (`write_raw`) for copying unmodified blobs during merge/diff operations
- **Configurable compression** — pure Rust zlib (default), system zlib, or zlib-ng

## Usage

```toml
[dependencies]
pbfhogg = "0.1"
```

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;
reader.for_each(|element| {
    if let Element::Way(way) = element {
        // process way
    }
})?;
# Ok::<(), std::io::Error>(())
```

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

Extract supports two strategies: `--simple` (single pass, fast, may have dangling refs) and complete-ways (default, two passes, all way nodes included).

## Performance

Read throughput — count all 59M elements in Denmark extract (483 MB), best of 3 runs, `zlib-ng`:

<!-- BENCH:START -->
| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.30s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| **pbfhogg** | pipelined | **1.6s** | `for_each_pipelined`, preserves file order |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | sequential | 3.1s | `for_each` |
| **pbfhogg** | blobreader | 3.2s | `BlobReader` sequential decode |
| **pbfhogg** | mmap | 3.2s | `MmapBlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat → opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |
<!-- BENCH:END -->

Merge — apply OSC diff (294 KB, ~4700 changesets) to Denmark PBF:

| Tool | Time | Notes |
|------|------|-------|
| **pbfhogg** | **3.0s** | parallel compression + blob passthrough |
| osmium 1.19 | 7.2s | `osmium apply-changes` |

System: Linux 6.18, Ryzen 9 7950X.

Measured with `scripts/bench.sh`. Results are logged to `benchmarks.tsv` for tracking over time.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
