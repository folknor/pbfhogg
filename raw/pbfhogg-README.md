<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="pbfhogg-logo-text-dark.svg">
    <img src="pbfhogg-logo-text.svg" width="300" alt="pbfhogg">
  </picture>
  <br>
  <em>Fast OpenStreetMap PBF reader and writer for Rust</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/pbfhogg"><img src="https://img.shields.io/crates/v/pbfhogg?color=fc8d62" alt="crates.io"></a>
  <a href="https://docs.rs/pbfhogg"><img src="https://img.shields.io/docsrs/pbfhogg?label=docs.rs" alt="docs.rs"></a>
  <a href="https://github.com/folknor/pbfhogg/actions"><img src="https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml?label=CI&logo=github" alt="CI"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
  <a href="LICENSE-APACHE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
</p>

---

Originally a fork of [osmpbf](https://github.com/b-r-u/osmpbf/), extended with PBF writing, pipelined parallel decoding, memory-mapped reading, and blob passthrough for efficient merge workflows.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `PbfWriter` and `BlockBuilder` — dense node packing, delta encoding, zlib compression
- **Memory-mapped reading** via `MmapBlobReader` for zero-copy blob iteration
- **Blob passthrough** (`write_raw`) for copying unmodified blobs during merge/diff operations
- **Blob indexdata** — embeds element type + ID range in BlobHeader for fast merge classification without decompression
- **Configurable compression** — pure Rust zlib (default), system zlib, or zlib-ng
- **O_DIRECT I/O** — optional `linux-direct-io` feature bypasses the page cache for planet-scale (80 GB+) reads and writes, preventing cache pollution on the host

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
| **pbfhogg** | **2.8s** | parallel compression + blob passthrough + blob indexdata |
| **pbfhogg** | 3.1s | first merge (no indexdata in input, falls back to decompression) |
| osmium 1.19 | 7.2s | `osmium apply-changes` |

System: Linux 6.18, Ryzen 9 7950X.

Measured with `scripts/bench.sh`. Results are logged to `benchmarks.tsv` for tracking over time.

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

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
