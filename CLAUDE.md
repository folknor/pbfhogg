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

Standalone development tool at `~/Programs/brokkr`. Installed via `cargo install --path ~/Programs/brokkr`. Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

Every measurable command is a top-level subcommand. Mode flags control behavior:

```
brokkr <command> [--dataset D] [--variant V]   # run once, print timing
brokkr <command> [--dataset D] --bench          # 3 runs, store in DB + sidecar profiler (100ms /proc sampling + markers)
brokkr <command> [--dataset D] --hotpath        # function-level timing
brokkr <command> [--dataset D] --alloc          # allocation tracking
brokkr <command> [--dataset D] --direct-io      # pass --direct-io to pbfhogg binary
brokkr <command> [--dataset D] --io-uring       # pass --io-uring to pbfhogg binary
```

`--stop MARKER` can be combined with any measured mode (`--bench`, `--hotpath`,
`--alloc`). Kills the pbfhogg process after the named marker is emitted in the
sidecar stream. Sidecar data is stored on forced exit so `brokkr results <uuid>
--markers --phases` works normally. Use for fast iteration on individual stages:
`brokkr renumber --dataset planet --bench 1 --stop RENUMBER_EXT_STAGE2D_END`.

I/O mode flags (`--direct-io`, `--io-uring`) are passed through to the pbfhogg binary and
create named variants in results (e.g., `add-locations-to-ways+direct-io`). Combine with
`--bench` for stored comparisons: `brokkr results --compare-last --variant add-locations-to-ways`.

pbfhogg commands (every CLI command is a brokkr subcommand):
- `brokkr inspect-tags --dataset denmark`
- `brokkr add-locations-to-ways --dataset europe --index-type external --bench`
- `brokkr add-locations-to-ways --dataset europe --direct-io --bench` — O_DIRECT variant
- `brokkr build-geocode-index --dataset denmark --hotpath`
- `brokkr multi-extract --dataset japan --regions 5 --bench` — single-pass multi-extract
- `brokkr read --bench` — multi-variant read benchmark
- `brokkr cat --dataset planet --bench` — indexdata-generation passthrough (no `--type` filter). Defaults to `--variant raw` since that's the natural bootstrap input. Supports `--bench`, `--hotpath`, `--alloc`, `--direct-io`; `--io-uring` is parsed but rejected at dispatch. Recorded as `variant="cat"` in results — distinct from `cat-way` / `cat-relation` / `cat-dedupe` which take the filtered full-decode path. See "Indexdata PBFs" below.
- `brokkr merge-changes --dataset planet --osc-range 4914..4920 --bench` — multi-OSC merge. Expands to every configured `osc.<seq>` in the inclusive range (in ascending order) and passes them as positional args to a single `pbfhogg merge-changes` invocation. Fails fast if any seq in the range isn't in `brokkr.toml`. `--osc-seq` (single-file) is preserved and conflicts with `--osc-range` at the parser level. Range runs land in the results DB with a `+range-LO-HI` variant suffix (e.g. `merge-changes+range-4914-4920`) so `brokkr results --command merge-changes` keeps single-seq and range runs distinguishable.
- `brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1` — diff two point-in-time snapshots of the same dataset. Unlike `brokkr diff` (which runs apply-changes internally and diffs against the merged output, creating blob-level byte-equality that short-circuits most of the decode), `diff-snapshots` compares two **independent** PBFs with zero byte-level overlap — every blob decodes on both sides. Measures the "full decode" cost of diff that `brokkr diff` can't. `--from` and `--to` accept `base` (sentinel for the legacy top-level `[datasets.<region>.pbf.*]` data) or any registered snapshot key. `--format default|osc` picks summary diff or OSC-format output. Single `--variant` (default `indexed`) applied to both sides. Records in results with `variant="diff-snapshots-<from>-to-<to>"` and `meta.from_snapshot` / `meta.to_snapshot` / `meta.format`. Needs a second snapshot registered via `brokkr download ... --as-snapshot <key>` before it can be used on a dataset (see `brokkr download` below). See "Snapshot model" section.
- `brokkr suite pbfhogg --bench` — full suite

