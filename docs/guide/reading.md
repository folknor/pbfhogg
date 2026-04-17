# Reading PBF Files

## ElementReader

`ElementReader` is the main entry point for reading PBF files. It parses the header eagerly at construction and provides several read modes for iterating over elements.

### Opening a file

```rust
use pbfhogg::{ElementReader, Element};

// Standard buffered I/O
let reader = ElementReader::from_path("input.osm.pbf")?;
# Ok::<(), std::io::Error>(())
```

With O_DIRECT (bypasses page cache, useful at planet scale):

```rust
use pbfhogg::ElementReader;

// Second argument: true = O_DIRECT, false = buffered
let reader = ElementReader::open("input.osm.pbf", true)?;
# Ok::<(), std::io::Error>(())
```

From any `Read + Send` implementation:

```rust
use pbfhogg::ElementReader;

let f = std::fs::File::open("tests/test.osm.pbf")?;
let buf_reader = std::io::BufReader::new(f);
let reader = ElementReader::new(buf_reader)?;
# Ok::<(), std::io::Error>(())
```

### Header access

The PBF header is available immediately after construction:

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("tests/test.osm.pbf")?;

if reader.header().is_sorted() {
    println!("PBF is sorted by type then ID");
}

if let Some(bbox) = reader.header().bbox() {
    println!("Bounding box: {},{},{},{}", bbox.left, bbox.bottom, bbox.right, bbox.top);
}
# Ok::<(), std::io::Error>(())
```

`header().is_sorted()` returns `true` when the PBF declares `Sort.Type_then_ID`. In debug builds, `for_each` and `for_each_pipelined` assert that node IDs are monotonically increasing when the sorted flag is set.

## Read modes

| Method | Order | Use case |
|--------|-------|----------|
| `for_each` | File order | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Fastest ordered read - parallel decompression overlapping I/O |
| `for_each_block_pipelined` | File order | Consumers that need owned `PrimitiveBlock`s for parallel per-block processing |
| `into_blocks_pipelined` | File order | Iterator interface - early exit, zipping two files |
| `par_map_reduce` | Arbitrary | Aggregation (counts, statistics) where order doesn't matter |

### for_each - sequential

Decodes on the calling thread with no background I/O. Simplest interface, but 6x slower than pipelined on large files. Good for correctness baselines and small files.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let mut ways = 0_u64;

reader.for_each(|element| {
    if let Element::Way(_) = element {
        ways += 1;
    }
})?;

println!("Number of ways: {ways}");
# Ok::<(), std::io::Error>(())
```

### for_each_pipelined - fastest ordered read

Uses a 3-stage pipeline (I/O thread, rayon decode pool, reorder buffer) to overlap reading, decompression, and element processing while preserving file order. Same `FnMut` signature as `for_each`. This is the production hot path.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let mut node_count = 0_u64;

reader.for_each_pipelined(|element| {
    match element {
        Element::Node(_) | Element::DenseNode(_) => node_count += 1,
        _ => {}
    }
})?;

println!("Nodes: {node_count}");
# Ok::<(), std::io::Error>(())
```

### par_map_reduce - parallel aggregation

Distributes blobs across rayon workers in unspecified order. Best for aggregation where order does not matter. Takes three closures: map, identity, and reduce.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let ways = reader.par_map_reduce(
    |element| match element {
        Element::Way(_) => 1_u64,
        _ => 0,
    },
    || 0_u64,
    |a, b| a + b,
)?;

println!("Number of ways: {ways}");
# Ok::<(), std::io::Error>(())
```

### for_each_block_pipelined - block-level access

Delivers owned `PrimitiveBlock`s in file order. The consumer can send blocks to other threads for parallel processing, enabling overlapped I/O + decode + consumer parallelism without blocking the pipeline.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let mut total = 0_u64;

reader.for_each_block_pipelined(|block| {
    block.for_each_element(|element| {
        total += 1;
    });
    Ok(())
})?;

println!("Total elements: {total}");
# Ok::<(), std::io::Error>(())
```

### into_blocks_pipelined - iterator interface

Returns an `Iterator<Item = Result<PrimitiveBlock>>` backed by a background pipeline thread. Supports early exit, zipping two files, and interleaving work. Requires `R: 'static` (`ElementReader<FileReader>` from `from_path` satisfies this).

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("tests/test.osm.pbf")?;

for block_result in reader.into_blocks_pipelined() {
    let block = block_result?;
    println!("Block with {} elements", block.elements().count());
}
# Ok::<(), std::io::Error>(())
```

### Pipeline tuning

The pipelined methods accept optional tuning via builder methods:

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("tests/test.osm.pbf")?
    .decode_threads(4)    // override decode pool size (default: available_parallelism - 2)
    .read_ahead(32)       // raw blobs buffered between I/O and decode (default: 16)
    .decode_ahead(64);    // decoded blocks buffered before consumer (default: 32)
# drop(reader);
# Ok::<(), std::io::Error>(())
```

