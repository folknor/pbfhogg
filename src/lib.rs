/*!
A fast reader and writer for the OpenStreetMap PBF file format (\*.osm.pbf).

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
pbfhogg = "0.5"
```

## Example: Count ways

Here's a simple example that counts all the OpenStreetMap way elements in a
file:

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;
let mut ways = 0_u64;

// Increment the counter by one for each way.
reader.for_each(|element| {
    if let Element::Way(_) = element {
        ways += 1;
    }
})?;

println!("Number of ways: {ways}");
# assert_eq!(ways, 1);
# Ok::<(), pbfhogg::Error>(())
```

## Example: Count ways in parallel

In this second example, we also count the ways but make use of all cores by
decoding the file in parallel:

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("tests/test.osm.pbf")?;

// Count the ways
let ways = reader.par_map_reduce(
    |element| {
        match element {
            Element::Way(_) => 1,
            _ => 0,
        }
    },
    || 0_u64,      // Zero is the identity value for addition
    |a, b| a + b   // Sum the partial results
)?;

println!("Number of ways: {ways}");
# assert_eq!(ways, 1);
# Ok::<(), pbfhogg::Error>(())
```

## Example: Write a PBF file

Build blocks with [`BlockBuilder`] and write them with [`PbfWriter`]:

```rust,no_run
use pbfhogg::write::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::host::Executor;

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
// `Executor::Ambient` frames on rayon's ambient pool; pass
// `Executor::Pool(pool)` to confine the work to a pool of your own.
let mut writer = PbfWriter::to_path(
    "output.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
    Executor::Ambient,
)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);

// Flush the block to the writer - compression dispatches to the executor
if let Some(block_bytes) = bb.take()? {
    writer.write_primitive_block(block_bytes)?;
}
writer.flush()?;
# Ok::<(), pbfhogg::Error>(())
```

## Example: inspecting an error

Every fallible entry point returns [`Error`], which keeps the typed
[`ErrorKind`] and - for failures detected by the framing layer - the byte
offset in the input. Converting to `std::io::Error` (via the [`From`] impl)
boxes both away, so match on the kind instead:

```rust
use pbfhogg::{ElementReader, ErrorKind};

match ElementReader::from_path("does-not-exist.osm.pbf") {
    Ok(_reader) => {}
    Err(e) => {
        if let ErrorKind::Io(io_err) = e.kind() {
            println!("I/O error: {io_err}");
        }
        if let Some(offset) = e.byte_offset() {
            println!("detected at byte {}", offset.0);
        }
    }
}
```

## Example: In-memory writing

For tests or small PBFs, use [`PbfWriter::new`] with any [`Write`](std::io::Write) impl:

```rust,no_run
use pbfhogg::write::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new().sorted().build()?;
let mut buf = std::io::Cursor::new(Vec::new());
let mut writer = PbfWriter::new(&mut buf, Compression::default());
writer.write_header(&header_bytes)?;

let mut bb = BlockBuilder::new();
// ... add elements, write blocks synchronously ...
writer.flush()?;
# Ok::<(), pbfhogg::Error>(())
```
*/

#![recursion_limit = "1024"]
// A no-`commands` build drops the geocode builder and the check / export /
// extract commands, and with them the only consumers of several internals
// (`scan::way`'s geocode tag scanning, `read::columnar`, `read::region_grid`,
// `commands::drain_batch_results`, ...). Those items having no caller in that
// configuration is the configuration working as intended, not dead code: the
// `all` and `consumer` sweeps still build them with their consumers present
// and still fail on genuinely unreachable code. Scoped to the library; the
// binary is never built without `commands`.
#![cfg_attr(not(feature = "commands"), allow(dead_code))]

// Module tree
pub(crate) mod blob_meta;
#[doc(hidden)]
pub mod commands;
pub(crate) mod content;
pub(crate) mod coord_fmt;
pub(crate) mod coord_pack;
// Public because the separate `pbfhogg-cli` crate arms the sidecar marker
// FIFO through `debug::initialize_marker_fifo`; the library never reads
// `BROKKR_MARKER_FIFO` itself.
pub mod debug;
mod error;
pub mod geo;
pub mod geocode_index; // format is always available; reader requires geocode-reader feature
pub(crate) mod idset;
pub mod osc;
pub(crate) mod osm_id;
pub(crate) mod owned;
#[doc(hidden)]
pub mod path_guard;
pub mod read;
pub(crate) mod reorder_buffer;
pub(crate) mod scan;
pub(crate) mod scratch;
#[doc(hidden)]
pub mod tag_expr;
pub mod write;

/// Boxed-error Result alias used by command implementations and lifted
/// command-shared library code. Distinct from [`crate::Result`] (which is
/// over the typed [`crate::Error`]). The boxed flavor is used where
/// callers only display the error and exit, so typed enums add complexity
/// with no matching benefit.
pub(crate) type BoxResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Public API re-exports
//
// 1. **Explicit item-level re-exports** flatten selected types into the crate
//    root so external consumers get a clean API:
//      use pbfhogg::{Element, BlobReader, PrimitiveBlock};
//
// 2. **Named module-level re-exports** create short `crate::blob`,
//    `crate::block_builder`, `crate::writer` paths used throughout the crate.
// ---------------------------------------------------------------------------

// Explicit re-exports: flat public API (`pbfhogg::Element`, `pbfhogg::BlobReader`, etc.)
pub use blob_meta::{BlobBbox, BlobFilter};
pub use error::{BlobError, Error, ErrorKind, Result};
pub use owned::{ParseTypeFilterError, TypeFilter};
pub use read::blob::{
    Blob, BlobDecode, BlobHeader, BlobReader, BlobReaderSource, BlobType, ByteOffset,
    MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE,
};
pub use read::block::{
    BlockElementsIter, BlockType, GroupIter, GroupNodeIter, GroupRelationIter, GroupWayIter,
    HeaderBBox, HeaderBlock, PrimitiveBlock, PrimitiveGroup,
};
pub use read::dense::{
    CheckedDenseTagIter, DenseNode, DenseNodeInfo, DenseNodeInfoIter, DenseNodeIter,
    DenseRawTagIter, DenseTagIter,
};
pub use read::elements::{
    CheckedTagIter, Element, Info, MemberId, MemberType, Node, RawTagIter, RelMember,
    RelMemberIter, Relation, TagIter, Way, WayNodeLocation, WayNodeLocationsIter, WayRefIter,
};
pub use read::reader::{ElementReader, PipelinedBlocks};

// Module re-exports: short internal paths (`crate::blob`, `crate::block_builder`, etc.)
// Required by imports and doc links in commands/, read/, and write/ modules.
#[doc(hidden)]
pub use commands::HeaderOverrides;
#[doc(hidden)]
pub use commands::{
    altw, apply_changes, cat, degrade, diff, digest, getid, getparents, inspect, merge_changes,
    renumber, repack, sort, tags_count, tags_filter, time_filter,
};
#[cfg(feature = "commands")]
#[doc(hidden)]
pub use commands::{check, export, extract};
#[doc(hidden)]
pub use commands::{has_indexdata, has_tagdata};
pub use read::{blob, block, dense, elements, reader};
pub(crate) use read::{file_reader, frame};
pub(crate) use write::file_writer;
pub use write::{block_builder, writer};

pub mod host;
pub mod process;