Utility commands (unchanged):
- `brokkr check [-- args]` — run clippy + tests. Supports `--features`, `--no-default-features`, `--package` / `-p`.
- `brokkr env` — show hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `brokkr results [UUID]` — look up specific result by UUID prefix (shows full detail + hotpath report + sidecar profile data).
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [--dataset D] [--meta K=V ...] [-n N] [--top N]` — query/compare benchmark results from SQLite. `--meta K=V` filters by metadata fields stored in the result row (e.g. `--meta strategy=smart`, `--meta format=osc`, `--meta merged_cache=miss`). Multiple `--meta` filters compose with AND semantics. Rows missing the requested key are silently excluded (so querying for a field that only post-fix runs record won't error on pre-fix rows, it'll just skip them). `--variant` and `--dataset` use substring match; `--meta` uses exact-match on key and value.
- `brokkr results <UUID> --timeline` — raw JSONL samples (t in fractional seconds). Query flags compose:
  `--summary` (per-phase table), `--fields rss,anon,majflt` (project fields),
  `--where "majflt>0"` (filter), `--every 50` (downsample), `--head N` / `--tail N`,
  `--stat anon` (min/max/avg/p50/p95), `--phase STAGE2` (filter to marker phase),
  `--range 10.0..82.0` (time window). Example: `--phase STAGE2 --stat anon`.
- `brokkr results <UUID> --markers` — raw JSONL marker events.
  `--durations` for START/END pair timing. `--phases` for durations + peak RSS/anon/majflt.
- `brokkr results --compare-timeline <uuid_a> <uuid_b>` — phase-aligned delta table.
- `brokkr results dirty` — access sidecar data from the most recent run, even if it was OOM-killed or crashed.
  Useful for inspecting memory trajectory of failed runs: `brokkr results dirty --timeline --stat anon`,
  `brokkr results dirty --timeline --fields anon --every 100`.
- `brokkr download <region> [--osc-seq N] [--as-snapshot <key> | --refresh] [--force]` — download region datasets.
  - **Primary PBF side (default invocation): no-op once `pbf.raw` is configured**, with an explicit multi-line log message naming the `--refresh` and `--as-snapshot` flags so users understand why nothing was downloaded and what to do if they wanted to refresh. brokkr never silently replaces the primary — rotation is opt-in via `--refresh`.
  - **OSC side: rolls forward on the primary.** Downloads every missing diff in `max_osc_seq+1..=target_seq`, appends new `[osc.<seq>]` entries to `brokkr.toml`, never overwrites existing ones.
  - **Indexed PBF: existence-checked, regenerated via `pbfhogg cat` on cache miss.** If `pbf.indexed`'s file is already on disk, SKIP. If absent, runs the passthrough cat path to produce it.
  - **`--as-snapshot <key>`** registers the download as an **additional snapshot** of an existing dataset, not a refresh of the primary. Writes under `[<host>.datasets.<region>.snapshot.<key>]` in `brokkr.toml` with snapshot-specific filenames. Requires the primary dataset to exist first — errors out if not. The snapshot key must match `[a-zA-Z0-9_-]+` and cannot be `base` (reserved as a CLI sentinel pointing at the legacy top-level data; see "Snapshot model" below). `--osc-seq N` combined with `--as-snapshot K` writes OSC entries into the snapshot's own `[...snapshot.<K>.osc.<seq>]` table, which is consumed by `--snapshot <key>` on apply-changes / merge-changes / diff / diff-osc / tags-filter-osc. Mutually exclusive with `--refresh`.
  - **`--refresh`** rotates the dataset to a newer upstream snapshot. Archives the existing primary `pbf.*` and `osc.*` entries into a `[snapshot.<key>]` block (key auto-derived from the existing `download_date` if present, else from the existing `pbf.raw` file's mtime, formatted `YYYYMMDD`), then downloads the new PBF and resets the top-level OSC chain. HEAD-checks the upstream `Last-Modified` header against the local state first and no-ops with a log line if upstream isn't newer. `--force` bypasses the HEAD check and rotates anyway — use when the heuristic gets it wrong (mtime touched by rsync, restored from backup, etc.). Collision handling: if the auto-derived archive key already exists under `[snapshot.<key>]`, brokkr errors out and asks the user to clean up the existing block and retry. No override flag — the user fixes the TOML and re-runs. Mutually exclusive with `--as-snapshot`.
  - Planet uses planet.openstreetmap.org replication; everything else uses Geofabrik.
  - Short aliases (denmark, europe, japan) or full paths (europe/france, asia/japan/kanto).
- `brokkr lock` — check if a brokkr command is currently running (PID, duration, I/O stats). Use this to poll long-running benchmarks instead of reading output files. Never compile or run CPU-intensive work while a bench holds the lock.
- `brokkr clean` — remove scratch temp files and verify output directories.
- `brokkr history [--command CMD] [--project P] [--failed] [--since DATE] [--slow MS] [-n N] [--all]` — query global command history.
- `brokkr verify <command> [--dataset name]` — cross-validate against reference tools.

### Default OSC resolver

`--osc-seq` auto-selects only when the table being looked at has **exactly one** `[osc.<seq>]` entry. When there are multiple OSCs, commands that need to pick one (`merge`, `apply-changes`, `diff`, `diff-osc`, `verify merge`, `verify diff`) **error out** with "multiple OSCs configured, pick one" unless `--osc-seq N` is passed explicitly. This matters for overnight bench suites: pin `--osc-seq` explicitly on every OSC-consuming command once a dataset (or a snapshot within it) has more than one diff configured, otherwise the command refuses to run. The resolver applies to both the top-level primary OSC table and snapshot-scoped OSC tables reached via `--snapshot <key>` — same ergonomics, the lookup is just scoped under a different parent.

### Snapshot model

A dataset can have one **primary** PBF plus any number of named **snapshots**. Primary lives at the top level of the dataset's TOML block — `[datasets.<region>.pbf.*]` and `[datasets.<region>.osc.*]`. Snapshots live under `[datasets.<region>.snapshot.<key>.*]`, each with its own `pbf` and optional `osc` tables. Primary is what every command reads by default; snapshots are addressable via `--snapshot <key>` on `apply-changes`, `merge-changes`, `tags-filter-osc`, `diff`, and `diff-osc`, or as `--from` / `--to` on `diff-snapshots`. Commands that currently have no snapshot-aware surface (`extract`, `add-locations-to-ways`, `build-geocode-index`, `merge`, `sort`, `renumber`, etc.) always read the primary — they don't need snapshot awareness today, and adding it is a followup if a concrete use case surfaces.

```toml
# Primary — read by default by every command; every command with `--snapshot base`
[plantasjen.datasets.planet.pbf.raw]
file = "planet-20260223.osm.pbf"
seq = 4704

