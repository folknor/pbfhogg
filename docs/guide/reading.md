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

let f = std::fs::File::open("crates/lib/tests/test.osm.pbf")?;
let buf_reader = std::io::BufReader::new(f);
let reader = ElementReader::new(buf_reader)?;
# Ok::<(), std::io::Error>(())
```

### Page cache

Readers leave the page cache alone by default: a file you read stays warm for a second pass, for another reader, and for other processes on the host. For a one-pass read of a file larger than RAM, `evict_page_cache(true)` on a `BlobReader` or `ElementReader`, or `ElementReader::from_path_with_eviction(path, true)`, drops the file's pages behind the read head instead, so the read does not displace everything else in the cache. On `ElementReader` prefer `from_path_with_eviction`, which decides before the header blob is read. O_DIRECT readers have no cached pages to drop.

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path_with_eviction("crates/lib/tests/test.osm.pbf", true)?;
# drop(reader);
# Ok::<(), std::io::Error>(())
```

### Header access

The PBF header is available immediately after construction:

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;

if reader.header().is_sorted() {
    println!("PBF is sorted by type then ID");
}

if let Some(bbox) = reader.header().bbox() {
    println!("Bounding box: {},{},{},{}", bbox.left, bbox.bottom, bbox.right, bbox.top);
}
# Ok::<(), std::io::Error>(())
```

`header().is_sorted()` returns `true` when the PBF declares `Sort.Type_then_ID`. The readers do not check the claim: elements are delivered in file order whatever the flag says.

## Read modes

| Method | Order | Use case |
|--------|-------|----------|
| `for_each` | File order | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Fastest ordered read - parallel decompression overlapping I/O |
| `for_each_block_pipelined` | File order | Consumers that need owned `PrimitiveBlock`s for parallel per-block processing |
| `into_blocks_pipelined` | File order | Iterator interface - early exit, zipping two files |
| `par_map_reduce` | Arbitrary | Aggregation (counts, statistics) where order doesn't matter |

### for_each - sequential

Decodes on the calling thread with no background I/O. Simplest interface, but much slower than pipelined on large files. Good for correctness baselines and small files.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;
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

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;
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

Distributes blobs across dedicated worker threads (not the host's `Executor`) in unspecified order. Best for aggregation where order does not matter. Takes three closures: map, identity, and reduce.

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;
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

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;
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

Returns an `Iterator<Item = Result<PrimitiveBlock>>` backed by a background pipeline thread (or `ErrorKind::Sealed`, starting nothing, when the reader's invocation has been sealed). Supports early exit, zipping two files, and interleaving work. Requires `R: 'static` (`ElementReader<FileReader>` from `from_path` satisfies this).

Dropping the iterator early closes its channel and joins the background thread, so the pipeline normally stops instead of reading the rest of the file. The drop waits for that thread: it cannot interrupt a read of the `Read` you supplied, so a read that never returns keeps the drop from returning. Making the stream progress is the caller's obligation. Dropping does not cancel the invocation, and `Invocation::cancel` does not interrupt an ongoing read either.

Cancellation is decided at your position in the iteration, not the pipeline's: once the invocation is cancelled, the next `next()` returns `ErrorKind::Cancelled` without waiting, whatever the pipeline had already read ahead (a queued error included), and the iteration ends. A `next()` already waiting when the cancel lands returns `Cancelled` in place of the block it receives. Any error item ends the iteration.

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;

for block_result in reader.into_blocks_pipelined()? {
    let block = block_result?;
    println!("Block with {} elements", block.elements().count());
}
# Ok::<(), std::io::Error>(())
```

### Pipeline tuning

The pipelined methods accept optional tuning via builder methods:

```rust
use pbfhogg::ElementReader;

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?
    .decode_threads(4)    // override decode pool size (default: sized from the host's resources)
    .read_ahead(32)       // raw blobs buffered between I/O and decode (default: 16)
    .decode_ahead(64);    // decoded blocks buffered before consumer (default: 32)
