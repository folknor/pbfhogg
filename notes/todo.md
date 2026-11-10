# pbfhogg TODO

## Carried over from the 0.6.0 release (2026-11-09)

The README's three tables come from the campaign at `53360af4`
(reference/performance/cross-command.md "README campaign"). Left open:

- [ ] **Planet isolation-cat row, held back from the README library
  table.** `d73fbee8`: library 43.1-55.4 s, CLI 55.9-69.5 s, ratio 1.28,
  where the typed `brokkr cat` cell on the same file and commit
  (`be808f35`) took 42.4 s. Explained 2026-11-10 as drive state, not
  code: every arm does identical syscall work and the whole delta is the
  writer waiting on input bytes; L1 matches the typed cat at 2.2 GB/s
  and later iterations sag to 1.6 GB/s after a burst. The session ran
  about 48 minutes disk-bound, the harness digesting each 92 GB output
  between arms, library arm always first, so every CLI arm runs deeper
  into the sag. A plain `copy-pbf --iterations 3` after a trim shows the
  same with no harness: 42.4, 41.1, 50.1 s (`41328086`). Left: a
  publishable row, three `brokkr library isolation-cat --dataset planet
  --bench 1` runs each after a trim and a rest of about ten minutes, and
  SMART thermal counters around one to tell throttling from SLC or GC
  (smartctl is not on the agent allowlist).
- [ ] **Performance guide plantasjen tables** (Japan extract, apply-changes
  at scale and with LocationsOnWays, write throughput, the io_uring and
  O_DIRECT tips) carry host and commit labels but were not re-measured on
  bygg.

## 0.7.0 items (moved from the 0.6.0 list on 2026-10-30)

Picked for 0.6.0 on 2026-07-30, right after the 0.5.0 release; deferred
when 0.6.0 was cut down to the library gate, multi-extract descriptor
admission and the regressions.

- [ ] **`Executor` residue** (decided 2026-10-28): `host::Executor` was
  narrowed to the library's shared rayon tasks (writer framing, command
  par-iterators); dedicated pipelines keep their own per-call workers.
  Left over from that decision, each optional and unmeasured:
  - merge-changes and the OSC tags-filter under `Parallelism::Fixed(n)`
    build their own pool and so bypass a caller `Executor::Pool`. The cleaner shape keeps the
    executor always and makes `n` a job-lane limit: the streaming window
    (`rayon::current_num_threads()`), the simplify `par_iter` over inputs
    and the `par_chunks` sort would each honour `n` without spawning jobs
    that block pool workers on a permit. Measured-path change.
  - Worker sets run in a `std::thread::scope` without joining their
    handles (the ordered and classify pools, the read pipeline's stage
    threads, and other per-stage threads; `parallel_writer` joins its
    handles and is not one) are waited for only to the end of their main
    functions: std's scope does not wait for thread-local
    destructors (found 2026-10-29 while joining the rayon pools, which are
    now `joined_pool::JoinedPool`s). Harmless today, since no library
    thread-local records into an invocation and caller code that runs on
    those threads is rare, but a "no library thread left after the call"
    promise would need explicit `JoinHandle::join`s there, as
    `JoinedPool` does.
  - `no-ambient-rayon` cannot see pool-method scopes or `std::thread`; a
    new dedicated `std` worker set passes lint unreviewed. (rayon's pool
    constructors are disallowed in `clippy.toml` outside
    `joined_pool::JoinedPool`.)

- [ ] **GeoParquet export**
  ([notes/geoparquet-export.md](geoparquet-export.md)).
  `pbfhogg export --format geoparquet`: stream tagged nodes and
  enriched ways into a conformant GeoParquet 2.0 file (1.1 fallback
  rail). Parked research note + vendored spec repo
  (`research/geoparquet/`) are the raw material; goes through the
  orchestrate spec loop before implementation. Supersedes the parked
  GeoPackage idea in the GeoJSON design note's deferred ledger.

