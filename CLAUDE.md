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
- `scripts/bench-self.sh [pbf] [runs]` — pbfhogg-only read benchmark (logs to benchmarks/benchmarks-self.tsv)
- `scripts/bench-self-write.sh [pbf] [runs]` — pbfhogg-only write benchmark (logs to benchmarks/benchmarks-self-write.tsv)
- `scripts/bench.sh [pbf] [runs]` — full comparison suite (pbfhogg vs osmpbf vs osmium vs planetiler)
- `scripts/bench-planetiler.sh [pbf] [runs]` — planetiler PBF read benchmark only
- `scripts/bench-commands.sh <cmd> [pbf] [runs]` — CLI command benchmark vs osmium (logs to benchmarks/benchmarks-commands.tsv). Commands: cat-way, cat-relation, tags-count, tags-count-way, tags-filter-way, tags-filter-amenity, tags-filter-twopass, getid, removeid, add-locations-to-ways, extract-simple, extract-complete, extract-smart, node-stats, all
- `scripts/run-hotpath.sh` — hotpath profiling (pipelined read + decode/write + merge, fixed dataset)
- `scripts/run-hotpath-alloc.sh` — hotpath allocation profiling (same commands, fixed dataset)

Bench scripts build internally — no need to run `build.sh` first.
If you need something these scripts don't cover, write a new script.

## Indexdata PBFs

To generate a PBF with blob-level indexdata (required for fast passthrough merges), use `cat`:
```
scripts/run.sh cat input.osm.pbf --type node,way,relation -o output-with-indexdata.osm.pbf
```
There is no `--add-indexdata` flag — `cat` embeds indexdata automatically when writing.

## Verify scripts

Cross-validation scripts live in `verify/`. Each compares pbfhogg output against osmium (and other tools where applicable) on real PBF data. They build pbfhogg internally.
- `verify/merge.sh [base.pbf] [changes.osc.gz]` — merge vs osmium/osmosis/osmconvert
- `verify/sort.sh [input.pbf]` — sort vs osmium sort
- `verify/cat.sh [input.pbf]` — cat with type filters vs osmium cat
- `verify/extract.sh [input.pbf]` — bbox extract (simple + complete-ways) vs osmium extract
- `verify/derive-changes.sh [old.pbf] [changes.osc.gz]` — derive-changes roundtrip vs osmium
- `verify/diff.sh [old.pbf] [changes.osc.gz]` — diff summary vs osmium diff
- `verify/add-locations-to-ways.sh [input.pbf]` — add-locations-to-ways vs osmium
- `verify/tags-filter.sh [input.pbf]` — tags-filter with 3 expressions vs osmium
- `verify/getid-removeid.sh [input.pbf]` — getid vs osmium getid, removeid complement test
- `verify/check-refs.sh [input.pbf]` — check-refs vs osmium check-refs

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

## Workspace

The repo is a Cargo workspace with two packages:
- **`pbfhogg`** (root) — library crate. Read/write API, commands.
- **`pbfhogg-cli`** (`cli/`) — binary crate. CLI dispatch via clap. Produces the `pbfhogg` binary.

Library users who only need read/write can depend on `pbfhogg` with `default-features = false, features = ["rust-zlib"]` to skip the `commands` feature (avoids `serde_json` and `roaring` deps used by `extract` and `check_refs`).

## Architecture

**Read path:** `BlobReader` (blob.rs) -> `PrimitiveBlock` (block.rs) -> `Element` (elements.rs)
- `ElementReader` (reader.rs): high-level sequential/parallel/pipelined iteration. Parses the PBF header eagerly at construction — `header()` returns `&HeaderBlock` with metadata including `is_sorted()` (Sort.Type_then_ID). Debug builds assert monotonic node IDs when sorted. `for_each_block_pipelined` delivers owned `PrimitiveBlock`s for consumers that need to send blocks to other threads for parallel processing. `into_blocks_pipelined` returns an `Iterator<Item = Result<PrimitiveBlock>>` for loop control (early exit, zipping two files); requires `R: 'static`.
- `MmapBlobReader` (mmap_blob.rs): zero-copy memory-mapped reading
- `IndexedReader` (indexed.rs): seekable reader with blob-level index for filtered queries
- `pipeline.rs`: 3-stage pipelined decoder (IO thread -> rayon pool -> reorder buffer)

**Write path:** `BlockBuilder` (block_builder.rs) -> `PbfWriter` (writer.rs)
- `BlockBuilder`: accumulates nodes/ways/relations, handles string table, delta encoding, dense packing. Max 8000 entities/block. One element type per block. All element types use direct wire-format encoding via reusable scratch buffers (`wire.rs` primitives).
- `wire.rs`: write-side protobuf encoding primitives (varint, zigzag, field encoders, packed repeated fields). Mirrors the read-side `src/read/wire.rs` decoding primitives.
- `PbfWriter`: blob framing, compression (zlib/zstd/none), raw passthrough for merges. Sync mode (`to_path`) or pipelined mode (`to_path_pipelined`) with parallel compression via rayon + reorder buffer. O_DIRECT variants (`to_path_direct`, `to_path_pipelined_direct`) bypass page cache. io_uring variant (`to_path_pipelined_uring`) uses registered buffers + WriteFixed for I/O-bound workloads.
- `uring_writer.rs`: io_uring writer thread — `AlignedBufferPool` (64×256KB registered buffers), `UringState` (buffered accumulation + WriteFixed submission + CQE reaping)

## Conventions

- All performance numbers (timings, allocations, throughput) in markdown files must include the git commit hash and hostname where the measurement was taken. Benchmark TSV files record this automatically via the bench scripts.
- Strict clippy lints enforced (see `[workspace.lints.clippy]` in Cargo.toml) -- notably `unwrap_used = "deny"` and `cognitive_complexity = "deny"`
- Coordinates use decimicrodegrees (10^-7 degrees) for node I/O in BlockBuilder
- Error types in `error.rs` follow the `csv` crate pattern (boxed ErrorKind). `MissingHeader` error if a PBF doesn't start with an OsmHeader blob.
- Tests live in `tests/` (roundtrip.rs, roundtrip_real.rs) and inline in blob.rs/indexed.rs

## Features (library crate)

- `rust-zlib` (default): pure Rust zlib via flate2
- `zlib`: system zlib
- `zlib-ng`: zlib-ng (mutually exclusive with above)
- `commands` (default): enables `check_refs`, `extract`, and their deps (`roaring`, `serde_json`)
- `linux-direct-io`: O_DIRECT read/write paths (bypasses page cache, requires `libc`)
- `linux-io-uring`: io_uring writer thread (requires `io-uring` + `libc`, Linux 5.1+, sufficient `RLIMIT_MEMLOCK`)
