# Bug hunt: core infrastructure and CLI

Hunt date 2026-08-12. Single Opus hunter, scope: `src/lib.rs`, `src/error.rs`,
`src/osm_id.rs`, `src/owned.rs`, `src/path_guard.rs`, `src/coord_fmt.rs`,
`src/debug.rs`, `src/reorder_buffer.rs`, `src/idset.rs`, `cli/src/main.rs`.
Findings only, unverified.

## Argument-surface and exit-code design

### Exit code 1 means both "diff found differences" and "the command failed"

`cli/src/main.rs`: `ExitWithCode(1)` for `diff` differences and
`inspect --indexed` false collapses onto `ExitCode::FAILURE` == 1, which is also
every error path. A caller scripting `pbfhogg diff` cannot distinguish "files
differ" from "file not found". osmium uses 1 for the semantic answer and 2 for
errors; `ExitWithCode` already exists to carry a code, so making generic errors
exit 2 is a one-line change plus a doc entry. `reference/cli-reference.md`
documents no exit-code semantics at all beyond `--indexed`.

Confidence: high.

### Silently-ignored flag combinations in `inspect`

`--show` returns early before every other flag is examined:
`inspect --show n1 --json --blocks --extended` runs `show_element` and discards
the rest with no diagnostic, while `--nodes --json` and `--get --json` are hard
errors. `--force` is threaded only into `node_stats`, so
`inspect --blocks --force` on an unindexed file silently ignores it. `-j/--jobs`
is documented as ignored outside `--nodes` and indeed is. The `run_inspect` mode
dispatcher (four overlapping early-return blocks, each recomputing its own
`has_other_flags`) is where these leak from; it wants to be an explicit mode enum
resolved once.

### Free-form `String` flags parsed by hand at four different layers

`--compression`, `--index-type`, `inspect tags --sort`, `bench-read --mode`,
`bench-write --writer`, `bench-merge --io-mode`, `--type`, `--clean` are all
`String`, validated (or not) in `main.rs`, in `commands/`, or via `FromStr` in
the library. Consequences: no shell completion, no `--help` enumeration of valid
values and inconsistent error phrasing. `--type` is now validated by a
hand-written `value_parser` in `main.rs`, which is the pattern this finding wants
replaced rather than repeated. `DiffFormat` / `InputKind` / `ExportTypeArg` show the intended pattern.
Converting the rest to `ValueEnum` (with `Compression` keeping a custom parser
for the `:LEVEL` suffix) removes a whole class of finding.

### `getid --verbose-ids` re-reads the entire output PBF

`cli/src/main.rs` -> `print_missing_ids` -> `parse_ids_from_pbf`. A full second
decode pass over the just-written output, undocumented in
`reference/cli-reference.md` ("Print requested IDs and report which were not
found"). At planet scale this roughly doubles the command's runtime for a
diagnostic. The information is available for free inside `getid` itself, which
already knows which requested IDs it emitted.

### Doc/code drift in `reference/cli-reference.md`

- `cat`: code rejects `--type` and `--clean` when combined with `--dedupe`; the
  doc does not mention it.
- `check`: code has `--quiet` `conflicts_with = "json"`; not documented.
- `diff`: `-j` is silently ignored under `-v/--verbose` (the doc admits verbose
  is sequential but does not say `-j` is accepted-and-ignored, unlike the
  `tags-filter` `-R`+`-j` case which is a hard error). The inconsistency between
  "reject" and "ignore" for the same situation is itself a design flaw.
- `degrade`: `--block-cap` and `getparents --full-scan-min-blobs` are
  `hide = true` test instruments - acceptable, but `--full-scan-min-blobs`
  reaches a separate library entry point (`getparents_with_min_blobs`) from CLI
  input with no bound checking.
- `AGENTS.md` lists `roaring` as a `commands` dependency; `Cargo.toml` no longer
  has it.

## Feature gating

### `default-features = false` is claimed to work but is never built

`brokkr.toml`'s `consumer` sweep sets `no_default_features = true` but then
re-enables `features = ["commands"]`, so no CI configuration ever compiles the
crate without `commands`. `src/commands/` is not gated as a whole - only `check`,
`export`, `extract`, `inspect::json`, and `geocode_index::builder` are - so
`altw`, `apply_changes`, `cat`, `degrade`, `diff`, `getid`, `getparents`,
`inspect`, `merge_changes`, `renumber`, `repack`, `sort`, `tags_count`,
`tags_filter`, `time_filter` all compile into a "library-only" build.
`AGENTS.md`'s "`default-features = false` skips `commands` feature" is
misleading, and the configuration is untested. Add a third sweep with genuinely
no features.

### The library/CLI boundary is porous by design

`lib.rs` re-exports the entire command layer plus `path_guard`, `tag_expr` and
`HeaderOverrides` as `#[doc(hidden)] pub` purely so `pbfhogg-cli` (a separate
crate) can call them. Every internal refactor of `commands/` is therefore a
semver-visible change disguised by `doc(hidden)`, and library consumers get a
large accidental API. The right shape pre-1.0 is a third crate
(`pbfhogg-commands`) between the format library and the binary, or making these
genuinely public and documented. Related: `error.rs` bakes CLI advice ("use
`--type node` or an altw input") into a library `Display` impl - the library
should not know CLI flag names.

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
- `cli/src/main.rs`: `Cli::parse()` calls `process::exit` on a parse error,
  bypassing the hotpath guard drop that the `ExitWithCode` machinery exists to
  protect. Use `Cli::try_parse()` if that report matters on the failure path.