- [ ] **GeoJSON export v2**
  ([notes/geojson-export-design.md](geojson-export-design.md)):
  relations -> MultiPolygon, raw-PBF index fallback, verify + throughput
  baseline (brokkr#139). Same `export` command GeoParquet extends, so
  touch it once for both formats. brokkr#139 is blocked on us
  (2026-11-09): brokkr builds no `export` command, so the throughput
  baseline is a pbfhogg script bench in `brokkr.d/benches.toml`; and its
  verify judges only the agreement surface (ways and nodes both tools
  export, by ID, geometry within a tolerance, tags), which needs pbfhogg
  to state the contract first, in ADR-0010 or the design note: which
  divergences from `osmium export` are allowed and which are bugs.

- [ ] **Multi-extract v2 complete/smart strategies**: per-region
  way/relation ID tracking. Single-pass multi-extract covers the simple
  strategy only; complete and smart fall back to sequential per-region
  runs. Memory scales with region count (per-region `bbox_node_ids` and
  `all_way_node_ids`); not yet measured.

## CLI surface redesign (deferred past 0.6.0, to 0.7.0)

Planned as one coherent CLI-surface cleanup for 0.6.0, then pushed back a
release. Every item here is a breaking or user-visible CLI change; do them
together so the surface changes once.

- [ ] **Refuse `--direct-io` where it reaches nothing.** Defer any new
  no-op-flag refusals to this 0.7.0 batch: they break existing command lines.
  Use the reach table and selected mode; keep existing check/OSC and
  compile-time-feature refusals. Until then, describe ignored paths in help
  and the capability matrix, without adding refusals.

- [ ] **CLI UX: scratch placement residue + mode naming** (raised
  2026-04-23). Both should be applied uniformly across every command
  that carries the pattern, not one-off per command.

  (A) **Scratch placement residue.** The posture is decided and shipped
  ([ADR-0018](../decisions/0018-scratch-is-one-run-directory-and-leftovers-are-reported.md)):
  every run makes one run directory under `--tmp-dir DIR`, which
  defaults to the output's directory, or the current directory when the
  output is a bare filename. Open: whether a bare-filename output should
  be refused instead of falling back to the current directory, and an
  audit of which large-scratch commands (extract complete/smart,
  renumber) carry `--tmp-dir` at all.

  (B) **Replace `--index-type` on `add-locations-to-ways` with a single
  user-facing override flag.** Today: `--index-type sparse|external|auto`,
  default `auto` since 0.6.0 (`7c282070`, in the CLI and the library's
  `IndexType` default). The three-value flag still exposes
  implementation names (`sparse`, `external`) that don't mean anything
  to a user picking a PBF tool.

  Proposed shape:
  - Default behaviour: today's `auto` logic, unchanged (reference/pipeline.md
    has its rules: external only where the input allows it and a known
    memory budget says sparse's store would outgrow the page cache,
    sparse otherwise).
  - Single override flag, opting INTO the in-memory path: `--in-memory`
    (working name; alternatives considered: `--low-disk`,
    `--minimal-disk`, `--no-spill`). The override only matters when auto
    would have picked external - i.e., the input is sorted + indexed but
    the user doesn't have the temp disk for external's scratch.
    Forcing external the other direction is pointless: auto would have
    already picked it when conditions were met, and external can't run
    when they aren't (it requires sorted + indexdata).

  Why one flag is enough: the asymmetry above. Two flags
  (`--force-sparse` + `--force-external`) was the obvious symmetric
  shape, but `--force-external` is either redundant (auto picks it) or
  fails (preconditions not met), so it earns nothing.

  Why `--in-memory` over `--low-disk`: framing the override by what the
  user gets ("keep the index in process memory, don't spill to a giant
  scratch file") reads more naturally than framing by what they avoid.
  Slight imprecision since sparse still mmaps a values file
  (`referenced_count * 8` bytes), but that file is dwarfed by external's
  planet scratch and the user's mental model is "don't make a huge temp
  file."

  Library-side API change: `IndexType` enum loses its `FromStr` (no
  string parsing) and the dense-removal migration hint goes with it -
  users on `--index-type dense` would get clap's "unrecognized
  argument" error rather than the friendly pointer at sparse. The
  `altw_dense_index_type_rejected_with_migration_hint` test goes
  away in the same change. Acceptable cost: dense has been gone since
  `b70dd8c` (2026-04-30); by the time `--index-type` itself goes, the
  migration hint has done its job.

  Other "mode-like" flags (`extract --strategy`, `bench-read --mode`,
  `bench-write --writer`, `bench-merge --io-mode`, `diff --format`) are
  inconsistent but each picks a value out of a closed set that DOES
  matter to the user (e.g., extract strategies have different output
  semantics, not just performance). Leave them alone.

  Breaking CLI change. No urgency.

- [ ] **CLI output contract: full stdout/stderr/exit-code audit and
  streamlining.** Complete inventory and rethink of what every
  subcommand writes to stdout vs stderr, and what status codes it
  returns. Questions the audit must answer, per command:
  - Is stdout reserved for the command's actual product (data, JSON,
    report) and stderr for narration/progress/warnings, per Unix
    convention? Are we compliant where we should be?
  - Does the stdout content make sense - is it consumable by a
    script/pipe without filtering?
  - Which commands support `--json`? For each: is the output ONE pure
    JSON object/stream and nothing else on stdout? Any command that
    emits anything in addition to the JSON on stdout is a bug to fix.
  - Exit codes: 0 success, nonzero failure - but are partial-success,
    verify-mismatch, and "check found problems" cases (`inspect
    --indexed` already does exit 0/1 deliberately) coherent across
    commands? Document the contract.
  Deliverable: an inventory table (command x stdout/stderr/--json/exit
  codes), the fixes, and a durable contract in reference/ so new
  commands conform by construction.

  Partially delivered already: exit codes now split into 0 success,
  1 semantic-negative answer, 2 failure, 64 usage error, documented in
  `docs/cli/commands.md` "Exit codes". The
  stdout half is partly delivered: the CLI writes stdout through
  `crates/cli/src/output.rs` (a closed reader is a silent exit 0, not a panic), the
  library's report types render into a caller-supplied `io::Write`, and
  text lints in `brokkr.d/lints.toml` ban `print!`/`println!` and raw stdout
  handles outside that file. The stderr half is delivered too: the library
  writes no stderr (it reports through `host::Diagnostics`), and the CLI
  writes stderr through `output::cli_eprintln!`, so a closed stderr stops the
  diagnostics without ending the run. Still open: the per-command inventory
  table and the `--json` purity check. Known
  trade-off: streaming `diff` exits 0 when stdout's reader goes away before the
  verdict exists, so under `set -o pipefail` `pbfhogg diff a b | head` reads as
  identical.

