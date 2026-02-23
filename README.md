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

## Performance

Read throughput — count all 59M elements in Denmark extract (483 MB), best of 3 runs, `zlib-ng`:

<!-- BENCH:START -->
| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.52s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | pipelined | **2.1s** | `for_each_pipelined`, preserves file order |
| **pbfhogg** | sequential | 5.2s | `for_each` |
| **pbfhogg** | blobreader | 5.4s | `BlobReader` sequential decode |
| **pbfhogg** | mmap | 5.5s | `MmapBlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat → opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |
<!-- BENCH:END -->

Merge — apply OSC diff (294 KB, ~4700 changesets) to Denmark PBF:

| Tool | Time | Notes |
|------|------|-------|
| **pbfhogg** | **5.2s** | blob passthrough for unaffected blocks |
| osmium 1.19 | 7.2s | `osmium apply-changes` |

System: Linux 6.18, Ryzen 9 7950X.

Measured with `scripts/bench.sh`. Results are logged to `benchmarks.tsv` for tracking over time.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