[plantasjen.datasets.planet.pbf.indexed]
file = "planet-20260223-with-indexdata.osm.pbf"

[plantasjen.datasets.planet.osc.4913]
file = "planet-20260329-seq4913.osc.gz"
# ... [osc.4914] ... [osc.4920]

# Additional snapshot, registered via `brokkr download planet --as-snapshot 20260411`
[plantasjen.datasets.planet.snapshot.20260411]
download_date = "2026-04-11"

[plantasjen.datasets.planet.snapshot.20260411.pbf.raw]
file = "planet-20260411.osm.pbf"
seq = 4921

[plantasjen.datasets.planet.snapshot.20260411.pbf.indexed]
file = "planet-20260411-with-indexdata.osm.pbf"

# Consumed by apply-changes / merge-changes / diff / diff-osc / tags-filter-osc
# when invoked with `--snapshot 20260411`
[plantasjen.datasets.planet.snapshot.20260411.osc.4922]
file = "planet-20260412-seq4922.osc.gz"
```

**The `base` sentinel.** `brokkr diff-snapshots --from <ref> --to <ref>` accepts either a snapshot key or the literal `base`. `base` resolves to the dataset's primary (top-level) data — that is, the legacy single-PBF-with-rolling-OSCs shape that every existing dataset already has. This is a CLI-layer alias so diff-snapshots can reference both old and new data without migration. The name `base` is reserved: `brokkr download <region> --as-snapshot base` is rejected by the CLI validator (`'base' is a reserved snapshot name`), and snapshot keys must match `[a-zA-Z0-9_-]+`.

Examples:
- `brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1` — diff the original frozen planet against a newly-registered snapshot. The archetypal "two real independent snapshots" benchmark.
- `brokkr diff-snapshots --dataset planet --from 20260411 --to 20260418 --format osc --bench 1` — diff two weekly snapshots, output OSC format. Requires both to be registered.
- `brokkr apply-changes --dataset planet --snapshot 20260411 --osc-seq 4922 --bench 1` — run the apply-changes benchmark against a historical snapshot's state rather than the primary. Produces a result row with `variant="apply-changes+snap-20260411"` and `meta.snapshot="20260411"`.
- `brokkr results --variant snap-20260411 -n 20` — list every command run recorded against snapshot `20260411`, across all subcommands, via the `+snap-<key>` variant suffix.

**Snapshot-scoped commands: `--snapshot <key>`.** `apply-changes`, `merge-changes`, `tags-filter-osc`, `diff`, and `diff-osc` all accept `--snapshot <key>`, which resolves PBF and OSC lookups against `[datasets.<region>.snapshot.<key>.*]` instead of the top-level. `--snapshot base` (or omitting the flag) preserves the default behavior — useful for scripts that parameterize over snapshot keys and want to include the primary without special-casing. Snapshot-scoped bench runs are recorded two ways: the variant column gains a `+snap-<key>` suffix (e.g. `apply-changes+snap-20260411`), and the result metadata records `meta.snapshot = <key>`. Either surface is queryable via `brokkr results --variant snap-20260411` (substring match across commands) or `brokkr results --meta snapshot=20260411` (exact match). The merge benchmark subcommand (`brokkr merge`) does NOT yet accept `--snapshot` — it lives in a legacy non-PbfhoggCommand dispatch path and would need a larger refactor; addressable via `apply-changes --snapshot <key>` as a workaround.

**`brokkr download <region> --refresh`** rotates the primary to a newer upstream snapshot and archives the old primary into a `[snapshot.<key>]` block, populating the snapshot tables automatically. The archive key auto-derives from `download_date` (if present) or the existing `pbf.raw`'s mtime, formatted `YYYYMMDD`. Refresh HEAD-checks upstream `Last-Modified` first and no-ops if upstream isn't newer (`--force` bypasses). After rotation, the archived state is reachable via `brokkr diff-snapshots --from <key> --to base` or `brokkr apply-changes --dataset <region> --snapshot <key> --osc-seq <N>`.

**`brokkr diff-snapshots` vs `brokkr diff`.** Different code paths, different things measured:
- `brokkr diff` runs `apply-changes` internally via `ensure_merged_pbf`, then diffs against that output. The two PBFs on each side share most blobs at the byte level (apply-changes uses raw-passthrough for non-touched blobs), so pbfhogg's diff can fast-path byte-equal blobs and skip decode on most of the input.
- `brokkr diff-snapshots` compares two **independent** snapshots (e.g. `planet-20260223` vs `planet-20260411`). Zero blob-level byte equality because each weekly planet dump re-encodes from scratch. Every blob decodes on both sides. Different working set, different peak memory, different wall time profile — measures the "full decode" cost of diff that `brokkr diff` can't surface.

Both are valid benchmarks and the README Planet scale table should eventually carry both rows.

### diff / diff-osc caching

`brokkr diff` and `brokkr diff-osc` are the only subcommand pair that shares state through scratch. Each invocation calls an internal `ensure_merged_pbf` setup step in `build_pbfhogg_context`: it produces `<pbf-stem>-osc<seq>-bench-merged.osm.pbf` in scratch, either by running apply-changes on cache miss or by reusing an existing file on cache hit. The cache key includes the OSC seq, so runs with different `--osc-seq` values cache independently.

Key semantics (matters when reading bench numbers and designing overnight suites):

- **Recorded `elapsed_ms` is cache-state-independent.** `ensure_merged_pbf` runs **before** the harness starts its per-iteration timer (`harness.rs:206`). So whether the cache was hit or missed, the `elapsed_ms` stored in `results.db` reflects only the diff work itself, never the apply-changes setup cost. For README Planet scale table numbers and regression queries, the recorded measurement doesn't depend on prior cache state.
- **Total brokkr-invocation wall DOES depend on cache state.** If you're timing `brokkr diff ...` from outside its harness (wall-clock measurement of the whole command), that number includes the setup cost on cache miss but not on cache hit. Only relevant when timing brokkr externally.
- **`brokkr merge` is completely independent** of `brokkr diff` / `diff-osc`. Merge writes to a different filename (`bench-merge-output.osm.pbf`), deletes it on exit, shares no state with diff's cache. Running merge before diff does NOT populate diff's cache.

Cache rebuild policy:

- **Measured modes (`--bench`, `--hotpath`, `--alloc`) rebuild the cache by default.** This keeps the total brokkr wall deterministic in measured-mode invocations (useful for anyone timing brokkr externally), even though it doesn't affect the recorded `elapsed_ms`.
- **Run mode (no measurement flag) reuses the cache** for dev-loop speed.
- **`--keep-cache`** on `diff` / `diff-osc` opts measured modes back into cache reuse. Useful when running `diff` and `diff-osc` back-to-back on the same `--osc-seq`: the second invocation reuses the cache from the first, saving ~10 min of duplicate apply-changes on planet. Because recorded `elapsed_ms` is cache-state-independent, this costs nothing for the bench numbers. **Planet overnight uses `--keep-cache` on both diff and diff-osc for this reason.**
- **`brokkr clean` wipes the cache** — it lives in scratch.

Telemetry:

- Result metadata records `meta.merged_cache = hit|miss` and `meta.merged_cache_age_s` (on hit). `brokkr results <uuid>` shows both. `brokkr results --command diff` queries can filter by cache state for post-hoc auditing. Hit/miss log lines at invocation time also include the cached file's age; the miss path times the apply-changes setup and logs the elapsed.

### brokkr.toml

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"   # purely informational — not functionally read by any brokkr code
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

# Datasets can accumulate OSC entries over time as `brokkr download` rolls forward.
# Planet after downloading 4913..4920 looks like:
[plantasjen.datasets.planet.osc.4913]
file = "planet-20260329-seq4913.osc.gz"
# [osc.4914] ... [osc.4920] also present (omitted for brevity)
```

