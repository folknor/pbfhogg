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
mod error;

#[allow(clippy::all, clippy::pedantic, clippy::restriction)]
pub(crate) mod proto {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}

// Item-level re-exports (public API: `use pbfhogg::Element`, etc.)
pub use read::blob::*;
pub use read::block::*;
pub use read::dense::*;
pub use read::elements::*;
pub use read::indexed::*;
pub use read::mmap_blob::*;
pub use read::reader::*;
pub use error::{BlobError, Error, ErrorKind, Result};

// Module-level re-exports (preserves `crate::blob`, `crate::block_builder`, etc.)
pub use read::{blob, block, dense, elements, indexed, mmap_blob, reader};
pub use write::{block_builder, writer};
pub use commands::{
    cat, check_refs, derive_changes, extract, fileinfo, getid, merge, sort, tags_count,
    tags_filter,
};