## Element types

PBF elements are represented by the `Element` enum:

```rust
use pbfhogg::Element;
```

| Variant | Description |
|---------|-------------|
| `Element::Node(node)` | A node with coordinates and tags |
| `Element::DenseNode(dense_node)` | Same as Node but with a different in-memory representation (dense packing) |
| `Element::Way(way)` | A way with node refs, tags, and optional node locations |
| `Element::Relation(relation)` | A relation with typed members, tags, and roles |

In practice, most PBF files use dense nodes exclusively. When matching nodes, always match both variants:

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
reader.for_each(|element| {
    match element {
        Element::Node(node) => {
            println!("Node {} at ({}, {})", node.id(), node.lat(), node.lon());
            for (key, value) in node.tags() {
                println!("  {} = {}", key, value);
            }
        }
        Element::DenseNode(node) => {
            println!("DenseNode {} at ({}, {})", node.id(), node.lat(), node.lon());
            for (key, value) in node.tags() {
                println!("  {} = {}", key, value);
            }
        }
        Element::Way(way) => {
            println!("Way {} with {} refs", way.id(), way.refs().count());
            for (key, value) in way.tags() {
                println!("  {} = {}", key, value);
            }
            // Node refs (IDs of referenced nodes)
            for node_id in way.refs() {
                // ...
            }
            // Node locations (if the PBF has LocationsOnWays)
            for (lat, lon) in way.node_locations() {
                // ...
            }
        }
        Element::Relation(rel) => {
            println!("Relation {} with {} members", rel.id(), rel.members().count());
            for member in rel.members() {
                println!("  {:?} role={}", member.member_id, member.role());
            }
        }
    }
})?;
# Ok::<(), std::io::Error>(())
```

### Coordinate methods

Both `Node` and `DenseNode` provide:

| Method | Return type | Description |
|--------|-------------|-------------|
| `lat()` | `f64` | Latitude in degrees |
| `lon()` | `f64` | Longitude in degrees |
| `decimicro_lat()` | `i32` | Latitude in decimicrodegrees (10^-7) |
| `decimicro_lon()` | `i32` | Longitude in decimicrodegrees (10^-7) |
| `nano_lat()` | `i64` | Latitude in nanodegrees (10^-9) |
| `nano_lon()` | `i64` | Longitude in nanodegrees (10^-9) |

### Way methods

| Method | Description |
|--------|-------------|
| `id()` | Element ID |
| `tags()` | Iterator over `(key, value)` string pairs |
| `refs()` | Iterator over referenced node IDs (`i64`) |
| `node_locations()` | Iterator over `(lat, lon)` pairs if the PBF has LocationsOnWays |

### Relation methods

| Method | Description |
|--------|-------------|
| `id()` | Element ID |
| `tags()` | Iterator over `(key, value)` string pairs |
| `members()` | Iterator over relation members with `member_id` (typed: `MemberId::Node`, `MemberId::Way`, `MemberId::Relation`) and `role()` |

## BlobReader - low-level access

`BlobReader` provides blob-level access to the PBF file. Each blob is a compressed chunk containing a `PrimitiveBlock` (typically 8000 elements). Most library consumers should use `ElementReader` instead - `BlobReader` is for when you need raw blob frames, file seeking, or multi-pass scanning.

```rust
use pbfhogg::{BlobReader, BlobType};

let mut reader = BlobReader::from_path("tests/test.osm.pbf")?;

for blob in &mut reader {
    let blob = blob?;
    if blob.blob_type() == BlobType::OsmData {
        let block = blob.to_primitiveblock()?;
        println!("Block with {} elements", block.elements().count());
    }
}
# Ok::<(), std::io::Error>(())
```

`BlobReader::open(path, true)` opens with O_DIRECT, same as `ElementReader::open`.

## IndexedReader - filtered reads

`IndexedReader` builds an in-memory index of blob positions and ID ranges, then seeks directly to relevant blobs. Useful when you need specific elements by ID from a large file.

```rust
use pbfhogg::IndexedReader;
use std::collections::BTreeSet;

let mut reader = IndexedReader::from_path("tests/test.osm.pbf")?;

// Find ways that reference specific node IDs
let node_ids: BTreeSet<i64> = [1, 2, 3].into_iter().collect();
let ways = reader.ways_with_node_ids(&node_ids)?;
# Ok::<(), std::io::Error>(())
```

`IndexedReader` is used internally by commands like `getid` and `add-locations-to-ways` for efficient targeted reads.
