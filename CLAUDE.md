# pbfhogg

Rust library and CLI tool for reading and writing OpenStreetMap PBF files.

## Rules

### General rules
Dont use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten!

### Memory rules
Do not use your Memory functionality. Update CLAUDE.md instead. This project is developed across several hosts and several users. Memories do not transfer across hosts or users. CLAUDE.md does.

### Bash rules
- Never use sed, find, awk, head, tail, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never chain/pipe commands with |
- Never capture stdout into env vars (`UUID=$(...)`).
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

### git commit rules
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Remember to update CHANGELOG.md for relevant commits (but not general small performance improvements.)

#### What gets added to CHANGELOG.md

Audience: library + CLI users deciding whether to upgrade. Not a commit digest.

**Add:** breaking changes (removed flags, widened bounds, format bumps); new capabilities; behavior changes at the same surface (silent truncation → hard error, new warnings); user-visible bug fixes; perf changes large enough to matter (headline numbers, not 5% sub-phase deltas).

**Skip:** internal refactors, module splits, helper extractions; sidecar instrumentation (markers, counters, `hotpath`) - serves brokkr, not users; F-numbered fix rollups; sub-phase timings that don't move the headline; test additions, code-quality cleanups, dead-code removal; doc-file edits (CORRECTNESS.md, DEVIATIONS.md, notes/*.md); private internals.

Test: would a user change what they do after reading this entry? If no, it belongs in `git log`.

The user can allow things that contravene these rules, for example allowing commits that are pure markdown updates. Do not ask them for this, they will tell you when.

## 'brokkr' tool

Dev tool at `~/Programs/brokkr`. Invoked as `brokkr` from project root (reads `./brokkr.toml`).

```
brokkr <command> [--dataset D] [--variant V]   # run once, print timing
brokkr <command> [--dataset D] --bench          # 3 runs, store in DB + sidecar profiler
brokkr <command> [--dataset D] --hotpath        # function-level timing
brokkr <command> [--dataset D] --alloc          # allocation tracking
brokkr <command> [--dataset D] --direct-io      # pass --direct-io to pbfhogg
brokkr <command> [--dataset D] --io-uring       # pass --io-uring to pbfhogg
```

`--stop MARKER` requires a measured mode. Kills process after the named marker. Accepts three spellings: verbatim (`--stop RENUMBER_EXT_STAGE2D_END`), the `-` sigil (`--stop -RENUMBER_EXT_STAGE2D` → resolves to `RENUMBER_EXT_STAGE2D_END`), and the bare-name fallback (`--stop RENUMBER_EXT_STAGE2D` → also resolves to `RENUMBER_EXT_STAGE2D_END`; the sidecar log line shows the resolved form).

Markers are point-in-time bookmarks - the FIFO protocol is `<timestamp_us> <name>` per marker, nothing else. The default `brokkr sidecar <uuid>` view segments the stream between consecutive markers; `brokkr sidecar <uuid> --durations` is the one view that opts into the `FOO_START` / `FOO_END` pairing convention for duration math.

### Sidecar conventions

Brokkr is convention-free at the protocol layer. Two optional naming conventions unlock richer views:

- **`FOO_START` / `FOO_END` marker pairs** drive `--durations` (per-span wall time) and anchor `--stop` alias resolution. Emit both markers around any phase you want to time.
- **`WAIT_<CATEGORY>_START` / `WAIT_<CATEGORY>_END` stall spans** drive `brokkr sidecar <uuid> --stalls`. Wrap blocking points (channel sends, spill waits, backpressure) in marker pairs whose name begins `WAIT_`. `--stalls` sums durations by category and reports each as a fraction of run wall-clock. Categories are free-form - pick names that match the blocking points you want to attribute (`WAIT_WRITER`, `WAIT_PAYLOAD`, `WAIT_SPILL`, etc.). Runs from before the convention simply report "no WAIT_* marker pairs" rather than empty output. **Only emit `WAIT_*` pairs on genuine blocks** - gate them behind a `try_send` / `try_recv` fast path, otherwise every non-blocking poll produces a zero-width pair and the FIFO floods.

Both conventions are additive. The default phase summary already shows per-phase user/kernel core split, `majflt`/`minflt` deltas, voluntary/involuntary context switches, and peak thread count - derived from `/proc` samples, so older sidecar rows get the enriched view retroactively on next query.

