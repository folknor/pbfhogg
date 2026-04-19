/*!
A fast reader and writer for the OpenStreetMap PBF file format (\*.osm.pbf).

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
pbfhogg = "0.2"
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
# Ok::<(), std::io::Error>(())
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
# Ok::<(), std::io::Error>(())
```

## Example: Write a PBF file

Build blocks with [`BlockBuilder`] and write them with [`PbfWriter`]:

```rust,no_run
use pbfhogg::write::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
let mut writer = PbfWriter::to_path(
    "output.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);

// Flush the block to the writer - compression dispatches to rayon
if let Some(block_bytes) = bb.take()? {
    writer.write_primitive_block(block_bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
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
# Ok::<(), std::io::Error>(())
```
*/

#![recursion_limit = "1024"]

// Module tree
pub mod read;
pub mod write;
#[doc(hidden)]
pub mod commands;
pub mod geo;
pub mod geocode_index;  // format is always available; reader requires geocode-reader feature
pub mod osc;
pub(crate) mod blob_meta;
pub mod debug;
mod error;
pub(crate) mod idset;
pub(crate) mod owned;
pub(crate) mod reorder_buffer;
pub(crate) mod scan;
#[doc(hidden)]
pub mod tag_expr;

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
pub use read::blob::{
    Blob, BlobDecode, BlobHeader, BlobReader, BlobReaderSource, BlobType, ByteOffset,
    MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE,
};
pub use read::block::{
    BlockElementsIter, BlockType, GroupIter, GroupNodeIter, GroupRelationIter, GroupWayIter,
    HeaderBBox, HeaderBlock, PrimitiveBlock, PrimitiveGroup,
};
pub use read::dense::{
    DenseNode, DenseNodeInfo, DenseNodeInfoIter, DenseNodeIter, DenseRawTagIter, DenseTagIter,
};
pub use read::elements::{
    Element, Info, MemberId, MemberType, Node, RawTagIter, RelMember, RelMemberIter, Relation,
    TagIter, Way, WayNodeLocation, WayNodeLocationsIter, WayRefIter,
};
pub use read::indexed::{IdRanges, IndexedReader};
pub use read::reader::{ElementReader, PipelinedBlocks};
pub use blob_meta::{BlobBbox, BlobFilter};
pub use error::{BlobError, Error, ErrorKind, Result};

// Module re-exports: short internal paths (`crate::blob`, `crate::block_builder`, etc.)
// Required by imports and doc links in commands/, read/, and write/ modules.
pub use read::{blob, block, dense, elements, indexed, reader};
pub(crate) use read::file_reader;
pub use write::{block_builder, writer};
pub(crate) use write::file_writer;
#[doc(hidden)]
pub use commands::has_indexdata;
#[doc(hidden)]
pub use commands::HeaderOverrides;
#[doc(hidden)]
pub use commands::{
    add_locations_to_ways, apply_changes, cat, diff, getid, getparents, inspect,
    merge_changes, renumber, sort,
    tags_count, tags_filter, time_filter,
};
#[cfg(feature = "commands")]
#[doc(hidden)]
pub use commands::{check, extract};
