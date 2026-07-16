# pbfhogg

Rust library and CLI tool for reading and writing OpenStreetMap PBF files.

## Rules

### General rules
- Dont use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten!
- Dont remind the user of CLAUDE.md rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.
- ./notes/* documents are transient. Do not reference them from code comments. Code comments should contain the full context - it will outlive the docs.
- ./reference/* is durable and lives on. Code comments may reference these.
- ./research/* holds full vendored source for related projects, readable from any agent sandbox.
- In ./notes/ documents try to refrain from referencing direct line numbers in the rust source. You can use line numbers, but they drift fast.
- When asked to write a plan or a specification, read `reference/technical-implementation-spec.md` first; it defines what such a document must contain.

### Memory rules
Do not use your Memory functionality. Update CLAUDE.md instead. This project is developed across several hosts and several users. Memories do not transfer across hosts or users. CLAUDE.md does.

### Bash rules
- Never capture stdout into env vars (`UUID=$(...)`).
- Never run raw cargo, curl, pkill. Use `brokkr`.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Hard-wrap the message body at ~72 columns, matching the existing history; the
  subject stays one concise line. The wall-of-text we keep producing comes from
  `git commit -m "<whole paragraph>"`: a single `-m` is recorded as ONE unwrapped
  line. Embed real line breaks so every body line wraps at ~72 (one `-m` per
  paragraph is fine only when each paragraph already carries its own newlines).
  Newlines are not metacharacters, so this composes with the no-metacharacters-in
  `-m` rule (CLAUDE.md Bash rules) - wrap with literal newlines while still
  avoiding braces, brackets, parens, angle brackets and the hash sign.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

## 'brokkr' tool

Dev tool at `~/Programs/brokkr`. Invoked as `brokkr` from project root (reads `./brokkr.toml`).

```
brokkr <command> [--dataset D] [--variant V]   # run once, print timing
brokkr <command> [--dataset D] --bench          # 3 runs, store in DB + sidecar profiler
brokkr <command> [--dataset D] --hotpath        # function-level timing
brokkr <command> [--dataset D] --alloc          # allocation tracking
brokkr <command> [--dataset D] --direct-io      # pass --direct-io to pbfhogg
brokkr <command> [--dataset D] --io-uring       # pass --io-uring to pbfhogg
brokkr <command> [--dataset D] --commit <ref>   # build+bench an old commit in a worktree (baselines)
```

`--commit <ref>` builds and benchmarks a prior commit in brokkr's own git
worktree and stores the result tagged to that commit - pass it to capture a
before/after baseline from your own branch.

Each worktree builds into its own `CARGO_TARGET_DIR`, so `--commit` cells
interleave freely with HEAD cells. The first build per commit is a cold
rebuild; worktrees persist and are reused, so that cost is once per
commit, not per cell (`brokkr clean --worktrees` removes them).

`--stop MARKER` requires a measured mode. Kills process after the named marker. Accepts three spellings: verbatim (`--stop RENUMBER_EXT_STAGE2D_END`), the `-` sigil (`--stop -RENUMBER_EXT_STAGE2D` → resolves to `RENUMBER_EXT_STAGE2D_END`), and the bare-name fallback (`--stop RENUMBER_EXT_STAGE2D` → also resolves to `RENUMBER_EXT_STAGE2D_END`; the sidecar log line shows the resolved form).

Markers are point-in-time bookmarks - the FIFO protocol is `<timestamp_us> <name>` per marker, nothing else. The default `brokkr sidecar <uuid>` view segments the stream between consecutive markers; `brokkr sidecar <uuid> --durations` is the one view that opts into the `FOO_START` / `FOO_END` pairing convention for duration math.

### Sidecar conventions

Brokkr is convention-free at the protocol layer. Two optional naming conventions unlock richer views:

- **`FOO_START` / `FOO_END` marker pairs** drive `--durations` (per-span wall time) and anchor `--stop` alias resolution. Emit both markers around any phase you want to time.
- **`<category>_wait_ns` counters** drive `brokkr sidecar <uuid> --stalls`. The target accumulates blocking time per category into a strictly-monotonic counter (one atomic add per blocking event); `--stalls` takes the max per name, strips the `_wait_ns` suffix for the category, and reports total stall time as a fraction of run wall-clock. That fraction can exceed 100 % when waits are summed across concurrent threads - read it as "average threads parked in this category". The writer's `writer_permit_wait_ns` / `writer_pipeline_send_wait_ns` / `writer_recv_wait_ns` are the worked examples.

  **`WAIT_*` marker pairs do NOT feed `--stalls`** (verified 2026-07-14). This doc previously claimed they did, and pbfhogg's `WAIT_P2_SEND` / `WAIT_S4_SEND` / `WAIT_S4_ROUTER` spans were built against that claim - they are real and useful, but they surface in `--durations` (as collapsed repeated spans), not in `--stalls`. To attribute a blocking point in `--stalls`, emit a `*_wait_ns` counter; the marker pair is for span timing. Emitting both is fine and they answer different questions.

  **Only emit `WAIT_*` pairs on genuine blocks** - gate them behind a `try_send` / `try_recv` fast path, otherwise every non-blocking poll produces a zero-width pair and the FIFO floods.

Both conventions are additive. The default phase summary already shows per-phase user/kernel core split, `majflt`/`minflt` deltas, voluntary/involuntary context switches, and peak thread count - derived from `/proc` samples, so older sidecar rows get the enriched view retroactively on next query.

`--bench N` runs N times, stores best. Default `--bench 3`.

Requires clean git tree (except for `*.md` and `.brokkr/results.db`); `--force` overrides (results not stored).

I/O flags (`--direct-io`, `--io-uring`) create named variants in results. `--force` is a per-subcommand flag (`brokkr <cmd> --force ...`, not `brokkr --force <cmd> ...`).

### pbfhogg commands (every CLI command is a brokkr subcommand)

- `brokkr inspect --tags --dataset denmark` (add `--type way` to narrow, or `--nodes` for node stats)
- `brokkr add-locations-to-ways --dataset europe --index-type external --bench`
- `brokkr build-geocode-index --dataset denmark --hotpath`
- `brokkr multi-extract --dataset japan --regions 5 --bench`
- `brokkr read --bench` - multi-variant read benchmark
- `brokkr cat [--type way|relation] [--dedupe] [--clean] --dataset planet --bench` - no flags = indexdata-generation passthrough (no re-decode). `--type way|relation` filters to one object kind. `--dedupe` runs the two-input dedupe path (supports `--io-uring`). `--clean` forces the full-decode / re-frame Framed path. Flags are orthogonal and combinable.
- `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench` - multi-OSC merge. Expands range to configured `osc.<seq>` entries. Variant suffix `+range-LO-HI`.
- `brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1` - diff two independent PBFs (no byte-level overlap, full decode both sides). `--from`/`--to` accept snapshot keys or `base`. `--format default|osc`.
- `brokkr repack --dataset denmark --bench` - re-encode a PBF with a configurable elements-per-blob cap. Flags: `--elements-per-blob N` (omit for pbfhogg default 8000), `--compression zlib:N|zstd:N|none`, `--snapshot KEY` (read from a registered snapshot), `--as-snapshot KEY` (promote final iteration into the dataset graph as `pbf.indexed`; `--replace-snapshot` to overwrite), `--force-repack` (skip indexdata requirement). Default writes to scratch, overwritten each `--bench` iteration.
- `brokkr degrade --dataset denmark --bench --strip-locations` - produce an adversarial PBF by stripping properties or perturbing structure. Transformation flags compose: `--strip-indexdata` (clear `BlobHeader.indexdata` on every OsmData blob; metadata-only fast path, no element decode), `--strip-locations` (drop `LocationsOnWays`, re-encode ways without inline coords), `--unsort` (clear `Sort.Type_then_ID` and emit one adjacent same-kind blob pair per kind with overlapping ID ranges). At least one transformation flag is required. Other flags: `--snapshot KEY`, `--as-snapshot KEY` (promotes to `pbf.raw` if `--strip-indexdata` else `pbf.indexed`; `--replace-snapshot` to overwrite), `--compression zlib:N|zstd:N|none`.
- `brokkr suite pbfhogg --bench` - full suite

### Utility commands

- `brokkr check [--profile <name>] [-- args]` - clippy + tests across every `[[check]]` sweep in `brokkr.toml`. Default profile is `tier1` (fast contracts; skips `tier2::`/`tier3::`/`platform::`/`serial::` modules). Other profiles: `full` (tier 3, includes ignored tests), `platform` (`--direct-io`/`--io-uring` tests), `serial` (`--test-threads=1`), `sort` (per-command tier-2 precedent). Each sweep rebuilds `pbfhogg-cli` with matching features via `build_packages`; `BROKKR_TEST_BIN_DIR` is exported so test code finds the right binary. Output minified: one-line summaries with file:line.
- `brokkr test <NAME> [--sweep <sweep>] [--timeout <secs>] [--raw]` - run one integration test by exact name with `--include-ignored` + `--nocapture` + single-threaded. Builds release unless `[test] debug = true` in `brokkr.toml` (debug builds compile much faster for iteration; compute-heavy tests run slower). Runs every `[[check]]` sweep configured in `brokkr.toml` (today: `all` + `consumer`) unless `--sweep <name>` narrows to one. Per-test hang watchdog defaults to 20 s; `--timeout` raises it, range 20-280. Footer prints `[test] PASS/FAIL` with wall time per sweep; `NO MATCH` indicates the name filter matched zero tests under that sweep (useful when a test is `#[cfg]`-gated behind a feature absent from the consumer build). `--raw` streams the unfiltered cargo output. Example: `brokkr test merge_cross_validate_osmium --sweep all --timeout 120`.
- **The 20 s per-test watchdog is fixed on `brokkr check`** - `--sweep`/`--timeout` exist only on `brokkr test`, and check profiles have no timeout override. Consequence: no test that runs longer than 20 s can ever pass any `brokkr check`. The `full` profile therefore skips the over-watchdog tests BY NAME in its brokkr.toml skip list (the two osmium cross-validations, `roundtrip_denmark` ~54 s, the six `geocode_index` real-data tests ~154 s) - keep that list in sync when adding slow `#[ignore]` tests - and those tests are exercised individually via `brokkr test <name> --timeout <secs>` (the release-gate workflow). A watchdog kill reports the victim as "did not finish within 20s" with a `futex_do_wait` wchan snapshot in `.brokkr/test-hung/`, which looks like a deadlock but usually just means the test is slower than the watchdog. Note also that `brokkr check -- <args>` passes args to *cargo*, not libtest - per-test skips cannot be injected from the CLI, which is why the skip list lives in the profile.
- `brokkr --version` - stamps brokkr's own git hash, a `-dirty` suffix, and build time. brokkr installs via `cargo install --path`, so when brokkr behaves unlike its source, check this first: a stale installed binary is the usual answer, and reinstalling beats working around a bug that is already fixed.
- `brokkr env` - hostname, kernel, governor, memory, drives, datasets. Also computes missing hashes.
- `brokkr results` - table of the last 20 results. `brokkr results [UUID]` - specific result.
- `brokkr results [--commit X] [--compare A B] [--command CMD] [--mode M] [--grep STR] [--grep-v STR] [--dataset D] [--meta K=V] [--env K=V] [-n N] [--top N]` - query/compare from SQLite. `--command` substring-matches; `--mode` filters by measurement mode (`bench`/`hotpath`/`alloc`); `--dataset` substring match on input file; `--meta` / `--env` exact match, composable with AND.
  - `--grep STR` substring-matches against both `cli_args` and `brokkr_args`, repeatable with AND semantics (`--grep apply-changes --grep zstd:1`).
  - `--grep-v STR` excludes; repeatable, excludes on ANY match; composes with `--grep`. **This is how you select the arm of an A/B distinguished only by an absent flag** - `--grep add-locations-to-ways --grep-v inject-prepass` gets the flag-OFF arm, which `--grep` alone cannot express. Both work with `--compare`.
- `brokkr results <uuid>` renders per-iteration walls for `--bench N` runs. **The iteration order is the diagnostic**: iteration 1 slow then 2/3 fast is a cold page cache; iteration 1 fast then 3 slow is drive-state exhaustion. These are opposite diagnoses, so never read a `--bench N` row as a single number when the walls disagree.
- `brokkr sidecar <UUID>` - per-phase JSONL summary (default view). Pass `--human` for a fixed-width table.
- `brokkr sidecar <UUID> --run N|all` - pick an iteration within a `--bench N` result (default: the best run). **The sidecar stores every iteration even though older `results` rows kept only the best wall** - this is how you recover a cold-vs-warm story from an already-recorded bench.
- `brokkr sidecar <UUID> --samples` - raw /proc samples as JSONL. Composable: `--fields`, `--where`, `--every`, `--head`/`--tail`, `--phase`, `--range`.
- `brokkr sidecar <UUID> --stat FIELD` - min/max/avg/p50/p95 for a /proc field (composes with `--phase`, `--range`, `--where`).
- `brokkr sidecar <UUID> --markers` - raw marker events (JSONL).
- `brokkr sidecar <UUID> --durations` - START/END pair timings. JSONL by default; `--human` for the table. Under `--human`, high-cardinality repeated spans collapse into one row with count/total/min/avg/max (keyed on observed cardinality, not on any name convention; disabled under `--run all`). **The min/max on a collapsed row is often the finding** - e.g. `SCHEDULE_SCAN_LOOP x3 min 22.6ms max 3087.4ms` is a cold walk and two warm ones in a single line. JSONL keeps one object per span.
- `brokkr sidecar <UUID> --counters [--grep SUBSTR]` - application counters. `--grep` keeps only counters whose name contains the substring; without it a run emitting a progress counter every 64 blobs buries the lines that matter. JSONL by default; `--human` for the table.
- `brokkr sidecar --compare <A> <B>` - phase-aligned delta (JSONL by default; `--human` for the table). Annotates host differences (memory / governor / kernel) between the two runs - **check it first**, since available RAM has explained more of our cross-run deltas than code has.
- `brokkr sidecar dirty` - sidecar data from the most recent forced/failed run (even if OOM-killed). UUID is required; `dirty` is the alias for that latest non-DB run.
- All `[sidecar]` narration lines (run provenance, run-index hints) go to stderr so stdout stays pure JSONL for piping into `jq`.
- `brokkr download <region> [--osc-seq N] [--as-snapshot <key> | --refresh] [--force]` - download datasets. Primary PBF: no-op once configured (use `--refresh` to rotate). OSC: rolls forward. Indexed PBF: regenerated on cache miss. `--as-snapshot <key>` registers additional snapshot. `--refresh` archives primary into `snapshot.<key>` and downloads new. Planet uses planet.openstreetmap.org; others use Geofabrik. Short aliases (denmark, europe) or full paths (europe/france).
- `brokkr lock` - check if a command is running. Never run `brokkr check` while the lock is held.
- `brokkr kill [--hard]` - asks the brokkr process holding the lock to wrap up cleanly and exit ASAP.
- `brokkr clean` - remove scratch temp files.
- `brokkr history [--command CMD] [--failed] [--since DATE] [--slow MS] [-n N]` - global command history.
- `brokkr verify <command> [--dataset name] [--snapshot key]` - cross-validate vs osmium/osmosis/osmconvert. `brokkr verify all` runs all. Default `--dataset denmark`, `--variant indexed`. `--snapshot` conflicts with explicit `--input`; OSC resolution for change-consuming verifies is three-way (see the snapshot model section). For commands with multiple modes (e.g. `add-locations-to-ways` has `--mode hash|sparse|external|all`, default `all`), pass the explicit mode to skip the others - e.g. `brokkr verify add-locations-to-ways --dataset denmark --mode sparse` runs only the sparse path against the hash reference. Brokkr may still accept `--mode dense` in its CLI surface; that maps to `pbfhogg --index-type dense` which errors out since dense was removed in commit (see `notes/altw.md`).

### OSC resolver

`--osc-seq` auto-selects only with exactly one `[osc.<seq>]` entry. Multiple OSCs require explicit `--osc-seq N`. Applies to both primary and snapshot-scoped OSC tables.

### Snapshot model

Datasets have one **primary** PBF (`[datasets.<region>.pbf.*]` + `[datasets.<region>.osc.*]`) plus optional named **snapshots** (`[datasets.<region>.snapshot.<key>.*]`). Primary is the default; snapshots addressable via `--snapshot <key>` on `apply-changes`, `merge-changes`, `tags-filter --input-kind osc`, `diff` (including `--format osc`), `--from`/`--to` on `diff-snapshots`, and as input to `repack` / `degrade`. `--snapshot base` = primary. Key `base` is reserved. Snapshot-scoped runs: find them with `brokkr results --grep <key>` (e.g. `--grep 20260411`); `--snapshot <key>` shows up verbatim in `cli_args`.

**Producer side** (`--as-snapshot KEY` on `download`, `repack`, `degrade`): writes the artifact into the dataset graph as a new `[snapshot.<key>]` block. `repack` always writes `pbf.indexed`. `degrade` writes `pbf.raw` if `--strip-indexdata` is set, otherwise `pbf.indexed`. `--replace-snapshot` overwrites an existing key (without it, an existing key is a hard error). The pbfhogg child runs to completion *before* the existence check, so omitting `--replace-snapshot` on an existing key wastes the build.

**Consumer-side `--snapshot` is complete** (brokkr commit `e635f5b`, confirmed 2026-07-10): every consumer command (`sort`, `inspect`, `cat`, `getid`, `getparents`, `check-refs`, `check-ids`, `renumber`, `time-filter`, `add-locations-to-ways`, `build-geocode-index`, `tags-filter` PBF and OSC, `extract`, `multi-extract`) accepts `--snapshot KEY`, resolved centrally with `--variant` composition, xxhash verification, and hard error on unknown keys. Snapshot runs are greppable via `brokkr results --grep <key>` (the flag lands verbatim in `brokkr_args`). `brokkr verify <cmd> --snapshot <key>` and `brokkr read --snapshot <key>` are also supported (brokkr 2026-07-10, commit `90749cc` + OSC-resolution follow-up); only the synthetic `bench-write`/`bench-merge` commands remain snapshot-unaware. Change-consuming verifies (merge / derive-changes / diff / all) resolve OSC three-way: snapshot-scoped when the snapshot has its own osc table (point-in-time snapshots), base fallback when it does not (encoding-only snapshots from repack/degrade - the base chain is the logically-correct diff stream for a same-sequence re-encode), narrated on stderr whenever `--snapshot` was passed. `verify all` skips merge/derive cleanly on snapshots with no resolvable OSC.

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

Commands requiring indexdata: `apply-changes`, `sort`, `add-locations-to-ways`, `extract --strategy complete|smart`, `tags-filter`, `getid`, `cat --type`, `inspect --tags --type`, `inspect --nodes`, `build-geocode-index`, `repack`. Use `--force` to run without (slower; for `repack` the flag is `--force-repack` to disambiguate from brokkr's own `--force`). `inspect --indexed` checks (exit 0/1).

### add-locations-to-ways index types

| Type | Memory | Temp disk | Best for |
|------|--------|-----------|----------|
| `sparse` (default) | ~540 MB + IdSet/rank index | `referenced_count * 8` bytes (japan 2 GB, europe ~29 GB) | rank-indexed flat mmap; small to europe scale |
| `external` | ~8.7 GB | ~256 GB (planet) | rank-bucketed counting sort, parallel stages; the only mode that survives at planet on 30 GB-class hosts |
| `auto` | (one of the above) | (one of the above) | scale-aware: sparse unless sorted+indexed AND estimated node store (nodes x 8 B from indexdata) exceeds 80 % of available RAM; falls back to external when the estimate is unavailable (reference/pipeline.md) |

`dense` was removed - sparse rank-indexed flat is faster than the prior dense path at every measured scale and works in regimes dense didn't (europe survives at ~6 minutes on a 27 GB-RAM host). See `notes/altw.md` "Don't re-attempt" and "Status".

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** ONE AT A TIME. Always wait for each to fully complete before starting the next.
- Multiple benchmark runs in an optimization workflow: run sequentially, report between runs.
- **Read the phase split before calling any delta a regression.** `brokkr sidecar <uuid> --human` attributes per-phase disk read, majflt and cores. Planet wall deltas are routinely environmental; one sidecar read regularly saves a bisect.
- **Interleave matched A/B cells.** `--bench N` is best-of-N *within* one cell, so it cannot cancel drift *between* cells - two adjacent `--bench 3` cells are still confounded. Alternate `--bench 1` cells, compare medians, and check sign consistency across pairs.
- **A same-day matched pair beats any historical number.** Old baselines were recorded under uncontrolled drive and page-cache state; when they disagree with a fresh matched pair, retire the historical figure rather than defend it.
- **Trim before a matched A/B suite** (`sudo fstrim -av`). The weekly fstrim timer cannot keep up with planet-scale scratch churn, and accumulated trim debt widens the drive-state band that interleaving has to cancel. "Drive state" in these docs means trim debt + SLC/GC behaviour on a near-full healthy drive (SMART-verified), never failing hardware.

Drive-state and cache-state evidence, with dates and UUIDs, lives in [`reference/performance.md`](reference/performance.md) - it is volatile and does not belong here.

## Workspace
Cargo workspace: **`pbfhogg`** (root, library) + **`pbfhogg-cli`** (`cli/`, binary, produces `pbfhogg`). Library users: `default-features = false` skips `commands` feature. `pbfhogg-cli` carries a no-op `commands` feature so brokkr can apply `--features commands` symmetrically across both crates - the CLI's lib dep already always pulls in `pbfhogg/commands`.

CLI-driven integration tests live in `tests/cli_*.rs` and drive the compiled `pbfhogg` binary via `CliInvoker` (`tests/common/cli.rs`). The CliInvoker resolves the binary via `BROKKR_TEST_BIN_DIR` first (set by brokkr per sweep), falling back to `CARGO_TARGET_DIR + cfg!(debug_assertions)` for plain `cargo test` runs.

`cargo clippy --all-targets` can cache and miss CLI crate violations. After changing `cli/src/main.rs`, verify with `brokkr check` (the `all` sweep includes `pbfhogg-cli` via `build_packages`).

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
- Tests: `tests/` (~30 files: `cli_*.rs` CLI-driven, stable-API integration tests, `fault_*.rs` per-binary fault injection) + `cli/tests/cli.rs` + inline unit tests in `src/**/*.rs`. Validation tier model in [`reference/testing.md`](reference/testing.md): tier 1 at file root, tier 2/3 in `mod tier2`/`mod tier3` submodules, platform-gated tests in `mod platform`, serial in `mod serial`. New `cli_*.rs` files use `CliInvoker` and the stable allowlist (`block_builder`, `writer`, `BlobReader`, `Element`, etc.) so internal-module rewrites don't force test edits.

## Features (library crate)
- `commands` (default): `check_refs`, `extract`, geocode builder + deps (`roaring`, `serde_json`, `s2`)
- `geocode-reader`: `geocode_index::Reader` for reverse geocoding (depends on `s2`). Included by `commands`.
- `linux-direct-io`: O_DIRECT read/write (requires `libc`)
- `linux-io-uring`: io_uring writer (requires `io-uring` + `libc`, Linux 5.1+)

Zlib backend: `zlib-rs` (pure Rust, no C). No feature flags for backend selection.
