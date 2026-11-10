# pbfhogg TODO

## Carried over from the 0.6.0 release (2026-11-09)

The README's three tables come from the campaign at `53360af4`
(reference/performance.md "README campaign"). Left open:

- [ ] **Planet isolation-cat row, held back from the README library
  table.** `d73fbee8`: library 43.1-55.4 s, CLI 55.9-69.5 s, ratio 1.28,
  where the typed `brokkr cat` cell on the same file and commit
  (`be808f35`) took 42.4 s. The CLI arm's interval is 13 s or more behind
  a plain cat, and the library arm swings 12 s between iterations.
  Unexplained.
- [ ] **Script benches with no clean cell since their engines moved**
  (`--iterations 1`, three invocations per condition; dry-run first with
  `brokkr bench --dry-run NAME ...`, the flag before the bench name;
  dedupe-self, dedupe-pair, vs-osmium and diff-text are v2 since
  brokkr#134, so compare on child walls):
  - dedupe-pair `--with one-relation`, cold x3, warm x2: `6fdde3f4` is
    the dirty variant measured at `bd2408d5`.
  - dedupe-self, denmark and `--dataset japan`, cold and warm x3, plus one
    `--cache warm --check yes`: `5432aac7` and `6fdde3f4` changed the
    engine.
  - sort-cold `--dataset europe --force yes` x3 after a trim, plus typed
    `brokkr sort --dataset europe --bench 1` x2: `5432aac7` was only ever
    an uncommitted variant.
  - diff-unindexed `--new 20260420`, all and `--type way`, x3:
    `b58e8ef3` is on its read path.
  - diff-text denmark `--new 320k` x3, and germany `--new 20260420
    --verbose yes --common no` x3: `3286a9c5` landed with no clean cell.
- [ ] **Remaining expect declarations (brokkr#71).** The check and
  diff-snapshots answers are declared in `brokkr.d/hosts/bygg.toml`. Not
  run: `diff` per OSC (a primary diffed against itself with its next OSC
  applied should be `differs`: denmark 4705, planet 4913, and so on). The
  `test-unsorted` snapshot is the same file as `unsorted` and has no
  `check-ids = "violations"` of its own.
- [ ] **check --index coverage of unindexable blobs.** `unindexed_candidate`
  needs an observable kind, so an empty or mixed-kind unindexed blob among
  indexed ones passes `check --index` while the consumers' guard in
  `src/read/frame.rs` refuses the same file. docs/guide/indexdata.md now
  states the exception; decide whether coverage should instead be
  independent of indexability.
- [ ] **README Tokyo extract line.** vs-osmium has an `extract` row now
  (dataset bbox, complete-ways on both sides); measure it on japan to
  bring the README line back.
- [ ] **Performance guide plantasjen tables** (Japan extract, apply-changes
  at scale and with LocationsOnWays, write throughput, the io_uring and
  O_DIRECT tips) carry host and commit labels but were not re-measured on
  bygg.
- [ ] **brokkr doc fix.** `brokkr man library isolation-scenarios` still
  lists "the capped malloc arenas" in the CLI interval (gone since
  `25172fc3`).

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
  default `sparse`. The three-value flag exposes implementation names
  (`sparse`, `external`) that don't mean anything to a user picking a
  PBF tool, and the `auto` value is what the default should have been
  in the first place.

  Proposed shape:
  - Default behaviour: today's `auto` logic. Pick external when the input
    is sorted + indexed (the fast path at planet scale), pick sparse
    otherwise.
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
  `cli/src/output.rs` (a closed reader is a silent exit 0, not a panic), the
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
  `StderrSink::event`, `cli/src/host.rs`). Still open: `--json`, a second
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

- [ ] **`--direct-io` is accepted and ignored on many paths.** The parameter
  is taken and discarded in altw `stage1_way_pass`,
  `build_node_index_sparse`, `collect_way_referenced_node_ids`,
  `collect_relation_member_ids` (every pread path, so the flag does nothing
  to reads in passes 0 and 1, stage 1 or stage 2),
  `getid::parse_ids_from_pbf`, tags-filter's `collect_relation_member_closure`
  and `collect_way_node_dependencies` (both scans pread through the walker's
  buffered descriptor), and `check --refs` / `check --locations` (both
  parallel unaligned preads; the CLI refuses the flag where neither the
  ID nor the index check is selected, and beside them it reaches only those).
  Extract's pread passes open buffered regardless (a comment explains
  alignment), multi-extract's single-pass path writes through
  `PbfWriter::new(BufWriter)` regardless, `export` has no I/O flag, and
  `inspect` prints that it ignored the flag. An operator who chose
  O_DIRECT to protect the page cache still fills it, with nothing logged.

  Where the flag is honoured it is often only for the header read and the
  first-blob indexdata probe. Sort and `cat --dedupe` are the exception
  that honours it throughout (the pass 1 header walk through
  `HeaderWalker::open_direct`, pass 2 through direct sources whose copied
  frames `src/write/copy_range.rs` reads in aligned chunks).
  Apply-changes (the front read only: the scanner's header walk, the
  worker pool's body preads, and the splice or owned-route passthrough
  reads all go through buffered descriptors), renumber (header only), repack and degrade's
  decode path (the classify pools pread through a buffered shared fd),
  time-filter's snapshot path, tags-filter's two-pass mode (pass 1, the
  relation closure, the way dependencies and pass 2 pread through walker
  descriptors; `-R`'s pipelined reader does honour it), and getparents'
  dispatch probe all read their bulk data buffered. Writes are
  honoured by every PBF writer that goes through `Output::pbf_writer`,
  except multi-extract's single-pass path.

  Two consequences are visible to a user. The "I/O capability matrix" in
  `docs/cli/commands.md` marks most of these commands "Yes (R+W)" or
  "Yes" and has no row for `digest`: it describes the flag's presence, not
  its reach. (A build without the `linux-direct-io` feature already refuses
  `--direct-io` while parsing on every command, through `DirectIoArg`'s
  value parser.) Even an honoured open can be silently buffered:
  `statx(STATX_DIOALIGN)` reports zero alignment where the filesystem
  falls back to buffered I/O (ext4 `data=journal`, inline-data or verity
  files; f2fs files whose writes are forced buffered), and
  `src/direct_io.rs` keeps the 4096 floor since the run stays correct.

  Shape suggested so far: do NOT remove the ignored `direct_io`
  parameters - an earlier drain removed the parallel diff's and it was put
  back as deliberate plumbing; they are where the flag reaches once those
  passes honour it. A walk can already honour it single-threaded
  (`HeaderWalker::open_direct` over `direct_reader::DirectPositional`);
  the classify and shard pools that pread the walker's descriptor from
  worker threads would each need a direct transport per worker, since that
  one shares a single aligned scratch buffer. For the surface, a per-command reach table in the CLI (read
  passes honoured, write honoured) that generates both the capability
  matrix and the `--direct-io` help text; refuse the flag where it reaches
  nothing (done for `check`, under the existing no-op-flag rule); one `Note:` per run when
  statx reports zero alignment. Enforce with one contract test per command
  that a no-feature build refuses `--direct-io` before opening any input,
  and a unit test of the table against the capability matrix.

## Lint exemptions (moved out of the hygiene drain 2026-10-26)

The maintainer wants this resolved, but not in the drain.

- [ ] **`too_many_arguments` and `too_many_lines` are denied and then
  allowed locally all over.** Both are `deny` in the workspace
  `[workspace.lints.clippy]` (root `Cargo.toml`), yet many functions carry
  a local `#[allow]`, often both lints in one attribute, across `src/`,
  `cli/src/` and the tests: the extract entry points,
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
  to git history; baseline in reference/performance.md). Item 1:
  varint encode fast path in the three wire_rewrite call sites via
  protohoggr's `encode_varint_to_slice` - plausible ~1-1.5 % of wall,
  needs a paired same-commit bench and widens the unsafe surface; skip
  unless renumber becomes critical-path. Item 2: **do not re-propose**
  deriving way ranks from the schedule instead of `way_id_set` - the
  IdSet serves R2d's reverse lookup, dense-ID-per-blob does not hold on
  real planets, and every alternative measured or sketched is strictly
  worse than the 160 MB it would save. Item 3: finer reframe_ms
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

