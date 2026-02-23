# pbfhogg

Rust library for reading and writing OpenStreetMap PBF files. Fork of [osmpbf](https://github.com/b-r-u/osmpbf/) with write support added.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use the scripts below.

## Scripts

Write new scripts in `scripts/` as needed. Follow these conventions:
- `scripts/build.sh` — build release binary
- `scripts/test.sh [args]` — run tests (passes args to `cargo test`)
- `scripts/clippy.sh [args]` — run clippy lints (passes args to `cargo clippy`)
- `scripts/run.sh [args]` — build + run the CLI (passes args to the `pbfhogg` binary)
- `scripts/bench-self.sh [pbf] [runs]` — pbfhogg-only benchmark for iteration (logs to benchmarks-self.tsv)
- `scripts/bench.sh [pbf] [runs]` — full comparison suite (pbfhogg vs osmpbf vs osmium vs planetiler)
- `scripts/bench-planetiler.sh [pbf] [runs]` — planetiler PBF read benchmark only

If you need something these scripts don't cover, write a new script.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

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