`--bench N` runs N times, stores best. Default `--bench 3`.

Requires clean git tree (except for `*.md` and `.brokkr/results.db`); `--force` overrides (results not stored).

I/O flags (`--direct-io`, `--io-uring`) create named variants in results. `--force` is a top-level flag before the subcommand.

### pbfhogg commands (every CLI command is a brokkr subcommand)

- `brokkr inspect --tags --dataset denmark` (add `--type way` to narrow, or `--nodes` for node stats)
- `brokkr add-locations-to-ways --dataset europe --index-type external --bench`
- `brokkr build-geocode-index --dataset denmark --hotpath`
- `brokkr multi-extract --dataset japan --regions 5 --bench`
- `brokkr read --bench` - multi-variant read benchmark
- `brokkr cat [--type way|relation] [--dedupe] [--clean] --dataset planet --bench` - no flags = indexdata-generation passthrough (no re-decode). `--type way|relation` filters to one object kind. `--dedupe` runs the two-input dedupe path (supports `--io-uring`). `--clean` forces the full-decode / re-frame Framed path. Flags are orthogonal and combinable.
- `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench` - multi-OSC merge. Expands range to configured `osc.<seq>` entries. Variant suffix `+range-LO-HI`.
- `brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1` - diff two independent PBFs (no byte-level overlap, full decode both sides). `--from`/`--to` accept snapshot keys or `base`. `--format default|osc`.
- `brokkr suite pbfhogg --bench` - full suite

### Utility commands

- `brokkr check [-- args]` - clippy + tests. Supports `--features`, `--no-default-features`, `-p`. Output minified: one-line summaries with file:line.
- `brokkr env` - hostname, kernel, governor, memory, drives, datasets. Also computes missing hashes.
- `brokkr results` - table of the last 20 results. `brokkr results [UUID]` - specific result.
- `brokkr results [--commit X] [--compare A B] [--command CMD] [--mode M] [--grep STR] [--dataset D] [--meta K=V] [-n N] [--top N]` - query/compare from SQLite. `--command` exact-matches the bare subcommand id (no `bench `/`hotpath ` prefix); `--mode` filters by measurement mode (`bench`/`hotpath`/`alloc`); `--grep STR` substring-matches against both `cli_args` and `brokkr_args` (use it to find runs by flag/axis, e.g. `--grep zstd:1` or `--grep snap-20260411`); `--dataset` substring match on input file; `--meta` exact match, composable with AND.
- `brokkr sidecar <UUID>` - per-phase JSONL summary (default view). Pass `--human` for a fixed-width table.
- `brokkr sidecar <UUID> --samples` - raw /proc samples as JSONL. Composable: `--fields`, `--where`, `--every`, `--head`/`--tail`, `--phase`, `--range`.
- `brokkr sidecar <UUID> --stat FIELD` - min/max/avg/p50/p95 for a /proc field (composes with `--phase`, `--range`, `--where`).
- `brokkr sidecar <UUID> --markers` - raw marker events (JSONL).
- `brokkr sidecar <UUID> --durations` - START/END pair timings. JSONL by default; `--human` for the table.
- `brokkr sidecar <UUID> --counters` - application counters. JSONL by default; `--human` for the table.
- `brokkr sidecar --compare <A> <B>` - phase-aligned delta (JSONL by default; `--human` for the table).
- `brokkr sidecar dirty` - sidecar data from the most recent forced/failed run (even if OOM-killed). UUID is required; `dirty` is the alias for that latest non-DB run.
- All `[sidecar]` narration lines (run provenance, run-index hints) go to stderr so stdout stays pure JSONL for piping into `jq`.
- `brokkr download <region> [--osc-seq N] [--as-snapshot <key> | --refresh] [--force]` - download datasets. Primary PBF: no-op once configured (use `--refresh` to rotate). OSC: rolls forward. Indexed PBF: regenerated on cache miss. `--as-snapshot <key>` registers additional snapshot. `--refresh` archives primary into `snapshot.<key>` and downloads new. Planet uses planet.openstreetmap.org; others use Geofabrik. Short aliases (denmark, europe) or full paths (europe/france).
- `brokkr lock` - check if a command is running. Never run `brokkr check` while the lock is held.
- `brokkr kill [--hard]` - asks the brokkr process holding the lock to wrap up cleanly and exit ASAP.
- `brokkr clean` - remove scratch temp files.
- `brokkr history [--command CMD] [--failed] [--since DATE] [--slow MS] [-n N]` - global command history.
- `brokkr verify <command> [--dataset name]` - cross-validate vs osmium/osmosis/osmconvert. `brokkr verify all` runs all. Default `--dataset denmark`, `--variant indexed`.

