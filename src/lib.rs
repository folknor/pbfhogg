/*!
A fast reader and writer for the OpenStreetMap PBF file format (\*.osm.pbf).

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
pbfhogg = "0.1"
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
*/

#![recursion_limit = "1024"]

#[cfg(any(
    all(feature = "rust-zlib", feature = "zlib"),
    all(feature = "rust-zlib", feature = "zlib-ng"),
    all(feature = "zlib", feature = "zlib-ng")
))]
std::compile_error!(
    "Multiple zlib features are enabled. Make sure to only activate one zlib feature,\n\
    for example by using these cargo flags: --no-default-features --features zlib-ng"
);

// Module tree
pub mod read;
pub mod write;
pub mod commands;
pub mod osc;
pub(crate) mod blob_index;
mod error;

#[allow(clippy::all, clippy::pedantic, clippy::restriction)]
pub(crate) mod proto {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}

// ---------------------------------------------------------------------------
// Public API re-exports
//
// We use TWO complementary re-export strategies here, and both are required:
//
// 1. **Wildcard item-level re-exports** (`pub use read::blob::*`, etc.)
//    These flatten every public type into the crate root so external consumers
//    get the cleanest possible API surface:
//      use pbfhogg::{Element, BlobReader, PrimitiveBlock};
//
// 2. **Named module-level re-exports** (`pub use read::blob`, etc.)
//    These create short `crate::blob`, `crate::block_builder`, `crate::writer`
//    module paths that are used extensively throughout the crate's own source:
//      - Code imports in commands/*.rs  (e.g. `use crate::block_builder::BlockBuilder`)
//      - Code imports in write/*.rs     (e.g. `use crate::elements::MemberId`)
//      - Code imports in read/*.rs      (e.g. `crate::blob::Blob` in pipeline.rs)
//      - Doc links in read/*.rs         (e.g. `[`PrimitiveBlock`](crate::block::PrimitiveBlock)`)
//    Without these, every internal `use crate::blob::...` would need to become
//    `use crate::read::blob::...`, affecting 15+ files across the crate.
//
// The two strategies do NOT conflict: wildcard re-exports provide `pbfhogg::Blob`
// while module re-exports provide `pbfhogg::blob::Blob`. No public names collide
// across the read sub-modules, so the wildcards merge cleanly.
// ---------------------------------------------------------------------------

// Wildcard re-exports: flat public API (`pbfhogg::Element`, `pbfhogg::BlobReader`, etc.)
pub use read::blob::*;
pub use read::block::*;
pub use read::dense::*;
pub use read::elements::*;
pub use read::indexed::*;
pub use read::mmap_blob::*;
pub use read::reader::*;
pub use blob_index::BlobFilter;
pub use error::{BlobError, Error, ErrorKind, Result};

// Module re-exports: short internal paths (`crate::blob`, `crate::block_builder`, etc.)
// Required by imports and doc links in commands/, read/, and write/ modules.
pub use read::{blob, block, dense, elements, file_reader, indexed, mmap_blob, reader};
pub use write::{block_builder, file_writer, writer};
pub use commands::{
    add_locations_to_ways, cat, derive_changes, diff, fileinfo, getid, merge,
    node_stats, sort, tags_count, tags_filter,
};
#[cfg(feature = "commands")]
pub use commands::{check_refs, extract};
