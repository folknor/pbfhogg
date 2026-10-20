# Hygiene hunt: test harness as it touches production

Scope: `tests/common/*`, `tests/fixture_helpers.rs`, the `test-hooks`
fault-injection seams in `src/`, the `tests/fault_*.rs` binaries that use
them, and `brokkr.toml` as the check/sweep/dataset configuration.
Findings are unranked. Each says what enforcement could hold the fixed
version.

## Bugs found on the way (concrete, not predicted)

### B1. `read_paths.rs` arms a process-global decode hook while ~30 sibling tests run concurrently

`admission_high_water_bounded_under_slow_first_decode` stores
`BLOCK_DECODE_SEQ = 1` in `src/read/pipeline.rs` `test_hooks`. Every
pipelined read in the process consults that static. `read_paths.rs` is a
single test binary with ~30 `#[test]` fns, most of which run the pipelined
reader, and libtest runs them on parallel threads (the `all` sweep has no
`test_threads = 1`; the only serial profile is `serial::`). Consequences:

- Any sibling whose pipeline reaches seq 1 while the hook is armed parks in
  `maybe_block_decode` until this test releases it; if this test fails its
  5 s assertion first, it panics without calling `reset()`, and the sibling
  spins forever (the 20 s watchdog then reports a "hang" in an unrelated test).
- `REORDER_FILLED_HIGH_WATER` is a CAS-max fed by every concurrent pipeline,
  so `<= 2` can fail because a sibling with a wider decode window pushed it,
  and the "later decode reached the reorder buffer" wait can be satisfied by
  a sibling instead of the pipeline under test.

The fault-injection split moved every other hook user into its own
one-test binary (`fault_*.rs`, each has exactly one `#[test]`); this one
was left behind. Fix: move it to `tests/fault_read_pipeline.rs`, or better,
see S1. Enforceable by a text rule: any `use pbfhogg::...test_hooks` outside
a file matching `tests/fault_*.rs` is an error, and each `fault_*.rs` holds
exactly one `#[test]` (a check script counting both is trivial).

### B2. `ADMIN_PIP_CALLS` is process-global but exposed as a per-reader method

`src/geocode_index/reader.rs` keeps `static ADMIN_PIP_CALLS` and exposes
`Reader::reset_admin_pip_calls(&self)` / `admin_pip_calls(&self)`. The
`&self` signature says "this reader's count"; the value is every reader's
count in the process. `tests/geocode_index.rs` (many tests, one binary,
parallel threads) resets it and then asserts `<= 1`; any concurrently
running geocode query test increments the same counter. This is a latent
flake that the signature actively hides. Fix: a per-`Reader` counter
(`AtomicUsize` field, or a `Cell` since queries take `&self` on one
thread), which makes the bug unrepresentable; no test-hooks static needed.

### B3. `minimal` sweep runs CLI tests against whatever binary another sweep left

`brokkr.toml` says build_packages is "Required for tests/cli_*.rs: without
it ... CliInvoker silently hits a stale binary". The `minimal` sweep has no
`build_packages` by design, but `cli_cat.rs`, `cli_sort.rs`, `cli_diff.rs`,
`cli_repack.rs`, `cli_degrade.rs`, `cli_exit_codes.rs` and the
`cli_invoker_runs_version_command` test in `fixture_helpers.rs` carry no
`#![cfg(feature = "commands")]`, so they compile and run in `minimal` and
drive the binary built by `all` or `consumer` - whichever ran last, with
different features. `cli_sort.rs`'s `cfg(not(feature = "linux-..."))`
feature-missing tests are true in `minimal` (lib built without the linux
features) while the binary may be the `all` build that has them. With
`parallel = {}` and sweeps possibly sharing a target dir, this is order-
and timing-dependent. The comment in `brokkr.toml` documents the hazard
and the config then commits it. Fix: either gate every `CliInvoker` user
out of `minimal` (a `cfg` on `tests/common/cli.rs` itself, so a test that
uses it without the gate fails to compile - structural), or have brokkr
refuse to export `BROKKR_TEST_BIN_DIR` for a sweep that did not build the
binary, so `pbfhogg_bin()` panics instead of silently using a foreign build.

