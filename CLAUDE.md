# pbfhogg

Rust library and CLI tool for reading and writing OpenStreetMap PBF files.

## Bash rules
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never capture stdout into env vars (`UUID=$(...)`) — shell state doesn't persist between tool calls. Read the output directly and use the value inline.
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

## Brokkr tool

Dev tool at `~/Programs/brokkr`. Invoked as `brokkr` from project root (reads `./brokkr.toml`).

```
brokkr <command> [--dataset D] [--variant V]   # run once, print timing
brokkr <command> [--dataset D] --bench          # 3 runs, store in DB + sidecar profiler
brokkr <command> [--dataset D] --hotpath        # function-level timing
brokkr <command> [--dataset D] --alloc          # allocation tracking
brokkr <command> [--dataset D] --direct-io      # pass --direct-io to pbfhogg
brokkr <command> [--dataset D] --io-uring       # pass --io-uring to pbfhogg
```

`--stop MARKER` requires a measured mode. Kills process after the named marker. Example: `brokkr renumber --dataset planet --bench 1 --stop RENUMBER_EXT_STAGE2D_END`.

`--bench N` runs N times, stores best. Default `--bench 3`. Requires clean git tree (ignoring `*.md` and `.brokkr/results.db`); `--force` overrides (results not stored).

I/O flags (`--direct-io`, `--io-uring`) create named variants in results. `--force` is a top-level flag before the subcommand.

### pbfhogg commands (every CLI command is a brokkr subcommand)

- `brokkr inspect-tags --dataset denmark`
- `brokkr add-locations-to-ways --dataset europe --index-type external --bench`
- `brokkr build-geocode-index --dataset denmark --hotpath`
- `brokkr multi-extract --dataset japan --regions 5 --bench`
- `brokkr read --bench` — multi-variant read benchmark
- `brokkr cat --dataset planet --bench` — indexdata-generation passthrough. `--variant raw` default. Distinct from `cat-way` / `cat-relation` / `cat-dedupe` (filtered full-decode path).
- `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench` — multi-OSC merge. Expands range to configured `osc.<seq>` entries. Variant suffix `+range-LO-HI`.
- `brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1` — diff two independent PBFs (no byte-level overlap, full decode both sides). `--from`/`--to` accept snapshot keys or `base`. `--format default|osc`.
- `brokkr suite pbfhogg --bench` — full suite

### Utility commands

- `brokkr check [-- args]` — clippy + tests. Supports `--features`, `--no-default-features`, `-p`. Output minified: one-line summaries with file:line.
- `brokkr env` — hostname, kernel, governor, memory, drives, datasets.
- `brokkr results` — most recent result. `brokkr results [UUID]` — specific result.
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [--dataset D] [--meta K=V] [-n N] [--top N]` — query/compare from SQLite. `--variant`/`--dataset` substring match; `--meta` exact match, composable with AND.
- `brokkr results <UUID> --timeline` — JSONL samples. Composable: `--summary`, `--fields`, `--where`, `--every`, `--head`/`--tail`, `--stat`, `--phase`, `--range`.
- `brokkr results <UUID> --markers` — marker events. `--durations` for pairs, `--phases` for durations + peak RSS/anon/majflt.
- `brokkr results --compare-timeline <A> <B>` — phase-aligned delta table.
- `brokkr results dirty` — sidecar data from most recent run (even if OOM-killed).
- `brokkr download <region> [--osc-seq N] [--as-snapshot <key> | --refresh] [--force]` — download datasets. Primary PBF: no-op once configured (use `--refresh` to rotate). OSC: rolls forward. Indexed PBF: regenerated on cache miss. `--as-snapshot <key>` registers additional snapshot. `--refresh` archives primary into `snapshot.<key>` and downloads new. Planet uses planet.openstreetmap.org; others use Geofabrik. Short aliases (denmark, europe) or full paths (europe/france).
- `brokkr lock` — check if a command is running. Never compile while a bench holds the lock.
- `brokkr clean` — remove scratch temp files.
- `brokkr history [--command CMD] [--failed] [--since DATE] [--slow MS] [-n N]` — global command history.
- `brokkr verify <command> [--dataset name]` — cross-validate vs osmium/osmosis/osmconvert. `brokkr verify all` runs all. Default `--dataset denmark`, `--variant indexed`.

### OSC resolver

`--osc-seq` auto-selects only with exactly one `[osc.<seq>]` entry. Multiple OSCs require explicit `--osc-seq N`. Applies to both primary and snapshot-scoped OSC tables.

### Snapshot model

Datasets have one **primary** PBF (`[datasets.<region>.pbf.*]` + `[datasets.<region>.osc.*]`) plus optional named **snapshots** (`[datasets.<region>.snapshot.<key>.*]`). Primary is the default; snapshots addressable via `--snapshot <key>` on `apply-changes`, `merge-changes`, `tags-filter-osc`, `diff`, `diff-osc`, or `--from`/`--to` on `diff-snapshots`. `--snapshot base` = primary. Key `base` is reserved. Snapshot-scoped results: variant `+snap-<key>`, metadata `meta.snapshot=<key>`.

`brokkr diff-snapshots` compares two independent PBFs (full decode both sides). `brokkr diff` runs apply-changes internally first (byte-equal blob fast-path). Different code paths, different things measured.

### brokkr.toml schema

- `pbf.<variant>` — PBF files keyed by variant (raw/indexed/altw). `--variant` selects (default: indexed).
- `osc.<seq>` — OSC diff files keyed by seq. `--osc-seq` selects.
- `snapshot.<key>` — named point-in-time captures with own `pbf`/`osc` tables.
- `download_date` — human annotation only, not read by code.
- `xxhash` — XXH128 hash. `brokkr env` shows computed values.

Results in `.brokkr/results.db` (SQLite, tracked in git).

## Indexdata PBFs

Generated by `brokkr download` (passthrough cat after raw PBF lands). Benchmark: `brokkr cat --dataset planet --bench 1`.

Commands requiring indexdata: `apply-changes`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`, `build-geocode-index`. Use `--force` to run without (slower). `inspect --indexed` checks (exit 0/1).

