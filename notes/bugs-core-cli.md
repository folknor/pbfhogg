# Bug hunt: core infrastructure and CLI

Hunt date 2026-08-12. Single Opus hunter, scope: `src/lib.rs`, `src/error.rs`,
`src/osm_id.rs`, `src/owned.rs`, `src/path_guard.rs`, `src/coord_fmt.rs`,
`src/debug.rs`, `src/reorder_buffer.rs`, `src/idset.rs`, `cli/src/main.rs`.
Findings only, unverified.

## Argument-surface and exit-code design

### Disclosed boundary: the exit-code split is unverified against brokkr's runner

The 0.6.0 exit-code split (0 success, 1 semantic-negative answer, 2 failure)
has *not* been verified by source inspection against brokkr, which shells out
to `pbfhogg` for every one of its subcommands: `~/Programs/brokkr` is not
readable from the sandboxes this loop ran in, on either the fix side or the
orchestrating side.

What was verified, empirically from this side: `brokkr inspect --force
--dataset denmark` and `brokkr inspect --force --dataset denmark --nodes` both
run clean, and neither injects `-j`, so neither the new `requires = "nodes"`
on `inspect -j`/`--force` nor the new `diff -v` + explicit `-j` conflict fires
under brokkr's own invocations. brokkr's `--force` is its own clean-tree
override and is not forwarded to the child.

The remaining theoretical exposure: a consumer - brokkr or any script - that
special-cases `== 1` as "the command failed" rather than testing `!= 0`. Such
a consumer now sees `2` where it used to see `1` for every genuine failure.
`1` itself kept its meaning for all four semantic answers, so the reverse
direction is safe. Tracked cross-repo in `TODO.md`.

Confidence: high that the exposure is narrow; unresolved only because the
other tree is unreadable.

### `run_inspect`'s mode dispatcher is still four overlapping early returns

Round 2 fixed the *silence*: `--show` is now `conflicts_with_all` against
every other inspect flag, and `-j/--jobs` / `--force` carry
`requires = "nodes"`, so none of these combinations is accepted-and-discarded
any more (clap rejects them at parse time, and `tests/cli_exit_codes.rs`
covers each one). What was not done is the shape the original finding also
asked for: `run_inspect` still has four early-return blocks (`--indexed`
alone, `--nodes` alone, `--get` on a header-only key, then the full scan), two
of which recompute their own local `has_other_flags`. Now that the illegal
combinations are rejected up front, several of those guards are dead weight
and the function wants an explicit mode enum resolved once. Behavioural risk
is low; this is a legibility item.

Confidence: high that the shape is still there, low that it hides a bug.

## Feature gating

### `src/commands/` is still not gated as a whole

Round 2 added the `minimal` sweep (`no_default_features`, `features = []`, no
`build_packages`), so the no-`commands` configuration is now compiled and
linted on every check run. Six test files that reach into the gated modules
carry a file-level or per-test `#![cfg(feature = "commands")]` so they drop
out there.

What remains is the underlying asymmetry the original finding named: the
`commands` feature gates only `check`, `export`, `extract`,
`commands::spatial`, `inspect::json` and the geocode builder. `altw`,
`apply_changes`, `cat`, `degrade`, `diff`, `getid`, `getparents`, `inspect`,
`merge_changes`, `renumber`, `repack`, `sort`, `tags_count`, `tags_filter`
and `time_filter` compile into a "library-only" build regardless. `AGENTS.md`
now describes that accurately rather than claiming the feature skips the whole
command layer, so the documentation is no longer wrong - but whether the
feature *should* gate the module wholesale is an open design question, and
answering it "yes" is a much larger change than a bug-fix round (every command
module, the `lib.rs` re-exports, and roughly 30 test files).

Confidence: high (this is a description of the current state, not a claim).

### The library/CLI boundary is porous by design

The concrete leak the original finding named is fixed:
`ErrorKind::MissingFeature("LocationsOnWays")` no longer writes
`; use --type node or an altw input` into a library `Display` impl, and
`ParseIndexTypeError` for the removed `dense` index type no longer names
`--index-type`. The CLI re-attaches its own flag advice in `export_error` in
`cli/src/main.rs`, where the flags actually exist.

The structural half is untouched, deliberately - the round-2 brief said to fix
the leak without restructuring the workspace. `lib.rs` still re-exports the
entire command layer plus `path_guard`, `tag_expr` and `HeaderOverrides` as
`#[doc(hidden)] pub` purely so `pbfhogg-cli` (a separate crate) can call them,
so every internal refactor of `commands/` is still a semver-visible change
disguised by `doc(hidden)`, and library consumers still get a large accidental
API. The right shape pre-1.0 is a third crate (`pbfhogg-commands`) between the
format library and the binary, or making these genuinely public and
documented.

Confidence: high.

### `getparents --full-scan-min-blobs` still has no bound checking

