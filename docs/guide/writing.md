# Writing PBF Files

## Overview

Writing a PBF file involves three components:

1. **HeaderBuilder** - constructs the PBF header (bounding box, sort flag, replication metadata)
2. **BlockBuilder** - accumulates elements (nodes, ways, relations) and serializes them into `PrimitiveBlock` bytes
3. **PbfWriter** - handles blob framing, compression, and file output

## HeaderBuilder

Every PBF file starts with an `OsmHeader` blob. `HeaderBuilder` constructs it:

```rust
use pbfhogg::write::block_builder::HeaderBuilder;

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)  // left, bottom, right, top
    .sorted()                       // declare Sort.Type_then_ID
    .build()?;
# Ok::<(), std::io::Error>(())
```

### Methods

| Method | Description |
|--------|-------------|
| `new()` | Create a blank header builder |
| `bbox(left, bottom, right, top)` | Set the bounding box (f64 degrees) |
| `sorted()` | Declare elements are sorted by type then ID |
| `source(s)` | Set `source` (field 17), e.g. the URL the data came from |
| `without_source()` | Clear a `source` carried over by `from_header` |
| `from_header(&existing_header)` | Copy bbox, `source`, and replication metadata from an existing `HeaderBlock` |
| `build()` | Serialize to protobuf bytes |

`from_header` is useful for commands that transform data while preserving metadata. It copies bbox, `source`, replication timestamp / sequence number / base URL, and the historical-information flag. It does **not** copy the optional-feature list (call `.sorted()` explicitly if the output is sorted) and it cannot carry unknown or extension fields - a rebuild through `HeaderBuilder` keeps only what the builder models.

```rust
use pbfhogg::ElementReader;
use pbfhogg::write::block_builder::HeaderBuilder;

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let header_bytes = HeaderBuilder::from_header(reader.header())
    .sorted()
    .build()?;
# Ok::<(), std::io::Error>(())
```

## BlockBuilder

`BlockBuilder` accumulates OSM elements and serializes them into `PrimitiveBlock` protobuf bytes. It handles string table construction, delta encoding, and dense node packing automatically. Each block holds up to 8000 entities (matching osmium's limit), and each block contains a single element type.

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();
```

### add_node

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();

// add_node(id, decimicro_lat, decimicro_lon, tags, metadata)
// Coordinates are in decimicrodegrees (10^-7 degrees)
// Copenhagen: 55.6761 N, 12.5683 E
bb.add_node(
    1,
    556_761_000,   // lat * 10^7
    125_683_000,   // lon * 10^7
    [("name", "Copenhagen"), ("place", "city")],
    None,          // optional metadata
);
```

Tags are passed as anything implementing `IntoIterator<Item = (&str, &str)>`. Slices, arrays, and vectors of tuples all work:

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();

// Array of tuples
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);

// Empty tags
bb.add_node(2, 556_000_000, 125_000_000, [], None);

// Vec of tuples
let tags = vec![("highway", "primary"), ("ref", "E47")];
bb.add_node(3, 555_000_000, 124_000_000, tags, None);
```

### add_way

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();

// add_way(id, tags, refs, metadata)
bb.add_way(
    100,
    [("highway", "residential"), ("name", "Main Street")],
    &[1, 2, 3, 4, 1],  // node ID refs (closed way)
    None,
);
```

### add_way_with_locations

For PBFs with LocationsOnWays (embedded node coordinates in ways):

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();

// add_way_with_locations(id, tags, refs, locations, metadata)
bb.add_way_with_locations(
    100,
    [("highway", "residential")],
    &[1, 2, 3],
    &[(556_761_000, 125_683_000), (556_800_000, 125_700_000), (556_850_000, 125_750_000)],
    None,
);
```

### add_relation

```rust
use pbfhogg::write::block_builder::{BlockBuilder, MemberData, MemberId};

let mut bb = BlockBuilder::new();

// add_relation(id, tags, members, metadata)
bb.add_relation(
    200,
    [("type", "multipolygon"), ("landuse", "forest")],
    &[
        MemberData { id: MemberId::Way(100), role: "outer" },
        MemberData { id: MemberId::Way(101), role: "inner" },
    ],
    None,
);
```

### take - flushing blocks

`take()` serializes the accumulated elements and returns the block bytes. Returns `None` if the builder is empty. The builder is reset after `take()` and can be reused.

```rust
use pbfhogg::write::block_builder::BlockBuilder;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);