### add-locations-to-ways index types

| Type | Memory | Temp disk | Best for |
|------|--------|-----------|----------|
| `dense` (default) | ~30+ GB mmap | none | RAM fits working set |
| `sparse` | ~540 MB + 16 GB disk | ~16 GB | memory-constrained, batched lookups |
| `external` | ~1.6 GB | ~112 GB (Europe) | memory-constrained, all sequential I/O. Planet: 1,462s, 3.9x faster than dense |

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** ONE AT A TIME. Always wait for each to fully complete before starting the next.
- Multiple benchmark runs in an optimization workflow: run sequentially, report between runs.

## Review tool

`review` fans out queries to persistent AI sessions. Configured in `.review.toml`.

Archetypes: `bugs`, `perf`, `arch`, `correctness`, `planet`. Groups: `sweep` = first 4, `everything` = all 5.

```
echo "question" | review sweep          # 4 archetypes
echo "question" | review planet         # planet sessions
echo "question" | review everything     # all 5
echo "question" | review perf --anchor  # re-anchor stale session
```

Use `--anchor` with identity reminder for stale/first-use sessions. **Use before implementing major changes.**

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

## Workspace

Cargo workspace: **`pbfhogg`** (root, library) + **`pbfhogg-cli`** (`cli/`, binary, produces `pbfhogg`). Library users: `default-features = false` skips `commands` feature. CLI integration tests in `cli/tests/cli.rs`.

`cargo clippy --all-targets` can cache and miss CLI crate violations. After changing `cli/src/main.rs`, verify with `brokkr check --package pbfhogg-cli`.

## Architecture

**Read path:** `BlobReader` (blob.rs) → `PrimitiveBlock` (block.rs) → `Element` (elements.rs)
- `ElementReader` (reader.rs): sequential/parallel/pipelined iteration. Header parsed eagerly. `is_sorted()` for Sort.Type_then_ID. `for_each_block_pipelined` delivers owned blocks. `into_blocks_pipelined` returns iterator (early exit, zipping); requires `R: 'static`.
- `IndexedReader` (indexed.rs): seekable reader with blob-level index.
- `pipeline.rs`: 3-stage pipelined decoder (IO → rayon → reorder buffer).

**Write path:** `BlockBuilder` (block_builder.rs) → `PbfWriter` (writer.rs)
- `BlockBuilder`: max 8000 entities/block, one type per block, direct wire-format encoding via `wire.rs`.
- `PbfWriter`: blob framing, compression (zlib/zstd/none), raw passthrough. `to_path` (parallel compression), `to_path_direct` (O_DIRECT), `to_path_uring` (io_uring), `new(writer)` (sync).
- `uring_writer.rs`: io_uring with 64×256KB registered buffers.

**Geocode index:** `geocode_index/` — `format.rs` (19-file on-disk format), `reader.rs` (mmap, S2 cell lookup), `builder.rs` (4-pass pipeline).

**Geometry:** `geo.rs` — point-in-polygon, ring assembly, Douglas-Peucker, antimeridian, cos-projection distance.

## Conventions

- Performance numbers in markdown must include commit hash and hostname. Results auto-stored in `.brokkr/results.db`.
- Strict clippy: `unwrap_used = "deny"`, `cognitive_complexity = "deny"`.
- Coordinates: decimicrodegrees (10^-7 degrees) for node I/O.
- Errors: `error.rs`, boxed ErrorKind pattern. `MissingHeader` if no OsmHeader blob.
- Tests: `tests/` (21 files) + `cli/tests/cli.rs` + inline unit tests.

## Features (library crate)

- `commands` (default): `check_refs`, `extract`, geocode builder + deps (`roaring`, `serde_json`, `s2`)
- `geocode-reader`: `geocode_index::Reader` for reverse geocoding (depends on `s2`). Included by `commands`.
- `linux-direct-io`: O_DIRECT read/write (requires `libc`)
- `linux-io-uring`: io_uring writer (requires `io-uring` + `libc`, Linux 5.1+)

Zlib backend: `zlib-rs` (pure Rust, no C). No feature flags for backend selection.