- [ ] **A `--json` diagnostics mode.** The global `-q, --quiet` landed
  (`docs/cli/commands.md` "Quiet": a kind filter in
  `StderrSink::event`, `crates/cli/src/host.rs`). Still open: `--json`, a second
  sink serializing each diagnostic event's kind, key and arguments, which
  needs no library changes. It is a new capability with a public format to
  design, and normalising the summary-line shapes and keys is its first
  step. Facts it has to cope with:
  - Raw `cli_eprintln!` lines that are not events: getid's `-v` requested
    and missing ID lists (refused with `-q`, so quiet never had to cover
    them), the `bench-read`, `bench-write` and `bench-merge` key=value
    blocks (brokkr may parse them; the quiet ruling left them as they are),
    and the final `Error:` line. A `--json` covering them routes them
    through the sink first.
  - Summary lines have no common shape: some lead with the command name
    (`time-filter:`, `add-locations-to-ways:`), some with a verb (`Wrote`,
    `Sorted`, `Renumbered`, `Merged`), apply-changes prints a multi-line
    block, each `extract --config` region's line carries its own
    `  [i] path - ` label, `export` keys itself `export_to.summary`, and
    getid and `getid --invert` share the key `getid.summary` while
    `cat --dedupe` uses `cat.dedupe.summary`. A `--json` sink would inherit all of it.
  - `Developer` events (`[altw external] ...`, the geocode hotpath
    counters) carry `key=value` text or a bracketed prefix of their own.

- [ ] **Aligned read transport for worker pools remains separate work.**
  Ignored `direct_io` parameters remain plumbing; accepting the flag does not
  promise direct reads on those paths. Worker pools need their own aligned
  transport per worker because the walker's shared scratch cannot cross threads.

## Lint exemptions (moved out of the hygiene drain 2026-10-26)

The maintainer wants this resolved, but not in the drain.

- [ ] **`too_many_arguments` and `too_many_lines` are denied and then
  allowed locally all over.** Both are `deny` in the workspace
  `[workspace.lints.clippy]` (root `Cargo.toml`), yet many functions carry
  a local `#[allow]`, often both lints in one attribute, across `crates/lib/src/`,
  `crates/cli/src/` and the tests: the extract entry points,
  `writer_from_header`, `getid_dispatched`, `filter_by_id`, the getparents
  dispatchers, altw's `write_output_*`, `tags_filter_*`, apply-changes'
  `worker_loop` and `handle_candidate`, several CLI `run_*` functions, and
  more. A denied lint with that many local exemptions enforces nothing.
  The flag-refusal half of the original finding is done
  (`docs/cli/commands.md`, "Flag rules"), and the library already
  takes one `WriteOptions` instead of positional bools.

  Suggested shape, from the 2026-10-25 and 2026-10-26 sessions, to spar
  before acting:
  - Convert every exemption to `#[expect(..., reason = "...")]` with a
    real reason. That changes no codegen, and an `expect` fails the build
    once its exemption goes stale, so some allows will turn out to be
    removable rather than convertible.
  - Group parameters into a struct only on crate-private entry points
    called once per run. Never on hot inner helpers (`write/encode.rs`,
    apply-changes' worker and drain, the altw stages, the diff and derive
    block-pair shard engine), and never on public signatures without a
    decision of their own, since that is a breaking change.
  - Then ban new `allow(clippy::too_many_` with a `[[textlint]]` rule in
    `brokkr.d/lints.toml` (the existing rules there show the syntax).

  The alternative the finding offered, no longer denying the lints, is
  still open.

## Roadmap at a glance

Open work is grouped here by what actually gates it. Completed items
are removed; git history keeps them. Detail stays in notes/.

**Active arcs (large open engineering, pick deliberately)**

- Geocode builder rewrite G1-G4
  ([notes/geocode-build-opportunities.md](geocode-build-opportunities.md)).

**Deferred with explicit triggers - do not chase without the trigger**

- Shape-3 serial schedule walk on a header-warm or large-blob file
  (reference/blob-density.md "The walk term": batched exact probes cannot
  help, and cold dense walks already read ahead; trigger: a high-blob-count
  planet becomes a real workload).
- **io_uring output rail on a real command** (formerly write-path plan
  items 5/5b; the plan note is retired to git history, everything else
  in it answered or shelved). Two-step: (5) characterize `--io-uring`
  on a europe real-file command that is not compression-bound; keep
  gate >= 5 % europe wall, discard if flat on the first A/B.
  (5b) batched SQE submission inside `uring_writer` - submit count
  must drop >= 2x with wall unchanged; only matters once a command is
  actually on the io_uring path. **Blocked on host config first**:
  RLIMIT_MEMLOCK soft limit is 8 MB, io_uring preflight needs >= 16 MB
  (limits.conf change + re-login), which also blocks the sort
  `--io-uring` re-pin. For one session, `sudo prlimit --pid <claude pid>
  --memlock=67108864:67108864` from another terminal raises it for every
  command run after.
- **libdeflate at planet apply-changes** (formerly write-path plan item
  2b). The sync-mode 1.92x compression win is measured and banked
  (`2cd6ed6`); the pipelined null result was Denmark-scale only, where
  decode was the wall. Revisit shape: europe apply-changes first signal
  then planet ship/no-ship, keep gate >= 3 % europe at equivalent
  output size AND an explicit revisit of the "zero C deps" policy from
  `d180d62` - the policy rejection stands until deliberately
  overturned. Never measure it on `brokkr bench encode-workload` (proven unable
  to discriminate).
- apply-changes #11 splice-in-place / #13 exact-membership metadata
  (see the apply-changes entry below).
- Milestones A/B/C + GPU (Non-traditional optimization research).
- **renumber polish** (formerly notes/renumber-optimization.md, retired
  to git history; baseline in reference/performance/README.md). Item 1:
  varint encode fast path in the three wire_rewrite call sites via
  protohoggr's `encode_varint_to_slice` - plausible ~1-1.5 % of wall,
  needs a paired same-commit bench and widens the unsafe surface; skip
  unless renumber becomes critical-path. Also: finer reframe_ms
  breakdown is measurement tooling with no consumer; build only
  alongside a specific optimization candidate.