- `pbf.<variant>` — PBF files keyed by variant name. `--variant` selects (default: `indexed`). Variants are *transforms* of the same underlying snapshot (raw / indexed / altw / locations), not different point-in-time captures.
- `osc.<seq>` — OSC diff files keyed by sequence number. `--osc-seq` selects. A dataset can have many OSCs configured; see "Default OSC resolver" above.
- `snapshot.<key>` — named point-in-time captures of the same dataset. Each snapshot has its own `pbf.<variant>` / `osc.<seq>` tables. Consumed by `diff-snapshots` (via `--from` / `--to`) and by `apply-changes`, `merge-changes`, `tags-filter-osc`, `diff`, `diff-osc` (via `--snapshot <key>`). The legacy top-level data is implicitly snapshot `base` (reserved name). Snapshot keys must match `[a-zA-Z0-9_-]+`. Populated by `brokkr download <region> --as-snapshot <key>` (manual registration) or `brokkr download <region> --refresh` (auto-archive during primary rotation). See "Snapshot model" above for the full schema, usage semantics, and the `meta.snapshot` / `+snap-<key>` result-column surface.
- `download_date` — human annotation only. No code path reads or writes it; it's round-tripped by the parser and otherwise ignored.
- `xxhash` — XXH128 file hash. Run `brokkr env` to see computed values.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git — stage it with your next commit when modified). `--bench` accepts an optional run count: `--bench N` runs the command N times and stores only the best (minimum) result. Default is `--bench 3`. Bench and hotpath commands require a clean git tree (ignoring `*.md` and `.brokkr/results.db`); use `--force` to run anyway (results will not be stored). **`--force` is a top-level flag before the subcommand**, e.g. `brokkr bench --force commands add-locations-to-ways`, NOT `brokkr bench commands add-locations-to-ways --force`.

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Indexdata PBFs