The seven `notes/bugs-*.md` documents are retired; `git log` holds the full
hunt record and the fixes. Everything those documents left deliberately open
is restated here, with the reason it was left open, because that is the part
that has to outlive the notes. Refuted findings did not move here - they are
recorded as comments at the code they concern (`src/debug.rs` on FIFO marker
atomicity, `src/read/pipeline.rs` on the seq-0 pool-failure push,
`src/commands/extract/multi.rs` on the reverted parallel writer,
`impl Drop for UringState` on the teardown asymmetry).

### Change stream (`src/osc/`, apply-changes, merge-changes, diff)

- [ ] **`diff -v` keyed tag lines are not bounded in count.**
  `VERBOSE_SEQUENCE_DIFF_MAX_EDITS` caps the positioned scripts (refs,
  members, duplicate-key tags) but its doc says keyed unique-key tag lines
  are not counted, so an element with thousands of changed unique keys
  prints thousands of `-"k"="v"` / `+` / `~` lines. Values are printed whole
  by contract (the -v paragraph in the CLI reference: complete literals, never
  truncated); whether the line count should get a cap and an omission line
  is an open contract choice. Found 2026-10-28 while settling -v escaping.

- [ ] **`VERBOSE_SEQUENCE_DIFF_MAX_WORK` is unmeasured.** Its doc in
  `src/tunables.rs` says "sized so one call stays in the low milliseconds;
  not measured". With `VERBOSE_SEQUENCE_DIFF_MAX_EDITS` (1000) now stopping
  the search past layer 1000, the work cap mostly matters for long snakes.
  A unit-level timing of a worst-case call (two ~30k sequences differing
  near the cap) would settle whether 1 << 20 is right. Low priority.

- [ ] **`diff -v` keyed tag lines carry no positions.** Unique-key tag lists
  print keyed `-"k"="v"`/`+"k"="v"`/`~"k"` plus `tags: order changed`, so
  `[a=1] -> [a=1,b=2]` and `[a=1] -> [b=2,a=1]` print the same `+"b"="2"`, and
  the detail cannot reconstruct the new tag order. Deliberate: the keyed
  view is the readable one for the common case, and the CLI reference says tags
  are keyed without positions. Revisit only if someone wants `-v` to be a
  reconstructible patch; then tags would go through the positioned script
  always.