### OSC resolver

`--osc-seq` auto-selects only with exactly one `[osc.<seq>]` entry. Multiple OSCs require explicit `--osc-seq N`. Applies to both primary and snapshot-scoped OSC tables.

### Snapshot model

Datasets have one **primary** PBF (`[datasets.<region>.pbf.*]` + `[datasets.<region>.osc.*]`) plus optional named **snapshots** (`[datasets.<region>.snapshot.<key>.*]`). Primary is the default; snapshots addressable via `--snapshot <key>` on `apply-changes`, `merge-changes`, `tags-filter --input-kind osc`, `diff` (including `--format osc`), or `--from`/`--to` on `diff-snapshots`. `--snapshot base` = primary. Key `base` is reserved. Snapshot-scoped runs: find them with `brokkr results --grep <key>` (e.g. `--grep 20260411`); `--snapshot <key>` shows up verbatim in `cli_args`.

`brokkr diff-snapshots` compares two independent PBFs (full decode both sides). `brokkr diff` runs apply-changes internally first (byte-equal blob fast-path). Different code paths, different things measured.

### brokkr.toml schema

- `pbf.<variant>` - PBF files keyed by variant (raw/indexed/altw). `--variant` selects (default: indexed).
- `osc.<seq>` - OSC diff files keyed by seq. `--osc-seq` selects.
- `snapshot.<key>` - named point-in-time captures with own `pbf`/`osc` tables.
- `download_date` - human annotation only, not read by code.
- `xxhash` - XXH128 hash. `brokkr env` shows computed values.

Results in `.brokkr/results.db` (SQLite, tracked in git).

## Indexdata PBFs

Generated by `brokkr download` (passthrough cat after raw PBF lands). Benchmark: `brokkr cat --dataset planet --bench 1`.

Commands requiring indexdata: `apply-changes`, `sort`, `add-locations-to-ways`, `extract --strategy complete|smart`, `tags-filter`, `getid`, `cat --type`, `inspect --tags --type`, `inspect --nodes`, `build-geocode-index`. Use `--force` to run without (slower). `inspect --indexed` checks (exit 0/1).

### add-locations-to-ways index types

| Type | Memory | Temp disk | Best for |
|------|--------|-----------|----------|
| `dense` (default) | ~30+ GB mmap | none | RAM fits working set |
| `sparse` | ~540 MB + 16 GB disk | ~16 GB | memory-constrained, batched lookups |
| `external` | ~8.7 GB | ~112 GB (Europe) | rank-bucketed counting sort, parallel stages. Planet: 1,075s (18 min), 5.4x faster than dense |

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** ONE AT A TIME. Always wait for each to fully complete before starting the next.
- Multiple benchmark runs in an optimization workflow: run sequentially, report between runs.

## Review tool

`review` fans out queries to persistent AI sessions. Configured in `.review.toml`. Please be careful to escape and quote the piped input properly. Reviewers have full access to the source tree and can read any document or code.

Archetypes: `bugs`, `perf`, `arch`, `correctness`, `planet`. Groups: `sweep` = first 4, `everything` = all 5.

```
echo "question" | review perf,bugs      # Archetypes comma-separated
echo "question" | review sweep          # 4 archetypes
echo "question" | review planet         # planet sessions
echo "question" | review everything     # all 5
echo "question" | review perf --anchor  # re-anchor stale session
```

Use `--anchor` with identity reminder for stale/first-use sessions. **Use before implementing major changes.**

Do not use the `review` tool without asking the user first.

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

**Geocode index:** `geocode_index/` - `format.rs` (19-file on-disk format), `reader.rs` (mmap, S2 cell lookup), `builder.rs` (4-pass pipeline).

**Geometry:** `geo.rs` - point-in-polygon, ring assembly, Douglas-Peucker, antimeridian, cos-projection distance.

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