- **time-filter, gated on a real history workload** (formerly
  notes/time-filter-optimization.md, retired to git history). The
  2026-07-13 scope decision: history-file support is functional but
  outside the 1.0 planet-validation surface - weakest demand signal in
  the roadmap, no history dataset configured, every bench to date
  measured a near-no-op. Re-entry trigger: a real history consumer
  appears; then a europe history variant in the brokkr host config, a bench of the
  actual multi-version workload, and the parallel history path (workers
  emit per-block winners plus head/tail partial groups, consumer
  stitches across blob seams). Also gated: adaptive passthrough via a
  ~20-line all-survive shadow counter (prior attempt measured 0.63 %
  all-survive on europe vs ~14 % break-even - only recent cutoffs
  qualify), and a blob-level timestamp-range index (format bump, only
  if time-filter becomes hot). Do-not-reattempt pins: per-element
  `Instant::now()` in the callback (doubled japan wall) and raw-frame
  passthrough via pread-from-workers (regressed europe 92.6 -> 112.2 s
  even at 0 % passthrough; future passthrough work stays on the
  pipelined reader).
- altw-external RAM/NVMe-gated tiers
  ([notes/altw-external.md](altw-external.md)).

**Blocked on dataset / config** - see the section of that name under
"Planet-scale validation coverage".

## Bug-hunt carry-over (hunt 2026-08-12, closed out 2026-08-20)

Remaining open findings from the closed hunt.

### Change stream (`crates/lib/src/osc/`, apply-changes, merge-changes, diff)

- [ ] **Route uniform partial metadata into partial-column dense groups.**
  Plain nodes whose partial metadata is uniform, such as apply-changes
  output, are still written as plain nodes; `BlockBuilder::add_node_partial`
  could put them in a dense group carrying exactly their columns when that
  keeps what they claim.

### Core infrastructure and CLI

- [ ] **`crates/cli/src/lib.rs` argument splatting is partially cleaned up.** Round 4
  introduced `WriteOpts` and per-command flag structs for `getid`,
  `tags-filter`, `check`, `inspect`, `diff` and the geocode-index levels, which
  removed every run of three or more adjacent same-typed parameters - the class
  where a transposition compiles silently. What remains is length rather than
  hazard: several `run_*` functions still carry
  `#[allow(clippy::too_many_arguments)]` with parameters of mixed types
  (the attributes themselves are the list). Low urgency; folds into the lint
  exemptions item above.

### Read path

- [ ] **An evicting `BlobReader` advises `POSIX_FADV_DONTNEED` over `0..offset`.**
  The API half is closed - eviction is opt-in on the public readers
  (`BlobReader::evict_page_cache(bool)`, `ElementReader::evict_page_cache(bool)`,
  `ElementReader::from_path_with_eviction`), and commands follow
  `Host::page_cache`, which the CLI sets to `Evict`. What remains open is the
  syscall shape: the
  advised range is the whole consumed prefix `0..current_offset`, re-issued per
  blob, so a run issues one `posix_fadvise` call per blob, each nominally covering a
  growing prefix where `offset_prev..offset` would express the same intent over
  a bounded range. The kernel side is cheap per already-evicted page, so this
  may well be worth nothing. **Do not "fix" it on inspection.** Changing *which*
  pages get dropped changes the page-cache state every subsequent benchmark on
  the host observes, so the measurement perturbs its own baseline: closing this
  means a scheduled interleaved matched A/B at planet scale on a trimmed drive,
  with the host's cache state accepted as spent for the duration. It has
  survived four rounds untouched for that reason.


### Spatial and node-location resolution


### Geocode index

- [ ] **Admin interior flags are sound; the reader cannot use them yet.** Since
  2026-11-03 ([ADR-0022](../decisions/0022-admin-edges-are-covered-in-the-point-in-polygon-geometry.md),
  superseding ADR-0015) the edge cells are a conservative cover of the
  reader's own point-in-polygon boundary (`crates/lib/src/geocode_index/builder/cover.rs`),
  so a flagged cell is wholly inside its polygon. Re-enabling a PIP skip
  needs two things the index does not have: a header signal that its flags
  come from that builder (older indexes carry flags on boundary cells), and a
  reader that trusts only the query's own cell's flag, not the eight
  neighbours'. A format change; measure the PIP cost it would save first.
  The level cap (19) stays either way: an exact cover's work still doubles
  per level, now bounded by a proved cell count rather than a sample count.
  The PIP cost is the query cost: admin point-in-polygon dominates every row
  of the query latency table in `reference/performance/geocode.md`. The query's own admin cell
  already lists every polygon containing it under an exact cover, so the
  eight neighbours' entries are pure extra PIP work; the same header signal
  would let the reader drop them.
- [ ] **The geocode walk costs per cell, not per lookup.** Where a query's
  answer lies past its 3x3 block, or nothing lies near (open sea), the
  reader walks cells (ADR-0023), paying s2's `edge_neighbors` (a Hilbert
  decode and four encodes) and `Cell::rect_bound` per cell (per-call cost
  unmeasured); that is the +4 to +8 % on uniform bbox points in the latency
  table. Walking in (face, i, j) arithmetic with a vertex cache would take
  most of it off. Only worth it if sea-heavy or remote query mixes matter.

## Item/command-specific plans

Per-command plans and open follow-ups. Details drift as work lands.