- [ ] **`merge-changes --simplify` holds the rendered XML of every input
  change** (peak anon at planet 7-OSC in reference/performance.md,
  merge-changes `--simplify`). The
  output's `(kind, id)` order is what forces holding it. Bounding it further
  means either spilling rendered batches to scratch and reading survivors
  back by range, or an index-only first pass plus a re-parse into key-range
  buckets; both add a full payload write and read.

- [ ] **Absent-versus-zero survives in the PBF metadata representation.**
  `osc::compact::ArenaMeta` carries presence only for version and user;
  uid, changeset and timestamp there, and every field of
  `write::block_builder::Metadata`, still share absence and zero, so an OSC
  element with `version="4"` and nothing else
  is written with `uid = 0`, `changeset = 0`, `timestamp = 0` - indistinguishable
  from a genuine user 0 or a genuine epoch timestamp. Same collapse already
  fixed for coordinates, for `push_info_attrs` and for OSC `version`; it
  survives here because absence and zero share one representation through the
  whole PBF write path. `cli/tests/cli_apply_changes.rs::merge_metadata_preservation`
  currently asserts the bug - it is documented to record the collapse rather
  than endorse it, and its assertion message says what must change. The fix is
  per-field presence for the remaining fields in both types plus `Option`-shaped fields through
  `BlockBuilder`: a write-path change, wide enough to deserve its own round.

### Core infrastructure and CLI

- [ ] **Replacing a directory output the caller cannot write fails at the
  move into its backup, and nothing says so up front.** The move renames
  the old directory into the backup container, a new parent, and Linux
  then requires write permission on the moved directory itself
  (`vfs_rename`, `MAY_WRITE` on a directory source whose parent changes).
  So an unprivileged `build-geocode-index --overwrite` over a read-only
  index (mode 0555, say) passes construction and fails at publication with
  "cannot move existing output ... aside"; nothing is lost (the empty
  container is removed) but the work is. True for every `Staging`
  placement, not introduced by `Staging::DirWithBackups`. Decide whether
  construction should check it and refuse before any work, and document
  it in `docs/cli/commands.md`. Found 2026-10-29 by the backup-placement
  item's spar.

- [ ] **A standalone pipelined `PbfWriter` flushes `Ok` after a write
  already returned its writer thread's error.** `pipeline_send_error`
  takes the join handle, so a later `flush` finds no thread to join and
  returns `Ok(())` for the `to_path*` family (tests pin it with
  `w.flush().unwrap()`); only a writer filling an `Output` stays failed,
  through its `Completion`. Later writes are refused since the framing
  barrier landed (2026-10-29), but a caller that ignores one write error
  and trusts `flush` is told the file is good. Decide whether a recovered
  failure should make `flush` fail too. Found 2026-10-29 by the framing
  barrier item's scout.

- [ ] **JSON output and the human text quoting escape different sets.** The
  human text (`diff -v`, `inspect --show`, the inspect report, `inspect
  tags`) goes through `text_quote::Quoted`, which also escapes C1 controls,
  DEL, U+2028/U+2029 and the bidi controls; `inspect --json` and export go
  through serde_json, which escapes only quote, backslash and C0 and leaves
  the rest literal (valid JSON, not terminal-safe). Both are correct for
  their consumer; open question is whether JSON printed to a terminal
  should match. Also: `inspect --get` prints raw values by design, so a
  newline in a header string makes a multi-line answer (documented; use
  `--json`). Found 2026-10-28 while quoting inspect text.

- [ ] **`inspect --blocks` / `--anomalies` and `--locations` still hold
  per-blob and per-way lists.** The ordering sequence is bounded now (a
  64-segment head plus a count), but `--blocks`/`--anomalies` keep a
  `BlockInfo` per data blob (`BlockAccum.block_infos`), and the JSON path
  re-collects them into a `Vec` of `Value`s even when `--blocks N` asks for
  only the first and last N. `--locations` keeps one `u32` per located way
  (`LocationStats.coord_counts`) and
  `locations_json` clones and sorts it for the percentiles, so twice that.
  Both are opt-in and not on brokkr verify's path. Fix shape: a first-N /
  last-N ring for the block listing with streaming per-kind stats (the
  anomaly medians need a decision: exact medians need the values, an
  approximate sketch does not), and a histogram over coord counts for the
  `--locations` percentiles (coord counts per way are capped by the way
  node limit, so an exact histogram is small). Found 2026-10-28 bounding
  the ordering sequence.

- [ ] **A decoded data blob with no elements is classified mixed.**
  `classify_block(false, false, false)` returns `BlockKind::Mixed`, so under
  `--distrust-metadata` / `--extended` / `--locations` an empty data blob
  makes `ordering.standard` and `data.objects_ordered` false and counts as
  a mixed block, while the index-only path reports the blob's declared kind.
  Whether an empty blob should be neutral to ordering (skipped) or flagged
  as its own anomaly is undecided; "mixed" is the wrong word for it either
  way. Found 2026-10-28 bounding the ordering sequence.

