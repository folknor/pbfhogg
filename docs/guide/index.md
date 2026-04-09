# Getting Started

## What is pbfhogg?

pbfhogg is a Rust library and CLI toolkit for reading, writing, and transforming OpenStreetMap PBF files. It's designed for planet-scale operations (80+ GB files) on normal hardware (30 GB RAM).

## Quick Start

### As a library

```toml
[dependencies]
pbfhogg = "0.2"
```

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;
reader.for_each(|element| {
    if let Element::Way(way) = element {
        println!("Way {} has {} refs", way.id(), way.refs().count());
    }
})?;
```

### As a CLI

```sh
cargo install pbfhogg-cli

# Inspect a PBF file
pbfhogg inspect denmark.osm.pbf

# Extract a region
pbfhogg extract denmark.osm.pbf -o copenhagen.osm.pbf -b 12.4,55.6,12.7,55.8

# Apply a daily diff
pbfhogg apply-changes denmark.osm.pbf changes.osc.gz -o updated.osm.pbf
```

## Features

- **Read** PBF files sequentially, in parallel, or with a 3-stage pipelined decoder
- **Write** valid PBF files with dense node packing, delta encoding, and configurable compression
- **Blob passthrough** — unmodified blobs are copied as raw bytes, no decompression needed
- **Blob indexdata** — element type, ID range, and spatial bbox embedded per blob for fast filtering
- **25+ CLI commands** — inspect, sort, extract, merge, diff, tags-filter, getid, and more
- **Cross-validated** against osmium on all commands

## Next Steps

- [Reading PBF Files](./reading) — ElementReader API, read modes, blob filtering
- [Writing PBF Files](./writing) — BlockBuilder, PbfWriter, compression options
- [Indexdata](./indexdata) — what it is, how to generate it, which commands use it
- [Performance](./performance) — benchmarks, optimization tips, planet-scale considerations
