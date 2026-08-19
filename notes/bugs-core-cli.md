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

## Smaller / lower confidence

- NOT A BUG, recorded so it is not re-filed: `src/debug.rs` marker emission was
  reported as able to tear a line across a partial `write_all` on the
  `O_NONBLOCK` FIFO. It cannot. Marker and counter lines are tens of bytes, and
  POSIX makes a write of at most `PIPE_BUF` (4096 on Linux) to a FIFO atomic -
  on a non-blocking fd it transfers every byte or none and fails with `EAGAIN`.
  Investigated round 3, 2026-08-20. Confidence: high.
- `src/error.rs`: errors still do not name the *file* or the *blob index* they
  came from. `Error::byte_offset()` gives the position within a stream, but
  `BlobReader` does not retain the path it was opened from, so an error from a
  multi-input command (`cat a.pbf b.pbf`, `apply-changes`, `merge-changes`)
  still does not say which input failed. Closing it means threading the path
  through `BlobReader` and `FileReader`. Confidence: high that the gap is real,
  low urgency.
- `decisions/0003-error-path-hygiene-via-pathguard.md` claims "every final
  output path is wrapped in `PathGuard::file`", but only `renumber` ever did.
  `sort`, `cat`, `extract`, `tags-filter`, `time-filter`, `apply-changes`,
  `merge-changes`, `repack`, `degrade` and the rest construct their writer on
  the user's `--output` path with no guard at all, so a mid-run failure leaves
  a truncated partial file behind under the user's chosen name. Round 3 fixed
  the two paths the report named (`export`, `renumber`) by routing them through
  the new `path_guard::OutputFile` (scratch sibling + rename on commit); the
  remaining commands should be converted to the same primitive, which also
  closes the truncate-on-open half for them. Surfaced by the round-3 fix pass.
  Confidence: high.
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