- [ ] **[reference/blob-density.md](../reference/blob-density.md)** - Geofabrik-style PBFs (small blobs, many of them) scale very differently from `planet.openstreetmap.org`-style PBFs (large blobs, few of them). The `HeaderWalker`-touching commands split into three shapes: selective header-walk (`getid`, `getparents`), resolved by ADR-0006's blob-count dispatch; pure parallel-classify (`tags-filter`), which gains on density; and the shape-3 hybrid (`check --refs`, `check --ids`, `cat --clean`, `repack`, `degrade`, `extract --smart`, all via `build_classify_schedules_split`), whose serial schedule walk regresses on high blob count with near-zero production bite. Per-phase walk durations cannot be summed (ALTW sparse P4, notes/altw.md): a walk costs only where the preceding phase evicted the headers. Do NOT chase the shape-3 callers unless a high-blob-count *planet* becomes a real workload.

  - [ ] **Shape-3 serial schedule walk (`build_classify_schedules_split`).** The single-threaded `HeaderWalker` loop in `crates/lib/src/scan/classify.rs` does one QD=1 pread per blob. Measured 2026-07-12, before the cold-walk read-ahead of `6fdde3f4`: ~70 us/blob, invisible at production planet's blob count but a fixed ~97-109 s at 1.45 M blobs. **Full six-caller 8k sweep done 2026-07-12** (table in `reference/blob-density.md` "Full shape-3 sweep"): the walk fraction splits the family, which **narrows the payoff** from the earlier "helps six commands" framing. Read-only callers are ~2/3 walk - `check --refs` 66 % (`1851f73a`), `check --ids` 67 % (dirty) - so flattening it roughly halves their 8k wall (~2.7x -> ~1.3x). Re-encoding callers are only 18-29 % walk - `extract --smart` 29 % (`4b82686f`), `cat --clean` 19 % (`3f4c222c`), `repack` 18 % (`8f275ebf`), `degrade` 18 % (`6f8a3e94`) - their 8k regression is dominated by framing + writing 1.45 M tiny blobs, which the walk fix does not touch. So: **if the primitive is ever built, it is a win for the read-only pair only** (and those two are the same commands whose getid/getparents selective-scan cousins already got ADR-0006 dispatch). The io_uring batched-header-walker this entry used to name cannot work (each header's offset comes from the one before, so there is never a second probe to batch); cold walks of small-blob files now read ahead in windows instead, which the 8k sweep predates - see `reference/blob-density.md` "The walk term". Unmeasured at 8k since. Still deferred: near-zero production bite.

