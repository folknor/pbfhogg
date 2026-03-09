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

- `brokkr check [-- args]` — run clippy + tests. Extra args forwarded to `cargo test` (e.g., `brokkr check -- --ignored`). Supports `--features`, `--no-default-features`, and `--package` / `-p` (forwarded to both clippy and test).
- `brokkr env` — show hostname, kernel, governor, memory, drives, tool versions, dataset status. Shows computed XXH128 hashes for each dataset file (copy into the `xxhash` field in `brokkr.toml`).
- `brokkr run [options] [-- args]` — build release CLI and run passthrough command args. Supports machine-readable timing:
  - `--time` prints key=value timing output
  - `--json` prints structured JSON timing output
  - `--runs N` repeats execution N times (summary stats include min/median/p95)
  - `--no-build` skips build and runs an already-built binary (fails clearly if missing)
  - examples: `brokkr run --time -- --help`, `brokkr run --json --runs 5 --no-build -- --version`
- `brokkr bench read [--dataset name] [--variant V] [--runs N] [--modes list]` — read benchmark (4 modes: sequential, parallel, pipelined, blobreader). Results stored in SQLite. Default variant: indexed.
- `brokkr bench write [--dataset name] [--variant V] [--runs N] [--compression list]` — write benchmark (sync + pipelined × compression). Default compressions: none,zlib:6,zstd:3. Results stored in SQLite. Default variant: indexed.
- `brokkr bench merge [--dataset name] [--variant V] [--osc-seq SEQ] [--runs N] [--uring] [--compression list]` — merge benchmark (I/O modes × compression). Default compressions: zlib,none. `--uring` adds io_uring variants with preflight checks. Results stored in SQLite. Default variant: indexed.
- `brokkr bench commands [command] [--dataset name] [--variant V] [--osc-seq SEQ] [--runs N]` — CLI command benchmark (27 commands, external timing). Use `all` for full suite. `diff` and `diff-osc` auto-generate a merged PBF (cached in scratch). Results stored in SQLite. Default variant: indexed.
- `brokkr bench extract [--dataset name] [--variant V] [--runs N] [--bbox bbox] [--strategies list]` — extract strategy benchmark (simple/complete/smart). Default dataset: japan. Default variant: indexed.
- `brokkr bench allocator [--dataset name] [--variant V] [--runs N]` — allocator comparison (default/jemalloc/mimalloc) via check --refs. Default variant: indexed.
- `brokkr bench blob-filter [--dataset name] [--indexed-variant V] [--raw-variant V] [--runs N]` — indexdata vs non-indexdata performance comparison. Default variants: indexed + raw.
- `brokkr bench planetiler [--dataset name] [--variant V] [--runs N]` — Planetiler Java PBF read benchmark. Auto-downloads JDK + Planetiler JAR. Default variant: indexed.
- `brokkr bench all [--dataset name] [--variant V] [--runs N]` — full suite: read + write + merge + commands + osmpbf/osmium/planetiler baselines. Default variant: indexed.
- `brokkr results [UUID]` — look up specific result by UUID prefix (shows full detail + hotpath report)
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [-n N] [--top N]` — query/compare benchmark results from SQLite. Use `--top 0` to show all hotpath functions. Use `--compare-last --command hotpath` to diff two most recent hotpath runs. Results stored by bench harness (clean tree only).
- `brokkr hotpath [--dataset name] [--variant V] [--osc-seq SEQ] [--alloc] [--runs N] [--test NAME]` — hotpath profiling (function-level timing/allocation metrics). Default dataset: denmark, runs: 1. `--alloc` uses `hotpath-alloc` feature for allocation tracking. Wall-clock stored in SQLite. Default variant: indexed. `--test` runs a single test: inspect-tags, check-refs, cat, apply-changes-zlib, apply-changes-none.
- `brokkr profile [--dataset name] [--variant V] [--osc-seq SEQ]` — two-pass profiling: timing pass (6 tests with `hotpath` feature) then allocation pass (2 tests with `hotpath-alloc` feature). Console output only, no SQLite. Default variant: indexed.
- `brokkr download <region> [--osc-url url]` — download region datasets from Geofabrik. Regions: malta, greater-london, switzerland, norway, japan, denmark, germany, north-america. Auto-generates indexed PBF via `cat`. Idempotent (skips existing files).
- `brokkr clean` — remove scratch temp files and verify output directories.
- `brokkr history [--command CMD] [--project P] [--failed] [--since DATE] [--slow MS] [-n N] [--all]` — query global command history (stored in `$XDG_DATA_HOME/brokkr/history.db`). Every brokkr invocation is recorded with timing, exit status, project, and git context. Works from any directory.

### brokkr.toml

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "3f1977fd..."
seq = 4704

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
xxhash = "fa581f7b..."
```