### B4. A tier-1 test requires `osmium` on PATH, with no skip

`tests/geocode_index.rs` `legal_non_dense_nodes_build_a_queryable_index` is
a plain `#[test]` that shells out to `osmium cat ... pbf_dense_nodes=false`
and `expect`s it. `reference/testing.md` states external tools are a brokkr
concern and "Contributors don't need them installed for clean `brokkr
check`"; the two other osmium users are `#[ignore = "external"]` with a
skip-on-missing. On a host without osmium the default `brokkr check` fails.
The fixture it needs (non-dense nodes) is something pbfhogg's own
`BlockBuilder` / `tests/common` could emit, removing the dependency.
Enforceable by a text rule: `Command::new("osmium")` only inside a fn
carrying `#[ignore = "external"]`.

### B5. Over-broad platform-skip predicates can turn real failures into skips

- `CliOutput::is_uring_unsupported` treats any stderr containing
  `"not supported"` as "io_uring unavailable". Any unrelated refusal that
  happens to say "not supported" (unsupported compression, unsupported
  feature in the input) is silently treated as a platform skip.
- `CliOutput::is_o_direct_unsupported` matches `"Invalid argument"` anywhere
  in stderr, which also matches clap-style argument errors in prose.
- `tests/common/mod.rs` `is_uring_unavailable` returns true for **any**
  `std::io::Error` and for any `pbfhogg::ErrorKind::Io`. Every I/O failure
  in an io_uring library test is a skip, including ENOSPC, EIO, and a
  genuine bug that returns an io error.

There are two independent implementations of "is this a platform
refusal": string matching for the CLI, type matching for the library, and
they disagree on what counts. Root cause: production emits no
machine-checkable signal for "platform does not support this". Fix: a
dedicated `ErrorKind::PlatformUnsupported { feature, source }` in
`src/error.rs`, a distinct CLI exit code for it (the project already has
`cli_exit_codes.rs`), and both predicates collapse to one check on that
kind / exit code. Enforceable: the type makes the refusal distinguishable;
a test asserts the exit code on a forced-unsupported path.

### B6. `BROKKR_TEST_PLATFORM` is set and nothing reads it

`brokkr.toml` `[test.profiles.platform]` sets
`env = { BROKKR_TEST_PLATFORM = "1" }`. No file under `src/`, `cli/`, or
`tests/` reads it. Either a leftover from an earlier design where platform
tests self-gated on it, or an intended "fail instead of skip when the
platform profile is running" switch that was never wired. The latter is
worth wiring: under the platform profile, the B5 skip predicates should be
hard failures, since a platform run that skips everything passes green.
Otherwise delete the key.

## 1. One value, one owner

### 1.1 Binary resolution: three rules, two of which diverge

- `tests/common/cli.rs` `pbfhogg_bin()`: `BROKKR_TEST_BIN_DIR`, else
  `CARGO_TARGET_DIR` else `CARGO_MANIFEST_DIR/target`, plus
  `cfg!(debug_assertions)` profile guess (its own doc admits the guess can
  pick the wrong subdir).
- `cli/tests/cli.rs`: `env!("CARGO_BIN_EXE_pbfhogg")` - cargo's own,
  always correct, but only available inside the `pbfhogg-cli` package.
- Note: `BROKKR_TEST_BIN_DIR` set but empty becomes `PathBuf::from("")`,
  i.e. `./pbfhogg` relative to the test's cwd - an empty value means "the
  current directory", not "unset".

