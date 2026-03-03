# pbfhogg

Rust library and CLI tool for reading and writing OpenStreetMap PBF files.

## Bash rules
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

## Brokkr tool

Standalone development tool at `~/Programs/brokkr`. Installed via `cargo install --path ~/Programs/brokkr`. Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

- `brokkr check [-- args]` — run clippy + tests. Extra args forwarded to `cargo test` (e.g., `brokkr check -- --ignored`).
- `brokkr env` — show hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `brokkr run [options] [-- args]` — build release CLI and run passthrough command args. Supports machine-readable timing:
  - `--time` prints key=value timing output
  - `--json` prints structured JSON timing output
  - `--runs N` repeats execution N times (summary stats include min/median/p95)
  - `--no-build` skips build and runs an already-built binary (fails clearly if missing)
  - examples: `brokkr run --time -- --help`, `brokkr run --json --runs 5 --no-build -- --version`
- `brokkr bench read [--dataset name] [--variant V] [--runs N] [--modes list]` — read benchmark (4 modes: sequential, parallel, pipelined, blobreader). Results stored in SQLite. Default variant: indexed.
- `brokkr bench write [--dataset name] [--variant V] [--runs N] [--compression list]` — write benchmark (sync + pipelined × compression). Default compressions: none,zlib:6,zstd:3. Results stored in SQLite. Default variant: indexed.
- `brokkr bench merge [--dataset name] [--variant V] [--osc-seq SEQ] [--runs N] [--uring] [--compression list]` — merge benchmark (I/O modes × compression). Default compressions: zlib,none. `--uring` adds io_uring variants with preflight checks. Results stored in SQLite. Default variant: indexed.
- `brokkr bench commands [command] [--dataset name] [--variant V] [--osc-seq SEQ] [--runs N]` — CLI command benchmark (19 commands, external timing). Use `all` for full suite. `diff` and `derive-changes` auto-generate a merged PBF (cached in scratch). Results stored in SQLite. Default variant: indexed.
- `brokkr bench extract [--dataset name] [--variant V] [--runs N] [--bbox bbox] [--strategies list]` — extract strategy benchmark (simple/complete/smart). Default dataset: japan. Default variant: indexed.
- `brokkr bench allocator [--dataset name] [--variant V] [--runs N]` — allocator comparison (default/jemalloc/mimalloc) via check-refs. Default variant: indexed.
- `brokkr bench blob-filter [--dataset name] [--indexed-variant V] [--raw-variant V] [--runs N]` — indexdata vs non-indexdata performance comparison. Default variants: indexed + raw.
- `brokkr bench planetiler [--dataset name] [--variant V] [--runs N]` — Planetiler Java PBF read benchmark. Auto-downloads JDK + Planetiler JAR. Default variant: indexed.
- `brokkr bench all [--dataset name] [--variant V] [--runs N]` — full suite: read + write + merge + commands + osmpbf/osmium/planetiler baselines. Default variant: indexed.
- `brokkr results [UUID]` — look up specific result by UUID prefix (shows full detail + hotpath report)
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [-n N] [--top N]` — query/compare benchmark results from SQLite. Use `--top 0` to show all hotpath functions. Use `--compare-last --command hotpath` to diff two most recent hotpath runs. Results stored by bench harness (clean tree only).
- `brokkr hotpath [--dataset name] [--variant V] [--osc-seq SEQ] [--alloc] [--runs N]` — hotpath profiling (function-level timing/allocation metrics). Default dataset: denmark, runs: 1. `--alloc` uses `hotpath-alloc` feature for allocation tracking. Wall-clock stored in SQLite. Default variant: indexed.
- `brokkr profile [--dataset name] [--variant V] [--osc-seq SEQ]` — two-pass profiling: timing pass (6 tests with `hotpath` feature) then allocation pass (2 tests with `hotpath-alloc` feature). Console output only, no SQLite. Default variant: indexed.
- `brokkr download <region> [--osc-url url]` — download region datasets from Geofabrik. Regions: malta, greater-london, switzerland, norway, japan, denmark, germany, north-america. Auto-generates indexed PBF via `cat`. Idempotent (skips existing files).
- `brokkr clean` — remove scratch temp files and verify output directories.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git).

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Indexdata PBFs

To generate a PBF with blob-level indexdata, use `cat`:
```
brokkr run cat input.osm.pbf --type node,way,relation -o output-with-indexdata.osm.pbf
```
There is no `--add-indexdata` flag — `cat` embeds indexdata automatically when writing.

`merge`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `tags-count --type`, and `node-stats` are much faster with indexed PBFs and will error if indexdata is missing. Use `--force` to override the check and run with raw PBFs (slower). `is-indexed` checks a PBF and exits 0 (indexed) or 1 (not indexed).

## Verify subcommands

Cross-validate pbfhogg output against reference tools (osmium, osmosis, osmconvert). All default to `--dataset denmark`.

- `brokkr verify sort [--dataset name] [--variant V]` — sort vs osmium sort
- `brokkr verify cat [--dataset name] [--variant V]` — cat with type filters vs osmium cat
- `brokkr verify extract [--dataset name] [--variant V] [--bbox bbox]` — bbox extract (simple/complete/smart) vs osmium extract
- `brokkr verify tags-filter [--dataset name] [--variant V]` — tags-filter with 3 expressions vs osmium
- `brokkr verify getid-removeid [--dataset name] [--variant V]` — getid/removeid vs osmium getid
- `brokkr verify add-locations-to-ways [--dataset name] [--variant V]` — add-locations-to-ways vs osmium
- `brokkr verify check-refs [--dataset name] [--variant V]` — check-refs vs osmium check-refs
- `brokkr verify merge [--dataset name] [--variant V] [--osc-seq SEQ]` — merge vs osmium/osmosis/osmconvert
- `brokkr verify derive-changes [--dataset name] [--variant V] [--osc-seq SEQ]` — derive-changes roundtrip vs osmium
- `brokkr verify diff [--dataset name] [--variant V] [--osc-seq SEQ]` — diff summary vs osmium diff
- `brokkr verify all [--dataset name] [--variant V] [--osc-seq SEQ] [--bbox bbox]` — run all verify commands sequentially

All `--variant` flags default to `indexed`. `--osc-seq` auto-selects if exactly one OSC is configured for the dataset.

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** Not two, not three — ONE AT A TIME. Benchmarks require exclusive access to CPU, memory, and I/O. Running multiple simultaneously makes every result wrong. Always wait for each to fully complete before starting the next. This applies to bench, verify, hotpath, and profile subcommands.
- When an optimization workflow requires multiple benchmark runs (baseline, mid-work, post-work), run each one **sequentially** and report results between runs. Do NOT launch them as parallel background tasks.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

## Workspace

The repo is a Cargo workspace with two packages:
- **`pbfhogg`** (root) — library crate. Read/write API, commands.
- **`pbfhogg-cli`** (`cli/`) — binary crate. CLI dispatch via clap. Produces the `pbfhogg` binary.

Library users who only need read/write can depend on `pbfhogg` with `default-features = false` to skip the `commands` feature (avoids `serde_json` and `roaring` deps used by `extract` and `check_refs`).

## Architecture

**Read path:** `BlobReader` (blob.rs) -> `PrimitiveBlock` (block.rs) -> `Element` (elements.rs)
- `ElementReader` (reader.rs): high-level sequential/parallel/pipelined iteration. Parses the PBF header eagerly at construction — `header()` returns `&HeaderBlock` with metadata including `is_sorted()` (Sort.Type_then_ID). Debug builds assert monotonic node IDs when sorted. `for_each_block_pipelined` delivers owned `PrimitiveBlock`s for consumers that need to send blocks to other threads for parallel processing. `into_blocks_pipelined` returns an `Iterator<Item = Result<PrimitiveBlock>>` for loop control (early exit, zipping two files); requires `R: 'static`.
- `IndexedReader` (indexed.rs): seekable reader with blob-level index for filtered queries
- `pipeline.rs`: 3-stage pipelined decoder (IO thread -> rayon pool -> reorder buffer)