- [ ] **degrade** - v2 ideas, pick up when a consumer surfaces: `--recompress C` (forces decode even for passthrough-eligible flags); richer `--unsort` chaos modes (rotate/shuffle/reverse - only when sort measurement needs more than one overlap per kind); a registered planet `unsorted` snapshot for `sort` benching; `--unsort` throughput (denmark 7.7 -> 27.3 s from the Owned round-trip through the merge thread; three sketches in the retired notes/degrade.md's git history, worker-fires-the-swap being the cleanest - acceptable today because `--unsort` is one-time generation work). Flag semantics in docs/cli/commands.md.

- [ ] **getparents FullScan arm** - one idea parked without a trigger: converting the FullScan arm from pipelined decode to `parallel_classify_phase` (pure per-blob work, lower memory ceiling; est. 10-20 % where oversubscription bites). Bench-gated, speculative, and ADR-0006's dispatch already routes the cases where it would matter most.

- [ ] **apply-changes** (plan note `apply-changes-opportunities.md` deleted 2026-07-13; git history has the full record) - `apply-changes --locations-on-ways`. Cross-disk `--io-uring` rows (93.0 / 127.9 / 82.8 s) still need re-measurement on the fixed writer (the `fa8251d` CopyRange fix); cross-disk `--compression none` + `--io-uring` is open until re-measured. Multi-file output / RAID-0 is unlanded and low priority. Remaining items:
  - [ ] **#11 splice-in-place for low-touch rewrites (deferred).** For `NeedsRewrite` blobs with <=K affected elements (K~64), splice raw wire bytes for unmodified element runs instead of full decode + re-encode (~1.5-2 s at daily; the scaffolding that used to sit in `crates/lib/src/write/raw_passthrough.rs` was deleted 2026-08-20; the per-group passthrough analysis is in this file's raw passthrough section). Saves classify/rewrite CPU but not output bytes, so it will not move the writer-bound wall. Revisit if the writer ceiling moves.
  - [ ] **#13 exact-membership metadata / sidecar (format project).** Per-blob ID-range-only metadata forces slow-path decode on pure creates inside an existing range: 15,224 FalsePositive blobs / 92,677 slow-path = 16 % of slow-path work wasted at planet. The false-positive distinction is documented on `block_overlaps_diff` in `crates/lib/src/commands/apply_changes/classify.rs` (`range_overlaps` true for create IDs inside the blob's range, `block_overlaps_diff` false because no element in the block matches; the old `classify_only` "FalsePositive" comment the note pointed at was removed in the streaming refactor). Two fix shapes: (a) wire-format exact-overlap scanner on decompressed bytes; (b) per-blob membership sketch in indexdata. Not a quick cleanup.
  - Open questions preserved from the note: actual overlap-blob ratio under larger OSCs (re-measure if input diffs grow; governs worker-pool load); byte-budget reorder capacity sizing (current setting works at planet; revisit on RSS/stall signals); scanner HeaderWalker vs worker throughput balance (no signal it's an issue).

- [ ] **[notes/altw-external.md](altw-external.md)** - `add-locations-to-ways --index-type external`. Current baselines and history are in `reference/performance/add-locations-to-ways.md` and [`notes/altw-optimization-history.md`](altw-optimization-history.md); failed attempts and the do-not-retry list are there too.

## Planet-scale validation coverage

README's planet table is the source of truth for "this command runs
cleanly on the reference host" (the README states its RAM). The README
refresh campaign settles the "not benched at planet" claims below as its
cells land. This section tracks the remaining axes and dataset gaps.

### Latent planet risks

- [ ] **altw sparse at planet where auto cannot see memory.** Sparse
  thrashes at planet on a 28 GB-class host: the rank-indexed flat scratch
  outgrows the page cache and pass 2's location write-back turns into
  random-access mmap faults (soft failure, unboundedly slow, not OOM;
  reference/performance/add-locations-to-ways.md "Auto routing
  threshold"). Since 0.6.0 the default `auto` routes such a run to
  external where it knows the memory budget. Left: auto keeps sparse when
  the budget is unknown (every non-Linux host, containers without a
  visible cgroup v2 mount) or available memory is at or below the 3 GiB
  headroom, so a planet run there still thrashes, announced only by the
  `altw.auto_select` Note. Open: whether that case should warn or refuse
  instead.

### Blocked on dataset / config

- [ ] **Additional planet snapshots** for `diff-snapshots`. The host
  config has only one point-in-time planet alternate, so the
  snapshot-range axis is a single pairing. Downloading another
  snapshot 2-4 weeks away would let us measure diff-wall vs
  snapshot-delta-size empirically.
- [ ] **No real dataset crosses the antimeridian (brokkr#138).** Denmark
  was byte-identical across four antimeridian fixes in the geocode
  builder, so the real-data gate is blind to that defect class. Split on
  2026-11-09, both halves now on pbfhogg: the dataset entry (Geofabrik
  `australia-oceania/fiji` via `brokkr download`, plus its
  `brokkr.d/hosts` block) is our config, and brokkr asks what queries a
  pbfhogg-built geocode index from the command line. Nothing does today:
  the CLI only builds the index, and the reader is reached through
  nidhogg. A lookup command (`pbfhogg geocode INDEX LAT LON` with
  machine-readable output) would let brokkr hold pinned points per
  dataset after its `expect` pattern; it is a feature, not yet decided.
  The alternative is a nidhogg-served gate. Answer brokkr#138 either way.

### Un-benched permutations (low priority)

Conditional measurement work; run only when the stated question arises:

- [ ] **ID-set shape for `getid` / `getparents`**. The lists are
  configurable per dataset (`getid_ids` / `getparents_ids`); no bench
  sweeps set shape (sparse vs dense, spread vs clustered, cold vs hot
  cache). Not needed for general validation; open only if ID-set shape
  becomes a perf question.
- [ ] **`--direct-io` at planet**. Covered: apply-changes, cat
  passthrough, `cat --dedupe` and sort (reference/performance/write-pipeline.md
  "`--direct-io` impact summary"). Not measured: extract,
  add-locations-to-ways external, merge-changes. Only matters if
  direct-io becomes a default on any of them.
## Performance

- [ ] **altw external pass A's exposure to sustained-write state
  (2026-11-09).** A short trim before a cell
  restores pass A (about 23 s against 36-41 s without one) and launch-time
  Dirty/Writeback is negligible (reference/performance/sort.md, "altw external
  pass A after a trim or after another cell", `098c89cb`); exposure scales
  with scratch bytes ("Packed id-shard records"). Left: an idle-only rest
  cell and no-trim gaps matched to the trim's duration (the harness
  refused `sleep`, so neither ran), so trim is not isolated from elapsed
  time; in-pass dirty throttling and device-side state need per-cell
  device stats (brokkr#74). Shared per-bucket shard files were built and
  refused (pass A +27 % trimmed, +55 % untrimmed, all in append-lock
  waits; "Shared per-bucket id shards, refused"). No implementation lever
  on the exposure is left in hand: the unbuilt shapes there (a bounded
  descriptor cache over private files, striped buckets) bound
  descriptors, not write exposure.
- [ ] **Byte-budgeted fused task engine (2026-11-08).** One execution model for decode, transform, encode
  and frame under one byte budget, replacing the unordered classify pool,
  `run_ordered`, the writer's separate rayon framing dispatch and
  apply-changes' private scanner/worker/drain pipeline. Admit tasks in
  sequence order against the budget; reserve a completion envelope before
  a task runs (piecemeal credit acquisition deadlocks: every worker holds
  input credit while waiting on output credit and the earliest task
  starves); release credit when storage becomes reusable (ordering
  reservation at ordered delivery, output lease at I/O completion). Makes
  transient memory a chosen constant independent of worker count. Moving
  storage from workers to tasks does not lower peak by itself at
  saturation; the budget value is the memory control, and fusion shrinks
  the envelope so the same budget sustains more concurrency. Risks: the
  admission protocol's liveness around the earliest task, oversized and
  hostile blobs, cancellation and error paths that release leases, and a
  byte ledger does not bound pages glibc retains. apply-changes now bounds
  outstanding seqs by count (`crates/lib/src/commands/apply_changes/window.rs`, 512, non-binding
  at planet with in-flight peaks of 300-410); a byte budget here would
  replace that window, not sit beside it.

- [ ] **glibc's mmap threshold, per command.** A CLI-wide fixed threshold
  was ruled out at planet; a per-command choice, the measurements it still
  needs across dataset sizes and hosts, and the structural route (recycling
  blob-scale buffers) are in `notes/mmap-threshold.md`.

- [ ] **Stall-counter follow-ups (found 2026-10-26 adding the unordered
  pools' `*_wait_ns` counters).** Each is its own change; none is measured.
  - apply-changes' `candidate_rx` is its own uncounted `Mutex<Receiver>`
    fan-out (`crates/lib/src/commands/apply_changes/streaming.rs`).
  - The first-error slot is a mutex locked on every claim in altw stages 2
    and 3 and geocode pass 1.5; an `AtomicBool` fast check would avoid the
    shared cache-line traffic. Planet-measured paths: needs its own A/B.
  - Every altw router publish takes a global `stats` mutex and the global
    `notify_mu`. Same caveat.

- [ ] **Expose phase events as a proper Rust event/hook API** - wrap
  instrumentation calls in per-command `probes` modules, then swap the
  backend from the current FIFO sink to `tracing` spans/events so
  library consumers can subscribe. **History (recovered 2026-07-13):**
  a probes-module PoC on time-filter landed (`2eae6ff`) and was
  reverted (`ddd6dc7`) after stress-testing against altw's ~140
  counters / 26 markers showed the design does not absorb the real
  aggregation shapes (AtomicU64 + thread::scope, channel-merged Stats,
  post-blob nanosecond accumulators); the plan doc
  (`notes/instrumentation-layering.md`) was retired with it (deleted in
  `9770abc`, recoverable from git history). If revisited, start from
  the revert message's guidance - cfg-gated empty-twin + ZST tuple-tail
  composition, shaped per command's actual question - not from the old
  rollout plan. No `probes` module or `tracing` usage exists in the
  tree today.

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks.

### Raw group passthrough

- [ ] Shadow-count fully selected groups on a real partial-match workload
  before implementing per-group raw passthrough. A consumer must choose
  between preserving the original string table for mixed raw/re-encoded
  groups and splitting those groups into separate output blobs. See
  `reference/pipeline.md`, "Block Builder", for the constraints.

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Command surface

- [ ] **`cat` as the universal cleaner** (raised 2026-10-26). Every other
  command refuses malformed or incomplete input loudly and points at `cat`;
  `cat` is the one command that turns such a file into a valid one, outside
  its hot standard paths (plain passthrough and indexdata generation stay as
  they are). It already plays that role for missing and partial indexdata,
  and `cat --clean user` skips resolving a user index it drops (ruled
  2026-10-26, `cat_clean_user_rescues_a_bad_user_string_index`). The
  project: decide which refusals `cat` should recover from instead, under
  which flags, and how each recovery is reported. First candidate: a repeated `Info` or
  `DenseInfo` message, refused while the block is parsed, before any
  `--clean` applies, so `cat --clean` cannot rescue that file today.
- [ ] Auto-selection: `--index-type auto` (sparse vs external) is the
  default since 0.6.0; blob-count dispatch landed for getid and getparents
  (ADR-0006) and `-j 0` sizes workers automatically. Extend to other
  decisions: sequential vs pread-from-workers based on available RAM and
  blob count elsewhere; compression level based on output target.

### Ecosystem

- [ ] Add GitHub Actions CI - clippy, tests, rustfmt, doc build on Linux
  (`.github/workflows/` holds only the docs-site deploy, `docs.yml`; no
  clippy/test workflow). The project's gate is `brokkr check`, which a
  hosted runner lacks; that is the design question. Once it exists, add
  the status badge
  (`https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`).
- [ ] Add GitHub Actions release pipeline - build binaries on tag push, attach to GitHub release
- [ ] CI with benchmark regression guard.
- [ ] API reference completeness: a rustdoc coverage audit of the public
  surface (the guide pages under docs/guide/ exist).
- [ ] PyO3 Python bindings (read/write API for the Python ecosystem).
- [ ] Packaged "planet on 32 GB" reference pipeline (documented, runnable).

### Non-traditional optimization research

Ordered by reviewer consensus (6 reviewers, 3 archetypes: perf, arch, planet).
The first three form a dependency chain. The last two are independent
hardware-level tuning. Investigate allocators and columnar together as
Milestone A, SIMD as Milestone B, huge pages and NUMA as Milestone C.

**Milestone A: data layout + allocation (investigate together)**

- [ ] **Global allocator (jemalloc/mimalloc) at planet scale** - an
  earlier Denmark-only bench showed <1 % wall, and both were removed as
  CLI features (they broke `--all-features` builds through duplicate
  `#[global_allocator]` definitions). The cross-thread-free pattern that
  motivated a retry is gone from the pipelined reader (see the
  decoded-block working-set note in `crates/lib/src/read/pipeline.rs`). Retry only
  if a fresh `--alloc` planet profile shows allocator-bound time;
  revisit when `tikv-jemallocator` tags a jemalloc 5.3.1 release.
  Allocator policy knobs are settled at the `set_mmap_threshold` doc in
  `crates/lib/src/process.rs`.

- [ ] **1. Custom allocators (per-block arena)** - 4/6 reviewers ranked 1st.
  Research note retired to git history 2026-07-16
  (notes/arena-allocator-research.md: crate landscape - bumpalo /
  bump-scope / hand-rolled - alloc profiling data, 5-step plan; its own
  status header says steps 1/1a/1b are DONE, the decode path is at
  libosmium parity, and steps 2-5 are speculative pending a fresh
  `--alloc` profile that shows churn that matters).
  Key finding: `parse_and_inline` generated ~829 MB alloc churn (Japan)
  from two temp `Vec<(u32, u32)>` per block; step 1 (thread-local scratch
  Vecs) took it to 48 MB (-94 %). Steps 2-5 escalate to bumpalo, columnar layout, pipelined reader
  re-enablement. Top crate candidates: `bumpalo` (v3.20, zero deps,
  stable), `bump-scope` (v2.2, scoped sub-allocations), or hand-rolled
  50-line bump allocator.

**Scratch buffer reuse audit (step 1 of arena research):** per-iteration
allocations that remain, ordered by impact:

- [ ] **Geocode pass 3 stage A par_iter** - per-way `Vec::new()` inside
  the two `flat_map_iter` closures in `crates/lib/src/geocode_index/builder/pass3.rs`.
  Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** - 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work. Shape of the fix (for when /
  if it's ever needed): change `BlockBuilder::add_relation` from
  `members: &[MemberData<'_>]` to `impl IntoIterator<Item = MemberData<'_>>`,
  add three parallel packed scratches on the builder
  (`member_roles_scratch`, `member_ids_scratch`, `member_types_scratch`)
  so the single-pass iteration can write all three protobuf member
  fields without re-scanning. Most callers already reuse a buffer
  (`members_buf.clear(); members_buf.extend(...)`) so the saving is
  small; the concentrated win is `apply-changes`
  (`rewrite_block.rs`, `element_writes.rs`) which builds fresh
  `Vec<MemberData>` per relation from OSC input with no reuse.

**Milestone B: vectorization (after columnar layout stabilizes)**

- [ ] **3. SIMD** - universal agreement: comes after columnar. Columnar
  now shipped for extract (single + multi-region). ASM inspection
  confirms LLVM does NOT autovectorize the bbox classify loop - the
  `push()` side effect prevents vectorization entirely.

  **Codegen finding:** explicit AVX2 intrinsics are the only path.
  The multi-bbox loop is a better SIMD target than single-bbox: N
  region tests per node amortizes setup (N=5 with AVX2 8-wide ≈ 1.6
  nodes of all 5 tests per vector op). Single-bbox is only 2.8% of
  total Europe extract time - not worth it alone.

  SIMD becomes worthwhile when:
  - The classify loop is a larger fraction of runtime (after write-path
    optimization makes classify the bottleneck)
  - Multiple consumers use columnar arrays (multi-region, polygon PIP)
  - Batch varint decode in protohoggr (different SIMD target, broader
    impact across all commands)

  Varint SIMD research (notes/SIMD.md, retired to git history
  2026-07-16) previously closed - scalar beats SIMD for individual
  LEB128 varints. Batch varint decode into contiguous arrays is a
  different problem (columnar enables this).

**Milestone C: hardware-level tuning (where perf counters justify it)**

- [ ] **4. Huge pages** - `MAP_HUGETLB` (2 MB pages) for large mmap'd
  structures. Sparse ALTW rank-flat values file, geocode index mmap
  reader, external join temp files. Unmeasured; the hoped-for gain is
  on random-access patterns. Requires hugepage availability
  (`sysctl` config) or `madvise(MADV_HUGEPAGE)` for THP. Linux-only.

- [ ] **5. NUMA-aware memory placement** - last by unanimous agreement
  (6/6). Only matters on multi-socket servers; the benchmark hosts are
  single-socket. Pread-from-workers pattern already has
  natural NUMA affinity (thread-local allocations, first-touch policy).
  `set_mempolicy(MPOL_BIND)` / `mbind()` for explicit placement.
  Candidates: pipelined reader decode pool, sparse ALTW rank-flat
  interleave, external join scatter buffers. 10-20% on dual-socket,
  0% on single-socket. Requires per-host tuning and NUMA hardware to
  validate.

**Separate track (GPU, independent of milestones A-C):**

- [ ] **GPU-accelerated point-in-polygon for geocode builder** - Pass 2
  tests billions of nodes against admin boundary polygons. NVIDIA's
  cuSpatial has production-quality PIP (winding number, handles holes).
  Depends on columnar batch processing for efficient host-to-device
  transfer. Rust interop via `cudarc`. Feature-gate behind `cuda`.
  Planet: 2.5B nodes, polygon set ~100 MB. Only worthwhile at
  Europe/planet scale. No precedent in OSM tooling.

### Research / stretch ideas

- [ ] Incremental geocode index update (daily diff → index patch, no full rebuild).
  Analysis note retired to git history 2026-07-16
  (notes/incremental-geocode-index.md: 4 approaches analyzed).
  Recommended: v1 append-only delta index with query-time merge
  (simplest, no format changes), v2 S2 cell-level partial rebuild
  (better query perf, proportional to diff size).
- [ ] Incremental extract update (`extract --apply-changes` - base extract + OSC +
  region → updated extract without re-reading planet).
  Recommended: compose two existing commands - `apply-changes` on
  the region extract (current apply-changes already tolerates OSC ops
  referencing elements outside the region; see reference/osmium-parity.md), then
  `extract` to re-filter to the bbox, orders of magnitude cheaper than
  the full-planet pipeline (unmeasured). Works for simple strategy immediately.
  Complete/smart strategies need planet access for newly referenced
  elements outside the bbox.
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
  (The two research notes, `spatial-index-in-pbf.md` and
  `way-blob-bbox-speculation.md`, were deleted 2026-07-13 - speculative
  only, conclusions retained here, full analysis in git history.)
  Node blob header scan is already fast. Way blob spatial bboxes are
  limited by chronological ID ordering (the old notes estimated a small
  skip fraction on Denmark; no recorded cell). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [ ] Per-blob summaries written by the producer (2026-11-03; a new
  product, not a fix). A versioned
  per-blob extension carrying measured counts, ID boundaries, timestamp
  range, bbox and metadata coverage would let a repeated inspect skip
  decompression. Stored summaries are claims, so this is "a description
  from claims" beside the measurement; partial coverage could be filled
  by scanning only the blobs without one. Needs a design document first.