Documented in `reference/cli-reference.md` as an unsupported hidden test
instrument, which was the round-2 fix for the doc/code drift finding. The
underlying observation stands: the flag reaches a separate library entry point
(`getparents_with_min_blobs`) straight from CLI input with no range check, so
`--full-scan-min-blobs 0` silently pins the FullScan arm. Harmless today
because it is hidden and test-only.

Confidence: high.

## Smaller / lower confidence

- `src/debug.rs`: the marker clock is `Instant::now()` captured lazily inside
  `OnceLock::get_or_init` at the first marker, not at process start,
  contradicting the doc comment "microseconds since process start". Every marker
  timestamp is offset by the pre-first-marker startup time, which matters when
  correlating against brokkr's `/proc` samples. Confidence: high that the doc is
  wrong, medium that it has misled a measurement.
- `src/debug.rs`: `O_NONBLOCK` is a hand-rolled `const` defined only under
  `target_os = "linux"` / `"macos"` inside a `cfg(unix)` block - any other unix
  (BSD, illumos) fails to compile with an undefined constant. `libc` is a
  mandatory dependency; use `libc::O_NONBLOCK`.
- `src/debug.rs`: `write_all` on an `O_NONBLOCK` FIFO can return `WouldBlock`
  after a partial write, emitting a torn marker line into the protocol stream
  rather than dropping the marker cleanly as documented. Also one `format!`
  allocation per marker/counter.
- `src/error.rs`: `ErrorKind` carries no file path, blob index, or byte offset on
  any variant. A `WireFormat { msg: &'static str }` from a 90 GB planet file
  tells the user nothing about where. Given the crate's own `ByteOffset` type,
  adding blob offset to `Blob` / `WireFormat` variants is cheap and would
  materially improve every truncation/corruption report.
- `src/error.rs`: `From<Error> for io::Error` via `io::Error::other` boxes the
  typed error; the doc examples in `lib.rs` use `Ok::<(), std::io::Error>(())`,
  so the public reader API's canonical usage immediately discards the typed
  `ErrorKind`.
- `src/path_guard.rs`: `commit(mut self)` takes `self` by value, so `path` is
  always `Some`; the `.expect("PathGuard::commit called after path released")` is
  unreachable. Also, `PathGuard::file` on a user-supplied `--output` deletes a
  pre-existing file of that name on any error, so `export -o important.json` on a
  failing input deletes `important.json`. The input/output aliasing check now
  rejects the case where the output *is* the input, but any other pre-existing
  file named as the output is still destroyed by a mid-run failure.
- `src/path_guard.rs`: the tests write to `std::env::temp_dir()` with
  `process::id()`-only names, contravening the project's "never `/tmp`" rule and
  colliding across concurrent sweeps. `tempfile` is already a dev-dependency.
- `src/reorder_buffer.rs`: `push` resizes `pending` to whatever seq arrives, so a
  stalled producer slot grows the buffer without bound; the bound lives entirely
  in the callers' permit accounting. A `max_window` field would make the
  invariant local.
- `src/idset.rs`: `merge_from(&IdSet)` and `merge(Self)` are near-duplicate
  implementations; the owned form's zero-copy chunk move is the only difference.
  Pick one.
- `cli/src/main.rs`: duplicated `#[allow(clippy::too_many_arguments)]`
  attributes, and `run_show_element` (3 parameters) carries
  `too_many_arguments` + `too_many_lines`. Symptomatic of the wider issue: 20+
  `run_*` functions take 8-14 positional arguments because every command's
  options are splatted rather than passed as the option struct the library
  already defines.
- `cli/src/main.rs`: `validate_input_output_aliases` canonicalizes inputs for
  *every* command up front, so a nonexistent input now surfaces as an
  aliasing-check IO error before the command's own error path ever runs. No
  command accepts `-`/stdin, so nothing functionally breaks, but the error text
  for a missing input file changed shape. Either canonicalize lazily or map the
  aliasing check's IO failure back to the command's own missing-input error.
  Surfaced by the round-1 fix-and-commit agent, 79ba805a. Confidence: high.
- Root-level `DOC-CONVENTIONS-AUDIT.md` violates the document-folder convention
  in `AGENTS.md`: the root exemption list is `AGENTS.md`, `CLAUDE.md`,
  `README.md`, `LICENSE`, `CHANGELOG.md` and their kin. It is a transient audit
  and belongs in `notes/`. Confidence: high.
- `cli/Cargo.toml`: the five-line comment justifying `serde_json`'s
  `float_roundtrip` feature is duplicated verbatim, immediately after itself,
  above the `serde_json` dependency line. Cosmetic; delete one copy.
  Confidence: high.
- `cli/src/main.rs`: `Cli::parse()` calls `process::exit` on a parse error,
  bypassing the hotpath guard drop that the `ExitWithCode` machinery exists to
  protect. Use `Cli::try_parse()` if that report matters on the failure path.