Indexed PBFs are generated automatically by `brokkr download` (see above): after the raw PBF lands, the download path runs the passthrough cat step via the `pbfhogg cat` binary and writes the result into `pbf.indexed`'s configured file. No manual step is needed as part of the normal download workflow.

To **benchmark** the passthrough cat path on its own (e.g., to measure wall time / peak RSS for the bootstrap step on a specific dataset), use the new `brokkr cat` subcommand:

```
brokkr cat --dataset planet --bench 1           # passthrough path, default --variant raw
brokkr cat --dataset planet --direct-io --bench # O_DIRECT variant
```

The passthrough path (no `--type`) adds indexdata via decompress+scan without re-compressing blobs — minimal memory, suitable for planet-scale files. Planet (87 GB): 497s buffered, 520s `--direct-io` (+5% slower); Denmark (461 MB): 2.8s buffered (commit `69a127f`, plantasjen). Buffered wins for sequential single-file passthrough — `--direct-io` only helps with concurrent read/write (merge). The `--type` filtered path (exposed as `brokkr cat-way`, `brokkr cat-relation`, `brokkr cat-dedupe`) also embeds indexdata but does full decode+re-encode (OOMs on planet at 30 GB host).

`apply-changes`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`, and `build-geocode-index` are much faster with indexed PBFs and will error if indexdata is missing. Use `--force` to override the check and run with raw PBFs (slower). `inspect --indexed` checks a PBF and exits 0 (indexed) or 1 (not indexed).

### add-locations-to-ways index types

`add-locations-to-ways` supports `--index-type dense|sparse|external` (default: `dense`):

- **`dense`** — Direct-mapped mmap array (`index[node_id] = (lat, lon)`). Fastest when the working set fits in RAM. At planet scale (~16 GB touched after pass 0 filtering), requires ~30+ GB free memory to avoid page cache thrashing.
- **`sparse`** — Planetiler-inspired chunk-indexed sparse array. ~540 MB RAM for chunk index + compact on-disk values file (~16 GB for planet). Way lookups are batched and sorted by file offset, converting random I/O into sequential scans. Works on memory-constrained hosts (tested on 30 GB host with planet). ~1.85x slower than dense on Denmark (all fits in RAM, overhead is pure CPU).
- **`external`** — External join via double radix permutation. Bounded memory (~1.4 GB stages 1-3, ~1.6 GB stage 4 at Europe scale), all sequential I/O. Uses ~4 GB temp disk at Denmark scale, ~112 GB at Europe. Best for memory-constrained hosts where dense thrashes and sparse is too slow. Denmark: 14s. Europe: 577s (9.6 min, 4.5x faster than dense, commit `6b09796`). Planet: 1,462s (24.4 min, 3.9x faster than dense). Requires sorted PBF input. Uses node-only wire-format scanner (no PrimitiveBlock) for stage 2, scatter buffer for stage 3, sequential reader with DecompressPool for stage 4.

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
- `brokkr verify multi-extract [--dataset name] [--regions N]` — single-pass vs sequential multi-extract element counts
- `brokkr verify all [--dataset name] [--variant V] [--osc-seq SEQ] [--bbox bbox]` — run all verify commands sequentially (includes multi-extract)

All `--variant` flags default to `indexed`. `--osc-seq` auto-selects if exactly one OSC is configured for the dataset.

## Benchmarking Rules
- **NEVER run benchmark, profiling, or verify commands in parallel.** Not two, not three — ONE AT A TIME. Benchmarks require exclusive access to CPU, memory, and I/O. Running multiple simultaneously makes every result wrong. Always wait for each to fully complete before starting the next. This applies to bench, verify, hotpath, and profile subcommands.
- When an optimization workflow requires multiple benchmark runs (baseline, mid-work, post-work), run each one **sequentially** and report results between runs. Do NOT launch them as parallel background tasks.

## Review tool

`review` fans out code review queries to persistent AI sessions. Configured in `.review.toml`.

Five archetypes, two groups:
- `bugs` — logic errors, edge cases, error handling, crashes
- `perf` — allocations, complexity, hot paths, scan structure
- `arch` — coupling, abstractions, API design, feature gates
- `correctness` — spatial algorithms, binary format fidelity, OSM data model
- `planet` — O_DIRECT, io_uring, mmap, allocators, streaming pipelines, memory-constrained processing

Groups: `sweep` = [bugs, perf, arch, correctness], `everything` = all five.

Usage:
- `echo "question" | review sweep` — ask 4 archetypes (stdin goes direct)
- `echo "question" | review planet` — ask planet sessions
- `echo "question" | review everything` — ask all 5 archetypes
- `echo "question" | review perf --anchor` — re-anchor with grounding prefix (for stale sessions)

Stdin goes directly to the sessions by default. Use `--anchor` to prepend the grounding prefix when a session has gone stale or for first use. When using `--anchor`, include a short identity reminder in stdin, e.g., "Remember: you're our planet-scale systems reviewer." or "Remember: you're our spatial correctness expert."

**Use this tool before implementing major changes.** Write up the problem, send to reviewers, wait for answers. The write-up + review cycle is faster than implement + discover it's wrong.

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

**Geocode index:** `geocode_index/` module
- `format.rs`: on-disk binary format (19 files, header/cells/entries/data/strings). Manual byte-level serialization, no `#[repr(C)]`.
- `reader.rs`: mmap reader with two-layer API. `query()` (allocation-free, nearest-of-each-type) and `candidates()` (Vec-backed, all matches). S2 cell neighborhood lookup, binary search on sorted cell arrays, segment-level distance scoring.
- `builder.rs`: 4-pass build pipeline. Pass 1: relations (admin boundaries). Pass 1.5: referenced node collection (IdSetDense). Pass 2: nodes + ways fused scan (compact rank-indexed coord array via IdSetDense rank, streaming data files to disk). Pass 3: bucketed S2 cell assignment (256 temp-file buckets per level). Interpolation endpoint resolution via spatial join against address points. Europe: 524s (8.7 min), 7.5 GB RSS (commit `dad0dbd`). Planet: 1,346s (22.4 min), 17.8 GB RSS.

