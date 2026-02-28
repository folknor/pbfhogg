# pbfhogg

Rust library for reading and writing OpenStreetMap PBF files. Fork of [osmpbf](https://github.com/b-r-u/osmpbf/) with write support added.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never chain commands with ;. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

## Brokkr tool

Standalone development tool at `~/Programs/brokkr`. Installed via `cargo install --path ~/Programs/brokkr`. Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

- `brokkr check [-- args]` — run clippy + tests. Extra args forwarded to `cargo test` (e.g., `brokkr check -- --ignored`).
- `brokkr env` — show hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `brokkr run [args]` — build release CLI and run with passthrough args (e.g., `brokkr run fileinfo tests/test.osm.pbf`).
- `brokkr bench read [--dataset name] [--pbf path] [--runs N] [--modes list]` — read benchmark (5 modes: sequential, parallel, pipelined, mmap, blobreader). Results stored in SQLite.
- `brokkr bench write [--dataset name] [--pbf path] [--runs N] [--compression list]` — write benchmark (sync + pipelined × compression). Default compressions: none,zlib:6,zstd:3. Results stored in SQLite.
- `brokkr bench merge [--dataset name] [--pbf path] [--osc path] [--runs N] [--uring] [--compression list]` — merge benchmark (I/O modes × compression). Default compressions: zlib,none. `--uring` adds io_uring variants with preflight checks. Results stored in SQLite.
- `brokkr bench commands [command] [--dataset name] [--pbf path] [--runs N]` — CLI command benchmark (19 commands, external timing). Use `all` for full suite. `diff` and `derive-changes` auto-generate a merged PBF (cached in scratch). Results stored in SQLite.
- `brokkr bench extract [--dataset name] [--pbf path] [--runs N] [--bbox bbox] [--strategies list]` — extract strategy benchmark (simple/complete/smart). Default dataset: japan.
- `brokkr bench allocator [--dataset name] [--pbf path] [--runs N]` — allocator comparison (default/jemalloc/mimalloc) via check-refs.
- `brokkr bench blob-filter [--dataset name] [--pbf-indexed path] [--pbf-raw path] [--runs N]` — indexdata vs non-indexdata performance comparison.
- `brokkr bench planetiler [--dataset name] [--pbf path] [--runs N]` — Planetiler Java PBF read benchmark. Auto-downloads JDK + Planetiler JAR.
- `brokkr bench all [--dataset name] [--pbf path] [--runs N]` — full suite: read + write + merge + commands + osmpbf/osmium/planetiler baselines.
- `brokkr results [--commit X] [--compare A B] [--command X] [--variant X] [-n N]` — query benchmark results from SQLite database. Results stored by bench harness (clean tree only).
- `brokkr hotpath [--dataset name] [--pbf path] [--osc path] [--alloc] [--runs N]` — hotpath profiling (function-level timing/allocation metrics). Default dataset: denmark, runs: 1. `--alloc` uses `hotpath-alloc` feature for allocation tracking. Wall-clock stored in SQLite.
- `brokkr profile [--dataset name] [--pbf path] [--osc path]` — two-pass profiling: timing pass (6 tests with `hotpath` feature) then allocation pass (2 tests with `hotpath-alloc` feature). Console output only, no SQLite.
- `brokkr download <region> [--osc-url url]` — download region datasets from Geofabrik. Regions: malta, greater-london, switzerland, norway, japan, denmark, germany, north-america. Auto-generates indexed PBF via `cat`. Idempotent (skips existing files).
- `brokkr clean` — remove scratch temp files and verify output directories.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git).

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Indexdata PBFs

To generate a PBF with blob-level indexdata (required for fast passthrough merges), use `cat`:
```
brokkr run cat input.osm.pbf --type node,way,relation -o output-with-indexdata.osm.pbf
```
There is no `--add-indexdata` flag — `cat` embeds indexdata automatically when writing.

## Verify subcommands

Cross-validate pbfhogg output against reference tools (osmium, osmosis, osmconvert). All default to `--dataset denmark`.

- `brokkr verify sort [--dataset name] [--pbf path]` — sort vs osmium sort
- `brokkr verify cat [--dataset name] [--pbf path]` — cat with type filters vs osmium cat
- `brokkr verify extract [--dataset name] [--pbf path] [--bbox bbox]` — bbox extract (simple/complete/smart) vs osmium extract
- `brokkr verify tags-filter [--dataset name] [--pbf path]` — tags-filter with 3 expressions vs osmium
- `brokkr verify getid-removeid [--dataset name] [--pbf path]` — getid/removeid vs osmium getid
- `brokkr verify add-locations-to-ways [--dataset name] [--pbf path]` — add-locations-to-ways vs osmium
- `brokkr verify check-refs [--dataset name] [--pbf path]` — check-refs vs osmium check-refs
- `brokkr verify merge [--dataset name] [--pbf path] [--osc path]` — merge vs osmium/osmosis/osmconvert
- `brokkr verify derive-changes [--dataset name] [--pbf path] [--osc path]` — derive-changes roundtrip vs osmium
- `brokkr verify diff [--dataset name] [--pbf path] [--osc path]` — diff summary vs osmium diff
- `brokkr verify all [--dataset name] [--pbf path] [--osc path] [--bbox bbox]` — run all verify commands sequentially

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** Not two, not three — ONE AT A TIME. Benchmarks require exclusive access to CPU, memory, and I/O. Running multiple simultaneously makes every result wrong. Always wait for each to fully complete before starting the next. This applies to bench, verify, hotpath, and profile subcommands.
- When an optimization workflow requires multiple benchmark runs (baseline, mid-work, post-work), run each one **sequentially** and report results between runs. Do NOT launch them as parallel background tasks.

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

- All performance numbers (timings, allocations, throughput) in markdown files must include the git commit hash and hostname where the measurement was taken. Benchmark results are stored automatically in `.brokkr/results.db` (SQLite).
- Strict clippy lints enforced (see `[workspace.lints.clippy]` in Cargo.toml) -- notably `unwrap_used = "deny"` and `cognitive_complexity = "deny"`
- Coordinates use decimicrodegrees (10^-7 degrees) for node I/O in BlockBuilder
- Error types in `error.rs` follow the `csv` crate pattern (boxed ErrorKind). `MissingHeader` error if a PBF doesn't start with an OsmHeader blob.
- Tests live in `tests/` (roundtrip.rs, roundtrip_real.rs) and inline in blob.rs/indexed.rs

## Features (library crate)

- `rust-zlib` (default): pure Rust zlib via flate2
- `zlib`: system zlib
- `zlib-ng`: zlib-ng (mutually exclusive with above)
- `commands` (default): enables `check_refs`, `extract`, and their deps (`roaring`, `serde_json`)
- `libdeflater`: use libdeflate for zlib compression (2-3x faster, requires C compiler)
- `linux-direct-io`: O_DIRECT read/write paths (bypasses page cache, requires `libc`)
- `linux-io-uring`: io_uring writer thread (requires `io-uring` + `libc`, Linux 5.1+, sufficient `RLIMIT_MEMLOCK`)