if let Some(bytes) = bb.take()? {
    // bytes is a &[u8] containing the serialized PrimitiveBlock
    println!("Block size: {} bytes", bytes.len());
}
# Ok::<(), std::io::Error>(())
```

`BlockBuilder` automatically flushes when it hits the 8000-entity limit or when the element type changes (e.g., switching from nodes to ways). Always call `take()` after adding all elements to flush the final partial block.

### Metadata

Optional metadata can be attached to any element:

```rust
use pbfhogg::write::block_builder::{BlockBuilder, Metadata};

let mut bb = BlockBuilder::new();

let meta = Metadata {
    version: 1,
    timestamp: 1700000000,  // seconds since Unix epoch
    changeset: 12345,
    uid: 42,
    user: "mapper",
};

bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], Some(&meta));
```

## PbfWriter

`PbfWriter` handles blob framing, compression, and file output. It has pipelined constructors (`to_path`, `to_path_direct`, `to_path_uring`) and synchronous ones (`to_path_sync`, `new`).

The `to_path*` constructors write the path they are given, in place. To get the guarantees the commands give their outputs - an existing file refused unless replacing it was asked for, a symlink refused, nothing under the final name until the write has succeeded - write through a `pbfhogg::Output`:

```rust
use pbfhogg::{Output, Overwrite};

let output = Output::new("output.osm.pbf", Overwrite::Refuse)?; // checks, stages
let mut writer = PbfWriter::to_path(
    output.path(),            // the staging file beside output.osm.pbf
    Compression::default(),
    &header_bytes,
    Executor::Ambient,
)?;
// ... write blocks ...
writer.flush()?;
output.publish()?;            // renamed into place only now
```

Dropping the `Output` without publishing removes the staging file. The command entry points (`sort::sort`, `cat::cat`, ...) take an `Output` by value and publish it themselves.

### to_path - pipelined parallel compression

The production write path. Compresses blobs in parallel using rayon, with a dedicated writer thread that reorders results back into sequence order.

```rust
use pbfhogg::write::block_builder::{HeaderBuilder, BlockBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::host::Executor;

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;

let mut writer = PbfWriter::to_path(
    "output.osm.pbf".as_ref(),
    Compression::default(),  // zlib level 6
    &header_bytes,
    Executor::Ambient,       // frame on rayon's ambient pool
)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);

if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}

writer.flush()?;
# Ok::<(), std::io::Error>(())
```

After successful writes, `flush()` checks completion and reports deferred errors. The file-backed pipelines and `to_path_sync` also sync the file. A synchronous `PbfWriter::new` only calls the underlying `Write::flush`, which does not guarantee durability. Dropping a writer without flushing is cleanup only and reports nothing, including a missing tail.

### Executor

Every pipelined constructor (`to_path`, `to_path_direct`, `to_path_uring`) takes an `Executor` (`pbfhogg::host::Executor`) naming where the framing and compression work runs. `Executor::Ambient` uses whatever rayon pool the calling thread is in, or rayon's global pool off any pool. `Executor::Pool(Arc<rayon::ThreadPool>)` confines the work to a pool you own:

```rust
use std::sync::Arc;
use pbfhogg::host::Executor;
use pbfhogg::write::block_builder::HeaderBuilder;
use pbfhogg::write::writer::{PbfWriter, Compression};

let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(4).build()?);
let header_bytes = HeaderBuilder::new().build()?;
let mut writer = PbfWriter::to_path(
    "output.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
    Executor::Pool(Arc::clone(&pool)),
)?;
// ... write blocks, then flush
writer.flush()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Three things follow from handing the writer a pool:

- Thread-local framing and decompression scratch persists on whichever pool's threads ran it, for as long as those threads live. Release every owner of the pool, including its writers, to let its workers exit and free that scratch; dropping a pool does not join its workers.
- A panic in the framing work is handled by the pool's `panic_handler`. With no handler, rayon aborts the process; with one, the lost blob surfaces as an error from `flush()`.
- A writer driven from a worker of its executor's pool - even a one-thread pool - waits for its framing work cooperatively and cannot deadlock on it. A writer moved between pools mid-stream under `Ambient` is not covered by that wait.

### to_path_direct - O_DIRECT writes

Bypasses the page cache. Prevents cache pollution at planet scale (80 GB+ output). Requires the `linux-direct-io` feature and a real filesystem (not tmpfs).