- `pbf.<variant>` — PBF files keyed by variant name. `--variant` selects (default: `indexed`).
- `osc.<seq>` — OSC diff files keyed by sequence number. `--osc-seq` selects.
- `xxhash` — XXH128 file hash. Run `brokkr env` to see computed values.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git). `--runs N` repeats each benchmark N times but only stores the best (minimum) result. Default is 3 runs. Bench and hotpath commands require a clean git tree (ignoring `*.md` and `.brokkr/results.db`); use `--force` to run anyway (results will not be stored). **`--force` is a top-level flag before the subcommand**, e.g. `brokkr bench --force commands add-locations-to-ways`, NOT `brokkr bench commands add-locations-to-ways --force`.

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Indexdata PBFs

To generate a PBF with blob-level indexdata, use `cat`:
```
brokkr run -- cat input.osm.pbf -o output-with-indexdata.osm.pbf
```
The passthrough path (no `--type`) adds indexdata via decompress+scan without re-compressing blobs — minimal memory, suitable for planet-scale files. Planet (87 GB): 497s buffered, 520s `--direct-io` (+5% slower); Denmark (461 MB): 2.8s buffered (commit `69a127f`, plantasjen). Buffered wins for sequential single-file passthrough — `--direct-io` only helps with concurrent read/write (merge). The `--type` filtered path also embeds indexdata but does full decode+re-encode (OOMs on planet at 30 GB host).

`apply-changes`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, and `inspect --nodes` are much faster with indexed PBFs and will error if indexdata is missing. Use `--force` to override the check and run with raw PBFs (slower). `inspect --indexed` checks a PBF and exits 0 (indexed) or 1 (not indexed).

### add-locations-to-ways index types

`add-locations-to-ways` supports `--index-type dense|sparse` (default: `dense`):

- **`dense`** — Direct-mapped mmap array (`index[node_id] = (lat, lon)`). Fastest when the working set fits in RAM. At planet scale (~16 GB touched after pass 0 filtering), requires ~30+ GB free memory to avoid page cache thrashing.
- **`sparse`** — Planetiler-inspired chunk-indexed sparse array. ~540 MB RAM for chunk index + compact on-disk values file (~16 GB for planet). Way lookups are batched and sorted by file offset, converting random I/O into sequential scans. Works on memory-constrained hosts (tested on 30 GB host with planet). ~1.85x slower than dense on Denmark (all fits in RAM, overhead is pure CPU).

## Verify subcommands

Cross-validate pbfhogg output against reference tools (osmium, osmosis, osmconvert). All default to `--dataset denmark`.

- `brokkr verify sort [--dataset name] [--variant V]` — sort vs osmium sort
- `brokkr verify cat [--dataset name] [--variant V]` — cat with type filters vs osmium cat
- `brokkr verify extract [--dataset name] [--variant V] [--bbox bbox]` — bbox extract (simple/complete/smart) vs osmium extract
- `brokkr verify tags-filter [--dataset name] [--variant V]` — tags-filter with 3 expressions vs osmium
- `brokkr verify getid-removeid [--dataset name] [--variant V]` — getid/getid --invert vs osmium getid
- `brokkr verify add-locations-to-ways [--dataset name] [--variant V]` — add-locations-to-ways vs osmium
- `brokkr verify check-refs [--dataset name] [--variant V]` — check --refs vs osmium check-refs
- `brokkr verify merge [--dataset name] [--variant V] [--osc-seq SEQ]` — apply-changes vs osmium/osmosis/osmconvert
- `brokkr verify derive-changes [--dataset name] [--variant V] [--osc-seq SEQ]` — diff --format osc roundtrip vs osmium
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
- **`pbfhogg-cli`** (`cli/`) — binary crate. CLI dispatch via clap. Produces the `pbfhogg` binary. Has its own integration tests in `cli/tests/cli.rs`.

Library users who only need read/write can depend on `pbfhogg` with `default-features = false` to skip the `commands` feature (avoids `serde_json` and `roaring` deps used by `extract` and `check_refs`).

### Clippy caching pitfall

`cargo clippy --all-targets` (workspace-wide, used by `brokkr check`) can use cached results and miss lint violations in the CLI crate. If you change `cli/src/main.rs`, always verify with `brokkr check --package pbfhogg-cli` to force a fresh clippy run on that package.

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
All merge variants under 600 MB RSS. io_uring wins 12-20% at this scale (page cache overflow).
