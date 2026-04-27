# pbfhogg

Rust library for reading, writing, and transforming OpenStreetMap PBF files. Designed for planet-scale operations on normal hardware.

Read 59 million elements in 0.31s (parallel) or 1.3s (pipelined, preserving file order). Write them back with pipelined compression in 6.3s. All encoding and decoding is hand-rolled wire-format protobuf - no external protobuf dependencies, no per-element allocation.

Developed on Linux, untested elsewhere. Optional features for O_DIRECT and io_uring are Linux-only.

For the CLI toolkit (`pbfhogg-cli`), see [the CLI crate](https://crates.io/crates/pbfhogg-cli).

## Usage

```toml
[dependencies]
pbfhogg = "0.3"
```

Library users who only need read/write can disable the `commands` feature to skip `serde_json` and `s2` dependencies:

```toml
[dependencies]
pbfhogg = { version = "0.3", default-features = false }
```

For reverse geocoding queries (memory-mapped index reader), enable just the `geocode-reader` feature:

```toml
[dependencies]
pbfhogg = { version = "0.3", default-features = false, features = ["geocode-reader"] }
```

### Reading

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

### Parallel aggregation

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;
let ways = reader.par_map_reduce(
    |element| match element {
        Element::Way(_) => 1,
        _ => 0,
    },
    || 0_u64,
    |a, b| a + b,
)?;
println!("Number of ways: {ways}");
# Ok::<(), std::io::Error>(())
```

### Writing

```rust
use pbfhogg::write::block_builder::{HeaderBuilder, BlockBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
let mut writer = PbfWriter::to_path("output.osm.pbf".as_ref(), Compression::default(), &header_bytes)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

### In-memory writing

For tests or small PBFs, use `PbfWriter::new` with any `Write` impl:

```rust
use pbfhogg::write::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new().sorted().build()?;
let mut buf = std::io::Cursor::new(Vec::new());
let mut writer = PbfWriter::new(&mut buf, Compression::default());
writer.write_header(&header_bytes)?;

let mut bb = BlockBuilder::new();
// ... add elements, write blocks synchronously ...
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

## Read modes

| Method | Order | Use case |
|--------|-------|----------|
| `for_each` | File order | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Fastest ordered read - parallel decompression overlapping I/O |
| `for_each_block_pipelined` | File order | Consumers that need owned `PrimitiveBlock`s for parallel per-block processing |
| `into_blocks_pipelined` | File order | Iterator interface - early exit, zipping two files |
| `par_map_reduce` | Arbitrary | Aggregation (counts, statistics) where order doesn't matter |

`for_each_pipelined` is the production hot path. It uses a 3-stage pipeline (I/O thread → rayon decode pool → reorder buffer) to overlap reading, decompression, and element processing while preserving file order.

`for_each_block_pipelined` and `into_blocks_pipelined` deliver owned `PrimitiveBlock`s that can be sent to other threads, enabling overlapped I/O + decode + consumer parallelism. `into_blocks_pipelined` requires `R: 'static` (`ElementReader<FileReader>` satisfies this).

`HeaderBuilder::from_header(&existing_header)` copies bbox and replication metadata from an existing PBF header - useful for transforms that preserve metadata.

## Features

| Feature | Description |
|---------|-------------|
| `commands` (default) | Enables `check_refs`, `extract`, geocode index builder, and their deps (`serde_json`, `s2`) |
| `geocode-reader` | Enables `geocode_index::Reader` for reverse geocoding queries (depends on `s2`). Included by `commands`. |
| `linux-direct-io` | O_DIRECT read/write paths - bypasses page cache for planet-scale I/O |
| `linux-io-uring` | io_uring writer with registered buffers - 20% faster writes above ~4 GB |

## Compression

`PbfWriter` supports three compression modes via `Compression`:

| Mode | Description |
|------|-------------|
| `Compression::Zlib(level)` | Standard PBF compression (default level 6), compatible with all tools |
| `Compression::Zstd(level)` | Better ratio and faster decompression than zlib |
| `Compression::None` | No compression - fastest writes, ideal for erofs or intermediate files |

Zlib uses `zlib-rs` (pure Rust, no C compiler needed). With pipelined writes (`to_path`), compression is dispatched to rayon and all modes converge to the decode + serialization floor.

## BlobHeader extensions

`PbfWriter` automatically embeds additional metadata in BlobHeader fields that standard PBF readers silently skip (per protobuf wire format rules for unknown fields):

- **Indexdata** (field 2): element type, ID range, and spatial bounding box per blob. Enables O(1) blob classification for merge, sort, and spatial filtering without decompression.
- **Tagdata** (field 4): set of unique tag key strings per blob. Enables skipping decompression of blobs that provably lack required tag keys.

## Correctness

See [CORRECTNESS.md](CORRECTNESS.md) for parser/encoder edge cases and data representation limits accepted by design, and [DEVIATIONS.md](DEVIATIONS.md) for intentional behavioral differences from osmium.

## Acknowledgements

pbfhogg started as a fork of [osmpbf](https://github.com/b-r-u/osmpbf/) by Thomas Brüggemann, which provided the foundation for PBF reading in Rust.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