# drop(reader);
# Ok::<(), std::io::Error>(())
```

Worker counts for the pipelined methods and `par_map_reduce` are sized from
a `host::Resources` (its `parallelism()`), reserving threads for the I/O
reader and the consumer. A reader opened with a host uses that host's
resources; any other reader probes the current machine, unless you supply
resources yourself with `.resources(Arc<dyn host::Resources>)`, for example
`host::Fixed` to get the same sizing on every machine. The setter is lazy: it
probes nothing itself, and it changes nothing else about the reader (its
invocation, diagnostics, executor and page-cache policy stay as they were).
An explicit `.decode_threads(n)` takes precedence over the resources for
both the pipelined methods and `par_map_reduce`, and the resources are then
not consulted at all.

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

let reader = ElementReader::from_path("crates/lib/tests/test.osm.pbf")?;
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
            // Inline node locations, one per ref, if the way stores them
            for location in way.node_locations() {
                // ...
            }
        }
        Element::Relation(rel) => {
            println!("Relation {} with {} members", rel.id(), rel.members().count());
            for member in rel.members() {
                // `role()` is a `Result`: a bad string index is an error.
                match member.role() {
                    Ok(role) => println!("  {:?} role={role}", member.id),
                    Err(e) => eprintln!("  {:?} bad role: {e}", member.id),
                }
            }
        }
    }
})?;
# Ok::<(), std::io::Error>(())
```

### Coordinate methods

`Node`, `DenseNode` and the `WayNodeLocation` items of
`Way::node_locations()` provide:

| Method | Return type | Description |
|--------|-------------|-------------|
| `lat()` | `f64` | Latitude in degrees, from the exact value |
| `lon()` | `f64` | Longitude in degrees, from the exact value |
| `decimicro_lat()` | `Result<i32>` | Latitude in decimicrodegrees (10^-7), truncated toward zero; an error if it does not fit `i32` |
| `decimicro_lon()` | `Result<i32>` | Longitude in decimicrodegrees (10^-7); an error if it does not fit `i32` |
| `nano_lat()` | `Result<i64>` | Latitude in nanodegrees (10^-9); an error if it does not fit `i64` |
| `nano_lon()` | `Result<i64>` | Longitude in nanodegrees (10^-9); an error if it does not fit `i64` |
| `nano_lat_exact()` | `i128` | Exact latitude in nanodegrees, never fails |
| `nano_lon_exact()` | `i128` | Exact longitude in nanodegrees, never fails |

The stored value (for dense nodes and way locations, the delta sum, which
wraps modulo 2^64 as in every reader) is scaled by the block's granularity
and offsets exactly. A result the return type cannot hold is an error, never
a wrapped value somewhere else on Earth; the `lat()` / `lon()` degrees never
wrap and never fail. The check is made at the accessor, so code that never
reads a coordinate never pays for it. `milli_timestamp()` still wraps on
overflow; `milli_timestamp_exact()` does not. None of the coordinate
accessors checks the geographic domain; judge `nano_lat_exact()` /
`nano_lon_exact()` against +-90 and +-180 degrees for that.

### Way methods

| Method | Description |
|--------|-------------|
| `id()` | Element ID |
| `tags()` | Iterator over `(key, value)` string pairs |
| `refs()` | Iterator over referenced node IDs (`i64`) |
| `node_locations()` | Iterator over `WayNodeLocation` items in `refs()` order when the way stores inline coordinates; read from the payload, not the header (`HeaderBlock::has_locations_on_ways()` reports the declaration) |

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

let mut reader = BlobReader::from_path("crates/lib/tests/test.osm.pbf")?;

for blob in &mut reader {
    let blob = blob?;
    if blob.get_type() == BlobType::OsmData {
        let block = blob.to_primitiveblock()?;
        println!("Block with {} elements", block.elements().count());
    }
}
# Ok::<(), std::io::Error>(())
```

`BlobReader::open(path, true)` opens with O_DIRECT, same as `ElementReader::open`.

### WayMembers-v1 / SharedNodePins-v1 (opt-in)

`add-locations-to-ways --inject-prepass` writes two private wire extensions
for downstream reuse: a per-way-blob bitmap in BlobHeader field 5
(`pbfhogg.WayMembers-v1`) and a per-way bitmap in `Way` field 20
(`pbfhogg.SharedNodePins-v1`). Reading them back requires opting in - way-member
parsing is disabled by default to avoid allocating metadata unused by normal
reads:

```rust
use pbfhogg::{BlobReader, BlobDecode, Element};

let mut reader = BlobReader::from_path("crates/lib/tests/test.osm.pbf")?;
reader.set_parse_waymembers(true);

for blob in &mut reader {
    let blob = blob?;
    // Raw bitmap + encoded way count, or None if absent/disabled/malformed.
    let _bitmap = blob.way_members();
    let _count = blob.way_member_count();

    if let BlobDecode::OsmData(block) = blob.decode()? {
        for element in block.elements() {
            if let Element::Way(way) = element {
                let _pins = way.shared_node_pins(); // raw field-20 bitmap
            }
        }
    }
}
# Ok::<(), std::io::Error>(())
```

Both accessors return raw bitmap bytes, not a typed structure - this is
low-level plumbing for tools that want to reuse pbfhogg's own prepass rather
than a general-purpose API. Most library consumers will never need it.