The forced duplication: lib-crate tests cannot use `CARGO_BIN_EXE_*` for a
binary in another package, which is why the env-var rule exists. The
structural fix is to move every `cli_*.rs` into `cli/tests/` (they already
drive only the binary), where `CARGO_BIN_EXE_pbfhogg` is authoritative and
cargo guarantees the binary was built with the test's features - which also
dissolves B3 and the `build_packages` stale-binary class entirely. If the
split stays, the rule should live in exactly one place and treat empty as
unset.

### 1.2 Repo-relative `target/` writes ignore `CARGO_TARGET_DIR`

`pbfhogg_bin()` honours `CARGO_TARGET_DIR`; every test that writes into
"target" hard-codes `CARGO_MANIFEST_DIR/target` instead:
`tests/roundtrip_real.rs` `output_path`, `tests/cli_apply_changes.rs`
`merge_cross_validate_osmium`, `tests/geocode_index.rs` (`target/geocode-test`,
`target/geocode-test-build`), and the in-crate unit test helper in
`src/path_guard.rs`. Same subject, two resolution rules. See 10.1 for why
`target/` is the wrong place anyway.

### 1.3 Denmark dataset paths duplicated from `brokkr.toml`, and already diverged

The literal `data/denmark-20260220-seq4704.osm.pbf` appears in
`tests/roundtrip_real.rs` and `tests/cli_apply_changes.rs`;
`...-with-indexdata.osm.pbf` in `tests/geocode_index.rs`;
`data/denmark-20260221-seq4705.osc.gz` in `cli_apply_changes.rs`. The owner
of these names is `brokkr.toml`. Divergence is already a fact: the `dm6`
host's denmark is `denmark-20260415.osm.pbf`, so on dm6 every one of these
tests silently "Skipping: not found" and passes. Override env vars exist
only for two of the four sites (`PBFHOGG_TEST_PBF`,
`PBFHOGG_TEST_PBF_INDEXED`), with two different names for the same
dataset in two variants, and none for the OSC. Fix: brokkr resolves the
dataset for `brokkr test` and exports one set of variables (e.g.
`BROKKR_DATASET_DENMARK_RAW`, `..._INDEXED`, `..._OSC`), the tests read
only those, and a missing variable under a profile that names the test is
a failure, not a skip. Enforceable by a text rule banning `"data/` literals
in `tests/`.

### 1.4 Scratch root: three spellings

`brokkr.toml` says unknown hosts fall back to `data/scratch`; every listed
host sets `scratch = "data/bench-tmp"`; `tests/geocode_index.rs`
`output_dir()` probes `data/scratch/geocode-denmark` "built by brokkr run".
On plantasjen and bygg brokkr writes under `data/bench-tmp`, so the probe
never hits and the test always rebuilds. The comment claims a sharing
contract with brokkr that the config does not honour.

### 1.5 `PBFHOGG*` in `capture_env` captures nothing production reads

`capture_env = ["PBFHOGG*", "MALLOC*"]`. Production (`src/`, `cli/src/`)
reads no `PBFHOGG_*` variable; the only readers are the two test-path
overrides in 1.3. Either it is anticipatory, or it captures a variable a
past version read. Say which in the comment, or drop it.

### 1.6 PBF framing / varint parsing reimplemented in the harness, twice

`tests/common/adversarial.rs` has its own `read_varint`, `write_varint`,
`skip_field`, `parse_datasize`, `locate_blobs`, `decompress_blob`;
`tests/common/mod.rs` `count_tagdata_blobs` has a third `read_varint`
(no overflow bound, panics on a short buffer) and its own frame walk. The
production owners are `src/read/wire.rs` / `src/read/blob_wire.rs`. An
independent implementation in an adversarial builder is defensible (an
oracle should not share the code under test), but then it should be one
oracle, not two that differ (adversarial's returns `Option`, mod.rs's
panics). Consolidate the harness copies into one module; record in it why
it deliberately does not reuse production's parser.

### 1.7 Test timeouts spelled independently