**Write path:** `BlockBuilder` (block_builder.rs) -> `PbfWriter` (writer.rs)
- `BlockBuilder`: accumulates nodes/ways/relations, handles string table, delta encoding, dense packing. Max 8000 entities/block. One element type per block. All element types use direct wire-format encoding via reusable scratch buffers (`wire.rs` primitives).
- `wire.rs`: write-side protobuf encoding primitives (varint, zigzag, field encoders, packed repeated fields). Mirrors the read-side `src/read/wire.rs` decoding primitives.
- `PbfWriter`: blob framing, compression (zlib/zstd/none), raw passthrough for merges. `to_path` uses parallel compression via rayon + reorder buffer. `to_path_direct` bypasses page cache via O_DIRECT. `to_path_uring` uses registered buffers + WriteFixed for I/O-bound workloads. `new(writer)` provides sync mode for in-memory / generic-Write usage.
- `uring_writer.rs`: io_uring writer thread — `AlignedBufferPool` (64×256KB registered buffers), `UringState` (buffered accumulation + WriteFixed submission + CQE reaping)

## Conventions

- All performance numbers (timings, allocations, throughput) in markdown files must include the git commit hash and hostname where the measurement was taken. Benchmark results are stored automatically in `.brokkr/results.db` (SQLite).
- Strict clippy lints enforced (see `[workspace.lints.clippy]` in Cargo.toml) -- notably `unwrap_used = "deny"` and `cognitive_complexity = "deny"`
- Coordinates use decimicrodegrees (10^-7 degrees) for node I/O in BlockBuilder
- Error types in `error.rs` follow the `csv` crate pattern (boxed ErrorKind). `MissingHeader` error if a PBF doesn't start with an OsmHeader blob.
- Tests live in `tests/` (roundtrip.rs, roundtrip_real.rs) and inline in blob.rs/indexed.rs

## Features (library crate)

- `commands` (default): enables `check_refs`, `extract`, and their deps (`roaring`, `serde_json`)
- `linux-direct-io`: O_DIRECT read/write paths (bypasses page cache, requires `libc`)
- `linux-io-uring`: io_uring writer thread (requires `io-uring` + `libc`, Linux 5.1+, sufficient `RLIMIT_MEMLOCK`)

Zlib backend is hardcoded to `zlib-rs` (pure Rust, no C compiler, faster than zlib-ng). No feature flags for backend selection. Sync zlib compression is 15-19% slower than the previous `libdeflater` (C) backend, but pipelined mode — the production path — shows no difference (decode-bound). The tradeoff is accepted: zero C dependencies for compression, one backend everywhere.

## Performance baselines (North America, 18.8 GB, 2.58B elements, commit `a6ebbfe`)

**Read:** parallel 22s, pipelined 57s, sequential 130s.
**Write:** pipelined zlib 4m27s, pipelined none/zstd ~4m20s, sync zlib 14m34s.
**Merge** (645K-change daily diff, 303K passthrough / 19.6K rewritten blobs):
buffered+zlib 17.3s, uring+zlib 15.2s, buffered+none 14.9s, **uring+none 11.9s**.
All merge variants under 600 MB RSS. io_uring wins 12-20% at this scale (page cache overflow). sqpoll adds no benefit.
