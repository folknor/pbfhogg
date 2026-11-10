# pbfhogg

Rust library and CLI tool for reading and writing OpenStreetMap PBF files.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.
- ./research/* holds full vendored source for related projects, readable from any agent sandbox. Competitor reference sources available for research: `research/libosmium/` (C++ OSM library, the reference implementation), `research/osmium-tool/` (C++ CLI on libosmium, the direct competitor and `brokkr verify` oracle), `research/traccar-geocoder/` (reverse geocoder, the geocode_index precedent). `research/geoparquet/` is the GeoParquet spec repository (spec, JSON schema, examples, test data) - a standards reference, not a competitor.
- When asked to write a plan or a specification, read `reference/technical-implementation-spec.md` first; it defines what such a document must contain.

### Bash rules

- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`.
- Never run the `pbfhogg` binary directly (`./target/release/pbfhogg` or any other path), even where the allowlist permits it. Every pbfhogg run goes through `brokkr`.

## 'brokkr' tool

Dev tool at `~/Programs/brokkr`. Invoked as `brokkr` from project root (reads `./brokkr.toml`, which includes the files in `brokkr.d/`: `checks.toml` for sweeps and test profiles, `lints.toml` for text lints and script checks, `benches.toml` for the script-bench registry, `hosts/<hostname>.toml` for each host's paths and datasets; "brokkr.toml" below means the composed config).

Script benches (`brokkr bench NAME`, `brokkr man bench`): each is a `[bench.NAME]` table in `brokkr.d/benches.toml` with `description`/`version`/`date`/`changelog`, its Lua in `benches/<family>/<NAME>.lua` (family = the pbfhogg command driven, or `compare` for cross-tool benches; NAME = file stem, globally unique, no family prefix). Never inline a script. The header of `brokkr.d/benches.toml` holds the full convention.

```sh
brokkr bench [--iterations N] [--hotpath | --alloc] [--commit REF] NAME --dataset D
brokkr bench --dry-run NAME --dataset D
brokkr bench NAME --help
```

Runner options go before NAME; script params go after it. `--iterations N`
defaults to 3 and stores the fastest iteration. `--hotpath` profiles functions;
`--alloc` profiles allocations. `--commit REF` builds an old revision in a
persistent worktree with its own target directory, using the current scripts.
`--dry-run` validates without building or measuring. See `brokkr man bench`.

Params and supported modes are declared in `brokkr.d/benches.toml` and each
script's header. Boolean params take `yes` or `no`, for example
`--direct-io yes` or `--io-uring yes` where supported. Marker stops are a
script param: `--stop RENUMBER_EXT_STAGE2D_END`, `--stop -RENUMBER_EXT_STAGE2D`
and `--stop RENUMBER_EXT_STAGE2D` resolve to the same end marker. Use
`--stop =MARKER` for an exact point marker name (or any exact marker name),
without appending `_END`; brokkr matches the resolved name verbatim with no
fallback. Stopped outputs are incomplete; consult the bench help for stop support.

Runs are stored even for dirty trees, marked dirty and excluded from
comparisons. There is no runner `--force` option. Markdown and config edits
are excluded from the measurement dirty policy. Script results live in the
per-user bench store, read through `brokkr results`; older typed results
remain in `.brokkr/results.db`.

### Sidecar conventions

Brokkr is convention-free at the protocol layer. Two optional naming conventions unlock richer views:

- **`FOO_START` / `FOO_END` marker pairs** drive `--durations` (per-span wall time) and anchor `--stop` alias resolution. Emit both markers around any phase you want to time.
- **`<category>_wait_ns` counters** drive `brokkr sidecar <uuid> --stalls`. The target accumulates blocking time per category into a strictly-monotonic counter (one atomic add per blocking event); `--stalls` takes the max per name, strips the `_wait_ns` suffix for the category, and reports total stall time as a fraction of run wall-clock. That fraction can exceed 100 % when waits are summed across concurrent threads - read it as "average threads parked in this category". The writer's `writer_permit_wait_ns` / `writer_pipeline_send_wait_ns` / `writer_recv_wait_ns` are the worked examples.

  **`WAIT_*` marker pairs do NOT feed `--stalls`** (verified 2026-07-14). This doc previously claimed they did, and pbfhogg's `WAIT_P2_SEND` / `WAIT_S4_SEND` / `WAIT_S4_ROUTER` spans were built against that claim - they are real and useful, but they surface in `--durations` (as collapsed repeated spans), not in `--stalls`. To attribute a blocking point in `--stalls`, emit a `*_wait_ns` counter; the marker pair is for span timing. Emitting both is fine and they answer different questions.

  **Only emit `WAIT_*` pairs on genuine blocks** - gate them behind a `try_send` / `try_recv` fast path, otherwise every non-blocking poll produces a zero-width pair and the FIFO floods.

Both conventions are additive. The default phase summary already shows per-phase user/kernel core split, `majflt`/`minflt` deltas, voluntary/involuntary context switches, and peak thread count - derived from `/proc` samples, so older sidecar rows get the enriched view retroactively on next query.

### pbfhogg benches

Bare `brokkr bench` lists the registered benches with their versions and
short descriptions. Use `brokkr bench NAME --help` for params.

Examples of combined params:

```sh
brokkr bench file-info --dataset denmark --mode tags --type way
brokkr bench merge-diffs --dataset planet --osc 4914..4920
brokkr bench --iterations 1 diff-pair --dataset planet --from base --to 20260411
```

`file-info --mode nodes` reads node statistics. Its tags mode suppresses row
printing with a high minimum count, matching the former typed workload.
`copy-pbf` defaults to indexdata-generation passthrough; `--clean yes` strips
version while re-encoding, `--type way|relation` filters, and `--dedupe yes`
merges the input with itself. Dedupe conflicts with cleaning and type filtering;
io_uring requires dedupe. `merge-diffs --osc` accepts sequences, lists and ranges.
`perturb-pbf` requires at least one of `--strip-indexdata yes`,
`--strip-locations yes` or `--unsort yes`. Snapshot promotion is not supported
by these scripts; see the snapshot model below.

`inspect` describes a file; `check` verifies claims and invariants. Flags that
answer "is this file right?" belong on `check`; descriptive flags on `inspect`.
`brokkr suite pbfhogg --bench` remains a generic suite command.

### Utility commands

- `brokkr check [--profile <name>] [-- args]` - clippy + tests across every `[[check]]` sweep in `brokkr.toml`. Default profile is `tier1` (fast contracts; skips `tier2::`/`tier3::`/`serial::` modules; the `--direct-io`/`--io-uring` tests are tier 1 and skip where the host refuses the feature). Other profiles: `full` (tier 3, includes ignored tests), `serial` (`--test-threads=1`), `sort` (per-command tier-2 precedent). The sweeps that run CLI tests (`all`, `consumer`, `musl`) select both packages, and cargo builds the `pbfhogg` binary those tests drive with the sweep's own features. Output minified: one-line summaries with file:line.
- `brokkr test <NAME> [--sweep <sweep>] [--timeout <secs>]` - run one integration test by exact name with `--include-ignored` + `--nocapture` + single-threaded. Builds release unless `[test] debug = true` in `brokkr.toml` (debug builds compile much faster for iteration; compute-heavy tests run slower). Runs every `[[check]]` sweep configured in `brokkr.toml` unless `--sweep <name>` narrows to one. Per-test hang watchdog defaults to 20 s; `--timeout` raises it, range 20-280. Footer prints `[test] PASS/FAIL` with wall time per sweep; `NO MATCH` indicates the name filter matched zero tests under that sweep (useful when a test is `#[cfg]`-gated behind a feature absent from the consumer build). It searches one package, `pbfhogg` unless `-p` says otherwise: CLI-driving tests live in `pbfhogg-cli` and need `-p pbfhogg-cli`. Example: `brokkr test -p pbfhogg-cli sort_cross_validate_osmium --sweep all --timeout 120`.
- **The 20 s per-test watchdog is fixed on `brokkr check`** - `--sweep`/`--timeout` exist only on `brokkr test`, and check profiles have no timeout override. Consequence: no test that runs longer than 20 s can ever pass any `brokkr check`. The `full` profile therefore skips the over-watchdog tests BY NAME in its skip list in `brokkr.d/checks.toml` - keep that list in sync when adding slow `#[ignore]` tests - and those tests are exercised individually via `brokkr test <name> --timeout <secs>` (`brokkr test -p pbfhogg-cli <name> --timeout <secs>` for a CLI-driving test, which a plain `brokkr test <name>` reports as no match). A watchdog kill reports the victim as "did not finish within 20s" with a `futex_do_wait` wchan snapshot in `.brokkr/test-hung/`, which looks like a deadlock but usually just means the test is slower than the watchdog. Note also that `brokkr check -- <args>` passes args to *cargo*, not libtest - per-test skips cannot be injected from the CLI, which is why the skip list lives in the profile.
- `brokkr --version` - stamps brokkr's own git hash, a `-dirty` suffix, and build time. brokkr installs via `brokkr install`, so when brokkr behaves unlike its source, check this first: a stale installed binary is the usual answer, and reinstalling beats working around a bug that is already fixed.
- `brokkr env` - hostname, kernel, governor, memory, drives, datasets. Also computes missing hashes.
- `brokkr results` - table of the last 20 results. `brokkr results [UUID]` - specific result.
- `brokkr results [--commit X] [--compare A B] [--command CMD] [--mode M] [--grep STR] [--grep-v STR] [--dataset D] [--meta K=V] [--env K=V] [--top N]` - query/compare from SQLite. `--command` substring-matches; `--mode` filters by measurement mode (`bench`/`hotpath`/`alloc`); `--dataset` substring match on input file; `--meta` / `--env` exact match, composable with AND.
  - `--grep STR` substring-matches against both `cli_args` and `brokkr_args`, repeatable with AND semantics (`--grep apply-changes --grep zstd:1`).
  - `--grep-v STR` excludes; repeatable, excludes on ANY match; composes with `--grep`. **This is how you select the arm of an A/B distinguished only by an absent flag** - `--grep add-locations-to-ways --grep-v inject-prepass` gets the flag-OFF arm, which `--grep` alone cannot express. Both work with `--compare`.
- `brokkr results <uuid>` renders per-iteration walls for `--iterations N` runs. **The iteration order is the diagnostic**: iteration 1 slow then 2/3 fast is a cold page cache; iteration 1 fast then 3 slow is drive-state exhaustion. These are opposite diagnoses, so never read a `--iterations N` row as a single number when the walls disagree.
- `brokkr sidecar <UUID>` - per-phase JSONL summary (default view). Pass `--human` for a fixed-width table.
- `brokkr sidecar <UUID> --run N|all` - pick an iteration within a `--iterations N` result (default: the best run). **The sidecar stores every iteration even though older `results` rows kept only the best wall** - this is how you recover a cold-vs-warm story from an already-recorded bench.
- `brokkr sidecar <UUID> --samples` - raw /proc samples as JSONL. Composable: `--fields`, `--where`, `--every`, `--head`/`--tail`, `--phase`, `--range`.
- `brokkr sidecar <UUID> --stat FIELD` - min/max/avg/p50/p95 for a /proc field (composes with `--phase`, `--range`, `--where`).
- `brokkr sidecar <UUID> --markers` - raw marker events (JSONL).
- `brokkr sidecar <UUID> --durations` - START/END pair timings. JSONL by default; `--human` for the table. Under `--human`, high-cardinality repeated spans collapse into one row with count/total/min/avg/max (keyed on observed cardinality, not on any name convention; disabled under `--run all`). **The min/max on a collapsed row is often the finding** - e.g. `SCHEDULE_SCAN_LOOP x3 min 22.6ms max 3087.4ms` is a cold walk and two warm ones in a single line. JSONL keeps one object per span.
- `brokkr sidecar <UUID> --counters [--grep SUBSTR]` - application counters. `--grep` keeps only counters whose name contains the substring; without it a run emitting a progress counter every 64 blobs buries the lines that matter. JSONL by default; `--human` for the table.
- `brokkr sidecar --compare <A> <B>` - phase-aligned delta (JSONL by default; `--human` for the table). Annotates host differences (memory / governor / kernel) between the two runs - **check it first**, since available RAM has explained more of our cross-run deltas than code has.
- `brokkr sidecar dirty` - sidecar data from the most recent forced/failed run (even if OOM-killed). UUID is required; `dirty` is the alias for that latest non-DB run.
- All `[sidecar]` narration lines (run provenance, run-index hints) go to stderr so stdout stays pure JSONL for piping into `jq`.
- `brokkr download <region> [--osc-seq N] [--as-snapshot <key> | --refresh] [--force]` - download datasets. Primary PBF: no-op once configured (use `--refresh` to rotate). OSC: rolls forward. Indexed PBF: regenerated on cache miss. `--as-snapshot <key>` registers additional snapshot. `--refresh` archives primary into `snapshot.<key>` and downloads new. Planet uses planet.openstreetmap.org; others use Geofabrik. Short aliases (denmark, europe) or full paths (europe/france).
- `brokkr lock` - show which command holds brokkr's machine-wide lock. Every brokkr command queues behind that lock on its own, so there is no need to check it before running anything. The lock is the point of brokkr: it is held machine-wide so that development work in any project never stalls on work in another.
- `brokkr kill [--hard]` - asks the brokkr process holding the lock to wrap up cleanly and exit ASAP.
- `brokkr clean` - remove scratch temp files.
- `brokkr history [--command CMD] [--failed] [--since DATE] [--slow MS] [-n N]` - global command history.
- `brokkr verify <command> [--dataset name] [--snapshot key]` - cross-validate vs osmium/osmosis/osmconvert. `brokkr verify all` runs all. Default `--dataset denmark`, `--variant indexed`. `--snapshot` conflicts with explicit `--input`; OSC resolution for change-consuming verifies is three-way (see the snapshot model section). For commands with multiple modes (e.g. `add-locations-to-ways` has `--mode hash|sparse|external|all`, default `all`), pass the explicit mode to skip the others - e.g. `brokkr verify add-locations-to-ways --dataset denmark --mode sparse` runs only the sparse path against the hash reference. Brokkr may still accept `--mode dense` in its CLI surface; that maps to `pbfhogg --index-type dense` which errors out since dense was removed in commit (see `notes/altw.md`).

### OSC resolver

`apply-diff` and `diff-applied` use `--osc N`; omission requires exactly one
entry in the selected snapshot's OSC table. `merge-diffs --osc` is required
and supports lists, repeated flags, `all` and inclusive ranges.
`tag-select` defaults to PBF input with `--input indexed`;
`tag-select --input-kind osc --input N` requires an explicit sequence.
`merge-workload --osc-seq N` selects from the primary OSC table only.
Generic commands retain their own OSC options; see their help.

### Snapshot model

Datasets have a primary PBF (`[datasets.<region>.pbf.*]`) and OSC table,
plus optional `[datasets.<region>.snapshot.<key>.*]` tables. `base` is reserved
for the primary. Snapshot-aware scripts take `--snapshot KEY`, usually with
`--variant`; `tag-select` uses `--input` for the PBF variant (default
`indexed`) or an explicit OSC sequence, scoped by `--input-kind`.
`diff-pair` takes `--from` and `--to`; non-indexed pairs require both
`--variant` and `--to-variant` set to the same key. `encode-workload` and
`merge-workload` remain primary-only. Bound params and input digests are
stored with script results; inspect them with `brokkr results UUID`.

**Producer gap (brokkr ticket 145):** script outputs are iteration-owned
scratch. Snapshot promotion via `--as-snapshot` and `--replace-snapshot`
still needs the typed repack/degrade producer until persistent output and
registration land. There is no equivalent bench invocation yet. Repack
registers `pbf.indexed`; degrade registers `pbf.raw` when stripping indexdata,
otherwise `pbf.indexed`. `brokkr download --as-snapshot KEY` remains available.

`brokkr verify <cmd> --snapshot KEY` remains available. Change-consuming
verifies use the snapshot OSC table if present, otherwise the primary OSC
table for encoding-only snapshots; they narrate fallback on stderr.
`verify all` skips merge/derive if no OSC resolves. The script benches use
only the selected snapshot's OSC table, without this fallback.

`diff-pair` compares independent PBFs with full decode on both sides.
`diff-applied` first applies changes, then uses the byte-equal blob fast path.

### brokkr.toml schema

- `pbf.<variant>` - PBF files keyed by variant (raw/indexed/altw). `--variant` selects (default: indexed).
- `osc.<seq>` - OSC diff files keyed by seq. `--osc` selects in apply/diff benches; `tag-select` uses `--input`, and `merge-workload` uses `--osc-seq`.
- `snapshot.<key>` - named point-in-time captures with own `pbf`/`osc` tables.
- `download_date` - human annotation only, not read by code.
- `xxhash` - XXH128 hash. `brokkr env` shows computed values.
- `pbf.altw-keep-untagged` - the LocationsOnWays base `brokkr verify all` runs its `apply-changes -l` check on (built with `--keep-untagged-nodes` from the same base the dataset's OSCs chain from; absent, the check is skipped). No brokkr producer registers it: build it with `brokkr run pbfhogg -- add-locations-to-ways ...` and add the block by hand with the hash `brokkr env` prints.
- `bbox` (dataset level) - default box for `extract` / `multi-extract` benches and `verify`; absent, verify skips extract and multi-extract. Choose it for verify cost on the host, not only for lying inside the region.
- `getid_ids` / `getparents_ids` (dataset level) - IDs the getid/getparents benches request; absent, brokkr's built-in Denmark IDs. A configured list qualifies the result rows' pairing key. `brokkr man datasets` has the full rules.

Legacy typed results are in `.brokkr/results.db` (SQLite, tracked in git); script results use the per-user bench store.

## Indexdata PBFs

Generated by `brokkr download` (passthrough cat after raw PBF lands). Benchmark: `brokkr bench --iterations 1 copy-pbf --dataset planet`.

Commands requiring indexdata: the per-command truth is the "Proceeding without indexdata" table in `docs/cli/commands.md`. For benchmarks, use the script-specific fallback param, such as `sort-pbf --force-sort yes`, `embed-locations --force-altw yes` or `reencode-pbf --force-repack yes`. `geocode-build` always enables its forced fallback. See each bench help. Use `brokkr bench index-integrity` to verify indexdata against payloads.

### add-locations-to-ways index types

| Type | Memory | Temp disk | Best for |
|------|--------|-----------|----------|
| `sparse` | Depends on the referenced-node working set | `referenced_count * 8` bytes of scratch storage | rank-indexed flat mmap; see [reference/performance/add-locations-to-ways.md](reference/performance/add-locations-to-ways.md) for measurements |
| `external` | One ID or slot bucket per stage worker, not a node-ID-wide index; the mode that fits planet on a 30 GB-class host | High scratch-disk demand; see [reference/performance/add-locations-to-ways.md](reference/performance/add-locations-to-ways.md) for measurements | ID- and slot-position-bucketed join; requires sorted input with indexdata |
| `auto` (default) | (one of the above) | (one of the above) | Runtime routing, sparse unless a known page-cache budget is exceeded; see [reference/pipeline.md](reference/pipeline.md) |

`dense` was removed. See [reference/performance/add-locations-to-ways.md](reference/performance/add-locations-to-ways.md) for measured comparisons with the rank-indexed sparse path.

## Benchmarking Rules
- **Measure what the decision needs, not a survey.** A handful of matched cells on the commands the change touches. A sweep across many commands (or more than about ten planet cells for one item) needs the maintainer's go-ahead first: planet cells cost minutes each and the maintainer's time is the budget. Record every cell run, with its conclusion, in the command's file under `reference/performance/` (indexed by its `README.md`), so no survey is ever repeated.
- **A recorded negative result is a ruling, not a hypothesis.** Before measuring anything, search git history (`git log -S`, `git log --grep`) and the command's file under `reference/performance/`, History section included, for earlier attempts at the same knob or shape. If one exists, its result stands: re-measuring needs a named change in the code since then that plausibly overturns it, and the maintainer's go-ahead. Allocator tuning (`mallopt`, mmap and trim thresholds, arena caps, `malloc_trim`) has been tried several times and is settled at the `set_mmap_threshold` doc in `crates/lib/src/process.rs`.
- **Ignore performance differences below 2-3 %.** They are noise for every decision here: not a regression to fix, not a win to claim, not a reason to run more cells. Do not chase them with extra benchmark runs or code-layout tweaks (`#[inline]` attributes and the like).
- **Modest memory at planet scale is not a reason to hold back a large wall win.** Report the peak; do not trade the win away for it.
- **NEVER run benchmark, profiling, or verify commands in parallel.** ONE AT A TIME. Always wait for each to fully complete before starting the next.
- **Never run `brokkr verify` on any dataset larger than germany.** Planet, europe and north-america are bench-only; correctness on real data is checked on denmark and germany.
- Multiple benchmark runs in an optimization workflow: run sequentially, report between runs.
- **Read the phase split before calling any delta a regression.** `brokkr sidecar <uuid> --human` attributes per-phase disk read, majflt and cores. Planet wall deltas are routinely environmental; one sidecar read regularly saves a bisect.
- **Interleave matched A/B cells.** `--iterations N` is best-of-N *within* one cell, so it cannot cancel drift *between* cells - two adjacent `--iterations 3` cells are still confounded. Alternate `--iterations 1` cells, compare medians, and check sign consistency across pairs.
- **A same-day matched pair beats any historical number.** Old baselines were recorded under uncontrolled drive and page-cache state; when they disagree with a fresh matched pair, retire the historical figure rather than defend it.
- **Balance what precedes each arm, not only the arm order.** A planet cell that follows a scratch-heavy cell (altw external, apply-changes, merge-changes) can run tens of seconds slower than the first cell after a trim or a rest, on identical code. When run-history effects appear, give each arm the same predecessor workload and the same first-after-trim position; alternation alone does not do it when the arms write different scratch volumes.
- **Read a span from its START/END pair, not from a segment row.** `brokkr sidecar --human` and `--compare` label each segment by the marker that begins it, so a row named `FOO_END` measures what ran after FOO, not FOO; `--durations` gives the span itself.
- **Trim before a matched A/B suite** (`sudo fstrim -av`). The weekly fstrim timer cannot keep up with planet-scale scratch churn, and accumulated trim debt widens the drive-state band that interleaving has to cancel. "Drive state" in these docs means trim debt + SLC/GC behaviour on a near-full healthy drive (SMART-verified), never failing hardware.

Drive-state and cache-state evidence, with dates and UUIDs, lives in [`reference/performance/README.md`](reference/performance/README.md) - it is volatile and does not belong here.

## Workspace
The root `Cargo.toml` is a virtual workspace manifest; all three members live under `crates/`. **`pbfhogg`** (`crates/lib/`, library) + **`pbfhogg-cli`** (`crates/cli/`, produces the `pbfhogg` binary; the program itself is the package's library target `pbfhogg_cli` in `crates/cli/src/lib.rs`, which `crates/cli/src/main.rs` only calls, so the package's tests can reach the command tree - it is not an API). Library users: `default-features = false` drops the `commands` feature (see Features below for exactly what that gates). `pbfhogg-cli` carries a no-op `commands` feature so brokkr can apply `--features commands` symmetrically across both crates - the CLI's lib dep already always pulls in `pbfhogg/commands`.

CLI-driven integration tests live in the `pbfhogg-cli` package, `crates/cli/tests/cli_*.rs` (modules of `crates/cli/tests/all.rs`), and drive the `pbfhogg` binary via `CliInvoker` (`crates/cli/tests/invoker.rs`). The binary is cargo's `CARGO_BIN_EXE_pbfhogg`, built for that test build with its own features, profile and target; nothing is resolved at run time. Command-table tests take their subcommand lists from `pbfhogg_cli::cli_command()`, the tree production parses, never from `--help` text. `crates/lib/tests/common` is shared: the CLI harness mounts it by `#[path]`.

The third member, **`pbfhogg-harness`** (`crates/harness/`, bin only, `publish = false`), is the library harness `brokkr library` builds and runs (`brokkr man library`). Its unit tests are in the bin; the tests that drive the executable are `crates/harness/tests/all.rs`, under the CLI's promise guard (`crates/cli/tests/guard.rs`, mounted by path).

`cargo clippy --all-targets` at the workspace root selects every member: the virtual manifest has no `default-members` restriction. Verify changes with `brokkr check` for the configured feature and target sweeps (the `all`, `consumer` and `musl` sweeps select both `pbfhogg` and `pbfhogg-cli`).

## Architecture
**Read path:** `BlobReader` (blob.rs) → `PrimitiveBlock` (block.rs) → `Element` (elements.rs)
- `ElementReader` (reader.rs): sequential/parallel/pipelined iteration. Header parsed eagerly. `is_sorted()` for Sort.Type_then_ID. `for_each_block_pipelined` delivers owned blocks. `into_blocks_pipelined` returns iterator (early exit, zipping); requires `R: 'static`.
- `pipeline.rs`: 3-stage pipelined decoder (IO → rayon → reorder buffer).

**Write path:** `BlockBuilder` (block_builder.rs) → `PbfWriter` (writer.rs)
- `BlockBuilder`: max 8000 entities/block, one type per block, direct wire-format encoding via `wire.rs`.
- `PbfWriter`: blob framing, compression (zlib/zstd/none), raw passthrough. `to_path` (parallel compression), `to_path_direct` (O_DIRECT), `to_path_uring` (io_uring), `new(writer)` (sync).
- `uring_writer.rs`: io_uring with 64×256KB registered buffers.

**Geocode index:** `geocode_index/` - `format.rs` (on-disk format), `reader.rs` (mmap, S2 cell lookup), `builder.rs` (4-pass pipeline).

**Geometry:** `geo.rs` - point-in-polygon, ring assembly, Douglas-Peucker, antimeridian, cos-projection distance.

## Conventions
- Library code never mutates process state, reads the environment or writes stderr; see ADR-0016.
- Performance numbers in markdown must include commit hash and hostname. Script results are auto-stored in the per-user bench store; legacy typed results remain in `.brokkr/results.db`.
- `reference/performance/` is a record of measured numbers and their verdicts, one file per command or subsystem with current baselines first and a History section after, not of machinery: how the code works is described in the code and the other reference documents.
- Strict clippy: `unwrap_used = "deny"`, `expect_used = "deny"` (tests exempt; a production `expect` carries `#[expect(clippy::expect_used, reason = "...")]` saying why it cannot fire - see `clippy.toml`), `cognitive_complexity = "deny"`.
- Tunables (sizes, depths, worker counts and caps, channel and reorder capacities, timeouts, thresholds, geocode policy values) live in `crates/lib/src/tunables.rs`, grouped by scope with their rationale. A new tunable goes there, not in a local `const` or an inline literal. Format and protocol constants (field numbers, record layouts, spec limits) stay with the code that owns the format.
- Coordinates: decimicrodegrees (10^-7 degrees) for node I/O.
- Tests never read downloaded datasets (Denmark, planet, OSC diffs) - every test builds its own input in scratch, and the suite must pass with an empty `data/`. Real-data coverage belongs to `brokkr verify` and `brokkr bench`.
- Tests never depend on the specifics of a host: no goldens blessed on one machine, no core-count, memory or rlimit preconditions. Library tests get a fixed machine from `common::test_host()` and unit tests from `Host::for_tests()` (both `host::Fixed`). Fixture PBFs are built at test time by the `crates/lib/tests/common` writers and generators, never checked in. The one exception is `crates/lib/tests/test.osm.pbf`, the doc-example input, regenerated by `crates/lib/examples/gen_test_pbf.rs` (`reference/testing.md` says why); new tests never read it. A test whose feature the host lacks (O_DIRECT, io_uring, osmium) skips and passes - that is harmless, and nothing turns such a skip into a failure.
- Errors: `error.rs`, boxed ErrorKind pattern. `MissingHeader` if no OsmHeader blob.
- Internal invariants are checked in release, never debug-only: make the type carry it, else `Error::internal` where the function returns `Result`, else a release panic (never in `Drop`); input-reachable conditions are refusals (ADR-0021, ADR-0004).
- Speed on normal input outranks defence against corrupt input: a check that only refuses malformed input and costs measured per-byte or per-element work on a hot path is the wrong trade, and such a finding ends as a code comment plus a reference-doc correction. A silent wrong answer (an empty user or role, a default metadata block, fewer nodes) is worse than a refusal: where input can make the answer wrong, refuse.
- Decoding follows protobuf's specification where that costs nothing, with osmium as the parity reference; every deliberate deviation is recorded in DEVIATIONS.md.
- Tests: `crates/lib/tests/` (stable-API integration tests, `fault_*.rs` fault injection) + `crates/cli/tests/` (every CLI-driven test: `cli_*.rs`, `cli.rs`) + inline unit tests in `crates/lib/src/**/*.rs` and `crates/cli/src/`. All three packages set `autotests = false`: every test file that can share a process is a module of its package's one `all` binary (`crates/lib/tests/all.rs`, `crates/cli/tests/all.rs`, `crates/harness/tests/all.rs`), and a new file not declared there is never compiled, which no test or coverage audit reports. A test that must own its process (it needs a process-wide setting to itself, such as its own rlimit) gets its own `[[test]]` target in that package's `Cargo.toml` instead, with the reason beside it; the `[[test]]` entries list them. Fault injection lives behind the `test-hooks` feature in `pbfhogg::test_hooks`: a `FaultPlan` carried by each invocation's cancellation token (plus a few per-object hooks), never process-global, so an armed fault cannot reach a sibling test and fault tests share the `all` binaries. Text rules in `brokkr.d/lints.toml` ban `static` items in `crates/lib/src/test_hooks.rs` and any other `test_hooks` module under `crates/lib/src/`. Full names gain the file as a module prefix (`cli_sort::backends::...`); the process-wide state each `all` shares is listed in its `all.rs` and in `reference/testing.md` "Test binaries". Validation tier model in [`reference/testing.md`](reference/testing.md): tier 1 at file root, tier 2/3 in `mod tier2`/`mod tier3` submodules, serial in `mod serial`; feature-gated O_DIRECT/io_uring tests are tier 1 and skip on a typed platform refusal. New `crates/cli/tests/cli_*.rs` files use `CliInvoker` and the stable allowlist (`block_builder`, `writer`, `BlobReader`, `Element`, etc.) so internal-module rewrites don't force test edits.

## Features (library crate)
- `commands` (default): gates `check`, `export`, `extract`, `commands::spatial`, `inspect::json` and the geocode builder, plus their deps (`serde_json`, `s2`). It does **not** gate `crates/lib/src/commands/` as a whole: `altw`, `apply_changes`, `cat`, `degrade`, `diff`, `getid`, `getparents`, `inspect` (minus its JSON output), `merge_changes`, `renumber`, `repack`, `sort`, `tags_count`, `tags_filter` and `time_filter` compile in a no-features build too. The `minimal` sweep in `brokkr.toml` is what keeps that configuration compiling; it is scoped to `-p pbfhogg` on purpose, because `pbfhogg-cli` pins `features = ["commands"]` on its lib dep and feature unification would switch `commands` back on the moment the CLI joins the build.
- `geocode-reader`: `geocode_index::Reader` for reverse geocoding (depends on `s2`). Included by `commands`.
- `linux-direct-io`: O_DIRECT read/write (requires `libc`)
- `linux-io-uring`: io_uring writer (requires `io-uring` + `libc`, Linux 5.1+)

Zlib backend: `zlib-rs` (pure Rust, no C). No feature flags for backend selection.

## Document folders

The standing layout, across every project, split by durability first, subject
second.

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | Durable in-repo reference for anyone working on or with the code - how the thing is built and why: `architecture.md`, `technical-implementation-spec.md`, `reference/performance/` (the durable record of measured numbers over time, per command), invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `decisions/` | Durable architecture decision records (ADRs) | Binding records of chosen design, cited from source and durable docs. |
| `docs/` | Durable in-repo documentation of how the thing is used - guides, CLI reference, the consumer-facing API surface. Sometimes exposed as a hand-edited VitePress gh-pages site | Same must-be-true rule. |
| `notes/` | Transient - work items (`todo.md`), future plans, hypotheticals, bug reports, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents are transient: they go in `notes/`. |

`reference/`, `decisions/` and `docs/` are durable and binding. The difference
is subject, not audience: `reference/` covers how the thing is built and why -
what you need in order to change it safely; `decisions/` records why major
choices were made; `docs/` covers how it is used. A developer or library
consumer reads all three. Where a project publishes a site, `docs/` is what
gets published; the folder means the same thing either way.
`notes/` is neither durable nor binding, which is the whole point of keeping it
separate: a document that may be wrong must not sit where a document that must
be right is expected.

The dependency direction is therefore one-way. `notes/` may cite `decisions/`,
`docs/` and `reference/`; nothing durable may cite `notes/` - not a code
comment, not `docs/`, not `reference/`. Source and durable docs may cite
`decisions/`. A code comment must carry its full context, because it outlives
the note.

**Root-level convention files are exempt.** `AGENTS.md`, `CLAUDE.md`,
`README.md`, `LICENSE`, `CHANGELOG.md` and their kin are found by tooling and by
convention at the repository root, and stay there. These folders govern
documents we chose where to put, not files whose location is dictated.

In `notes/`, `docs/` and `reference/` alike, avoid citing source line numbers -
they drift fast.