`CliInvoker` `DEFAULT_TIMEOUT` 60 s; brokkr's per-test watchdog 20 s (fixed
on `brokkr check`); in-test waits in `read_paths.rs` of 5 s and 10 s. The
60 s default can never fire under `brokkr check`, because the 20 s
watchdog kills the test first and reports a `futex_do_wait` "hang" instead
of the CliInvoker's clear "timed out" message with stderr. The doc comment
on `CliInvoker` calls the timeout "load-bearing for brokkr check"; under
the current watchdog it is not. Set the default below the watchdog (or
read the watchdog from an env var brokkr exports) so the message that
names the subject is the one an operator sees.

## 2. Values nobody can find, change, or trust

- The set of fault-injection hooks has no index. They are scattered over
  ~12 modules (`read/pipeline.rs`, `read/region_grid.rs`,
  `write/parallel_writer.rs`, `write/parallel_gzip.rs`,
  `write/uring_writer.rs`, `commands/diff/parallel.rs`,
  `commands/diff/derive_parallel.rs`, `commands/altw/external/stage3.rs`,
  `geocode_index/builder/pass3.rs`, `geocode_index/reader.rs`,
  `commands/apply_changes/{rewrite,streaming}.rs`) with re-export shims in
  four different `mod.rs` files under four naming schemes
  (`read::pipeline_test_hooks`, `read::region_grid_test_hooks`, a
  `write::` re-export, `altw::` re-export). A single `pbfhogg::test_hooks`
  facade module would answer "what can a test inject" in one file.
- `apply_changes` is the one seam with an injection point: `MergeOptions`
  carries `#[cfg(feature = "test-hooks")] panic_at_blob_seq`. Every other
  seam uses process globals. That field also leaks into every
  construction site (`cli/src/main.rs` spells `panic_at_blob_seq: None`
  under a cfg twice, and `rewrite.rs` hand-writes a `Default` because
  `derivable_impls` only holds without the feature). See S1.

## 3. One channel

- Test skips are `eprintln!("Skipping: ...")` followed by `return`, which
  libtest reports as PASS. There is no channel by which a skip is visible
  in `brokkr check` output; combined with 1.3 and B5, a host can pass the
  whole real-data and platform tiers having run none of them. A shared
  `skip!(reason)` helper in `tests/common` that writes to a known marker
  (or panics under a "must run" env set by the owning profile, B6) would
  make skips countable.
- Fault-hook panics use a consistent `"test-hooks: ..."` prefix - good, and
  a test could assert the prefix to prove the panic came from the hook and
  not an incidental one.

## 4. Errors

- `tests/common/mod.rs` `snapshot_dir_inner` swallows `read_dir` errors and
  returns an empty set; `assert_scratch_unchanged` then compares two empty
  sets and passes. If the scratch root is wrong (typo, not yet created),
  the leak check is vacuous. The root call should `expect`; only
  subdirectories removed mid-walk merit tolerance.
- `ensure_index_exists` in `geocode_index.rs` turns any build failure into
  `None` via `.ok()?`, which callers treat as "skip". A real builder
  regression on Denmark reads as "data missing".
- `std::fs::create_dir_all(&dir).ok()` and `remove_dir_all(...).ok()` in
  the real-data tests swallow the reason the subsequent write fails.

## 5. Policy invented per call site

- Hook reset discipline: `parallel_writer` documents "call `reset` at the
  start AND end"; `read/pipeline.rs` test does start and end but skips end
  on panic; `ADMIN_PIP_CALLS` has reset but no arm/disarm; `region_grid`
  resets on both sides. No guard type. A `HookGuard` returned by an arm
  function, which resets on `Drop`, makes the end-reset unskippable even on
  panic (structural).