- [ ] **`cli/src/lib.rs` argument splatting is partially cleaned up.** Round 4
  introduced `WriteOpts` and per-command flag structs for `getid`,
  `tags-filter`, `check`, `inspect`, `diff` and the geocode-index levels, which
  removed every run of three or more adjacent same-typed parameters - the class
  where a transposition compiles silently. What remains is length rather than
  hazard: several `run_*` functions still carry
  `#[allow(clippy::too_many_arguments)]` with parameters of mixed types
  (the attributes themselves are the list). Low urgency; folds into the lint
  exemptions item above.

- [ ] **The promise guard has two blind spots.** Every CLI test runs
  `pbfhogg` under the seccomp promise guard (`cli/tests/guard.rs`), which
  enforces the no-helper-process and inherited-descriptor promise of
  docs/cli/commands.md "Names pbfhogg leaves behind" for the whole
  run. It cannot see: (1) io_uring submissions - an `IORING_OP_CLOSE` (or
  similar) on the inherited descriptor is no syscall the filter checks; the
  `--io-uring` writer is the only ring and never touches an inherited
  descriptor, and only the brokkr text lints (`brokkr.d/lints.toml`) guard
  that. (2) a
  process-creating `clone3` caller that ignores the `ENOSYS` the guard
  returns and carries on (glibc and musl fall back to `clone`, which the
  guard kills; a raw caller need not). Closing (1) would need a
  ring-op allowlist (`IORING_REGISTER_RESTRICTIONS`) or a guard that bans
  `io_uring_setup` on runs without `--io-uring`. (hotpath builds, which the
  guard never runs, are covered by the `no-hotpath-cpu` script check on the
  resolved graph; its residue is that it certifies the checked resolution
  only, trusting Cargo's source configuration, and not a build passing
  `hotpath/hotpath-cpu` on the command line.)

- [ ] **The O_DIRECT path constructors' non-regular-target check is not
  race-free.** `direct_io::create_for_write` (behind
  `PbfWriter::to_path_direct` and `to_path_uring`) stats the path before
  its `O_DIRECT` open, re-stats it when the open fails `EINVAL` or
  `EOPNOTSUPP`, and fstats the opened descriptor. A FIFO renamed into place
  between the first stat and the open still blocks the open, and a device
  renamed in sees the open (it is never written). A failed re-stat keeps
  the open's own classification, so an `EINVAL` from a swapped-in device can
  still read as `direct-io` unavailable. Closing it would need a
  non-blocking probe open plus a `/proc/self/fd` reopen (the input-pinning
  pattern), or `O_NONBLOCK` cleared afterwards, which the `fcntl F_SETFL`
  text lint in `brokkr.d/lints.toml` confines to `src/read/input.rs`. Judged
  outside the path constructors' contract (their caller owns the
  destination; `Output` stages a fresh file) when this landed; recorded so
  the boundary is a decision, not an oversight.

- [ ] **Uncited performance figures on the site.** `docs/guide/performance.md`
  (the N. America apply-changes table, the Japan osmium sentence, the blob
  share and RSS claims) and `docs/guide/indexdata.md` (the blob share and
  size-overhead claims) carry numbers with no commit or host, against the
  AGENTS.md convention. The four guide pages that were added to the sidebar
  had theirs replaced by a pointer to the Performance page; these two pages
  still need each figure either sourced from `.brokkr/results.db` /
  `reference/performance.md` with commit and host, or dropped. The same
  holds for inspect's index-only timing in the CLI reference
  `docs/cli/commands.md` and the apply-changes sentence of `cli/README.md`.
