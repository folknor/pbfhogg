# pbfhogg

Rust library for reading and writing OpenStreetMap PBF files. Fork of [osmpbf](https://github.com/b-r-u/osmpbf/) with write support added.

## Build

```sh
cargo build
cargo test
cargo run --example gen_test_pbf   # regenerate tests/test.osm.pbf
```

Edition 2024, MSRV not pinned. Protobuf codegen runs at build time via `build.rs`.

## Architecture

**Read path:** `BlobReader` (blob.rs) -> `PrimitiveBlock` (block.rs) -> `Element` (elements.rs)
- `ElementReader` (reader.rs): high-level sequential/parallel/pipelined iteration
- `MmapBlobReader` (mmap_blob.rs): zero-copy memory-mapped reading
- `IndexedReader` (indexed.rs): seekable reader with blob-level index for filtered queries
- `pipeline.rs`: 3-stage pipelined decoder (IO thread -> rayon pool -> reorder buffer)

**Write path:** `BlockBuilder` (block_builder.rs) -> `PbfWriter` (writer.rs)
- `BlockBuilder`: accumulates nodes/ways/relations, handles string table, delta encoding, dense packing. Max 8000 entities/block. One element type per block.
- `PbfWriter`: blob framing, zlib compression, raw passthrough for merges

**Proto:** `src/proto/{fileformat,osmformat}.proto` compiled by `protobuf-codegen` in `build.rs`

## Conventions

- Strict clippy lints enforced (see `[lints.clippy]` in Cargo.toml) -- notably `unwrap_used = "deny"` and `cognitive_complexity = "deny"`
- Coordinates use decimicrodegrees (10^-7 degrees) for node I/O in BlockBuilder
- `pub(crate) mod proto` is `#[allow(clippy::all)]` (generated code)
- Error types in `error.rs` follow the `csv` crate pattern (boxed ErrorKind)
- Tests live in `tests/` (roundtrip.rs, roundtrip_real.rs) and inline in blob.rs/indexed.rs

## Features

- `rust-zlib` (default): pure Rust zlib via flate2
- `zlib`: system zlib
- `zlib-ng`: zlib-ng (mutually exclusive)