- Stale safety rationale, repeated: the comments on
  `write/parallel_writer.rs`, `write/parallel_gzip.rs`,
  `write/uring_writer.rs` and `commands/diff/parallel.rs` all say "tests
  serialize access via `--test-threads=1` (enforced by `brokkr test`)".
  That has not been true since the per-binary split; `brokkr.toml` now runs
  sweeps with `parallel = {}` and no thread limit. The actual invariant is
  "one test per `fault_*.rs` binary", which no comment states and nothing
  checks (and B1 already violates for the pipeline hooks). Reword the
  comments to name the real invariant and add the check from B1.
- Temp-dir policy: `src/path_guard.rs` test says "Never
  `std::env::temp_dir()`: ... collide across the concurrent check sweeps".
  `src/tag_expr.rs` (`pbfhogg_test_expr`, a fixed name with no suffix),
  `src/osc/parse.rs` and `src/osc/write.rs` (`pbfhogg_osc_*_test_{suffix}`)
  do exactly that, and also violate the project's no-`/tmp` rule. The
  `all`, `consumer` and `minimal` sweeps compile and run these unit tests
  concurrently, so the fixed-name `tag_expr` dir is a live cross-sweep
  collision. Enforceable: `clippy.toml` `disallowed-methods` for
  `std::env::temp_dir` (the project already runs strict clippy), with the
  sanctioned helper being `tempfile::TempDir::new_in(<owned root>)`.

## 6. Ambient dependencies

- `src/debug.rs` reads `BROKKR_MARKER_FIFO` from the environment inside a
  `OnceLock` on first marker emission. It is resolved once per process, so
  a test cannot point two runs at different FIFOs in one process, and a
  test that wants to assert the markers a command emits (a natural
  contract test for the brokkr protocol) has no way in except spawning the
  CLI. Also, the open is `O_NONBLOCK` and `.ok()?`: a FIFO path that exists
  with no reader, or a typo, silently disables all instrumentation for the
  process with no diagnostic. A one-line stderr warning when the variable
  is set but the open fails would turn "sidecar shows no markers" from a
  mystery into a message.
- The fault hooks themselves are the ambient-state pattern: production
  reaches a process-global to decide whether to panic, rather than being
  handed a hook. See S1.

## 7. Shared state whose safety is not structural

Covered by B1, B2 and the stale-rationale item in 5. Additionally:
`read/pipeline.rs` `maybe_block_decode` spins with `sleep(1ms)` on a
`Relaxed` flag with no timeout; if the arming test dies, the blocked
worker never exits. A bounded wait (panic after N seconds with a
"test-hooks: blocked decode never released" message) would convert a
watchdog "hang" into a named failure.

## 8. Unbounded growth

- `CliInvoker::run` drains child stdout/stderr into unbounded `Vec`s. A
  command that misbehaves and loops writing (e.g. `diff` or `inspect`
  output on a corrupted adversarial fixture) grows the test's memory until
  the timeout or watchdog fires. A cap (keep the first and last N MB, mark
  truncation) bounds it.
- `target/geocode-test*`, `target/denmark-roundtrip.osm.pbf`,
  `target/merge-xval-*.osm.pbf` are never removed; each is Denmark-sized
  and survives every run until `cargo clean`.

## 9. No longer load-bearing

- `BROKKR_TEST_PLATFORM` (B6): set, never read.
- `region_grid_test_hooks::reset` is used; but `pipeline_test_hooks`
  re-exports `REORDER_WINDOW_HIGH_WATER`, which no test reads (only
  `REORDER_FILLED_HIGH_WATER` is asserted) - verify before deleting; grep
  shows no reader in `tests/`.
- `tests/common/cli.rs` `path_arg` is a one-line wrapper over
  `as_os_str()`; check whether anything calls it.
- `[test.profiles.serial]` exists "reserved for any future serial-only
  test"; there are none. It is configuration with no subject, and the
  comment beside it describes the extra work needed if it ever gets one.
