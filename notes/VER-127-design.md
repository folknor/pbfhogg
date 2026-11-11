# VER-127 design: test filesystem

Settled under spar (codex, 2026-11-11). The rulings below are closed: later
links implement them and do not reopen them. Source item: VER-127 in
`notes/verification.md`, which stays there until the last link closes the
chain.

## Facts the design rests on (checked at 9392f187)

- `data/test-scratch/` held 16 leftover `.tmp*` trees (one from 2026-11-09
  holding a CLI child's `.pbfhogg-run-*` directory) and a stray
  `old_blob_meta.rs`. Residue is real, not hypothetical.
- brokkr#152 (export a scratch root, clear it at start of run) is open and
  unshipped; its variable name is undecided. brokkr#13 declined
  `PBFHOGG_TEST_*` exports and 94453ae6 removed env-overridable roots.
- `disallowed_methods` is `allow` workspace-wide and denied only in the
  library's non-test code and the CLI's `src`. Integration test crates call
  clippy-banned `std::fs` functions in about 35 files under the one root
  `clippy.toml`, so clippy seals cannot be enabled for test code. **The
  item's clippy seals for tests are refused; textlint is the only lever for
  test code.** (`std::env::temp_dir` is already sealed for shipped code.)
- Production uses of `tempfile` and `Command::new`: zero. Ambient
  `tempfile` constructors in tests: zero.
- Every `catch_unwind` in `crates/lib/tests` keeps its scratch guard outside
  the caught closure.

## Rulings

1. **One leaf crate owns test scratch.** `crates/test-scratch`, package
   `pbfhogg-test-scratch`, `publish = false`, no features, depends only on
   `tempfile`, no dependency on `pbfhogg`. Library, CLI and harness take it
   as a path-only dev-dependency with no version (stripped by `cargo
   publish`; the published archive cannot run the repository tests, which
   is accepted). A feature on the library (rejected: feature unification
   with `minimal`) and keeping three copies (rejected: three
   implementations of one rule) were the rivals.
2. **Fixed location, not discovered.** The workspace root is
   `CARGO_MANIFEST_DIR/../..` of the leaf crate, checked once per process
   (`OnceLock`) for a `Cargo.toml` with a line equal to `[workspace]`. That
   check is a layout sanity check, claimed as nothing more. Not
   relocatable. No `Cargo.lock` walk anywhere.
3. **`repository_root()` and the scratch base are separate.** Reading the
   checkout never creates `data/test-scratch`; the base is created once, on
   first `scratch()`.
4. **No env override until brokkr#152 names its variable.** A variable no
   runner sets is dead code, and the prior ruling removed one.
5. **Retention contract, stated literally:** a guard destroyed during
   unwinding keeps its tree; everything else is removed. Unwinding is not a
   test verdict (a guard created inside a caught closure is kept even when
   the test passes; a `prop_assert!` failure loses its case's tree).
   Residue grows on unwinding, explicit `keep()`, cleanup failure and
   process termination. A test binary never sweeps.
6. **Cleanup failure is loud.** A non-unwinding drop calls `TempDir::close`
   and panics naming the path on failure, noting the tree may be partial.
   The `TempDir` sits in an `Option` taken exactly once. The panic fails the
   test only when the guard drops on the test thread or on a worker whose
   failure is propagated; `static` and thread-local ownership are outside
   the fixture contract; guards stay outside closures whose panic a test
   expects. Drop does not repair permissions: a permission test restores
   modes itself or calls `keep()`.
7. **Call-site names stay.** `common::test_scratch()` and
   `path_guard::tests::scratch()` are one-line re-exports; renaming the
   ~1400 call lines buys nothing. The CLI copy is deleted; its callers use
   the leaf directly.
8. **Spawn ownership is per policy, not one exemption list.** The CLI
   invoker, one bounded oracle runner (osmium), the harness executable
   runner and the guard's self-reinvocation are the four sanctioned spawn
   sites. The harness runner is not the CLI invoker and is not exempted by
   file.
9. **The invoker borrows scratch at builder construction**
   (`CliInvoker<'a>` borrowing `&'a TestScratch`, transferred into
   `RunningCli<'a>`), not at spawn. Defaults place cwd, output, staging and
   `--tmp-dir` inside the borrowed scratch, but explicit paths a test passes
   (missing parents, invalid staging, existing outputs) are never
   normalised or overridden: refusal tests must still be expressible.
10. **Production harness code does not depend on the dev-only leaf.** The
    VER-094 sites (`crates/harness/src/scenarios/mod.rs` `absent`,
    `runner.rs` `check_bindings`) get their own fallible inspection
    (`symlink_metadata`, only `NotFound` means absent).
11. **Enforcement is spelling-bound textlint with `must_scan`**, rules
    enumerating their source roots, and the limit is stated: renamed
    imports and writes by absolute path outside the repository stay unseen.

## Slice plan

### S1 core (done in this link)

- New `crates/test-scratch` (`Cargo.toml`, `src/lib.rs`: `TestScratch` with
  `path()`, `keep()`, the Drop contract; `scratch()`; `repository_root()`;
  four unit tests). Workspace member; in the `all` and `musl` sweeps.
- Dev-dependency in `crates/lib`, `crates/cli`, `crates/harness`.
- Deleted `crates/lib/tests/common/scratch.rs` and
  `crates/cli/src/test_scratch.rs`; `common/mod.rs` and
  `path_guard::tests` re-export; `crates/harness/src/test_scratch.rs` keeps
  only `marker_fifo`, built on the leaf.
- `repository_root()` replaces the `Cargo.lock` walk in `crates/cli/src/lib.rs`
  (two CLI-reference tests), `crates/cli/src/io_reach.rs` and
  `crates/harness/src/describe.rs`.
- `TempDir` annotations became `TestScratch` (24 files).
- The two `should_panic` scratch tests in `crates/lib/tests/fixture_helpers.rs`
  catch the panic with the guard outside and assert the message.
- Textlint renamed `test-scratch-via-test-scratch-crate`, now also refuses
  `TempDir::new_in`; `reference/testing.md` "Test scratch" and `AGENTS.md`
  Workspace updated.

### S2 path helpers (done in this link)

On `TestScratch`, in the leaf crate, each with a unit test there:

- `child(name) -> PathBuf`: returns a path, creates nothing; rejects
  anything but one `Component::Normal`. Lexical containment only (an existing
  symlink can still escape); say so in the doc.
- `cell() -> PathBuf`: next numbered subdirectory from a per-instance
  counter (checked increment), created with `create_dir`; an existing entry
  is an error, never reused.
- `path_state(path) -> io::Result<Option<FileType>>` via `symlink_metadata`,
  `NotFound` is `Ok(None)`, every other error is returned.
- `anonymous_file() -> File` replacing `tempfile::tempfile_in` in
  `crates/lib/src/debug.rs` (two sites) and
  `crates/lib/src/commands/apply_changes/streaming.rs`.
- `deny_write(path) -> DenyWrite<'_>`: guard borrowing the scratch. Contract
  is operation-specific: it establishes that creating an entry in `path`
  fails, and nothing else (not removal, rename or open of existing
  entries). Probe outcomes: denial holds -> guard; the operation still
  succeeds (root, capabilities) -> an unavailable reason the caller skips on;
  setup or inspection fails unexpectedly -> panic. Restoration on drop;
  restoration failure during unwinding is reported (stderr) without a second
  panic. Restoration is tied to the directory's identity (hold an open
  directory descriptor and `fchmod` it), so a test that renames the parent
  (the backup-removal test in `crates/cli/tests/cli_scratch.rs`) is still
  restored.
- Migrate: root-check early returns and manual permission restoration in
  `crates/cli/tests/cli_scratch.rs`, `cli_output_contract.rs`,
  `crates/lib/tests/host_contract.rs` and the `crates/lib/src/scratch.rs`,
  `output.rs`, `read/input.rs` unit tests (VER-017 jointly, VER-080).
  `crates/lib/tests/edge_cases.rs` and `common/mod.rs` `snapshot_dir_inner`
  use `path_state` (VER-055).
- VER-094, separately: production harness `scenarios/mod.rs` `absent` and
  `runner.rs` `check_bindings` get a local fallible check, not the leaf.
- Must not change: the retention contract, call-site names, any production
  behaviour.

S2 completion:
- All five helpers implemented with leaf unit tests, including collision refusal,
  non-NotFound inspection errors, dangling symlinks, and restoration after rename
  during unwinding. The free `path_state` entry point shares the method's
  implementation for recursive snapshot helpers without a scratch parameter.
- All listed migrations done. The existing-lock test probes opening its existing
  read-only file directly: directory creation denial cannot prove that operation.
  Procfs reopening and different-device prerequisites now announce skips too.
- Snapshot regression covers an escaping directory symlink and a cycle. Harness
  inspection regression covers presence, absence, dangling links and a non-NotFound
  error. Harness callers propagate inspection errors.
- Closed VER-017, VER-055, VER-080 and VER-094. VER-015 narrowed to enforcement
  residue, assigned to S5 as planned (small: constructor spellings and sentinels).
- No new work deferred to later slices, no retention or production library
  behaviour change, and no lateral finding.
- Validation: `brokkr check` tier1 passed (all seven clippy sweeps; 12530
  tests passed, 22 ignored across six test sweeps). `brokkr fmt --all` and
  `git diff --check` passed. The renamed-directory unwinding test also
  passed individually through brokkr after the final best-effort stderr edit.
  An attempted whole-leaf `brokkr test tests::` was refused by the runner's
  all-tests filter policy; the tier1 run already exercised its nine tests.

### S3 fixtures off the checkout (done in this link)

- `crates/lib/src/read/blob.rs` (three executable tests) and
  `read/reader.rs` (three) stop reading `tests/test.osm.pbf`: build a
  fixture in scratch that preserves what each test exercises (several
  blobs; truncation inside the final payload). Doc examples keep reading it.
  `reference/testing.md` "No checked-in input data" loses the "a few unit
  tests read it too" sentence (VER-057).
- `crates/lib/src/write/file_writer.rs` file-classification test uses a
  scratch file, not `Cargo.toml` (VER-058).
- `crates/lib/tests/proptests.rs`: `failure_persistence: None`; a failing
  case is promoted by hand to a named test. Update the file header and
  `reference/testing.md` "Property tests" (VER-048).
- `crates/lib/tests/fault_derive_parallel.rs` static fixture-name counters
  become `cell()` (VER-037).
- `crates/cli/tests/cli_truncation_sweep.rs`: fresh output per cell via
  `cell()`, plus failure classification and a valid-input control per
  configuration, so a usage error or unrelated crash never counts as
  corruption detected (VER-072).

S3 completion:
- `test_support::multi_blob_pbf` writes a header and three data blobs in scratch,
  recording the final frame while writing. Its cut point uses only the length
  prefix, not a production reader. Blob and element reader executable tests use
  it; doc examples retain their short checked-in input path.
- Regular-file classification uses an owned scratch file. Property persistence
  is explicitly `None`, with manual promotion to named regression tests documented.
- Plain and gzip derive helpers use fresh `cell()` directories with fixed names.
  Every truncation invocation has its own output cell, each configuration first
  succeeds on intact input, and hard cuts require exit 2 plus a framing error
  naming the input. Usage errors and abnormal exits fail all cells.
- Closed VER-037, VER-057, VER-058 and VER-072. VER-048 narrowed to environment
  and seed-policy residue, outside this filesystem chain. No lateral finding.
- S5 retains executable-fixture path enforcement and gains a small static
  fixture-counter seal (one spelling-bound rule plus sentinel); no S4 work added.
- Validation: `brokkr fmt --all` and `git diff --check` pass. Through
  `brokkr test --sweep all`, the truncation sweep (8.5 s), both public-reader
  cache tests, three-detector payload cut, pipeline completion/early-drop test,
  regular-file classification, both derive matrices and all properties pass.
  Final `brokkr check` passes gremlins, textlint, bench/script checks and publish
  cycle, then stops on the earlier-slice `clippy::let_underscore_must_use` at
  `crates/test-scratch/src/lib.rs:271`. No other clippy error remains across
  seven sweeps; the full tier1 test phase therefore did not run. That existing
  leaf lint is left for the chain's green pass, not rewritten by S3.

### S4 children (done in this link)

- `CliInvoker<'a>` borrows `&'a TestScratch` at construction;
  `RunningCli<'a>` carries the borrow; Drop kills, reaps and joins the
  capture threads with a bound (no unbounded pipe-drain join after a
  timeout). Survives wait errors and assertion failures.
- Defaults: cwd = scratch, stdin null, environment cleared to an allowlist
  (`PATH`, `HOME`, locale; nothing `PBFHOGG_*`, `BROKKR_*`, `RAYON_*`,
  `HOTPATH_*`) with typed setters for each test hook variable the CLI reads
  (`crates/cli/src/signals.rs`, `reference/testing.md` Conventions) (VER-077,
  VER-076 jointly).
- Output, staging and `--tmp-dir` defaults inside the scratch; explicit
  paths passed through untouched (ruling 9).
- One bounded oracle runner for osmium, shared by
  `crates/cli/tests/cli_sort.rs`, `cli_digest.rs`,
  `cli_apply_changes_osmium.rs` and `crates/lib/tests/geocode_index.rs`.
  Location: a new `crates/lib/tests/common/oracle.rs`; `common/mod.rs`'s
  "Fixtures only: nothing here spawns the pbfhogg binary" header is
  reworded to "nothing here spawns pbfhogg; the oracle runner spawns
  external reference tools". The runner sets cwd and an explicit scratch
  (`TMPDIR` inside the borrowed scratch) for osmium and resolves the
  executable deliberately (VER-045, VER-046 jointly).
- Harness: a bounded runner in `crates/harness/tests/exe.rs` (`RunDir::launch`
  and the other direct spawn) under the same ownership rules, keeping its
  existing env scrub.
- `crates/cli/tests/guard.rs` self-reinvocation stays its own sanctioned
  spawn.
- Invoker arguments accept `OsStr`/`Path` without converting to UTF-8; the
  panicking `to_str` conversions in invocation helpers go (VER-016 residue).
- Must not change: invoker timeout below the 20 s watchdog; refusal tests'
  explicit arguments.

S4 completion:
- `CliInvoker<'a>` and `RunningCli<'a>` borrow scratch at construction and
  through cleanup. `run` is `spawn().wait()`; the shared test-only `OwnedChild`
  retains the child across wait errors and assertion unwinding. Capture readers
  use nonblocking pipes and cancellation, retain the first/last MiB, report read
  errors and bound EOF and cleanup waits. The default timeout remains 10 seconds.
- Every CLI construction now supplies a scratch borrow. Cwd, stdin and inherited
  environment are explicit, TMPDIR is private, and each hook has a named setter
  (count and boolean knobs take their native types). Native argument lists replace
  UTF-8 path conversions, including mixed argument-table helpers. Explicit paths
  and missing argument refusal cases remain untouched. Relative output and the
  CLI's default staging/scratch resolve from its private cwd or explicit output.
- `common/oracle.rs` is the sole osmium construction site for all four listed
  callers: deliberate absolute resolution, version reporting, missing-only skips,
  isolated cwd/TMPDIR and bounded ownership. Harness executable launches use the
  same ownership primitive with their independent environment/protocol policy.
- No counterpart/replay types or fields were changed or extended, following the
  user's removal ruling (bz pbfhogg#54). No S4 work depends on them.
- Regression tests cover execution policy, native paths, wait-error unwinding and
  reaping, capture read errors and cancellation with another writer holding a pipe.
- Closed VER-046, VER-076 and VER-077. VER-016 now only has S5/S6 residue;
  VER-045 retains only the synthetic plain-node coverage gap outside this chain.
  Lateral VER-128 records missing unwind ownership of the harness FIFO collector.
- No implementation work deferred. S5 retains its planned spelling-bound seals;
  include `common/process.rs` in the source roots, since child ownership is shared
  there (small: the existing source-root inventory, no extra spawn exemption).

S4 validation:
- `brokkr test -p pbfhogg-cli cli_ --sweep all --timeout 120`: 683 selected
  tests pass, including all three external osmium comparisons, interruption
  shards and the invoker regressions. Initial argument-migration mistakes in
  comma-bearing literals were corrected before this clean rerun.
- `brokkr test -p pbfhogg-cli common::process::tests --sweep all`: both capture
  regressions pass. `brokkr test -p pbfhogg-harness exe:: --sweep all`: all
  executable tests pass. The ignored plain-node geocode test also passes with
  osmium through `brokkr test ... --sweep all`.
- `brokkr fmt --all` and `git diff --check` pass. Final `brokkr check` passes
  gremlins, textlint, bench/script checks and publish cycle; across seven clippy
  sweeps its only error is the earlier-slice `let_underscore_must_use` at
  `crates/test-scratch/src/lib.rs:271`. The check's tier1 tests do not run after
  that lint failure. No earlier-slice implementation was reverted or rewritten.

### S5 enforcement (done in this link)

After S2 and S4. Textlint rules, each with `must_scan` sentinels and an
explicit `paths` list naming every source root (lib/cli/harness `src` and
`tests`, `crates/lib/examples`):

- `tempfile::` constructors including `tempfile_in` / `tempdir_in` outside
  `crates/test-scratch`;
- `Command::new` in test code outside the invoker, oracle runner, harness
  runner and `guard.rs`;
- `CARGO_MANIFEST_DIR` outside `crates/test-scratch` and the named
  source-contract files (`write/io_buffer_contract.rs`,
  `crates/cli/src/library_text_rule.rs`, `crates/lib/examples/gen_test_pbf.rs`);
- `test.osm.pbf` in executable tests (doc comments exempt by region);
- `data/` string literals.
- Keep the shared `crates/lib/tests/common/process.rs` owner in the scanned
  source roots. It accepts configured commands; it is not an additional
  `Command::new` exemption (S4 follow-through, small inventory adjustment).
- Static fixture-name counters in test helpers (S3 follow-through; small, one
  spelling-bound rule plus a sentinel).

State the spelling limit in the rule comments and in `reference/testing.md`.

S5 completion:
- Six rules installed with explicit lib/cli/harness source and test roots plus
  library examples, each with scan sentinels. The shared process owner is scanned,
  with no extra spawn exemption. Constructor rules include anonymous-file and
  in-directory spellings; fixture-counter names include the former RUN spelling.
- Manifest discovery has only the named source-contract/generator exceptions.
  String-region fixture checks exempt doc comments; generated read-path, inspect
  and extract fixtures now use generated.osm.pbf so no executable basename
  exception is needed. Fixed data literals are refused.
- reference/testing.md states the spelling limit: renamed imports and absolute
  writes outside the repository remain unseen. Closed VER-015; VER-016 now only
  has the blocked S6 runner-root residue. VER-014's scratch-sentinel example was
  removed; its other rule-coverage residue stays outside this chain.
- No lateral findings and no implementation deferred. S6 is small and remains
  blocked on brokkr#152. Brokkr-only clearing, post-run checks and verify workspace
  ownership remain outside this repository slice.

S5 validation:
- All six rules reject temporary forbidden-spelling probes; excluding the shared
  process sentinel produces the expected must_scan refusal. Probes and temporary
  exclusions were restored. The clean selected-rule run passes.
- brokkr test read_paths::, inspect:: and commands::extract::tests::config in
  the all sweep pass. brokkr fmt --all and git diff --check pass.
- brokkr check passes convention checks (including textlint, scripts and publish
  cycle), then stopped on a leaf let_underscore_must_use in
  crates/test-scratch/src/lib.rs. That lint is fixed (eprintln in the panicking
  drop path) and the full `brokkr check --gate` passes (12750 tests passed,
  29 ignored; 12610 expected executions all passed).

### S6 env override (small; blocked on brokkr#152)

When brokkr ships its variable: read it once as `OsStr` in the leaf's
scratch-base init, require absolute, canonicalize, panic naming the
variable; fallback stays the fixed location. Not before.

## Remaining after the chain's commit

Only S6 (env override, blocked on brokkr#152) remains in this repository.
Brokkr-side items: brokkr#152 (export and clear a scratch root), brokkr#153
(post-run tree check), brokkr#154 (verify workspace).

## Tickets each slice touches (commented after the chain's commit landed)

- S1: brokkr#152 (pbfhogg-side crate and root done; note observed residue).
- S4: none in bz.
- S5: brokkr#157 (rules added with `must_scan`).
- S6: brokkr#152.
- brokkr#153 (post-run tree check) and brokkr#154 (verify workspace): no
  pbfhogg slice; brokkr-side only.

## Entries

- Narrowed by S1 (rewritten at source to their residue): VER-016 (one
  resolver; residue is the runner root, S6, and UTF-8 path panics in
  invocation helpers, which belong to S4) and VER-015 (ban covers
  `TempDir::new_in`; `tempfile_in`, `must_scan` and the spelling limit remain,
  S2 and S5).
- Closed by S2: VER-017, VER-055, VER-080, VER-094.
- Closed by S3: VER-037, VER-057, VER-058, VER-072. VER-048 narrowed to
  environment and seed-policy residue outside this chain.
- Closed by S4: VER-046, VER-076, VER-077. VER-016 retains S5/S6 residue;
  VER-045 retains only external-tool-dependent coverage outside this chain.
- Lateral S4 finding: VER-128, harness FIFO unwind cleanup (small, outside chain).

- Closed by S5: VER-015. VER-016 narrowed to the blocked S6 runner root;
  VER-014 loses only its scratch-allocation example.
