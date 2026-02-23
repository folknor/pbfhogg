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

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