**Geometry:** `geo.rs` — shared primitives (point-in-ring, antimeridian handling, point-in-polygon with holes, Douglas-Peucker simplification, ring assembly, cos-projection distance).

## Conventions

- All performance numbers (timings, allocations, throughput) in markdown files must include the git commit hash and hostname where the measurement was taken. Benchmark results are stored automatically in `.brokkr/results.db` (SQLite).
- Strict clippy lints enforced (see `[workspace.lints.clippy]` in Cargo.toml) -- notably `unwrap_used = "deny"` and `cognitive_complexity = "deny"`
- Coordinates use decimicrodegrees (10^-7 degrees) for node I/O in BlockBuilder
- Error types in `error.rs` follow the `csv` crate pattern (boxed ErrorKind). `MissingHeader` error if a PBF doesn't start with an OsmHeader blob.
- Tests live in `tests/` (21 test files covering all commands, roundtrip, read paths, corrupt input) and `cli/tests/cli.rs` (CLI integration tests), plus inline unit tests in source files

## Features (library crate)

- `commands` (default): enables `check_refs`, `extract`, geocode index builder, and their deps (`roaring`, `serde_json`, `s2`)
- `geocode-reader`: enables `geocode_index::Reader` for reverse geocoding queries (depends on `s2`). Included by `commands`. Downstream consumers (e.g., nidhogg) can enable this without the full `commands` feature.
- `linux-direct-io`: O_DIRECT read/write paths (bypasses page cache, requires `libc`)
- `linux-io-uring`: io_uring writer thread (requires `io-uring` + `libc`, Linux 5.1+, sufficient `RLIMIT_MEMLOCK`)

Zlib backend is hardcoded to `zlib-rs` (pure Rust, no C compiler, faster than zlib-ng). No feature flags for backend selection. Sync zlib compression is 15-19% slower than the previous `libdeflater` (C) backend, but pipelined mode — the production path — shows no difference (decode-bound). The tradeoff is accepted: zero C dependencies for compression, one backend everywhere.

## Performance baselines (North America, 18.8 GB, 2.58B elements, commit `a6ebbfe`)

**Read:** parallel 22s, pipelined 57s, sequential 130s.
**Write:** pipelined zlib 4m27s, pipelined none/zstd ~4m20s, sync zlib 14m34s.
**Merge** (645K-change daily diff, 303K passthrough / 19.6K rewritten blobs):
buffered+zlib 17.3s, uring+zlib 15.2s, buffered+none 14.9s, **uring+none 11.9s**.
All merge variants under 600 MB RSS. io_uring wins 12-20% at this scale (page cache overflow).