- `reference/testing.md` cites `tests/merge.rs::merge_cross_validate_osmium`
  and `tests/merge.rs:1271-1295`; the test lives in
  `tests/cli_apply_changes.rs` and `tests/merge.rs` does not exist. Durable
  doc citing a dead path (and a line range, which the doc rules forbid).
- `capture_env` `PBFHOGG*` (1.5).

## 10. Where it assumes it runs, and what it touches

### 10.1 Writes into `target/`

`target/` belongs to cargo. Tests write `target/denmark-roundtrip.osm.pbf`,
`target/merge-xval-pbfhogg.osm.pbf`, `target/merge-xval-osmium.osm.pbf`,
`target/geocode-test`, `target/geocode-test-build`, and
`src/path_guard.rs` unit tests make tempdirs under `target/`. Problems:
fixed names collide when two sweeps run the same ignored test; the files
are invisible to git and survive until `cargo clean` destroys them;
`ensure_index_exists` does `remove_dir_all(target/geocode-test)` before
rebuilding, so two concurrent geocode tests can delete each other's index
mid-read. The owned location is brokkr's scratch root (`scratch =` in
`brokkr.toml`), which brokkr creates, cleans (`brokkr clean`) and already
places on the right drive per host; brokkr should export it
(`BROKKR_SCRATCH_DIR`) and tests should create `TempDir::new_in` under it.
For plain-cargo runs, a project-owned `data/test-scratch/` (gitignored and
documented as the test harness's) is acceptable; `target/` and `/tmp` are
not.

### 10.2 `std::env::temp_dir()` in unit tests

See 5. `/tmp` is shared with every process on the host.

### 10.3 Host assumptions in the harness

- `pbfhogg_bin()` joins `"pbfhogg"` with no `EXE_SUFFIX`; harmless on
  Linux-only today but `CARGO_BIN_EXE_*` (1.1) gets it right for free.
- The platform skip predicates (B5) are the harness's model of "a host
  that differs"; on such a host the result is a silent pass, which is the
  worst of the three outcomes.
- Real-data tests assume `CARGO_MANIFEST_DIR/data` is the data root; the
  per-host `data =` key in `brokkr.toml` says otherwise is allowed.

## S. Structural recommendations

### S1. Replace process-global fault hooks with a handed-in hook object

Today: a static per seam, `cfg`-gated, armed by the test before the call,
consulted by production deep in worker threads. The rationale in the
comments is that threading a config through every entry point is
invasive. But that is exactly what `apply_changes` already does
(`panic_at_blob_seq` on `MergeOptions`), and the global form has produced
B1 and B2 and a stale serialization comment in four files. Proposal: one
`FaultPlan` type (cfg `test-hooks`, zero-sized otherwise) carried by the
options struct each command already takes, cloned into workers as an
`Arc`. Arming becomes per-invocation, sibling tests cannot see each
other's hooks, the one-test-per-binary rule stops being necessary, and
the `cfg` noise at construction sites collapses into a `Default` field
that is zero-sized in release. Enforceable: with no `pub static` in any
`test_hooks` module, there is nothing global to race on (a text rule can
ban `static` inside `mod test_hooks`).

### S2. Move CLI-driving tests into `cli/tests/`

Removes `BROKKR_TEST_BIN_DIR`, the profile heuristic, the
`build_packages` stale-binary class, and B3, because cargo builds the
binary with the test's own feature set and hands its path via
`CARGO_BIN_EXE_pbfhogg`. The stable-API allowlist the AGENTS.md describes
is unaffected; `tests/common` becomes a shared path dependency or a small
dev-only crate.

### S3. brokkr owns dataset and scratch resolution for tests

brokkr already resolves datasets, hashes and scratch per host. Exporting
them to `brokkr test` / `brokkr check` (and failing, not skipping, when a
profile that names a real-data test lacks the dataset) removes every
hard-coded `data/...` literal, the `PBFHOGG_TEST_*` overrides, the
`data/scratch` vs `data/bench-tmp` disagreement, and all writes into
`target/`.