- [ ] **Old site fragments into the correctness and deviations pages land on
  a pointer.** Those two site pages became pointers to the root
  `CORRECTNESS.md` and `DEVIATIONS.md` on GitHub, so an outside bookmark such
  as `/pbfhogg/cli/deviations#diff-content-equality-vs-version-ordering`
  now opens the pointer page, not the section. Accepted when the copies were
  removed (recreating the headings would be a second copy to drift); a
  fragment-forwarding script on those two pages is the fix if it matters.
  The site's search also no longer indexes their content.
  The CLI commands page lost three site fragments the same way when the
  repository's CLI reference replaced it (headings renamed so both renderers
  slug them alike): `#index-verifying-blob-level-metadata`,
  `#locations-verifying-inline-way-locations` and
  `#locations-on-ways-maintaining-a-locationsonways-file` now open the page
  top; they are `#index-verification`, `#inline-location-verification` and
  the `--locations-on-ways` section (check what each renderer slugs
  that heading to before citing an anchor). Repository links of the form
  `blob/main/reference/cli-reference.md#...` (outside this tree: issue
  comments, other projects' notes) no longer resolve at all; the content
  is `docs/cli/commands.md` under the same section names except
  "`-j/--jobs`" (now "Worker threads"), "`--force`" (now "Proceeding
  without indexdata") and the two `check` subsections above.

### Read path

- [ ] **The CLI's `cat` never evicts its inputs, though the CLI runs on
  `PageCachePolicy::Evict`.** Found 2026-10-30: cat walks raw
  frames (`FileReader` through `Frames::of_input`), which has no eviction
  hook, so `Host::page_cache` does not reach it; `fadvise_input_evictions`
  stays 0 for a cat on either policy. Other raw-frame and pread walkers
  (getid's schedule walk, degrade, `check` index, cat reindex, the
  pread-from-workers passes of tags-filter and others) are likely the same,
  unaudited. Undecided whether a passthrough cat should evict at all (it
  is the fastest planet path and buffered prefetch helps it); either way
  the `Host::page_cache` wording in docs/guide/advanced.md now says cat
  does not evict; the other walkers are not yet named there. Decide,
  then make the code or the wording true.

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

- [ ] **The PBF read path does not validate coordinate ranges.** `parse_bbox`
  now rejects a latitude outside -90..90 or a longitude outside -180..180, and
  every path that joins node coordinates into ways (both add-locations-to-ways
  stores, and the apply-changes `--locations-on-ways` kept-node sink) refuses a
  joined node outside the same domain; `check --locations` reports one. The
  general read path (`read/elements.rs`, the dense node iterators) and the node
  scanner (`scan/node.rs`, which bounds only the `i32` representation) still
  hand out whatever the file says. Recorded because the bbox work made the
  asymmetry visible; changing the read path is a deliberate decision, not a
  follow-up.

- [ ] **Join-time domain checks judge the truncated value.** add-locations-to-ways
  and apply-changes `--locations-on-ways` test the domain on the
  decimicrodegree value after truncation toward zero (`coord_pack::in_domain`),
  so a node within 100 nanodegrees past +-90 / +-180 degrees (possible only
  under a non-default granularity or offset) is accepted and written at the
  limit, while `check --locations` judges the exact nanodegree value and
  reports it. The written coordinate is in the domain; the two commands
  disagree only about whether the input was. Judging the exact value would
  need the scanner to hand out nanodegrees or a remainder flag on the hot
  path; left as is unless a real producer is found writing such values.

- [ ] **Ring simplification can change polygon topology.** Douglas-Peucker can
  introduce self-intersections that flip containment near a boundary. The
  recursion / stack-overflow defect is fixed (explicit stacks); the topology
  sub-point is open because no topology-preserving replacement and no biting
  real caller-order fixture was established. Precise scope so it is not
  rediscovered:
  - **Producer.** `src/geo.rs` Douglas-Peucker, called from
    `geocode_index/builder/admin.rs::assemble_one_relation`, which simplifies
    every outer and inner ring of every admin relation.
  - **Knob.** `--max-admin-vertices`, default 500. `admin_vertex_cap` now reads
    it as the *local-boundary* budget and multiplies by admin level (16x at
    levels 0-2, 8x at 3, 4x at 4, 2x at 5, 1x below), so a country gets a
    much larger budget than a municipality. That removed the worst disparity,
    and stopped under-covering Denmark's own coarse ring, but does NOT close
    this. The knob
    is still a vertex count, not a distance tolerance: the effective epsilon is
    whatever it takes to reach the budget, so the error stays unbounded in
    metres and a topology flip remains possible on any ring long enough to be
    simplified at all.
  - **Consumer, and why it matters most.** `geocode_index/reader.rs` runs
    `admin_polygon_contains` (point-in-polygon) against the simplified rings for every admin result.
    A self-intersection near the boundary makes the even-odd crossing count
    wrong on one side of the fold, so a query resolves to the *wrong*
    administrative area - a wrong answer, not a missing one, and undetectable in
    the reader because the ring is well-formed as a byte sequence.
  - **Blast radius.** Points within the simplification error of a boundary,
    which at a country-scale ring and 500 vertices can be kilometres.
  - **What a fix needs.** Either a topology-preserving simplifier (reject a
    candidate removal whose new segment crosses another segment of the same or a
    sibling ring), or a metric tolerance in place of the vertex cap so the error
    is bounded in metres. Nothing in the admin cell cover forces a vertex
    count: its work is linear in the stored edges.
  - **What is missing to act.** A fixture: a ring whose DP simplification at a
    given cap provably self-intersects, plus a query point whose containment
    flips. Without it any fix is unfalsifiable.

### Geocode index

- [ ] **Admin interior flags are sound; the reader cannot use them yet.** Since
  2026-11-03 ([ADR-0022](../decisions/0022-admin-edges-are-covered-in-the-point-in-polygon-geometry.md),
  superseding ADR-0015) the edge cells are a conservative cover of the
  reader's own point-in-polygon boundary (`src/geocode_index/builder/cover.rs`),
  so a flagged cell is wholly inside its polygon. Re-enabling a PIP skip
  needs two things the index does not have: a header signal that its flags
  come from that builder (older indexes carry flags on boundary cells), and a
  reader that trusts only the query's own cell's flag, not the eight
  neighbours'. A format change; measure the PIP cost it would save first.
  The level cap (19) stays either way: an exact cover's work still doubles
  per level, now bounded by a proved cell count rather than a sample count.
  The PIP cost is the query cost: admin point-in-polygon dominates every row
  of the query latency table in `reference/performance.md`. The query's own admin cell
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

  - [ ] **Shape-3 serial schedule walk (`build_classify_schedules_split`).** The single-threaded `HeaderWalker` loop in `src/scan/classify.rs` does one QD=1 pread per blob. Measured 2026-07-12, before the cold-walk read-ahead of `6fdde3f4`: ~70 us/blob, invisible at production planet's blob count but a fixed ~97-109 s at 1.45 M blobs. **Full six-caller 8k sweep done 2026-07-12** (table in `reference/blob-density.md` "Full shape-3 sweep"): the walk fraction splits the family, which **narrows the payoff** from the earlier "helps six commands" framing. Read-only callers are ~2/3 walk - `check --refs` 66 % (`1851f73a`), `check --ids` 67 % (dirty) - so flattening it roughly halves their 8k wall (~2.7x -> ~1.3x). Re-encoding callers are only 18-29 % walk - `extract --smart` 29 % (`4b82686f`), `cat --clean` 19 % (`3f4c222c`), `repack` 18 % (`8f275ebf`), `degrade` 18 % (`6f8a3e94`) - their 8k regression is dominated by framing + writing 1.45 M tiny blobs, which the walk fix does not touch. So: **if the primitive is ever built, it is a win for the read-only pair only** (and those two are the same commands whose getid/getparents selective-scan cousins already got ADR-0006 dispatch). The io_uring batched-header-walker this entry used to name cannot work (each header's offset comes from the one before, so there is never a second probe to batch); cold walks of small-blob files now read ahead in windows instead, which the 8k sweep predates - see `reference/blob-density.md` "The walk term". Unmeasured at 8k since. Still deferred: near-zero production bite.

- [ ] **degrade** - v2 ideas, pick up when a consumer surfaces: `--recompress C` (forces decode even for passthrough-eligible flags); richer `--unsort` chaos modes (rotate/shuffle/reverse - only when sort measurement needs more than one overlap per kind); a registered planet `unsorted` snapshot for `sort` benching; `--unsort` throughput (denmark 7.7 -> 27.3 s from the Owned round-trip through the merge thread; three sketches in the retired notes/degrade.md's git history, worker-fires-the-swap being the cleanest - acceptable today because `--unsort` is one-time generation work). Flag semantics in docs/cli/commands.md.

- [ ] **getparents FullScan arm** - one idea parked without a trigger: converting the FullScan arm from pipelined decode to `parallel_classify_phase` (pure per-blob work, lower memory ceiling; est. 10-20 % where oversubscription bites). Bench-gated, speculative, and ADR-0006's dispatch already routes the cases where it would matter most.

- [ ] **apply-changes** (plan note `apply-changes-opportunities.md` deleted 2026-07-13; git history has the full record) - `apply-changes --locations-on-ways`. Cross-disk `--io-uring` rows (93.0 / 127.9 / 82.8 s) still need re-measurement on the fixed writer (the `fa8251d` CopyRange fix); cross-disk `--compression none` + `--io-uring` is open until re-measured. Multi-file output / RAID-0 is unlanded and low priority. Remaining items:
  - [ ] **#11 splice-in-place for low-touch rewrites (deferred).** For `NeedsRewrite` blobs with <=K affected elements (K~64), splice raw wire bytes for unmodified element runs instead of full decode + re-encode (~1.5-2 s at daily; the scaffolding that used to sit in `src/write/raw_passthrough.rs` was deleted 2026-08-20; the per-group passthrough analysis is in this file's raw passthrough section). Saves classify/rewrite CPU but not output bytes, so it will not move the writer-bound wall. Revisit if the writer ceiling moves.
  - [ ] **#13 exact-membership metadata / sidecar (format project).** Per-blob ID-range-only metadata forces slow-path decode on pure creates inside an existing range: 15,224 FalsePositive blobs / 92,677 slow-path = 16 % of slow-path work wasted at planet. The false-positive distinction is documented on `block_overlaps_diff` in `src/commands/apply_changes/classify.rs` (`range_overlaps` true for create IDs inside the blob's range, `block_overlaps_diff` false because no element in the block matches; the old `classify_only` "FalsePositive" comment the note pointed at was removed in the streaming refactor). Two fix shapes: (a) wire-format exact-overlap scanner on decompressed bytes; (b) per-blob membership sketch in indexdata. Not a quick cleanup.
  - Open questions preserved from the note: actual overlap-blob ratio under larger OSCs (re-measure if input diffs grow; governs worker-pool load); byte-budget reorder capacity sizing (current setting works at planet; revisit on RSS/stall signals); scanner HeaderWalker vs worker throughput balance (no signal it's an issue).

- [ ] **[notes/altw-external.md](altw-external.md)** - `add-locations-to-ways --index-type external`. Current baselines and history are in `reference/performance.md` and [`notes/altw-optimization-history.md`](altw-optimization-history.md); failed attempts and the do-not-retry list are there too.

## Planet-scale validation coverage

README's planet table is the source of truth for "this command runs
cleanly on the reference host" (the README states its RAM). The README
refresh campaign settles the "not benched at planet" claims below as its
cells land. This section tracks the remaining axes and dataset gaps.

### Latent planet risks

- [ ] **altw sparse at planet.** The default `--index-type sparse`
  thrashes at planet on a 28 GB-class host: the rank-indexed flat scratch
  outgrows the page cache and pass 2's location write-back turns into
  random-access mmap faults (soft failure, unboundedly slow, not OOM;
  reference/performance.md "Auto routing threshold"). Planet runs need
  `--index-type external`, which the README says. Open: whether the
  default should route to external, or refuse, at planet scale.

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

Known to work, no performance question open, but not in the results DB:

- [ ] **ID-set shape for `getid` / `getparents`**. The lists are
  configurable per dataset (`getid_ids` / `getparents_ids`); no bench
  sweeps set shape (sparse vs dense, spread vs clustered, cold vs hot
  cache). Not needed for general validation; open only if ID-set shape
  becomes a perf question.
- [ ] **`--direct-io` at planet**. Covered: apply-changes, cat
  passthrough, `cat --dedupe` and sort (reference/performance.md
  "`--direct-io` impact summary"). Not measured: extract,
  add-locations-to-ways external, merge-changes. Only matters if
  direct-io becomes a default on any of them.
- [ ] **`renumber` with non-default flags**. Has no non-default flags
  in pbfhogg today (just the one variant since the in-memory path
  was retired). If a future variant adds flags this reopens.
- [ ] **`brokkr bench decode-workload` / `brokkr bench encode-workload` / `brokkr bench merge-workload` at planet**.
  Synthetic benchmarks, intentionally excluded from the README user
  surface. Periodically-useful diagnostic tools; not a validation
  target.

## Performance

- [ ] **altw external pass A's exposure to sustained-write state
  (2026-11-09).** A short trim before a cell
  restores pass A (about 23 s against 36-41 s without one) and launch-time
  Dirty/Writeback is negligible (reference/performance.md, "altw external
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
- [ ] **cat --clean's window binds on germany (2026-11-09).** At
  `b58e8ef3` on bygg, germany `cat --clean` parks workers at the ordered
  pool's window for 12 to 15 % of the wall (`ordered_pool_window_wait_ns`)
  while planet records 0; the read-ahead advice costs 30 ms there, so the
  binding is the head blob's own work behind a 64-slot window
  (`POOL_REORDER_CAPACITY_WIDE`). Whether a wider window or a different
  germany blob mix explains it, and whether it moves the wall, is not
  measured (reference/performance.md, "Read-ahead advice after the
  credit").
- [ ] **getid's full-scan arm reads every payload it then skips
  (2026-11-09).** `filter_by_id_streaming` uses `Frames::next_raw`, so a
  blob the indexdata prescreen rejects has its whole payload copied into
  memory first. `next_header` already returns the index, so rejected
  blobs could `skip_payload` (a `seek_relative` inside the `BufReader` for
  small blobs). That changes the arm's I/O shape, which ADR-0006 measured
  as a sequential read, so it wants its own matched cells at low and high
  blob density.
- [ ] **Sort pass 2 parallelism is bounded by overlap component count
  (2026-11-08).** One sweep per component: an input whose every kind is
  one connected component (the Denmark `chained` snapshot) sweeps in
  three jobs, the node component dominating (`SORT_PASS2` 24.1 s at
  `d044980e`, bygg; reference/performance.md, "Sort across the blob-merge
  engine move on unsorted Denmark"). Thread utilization and the critical
  path were not measured; this records the limit, not a defect or a
  remedy.
- [ ] **Same-ID groups in the overlap sweep are held whole
  (2026-11-09).** The sweep still gathers every copy
  of one ID before writing any (`emit_group` in
  `src/commands/blob_merge/sweep.rs`), uncharged against
  `SWEEP_PINNED_BYTES`, because a cursor delivers an ID's copies in block
  order and `sort` accepts descending versions inside a blob. Output
  ships block by block inside a group and the dedupe lookup is indexed
  past `SWEEP_GROUP_INDEX_THRESHOLD`, so only the hold remains. Removing
  it means a merge keyed on (ID, version, tie) with an eager (ID,
  version) pass over every overlap blob before its cursor opens (sorted
  views where it fails): a second pass over normal input, and live
  cursor blocks and views are still held. Only adversarial or
  many-version input reaches the hold.
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
  outstanding seqs by count (`apply_changes/window.rs`, 512, non-binding
  at planet with in-flight peaks of 300-410); a byte budget here would
  replace that window, not sit beside it.

- [ ] **glibc's mmap threshold, per command.** A CLI-wide fixed threshold
  was ruled out at planet; a per-command choice, the measurements it still
  needs across dataset sizes and hosts, and the structural route (recycling
  blob-scale buffers) are in `notes/mmap-threshold.md`.

- [ ] **A cold schedule header walk costs ~6 s at planet (found
  2026-10-30).** Cells that run without the headers in page cache spend
  about 6 s per schedule walk (every command that schedule-walks without
  cached headers; tags-filter two-pass walks several times, see below); the
  RSS-pass baselines had them cached. Read single-cell walls with that in
  mind. Measured before the cold-walk read-ahead of `6fdde3f4`.

- [ ] **Stall-counter follow-ups (found 2026-10-26 adding the unordered
  pools' `*_wait_ns` counters).** Each is its own change; none is measured.
  - apply-changes' `candidate_rx` is its own uncounted `Mutex<Receiver>`
    fan-out (`apply_changes/streaming.rs`).
  - The first-error slot is a mutex locked on every claim in altw stages 2
    and 3 and geocode pass 1.5; an `AtomicBool` fast check would avoid the
    shared cache-line traffic. Planet-measured paths: needs its own A/B.
  - Every altw router publish takes a global `stats` mutex and the global
    `notify_mu`. Same caveat.
  - The `Mutex<Receiver>` desc-lock reading that used to be listed here
    is in reference/performance-history.md, "Mutex<Receiver> blob dispatch
    is not a bottleneck".

- [ ] **tags-filter walks the headers again for each later pass.** The
  relation closure, the way dependencies and the pass-2 schedule each run
  their own header walk where pass 1's walk could feed them all: repeated
  work on a measured planet path, left alone when tags-filter adopted input
  pinning (`0f64d8f4`) for a measured change of its own. Read altw sparse
  P4 first (notes/altw.md, "four redundant header walks", built and
  reverted twice): consolidating walks pays only where something evicts the headers
  between them, and never sum per-phase walk durations.

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

## Cross-pipeline optimization

Cross-thread buffer retention is solved: `DecompressPool`
(`src/read/decompress.rs`) recycles decompression buffers in the
pipelined reader. Thread oversubscription was the open concern; the
ordered and classify pools now bound their workers, and the fused task
engine entry under Performance is the live design.

Altw's optimization history, with what was tried and refused, is
[notes/altw-optimization-history.md](altw-optimization-history.md).
See [reference/pipelined-reader-paths.md](../reference/pipelined-reader-paths.md)
for the pipelined-reader audit. Sequential conversion was attempted for
getparents (commit `c912e4d`) and reverted - 4.7x regression on
Denmark (1400ms vs 300ms). Decompression dominates, not per-block
processing. **Do not convert decompression-dominated pipelined paths
to sequential.** Renumber converted separately (external join
architecture, not driven by retention/oversubscription).

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks.

### Raw group passthrough

Blob-level raw passthrough is closed for tags-filter (the pin is the
comment block at the pass-2 worker in `src/commands/tags_filter/mod.rs`).
time-filter still decodes and re-encodes via BlockBuilder; renumber's
external mode splices ids on the wire (reference/performance.md,
"Renumber (external mode)").

Per-group raw passthrough, for partial-match blobs (e.g. extract boundary
blobs where some groups match and some do not), is **not built**. Seven
unused scaffolding primitives carried the idea for four months without a
consumer - `frame_raw_block` in `src/write/raw_passthrough.rs`, the
`PrimitiveBlock::raw_group_bytes` / `raw_stringtable_bytes` /
`block_scalars` / `group_count` accessors, and the `WireBlock::raw_stringtable`
/ `raw_proto` accessors underneath them - all behind blanket or per-item
`allow(dead_code)`. They were deleted on 2026-08-20 (`fb079ac5`).

The design question any consumer has to solve is mixing raw and
re-encoded groups in one output blob: raw groups carry the original
string-table indices, `BlockBuilder` issues fresh ones. Two viable
shapes: a string-table-aligned re-encode (copy the original `StringTable`
whole and re-encode partial groups against its indices, a different write
path, one output blob per input blob), or split output (raw-only and
re-encoded groups as separate blobs, both paths exist, but it doubles the
boundary-blob count).

Two shadow-counter measurements at planet scale put the idea on the
shelf: tags-filter (2026-04-18, shadow counter `a5c6854` reverted in
`0ef4107`, UUID `8c786794`, `w/highway=primary`), 0 of 50,364 pass-2
blobs all-included, and multi-extract (2026-04-20, UUID `dad573cb`,
5-region `--config --simple`), 0 of 32,835 node blobs qualifying, pinned
in `src/commands/extract/multi.rs` `try_extract_multi_single_pass`. Both are
structural: blobs are ID-sorted and OSM IDs are chronological, not
geographic or tag-coherent, so an 8,000-element blob scatters in both
dimensions. A group is smaller, so its qualifying fraction is not
automatically zero; shadow-count at group granularity on a real workload
before writing any code.

### Write-path throughput

`BlockBuilder` (`src/write/block_builder.rs`) interns strings in one
arena (`src/write/string_table.rs`). Varint SIMD is closed: scalar beats
SIMD 2-6x on LEB128 varints because the 1-byte fast path is perfectly
branch-predicted (the retired notes/SIMD.md, in git history); only batch
decode into columnar arrays remains a live SIMD direction.

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
- [ ] **Missing-indexdata and unsorted refusals name one remedy** (raised
  2026-10-26). `IndexdataMismatch` advice now names `check --index` and
  `cat --reindex`; `MissingIndexdata` (partial and whole) names `cat` only
  and `Unsorted` names `sort --distrust-metadata` only, and diff's
  claimed-count trust is uncovered. Decide whether they should also name
  `check --index` (to find out what is wrong).
- [ ] Auto-selection: `--index-type auto` exists (sparse vs external) but
  is not the default; blob-count dispatch landed for getid and getparents
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
  decoded-block working-set note in `src/read/pipeline.rs`). Retry only
  if a fresh `--alloc` planet profile shows allocator-bound time;
  revisit when `tikv-jemallocator` tags a jemalloc 5.3.1 release.
  Allocator policy knobs are settled at the `set_mmap_threshold` doc in
  `src/process.rs`.

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
  the two `flat_map_iter` closures in `src/geocode_index/builder/pass3.rs`.
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