```rust
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::write::block_builder::HeaderBuilder;
use pbfhogg::host::Executor;

let header_bytes = HeaderBuilder::new().build()?;
let mut writer = PbfWriter::to_path_direct(
    "output.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
    Executor::Ambient,
)?;
// ... write blocks, then flush
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

### to_path_uring - io_uring writes

Uses io_uring `WriteFixed` with pre-registered page-aligned buffers. 20% faster than buffered writes above ~4 GB input size. Requires the `linux-io-uring` feature and Linux 5.1+ with sufficient `RLIMIT_MEMLOCK`.

```rust
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::write::block_builder::HeaderBuilder;
use pbfhogg::host::Executor;

let header_bytes = HeaderBuilder::new().build()?;
let mut writer = PbfWriter::to_path_uring(
    "output.osm.pbf".as_ref(),
    Compression::None,
    &header_bytes,
    Executor::Ambient,
)?;
// ... write blocks, then flush
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

### to_path_sync - synchronous file output

Compresses and writes on the calling thread into a file it creates, through a 256 KiB buffer it owns. `flush()` writes out the buffer and syncs the file, like the pipelined constructors. Useful when one thread drives many outputs at once, where a pipeline per output would multiply threads. No background threads, no rayon.

```rust
use pbfhogg::write::block_builder::HeaderBuilder;
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new().sorted().build()?;
let mut writer = PbfWriter::to_path_sync("output.osm.pbf".as_ref(), Compression::default())?;
writer.write_header(&header_bytes)?;
// ... write blocks, then flush (which syncs the file)
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

### new - synchronous mode

For tests, small PBFs, or any `Write` implementation (in-memory buffers, network streams). No background threads, no rayon.

```rust
use pbfhogg::write::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new().sorted().build()?;
let mut buf = std::io::Cursor::new(Vec::new());
let mut writer = PbfWriter::new(&mut buf, Compression::default());
writer.write_header(&header_bytes)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Test")], None);
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;

let pbf_bytes = buf.into_inner();
println!("Wrote {} bytes", pbf_bytes.len());
# Ok::<(), std::io::Error>(())
```

Note: with `new()` and `to_path_sync()`, you must call `write_header()` explicitly before writing data blocks. The pipelined constructors write the header automatically.

## Compression

The `Compression` enum controls blob compression:

| Mode | Description |
|------|-------------|
| `Compression::Zlib(level)` | Standard PBF compression. Level 6 is the default, matching osmium. Level 0-9. |
| `Compression::Zstd(level)` | Better ratio and faster decompression than zlib. Not all PBF consumers support zstd yet. |
| `Compression::None` | No compression. Fastest writes, largest files. Ideal for intermediate files or erofs storage. |

`Compression::default()` is `Zlib(6)`.

With pipelined writes (`to_path`), compression is dispatched to rayon and all modes converge to the decode + serialization floor. The choice mainly affects file size and downstream read speed.

Zlib uses `zlib-rs` (pure Rust, no C compiler needed).

## Writer methods

| Method | Description |
|--------|-------------|
| `write_header(&[u8])` | Write the OsmHeader blob (`new` and `to_path_sync` only; the pipelined constructors do this automatically) |
| `write_primitive_block(&[u8])` | Write an OSMData blob from serialized `PrimitiveBlock` bytes |
| `write_raw(&[u8])` | Write pre-framed raw bytes (for blob passthrough without re-compression) |
| `flush()` | Drain the pipeline and finalize the file. Must be called before dropping the writer. |

### Raw passthrough

`write_raw` accepts pre-framed blob bytes (header + compressed data) and passes them through without decompression or re-compression. This is how commands like `apply-changes` and `sort` achieve near-zero overhead for unmodified blobs - they copy the raw bytes from the input file directly into the output.

## Complete example

Reading a PBF, filtering ways, and writing the result:

```rust
use pbfhogg::{ElementReader, Element};
use pbfhogg::write::block_builder::{HeaderBuilder, BlockBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::host::Executor;

let reader = ElementReader::from_path("tests/test.osm.pbf")?;

let header_bytes = HeaderBuilder::from_header(reader.header())
    .sorted()
    .build()?;

let mut writer = PbfWriter::to_path(
    "highways.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
    Executor::Ambient,
)?;

let mut bb = BlockBuilder::new();

reader.for_each(|element| {
    if let Element::Way(way) = element {
        let dominated_by_highway = way.tags().any(|(k, _)| k == "highway");
        if dominated_by_highway {
            let tags: Vec<(&str, &str)> = way.tags().collect();
            let refs: Vec<i64> = way.refs().collect();
            bb.add_way(way.id(), tags, &refs, None);

            // Flush full blocks
            if let Ok(Some(bytes)) = bb.take() {
                let _ = writer.write_primitive_block(bytes);
            }
        }
    }
})?;

// Flush the final partial block
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
```
