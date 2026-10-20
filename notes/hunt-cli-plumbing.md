# Hygiene hunt: CLI and shared plumbing

Scope: `cli/src/main.rs`, `src/commands/{inspect,check,tags_count}/`,
`src/commands/mod.rs`, `src/{error,debug,path_guard,lib,coord_fmt,osm_id,owned}.rs`,
both crate manifests, workspace lints, `brokkr.toml` check config. Findings
go outside scope where the thread led there. Unranked. Each one says whether
the fixed version could be enforced, and by what.

Existing enforcement worth knowing, since it shows what this build can hold:
the workspace clippy deny list (`unwrap_used`, `too_many_arguments`,
`cognitive_complexity`, `too_many_lines`, cast lints), three `[[check]]`
sweeps (`all`, `consumer`, `minimal`) that keep feature shapes compiling, and
the CLI's typed-value parsing (`TypeFilter`, `Compression`, `ValueEnum`s) that
pushes some flag validation into clap. No `clippy.toml` exists, so
`disallowed-methods` / `disallowed-macros` are not in use yet. Several of
the fixes below would be one `disallowed-*` entry each.

---

## 1. One value, one owner

### 1.1 "jobs = 0 means auto" is resolved by four different rules
The same `-j/--jobs` flag, default `0`, means different things depending on
the command, and the resolution happens at different layers:

- `diff` / `diff --format osc`: the CLI resolves `0` itself to
  `available_parallelism()` (`run_diff`, `run_derive_changes`, two identical
  copies of the same 6-line block).
- `tags-filter`, `apply-changes`, `merge-changes`: the CLI maps `0` to
  `None` (three more copies of `if jobs == 0 { None } else { Some(jobs) }`)
  and the library picks. For `apply-changes` the library's choice is
  `nproc - 2` (`apply_changes/rewrite.rs`, `scan/classify.rs`), and the
  `--help` text says so; for `tags-filter` the help says
  `available_parallelism()`.
- `inspect --nodes`, `inspect tags`: the CLI passes the raw `usize` and the
  library does `(jobs > 0).then_some(jobs)` itself (`tags_count`).

So "auto" is either nproc or nproc-2 depending on the command, there are at
least six copies of the 0-to-Option conversion, and the library `jobs` field
type is `usize` in some options structs and `Option<usize>` in others.
Separately, about twenty library sites call `available_parallelism()`
directly to size decode pools (extract, altw stages, tags_filter, renumber,
geocode builder, pipeline.rs, reader.rs), each with its own fallback and
its own `-2` or no `-2`.
**Fix:** one `Jobs` type (`Auto | Fixed(NonZeroUsize)`) parsed by clap and
carried into every options struct; one `resolve_workers(Jobs, reserve)` in
the library that owns the nproc read. **Enforceable:** yes - the type makes
`0`-as-sentinel unrepresentable, and `clippy::disallowed_methods` on
`std::thread::available_parallelism` (allowed only in the resolver module)
stops new direct reads.

### 1.2 The 8000-element block cap has three owners
`write/block_builder.rs` `MAX_ENTITIES_PER_BLOCK = 8000` (private),
`degrade::DEFAULT_BLOCK_CAP = 8000` (pub), and `repack`'s
`#[arg(default_value_t = 8000)]` literal in `cli/src/main.rs`. The repack help
text also hard-codes "Default 8000". Nothing ties them together; changing the
builder's default leaves repack and degrade on the old number.
**Fix:** one pub const in `block_builder`, read by both CLI defaults and by
degrade. **Enforceable:** partly - a unit test asserting
`degrade::DEFAULT_BLOCK_CAP == block_builder::DEFAULT_CAP` is cheap, but the
real fix (single const) makes the test unnecessary.

### 1.3 `BROKKR_TEST_BIN_DIR` resolution rule stated twice, already diverged
`tests/common/cli.rs` module doc (top of file) states the binary lookup
rule as "`CARGO_TARGET_DIR` if set, otherwise `CARGO_MANIFEST_DIR/target`"
and never mentions `BROKKR_TEST_BIN_DIR`; the function doc on
`pbfhogg_bin()` gives the real three-step order with `BROKKR_TEST_BIN_DIR`
first. The copies disagree today. Empty-value semantics are unspecified in
both: `BROKKR_TEST_BIN_DIR=""` resolves to the relative path `pbfhogg` and
then asserts with a message blaming the brokkr sweep config.
`cli/tests/cli.rs` uses a third mechanism (`env!("CARGO_BIN_EXE_pbfhogg")`).
**Fix:** delete the module-level restatement and point at the function; treat
empty as unset. **Enforceable:** no mechanical check for doc drift; the
empty-value rule can be a unit test of the resolver.

### 1.4 `BROKKR_MARKER_FIFO` name is a protocol constant owned by two repos
`debug.rs` spells it once (good), but the name and the line format
(`<us> <name>\n`, `<us> @<name>=<value>\n`) are a contract with brokkr that
nothing in this repo pins. It must be duplicated (brokkr is a separate
crate), and nothing keeps the copies in step. **Fix:** a `const` plus a
test that formats a marker and a counter and compares against literal
golden lines (the existing `line_formats_marker_exactly` test covers markers
only, not the `@name=value` counter shape). **Enforceable:** golden test
here, mirrored by a parse test in brokkr against the same literals.

### 1.5 Sorted-input error text duplicated, and wrong for most callers
`commands/mod.rs` has `require_sorted` and `require_sorted_err`, two
byte-identical format strings. Both end with "Streaming diff requires sorted
inputs to operate in constant memory." That sentence is emitted for
`renumber` ("Input PBF"), `time-filter` ("Input history PBF") and `cat
--dedupe` too, so three of the five callers tell the operator that a diff
needs sorting when they ran something else. Fact, not prediction.
**Fix:** one function; the caller supplies the reason (as `require_indexdata`
already does). **Enforceable:** a signature that takes `reason: &str`
makes the default text impossible.

### 1.6 Way-level metadata loss warning has two copies
`commands::warn_locations_on_ways_loss` and an inline copy of the same
"Warning: input PBF has way-level enrichment metadata..." text in
`repack/mod.rs`. **Fix:** call the helper. **Enforceable:** only by review,
or by the general diagnostics channel in 3.1 which would own warning text.

### 1.7 `writer_from_header_bytes` and `writer_from_header_bytes_parallel`
Two 30-line functions in `commands/mod.rs` that differ in one call
(`to_path` vs `to_path_parallel`), each with its own copy of the
"--io-uring requires the linux-io-uring feature" / "--direct-io requires..."
error strings, plus `writer_from_header` / `writer_from_header_parallel`
wrapping them. Four functions for one decision. Decision 0014 keeps
backends separate, which does not need four entry points. **Fix:** one
function taking an output-backend enum (`Buffered | Parallel | Direct |
Uring`) chosen once from `WriteOpts`. **Enforceable:** yes, by the enum.

### 1.8 `has_indexdata` / `has_tagdata` are the same loop twice
Identical header walk in `commands/mod.rs`, differing in which field is
returned. Both also trust the first OsmData blob only. **Fix:** one
`probe_first_data_blob(path) -> Option<BlobHeaderInfo>` and two one-line
accessors. **Enforceable:** no.

### 1.9 Header override keys spelled in three places
`HeaderOverrides::parse` match arms, the CLI `--output-header` help text,
and the error message. No list exists to enumerate. **Fix:** a
`const KEYS: &[&str]` or a `ValueEnum` for the key, with help generated from
it. **Enforceable:** yes if the key is parsed by clap into an enum.

### 1.10 `inspect --get` key namespace has two owners
The CLI's `InspectMode::HeaderGet` arm hard-codes `file.name`, `file.size`,
`file.format`, `header.bbox`, `header.writing_program`,
`header.replication.*`, while `InspectReport::get_value` owns the rest, and
`InspectMode::resolve` has a third list (prefixes `data.`, `metadata.`,
`elements.`, `blocks.`, `indexed`) deciding which side answers. A key added
to the report side that does not match those prefixes is routed to the
header path and errors with "unknown key". `run_inspect` then has a fourth
check (`data.`/`metadata.` forces `--extended`). **Fix:** move the whole key
table into `inspect` with a per-key "needs scan / needs extended" attribute;
the CLI asks the table. **Enforceable:** a test that every key in the table
resolves on a fixture under both paths.

### 1.11 Two ID-spec parsers for the same syntax, already diverged
`inspect --show` parses `n123` / `node/123` via `parse_element_spec` in
`cli/src/main.rs`; `getid` / `getparents` parse `n123` via
`getid::parse_id_spec_inner`. `node/123` works for `--show` and is rejected
by `getid`; negative IDs are rejected by getid (ADR-0002) and accepted by
`--show`. `parse_element_spec` also carries an `unreachable!()`.
**Fix:** one `ElementRef` `FromStr` in the library (beside `TypeFilter`),
used by both, with a clap `value_parser`. **Enforceable:** yes, by type.

### 1.11b `ShowElementType` / `DefaultType` / `DefaultTypeArg` / `ExportTypeArg` / `ElemKind`
Five enums for "node, way or relation" crossing the CLI/library boundary,
plus `TypeFilter` and hand-written `match`es converting each pair in
`main.rs`. **Fix:** one public `ElementKind` with `ValueEnum` derived
behind a CLI-side newtype, or a `clap` feature on the library.
**Enforceable:** yes, by type.

### 1.12 Decimicrodegree conversion: two arithmetic conventions
`coord_fmt::from_decimicro` divides (`f64::from(d) / 1e7`); export, extract,
geo.rs, the geocode builder and `commands/spatial.rs` multiply
(`x as f64 * 1e-7`). `spatial.rs` documents at length that these two are
not the same value in binary floating point and that `* 1e-7` is the
canonical one. `coord_fmt` is the module whose job is to own this, and it
uses the other convention; diff text output and OSC writer go through it,
export goes through `* 1e-7`. At `{:.7}` formatting the two usually print
the same digits, so this is latent rather than observed. About thirty
inline `* 1e-7` sites exist. **Fix:** one `e7_to_deg` / `deg_to_e7` pair
(in `coord_fmt`, matching `spatial.rs`'s convention), and route every site
through it. **Enforceable:** a text rule (grep for `1e-7` / `1e7` outside
`coord_fmt.rs` and `spatial.rs`) wired into `brokkr check`.

### 1.13 Workspace metadata partially duplicated, MSRV already diverged
Root `rust-version = "1.99"`, CLI `rust-version = "1.96"`. The CLI depends
on the library by path, so it cannot build on 1.96; its MSRV claim is
false. The CLI also restates `authors` instead of `authors.workspace = true`.
**Fix:** `rust-version` and `authors` in `[workspace.package]`, inherited by
both. **Enforceable:** yes, by inheritance; `cargo msrv verify` in a sweep
for the value itself.

### 1.14 Exit-code contract documented twice, one copy wrong
`reference/cli-reference.md` and `docs/cli/commands.md` both carry the exit
code table. The reference copy says that for `1` "Nothing is printed to
stderr for it beyond the command's normal output"; the `ExitStatus` doc in
`main.rs` says the same. `inspect --show` prints `Element not found: <spec>`
to stderr before exiting 1 (`run_show_element`). Either the message or the
contract is wrong. **Fix:** decide, then keep the table in one document
and link from the other. **Enforceable:** a CLI test asserting empty stderr
on every `AnswerNegative` path.

### 1.15 `serde_json` `float_roundtrip` repeated in both manifests
Deliberate and commented. The reason given ("stating it here makes the
guarantee explicit") is fine; nothing keeps the copies in step (a version
bump in one only). **Fix:** `[workspace.dependencies] serde_json = {...}`
inherited by both. **Enforceable:** yes, by inheritance.

---

## 2. Values nobody can find, change, or trust

### 2.1 No inventory of tunables
The tunables for this system are spread across: clap defaults in `main.rs`
(geocode levels and radii, `--max-errors 100`, `--elements-per-blob`,
`--compression zlib`, bench-write's `zlib:6`), module consts (`check`'s
`NODE_ID_FLOOR` etc., `debug::PIPE_BUF`, `MAX_ENTITIES_PER_BLOCK`), library
literals (`M_ARENA_MAX = 2` in nine places, see 6.2; the altw `auto`
80 %-of-RAM threshold; `nproc - 2`), and `brokkr.toml`. Nothing answers
"what can I tune here". Two different compression defaults exist: every
command defaults to `zlib` (level unspecified, library default) while
`bench-write` defaults to `zlib:6`, so the benchmark does not measure what
the commands do if the library default is not 6.
**Fix:** a `tunables` module (or `reference/` table generated from it)
that owns every default; clap reads `default_value_t` from it.
**Enforceable:** partly - a test that renders the table and compares it to
a checked-in snapshot makes additions visible.

### 2.2 Inspect `--anomalies` thresholds hard-coded in help text
"<50% or >150% of median" appears in the `--anomalies` help; the numbers
live in the inspect report code. Two owners for one threshold.
**Enforceable:** generate the help string from the const (`concat!` cannot
take floats; a `const` string beside the numbers plus a test).

### 2.3 `check` ID floors encode the live OSM ID space
`NODE_ID_FLOOR = 14e9`, etc., with comments stating today's live counts
("live OSM is ~13e9"). Those comments go stale by construction (this is
the exact drift the global doc rule warns about). The floor only matters
when indexdata is absent or understated; with the file-size ceiling now in
place the floors could be derived (or dropped). **Fix:** reword the
comments so they do not carry a dated count, or drop the floors in favour
of the file-derived bound. **Enforceable:** no.

### 2.4 `getparents --full-scan-min-blobs` is the only injection point for ADR-0006
The dispatch threshold has a hidden CLI override and a separate library
entry point (`getparents_with_min_blobs`) for it; other commands that use
`dispatch_scan_arm` have no override. A test touching those commands takes
whatever the default says. **Fix:** put `min_blobs` in each options struct
with a default, rather than a sibling `_with_min_blobs` function per
command. **Enforceable:** by signature.

### 2.5 Flags validated at use, deep in the run
Most cross-flag validation in `main.rs` happens after parse but before any
work (good), but several checks are deferred into the library or into the
run itself:
- `--io-uring` / `--direct-io` without the feature: rejected in
  `writer_from_header_bytes*`, i.e. after the input has been opened,
  headers read and (for some commands) a full first pass completed. On
  `add-locations-to-ways` or `apply-changes` the refusal can come minutes in.
- `--output-header` and `--generator` are parsed at dispatch per command
  (`HeaderOverrides::parse` called in fourteen arms) rather than as a clap
  `value_parser`; same for `--drop-ids`, `--start-id` (parsed in
  `run_renumber`, after alias checks), `time-filter`'s timestamp, `--bbox`
  (parsed in `run_extract` / `ExportOptions::new`), and `--show`.
  The `main.rs` comment "Every free-form flag is parsed exactly once, here
  at the argument boundary" is not true for these.
- `tags-filter` with no expressions, `getid` with no IDs: rejected after
  the fact in `run_*`.
**Fix:** `value_parser`s for every free-form flag; feature-availability
checks at argument time (a clap `value_parser` on the bool that refuses
when `cfg!(not(feature))`). **Enforceable:** a test per flag that the
refusal happens with no input file present (proves it ran before I/O).

---

## 3. One channel, one implementation

### 3.1 The library writes to stderr directly, everywhere
About 40 library files call `eprintln!` (counted with `rg -c`): summaries via
`Stats::print_summary` (every command), progress (`osc/parse.rs` "[1/3]
Parsing diff..."), warnings, hints, and debugging (`[coord_payloads]`,
`[altw external] num_workers = ...`, `[uring_writer] error:`,
`[parallel_writer] thread exit:`). `inspect` and `check` reports print via
`println!` inside library methods (`print_report`, `print_human`). A
library consumer cannot silence, redirect or capture any of it, and the CLI
has no `--quiet` beyond `diff -q` / `check --quiet`. Prefixes are
inconsistent for the same class of event: `Warning:`, `warning:`,
`Warning!` (CLI, two sites), `hint:`, `[tags-count]`, `[getid --invert]`,
`[inspect]`, `time-filter:`, `add-locations-to-ways:`, bare text.
**Fix:** library returns stats/report values and never prints; one
`diag` module (or a `Reporter` trait handed in through the options) owns
warnings and progress; the CLI owns formatting and `--quiet`. **Enforceable:**
yes - `clippy::print_stderr` and `clippy::print_stdout` set to `deny` in the
library crate (with `allow` in `cli/`), which is exactly the kind of rule
the workspace lint table already holds.

### 3.2 Writer errors logged and also returned
`uring_writer_thread` and `parallel_writer_thread` print the error and
return it, so the operator sees it twice (once with a `[uring_writer]`
prefix, once as `Error: ...` from `main`), in different words. **Fix:**
return only. **Enforceable:** covered by 3.1's lint.

### 3.3 Operator text names CLI commands from inside the library
`require_indexdata` tells the user to run `pbfhogg cat input.osm.pbf -o
indexed.osm.pbf` and to "pass --force"; `require_sorted*` says `pbfhogg
sort`; `verify_ids` warns with the same `pbfhogg cat` recipe; writer
constructors say `--io-uring requires ...`; `inspect`'s scan says
`--direct-io not used...`. `error.rs` explicitly refuses to name CLI flags
in the library ("made the library a dependency of the CLI's argument
surface") and `export_error` in `main.rs` moved one such hint to the CLI.
The rule is applied in one place and broken in about ten. **Fix:** typed
errors (`ErrorKind::MissingIndexdata { path }`, `Unsorted { path, role }`,
`FeatureNotCompiled(&'static str)`) with CLI-side advice, as `export_error`
already does. **Enforceable:** a text rule rejecting `pbfhogg ` and `--`
followed by a flag name inside string literals under `src/` (allowlist for
tests).

### 3.4 Errors printed without their source chain
`main` prints `Error: {e}` for a `Box<dyn Error>`. Library errors wrapped as
`Box<dyn Error>` via `format!(...).into()` lose structure, and anything with
a `source()` (an `io::Error` inside `pbfhogg::Error`) prints only its top
`Display`. For `ErrorKind::Io`, the io error's Display carries no path
(e.g. "No such file or directory (os error 2)") unless the framing layer
stamped `in <path>`, which it does only for blob reads. Open failures in
`FileReader::open`, `std::fs::File::create(out.path())` in `run_export`,
`canonicalize` in the alias check, and `parse_ids_from_file` all surface as
bare OS messages with no subject. `create` on the scratch path would name
`.pbfhogg-tmp-<pid>-<n>.out.pbf`, not the user's path, if it named anything.
**Fix:** a context-adding wrapper at every open/create (`open_input(path)` /
`create_output(path)`) and print the full `source()` chain in `main`.
**Enforceable:** `disallowed_methods` on `File::open` / `File::create` /
`fs::read_to_string` in `src/` outside the wrappers.

### 3.5 Significant events with no log
- `OutputFile::commit` replacing an existing file: silent. A user who
  pointed `-o` at the wrong existing file gets no notice it was replaced.
- `bench-merge` deletes its output path before running (see 4.4): silent.
- `check --index` / `check --refs` with `--direct-io`: the flag is accepted
  and dropped silently in `check_refs` (`let _ = direct_io;`) and in
  `getid::parse_ids_from_pbf(_direct_io)`. `inspect` at least prints that it
  ignored it. Same flag, three behaviours.
- `owned::element_metadata` turns a string-table decode error on `user()`
  into `""` and re-encodes it, silently rewriting data (see 4.2).

### 3.6 The `tags-filter -R -t` and `getid -t` warnings belong to clap
Both are "flag has no effect with this combination" checks emitted as
`Warning!` to stderr from `run_*`. Everywhere else in `main.rs` the same
class of event is a hard refusal (`-j` with `-R`, `--distrust-metadata` in
OSC mode, `-v` with `-j` on diff, `--force` outside `--nodes`). Inconsistent
policy for the same event (see 5.1). **Fix:** `requires` / `conflicts_with`
in clap. **Enforceable:** yes, by clap.

---

## 4. Errors

### 4.1 Aborts on operator-controlled input in production paths
- `check/verify_ids.rs`: `slot.expect("parallel_classify_phase must deliver
  every blob")` twice. That is an internal invariant, but it sits on the
  `check` path whose job is to survive hostile files; if a malformed input
  makes the classify phase skip a slot, `check` panics instead of reporting.
  `unwrap_used` is denied workspace-wide but `expect_used` is not, so this
  shape is not caught. **Enforceable:** add `clippy::expect_used = "deny"`
  (with `#[cfg(test)]` allowance via `allow-expect-in-tests` in a
  `clippy.toml`).
- `parse_element_spec` `unreachable!()`, `getid::parse_id_spec`
  `unreachable!()` x2: currently unreachable by construction, but held only
  by the shape of the preceding branches. A `char`-typed prefix is the
  reason they exist; an enum removes them.
- `osm_id_key(i64::MIN)`: `saturating_neg` maps `i64::MIN` to `(1, i64::MAX)`,
  the same key as `-i64::MAX`. Two distinct IDs compare equal in the
  canonical order, so dedupe or merge-join over a crafted file collapses
  them. The agreement test uses `i64::MIN + 1` and so steps around it. Not
  a panic, a wrong answer on hostile input. **Enforceable:** a proptest over
  full `i64` asserting `osm_id_key` is injective.

### 4.2 Errors swallowed into default values
`owned.rs` `element_metadata` / `dense_node_metadata`: `user()` errors
(string-table corruption) become `""`, `milli_timestamp`, `changeset`,
`uid` absences become `0`, and the result is re-encoded by sort, cat
dedupe, degrade and time-filter. The doc comment says so, so it is a
decision, but it is the opposite of decision 0004's "defensive input
errors" stance elsewhere, and it is invisible to the operator. **Fix:**
propagate, or count and report "N elements had undecodable user names".
**Enforceable:** no mechanical rule; a fault-injection test with a bad
user sid already exists (`owned.rs` tests) and asserts the swallowing -
it would flip to assert the error.

### 4.3 `ExitWithCode` travels as an error through `Box<dyn Error>`
The negative-answer path is modelled as an `Err` downcast in two places
(`main`, `guarded_output`). Any wrapper that re-boxes or maps the error
(e.g. a future `.map_err(|e| format!(...))`) would turn "diff found
differences" into exit 2 with `Error: exit with code 1` printed. **Fix:**
`run_*` returns `Result<Outcome, Error>` with `Outcome::{Positive,
Negative}`. **Enforceable:** yes, by type.

### 4.4 `bench-merge` deletes the user-named output up front
`run_bench_merge` does `drop(std::fs::remove_file(output))` before running,
and is not routed through `guarded_output`. Every other command was moved
to `OutputFile` precisely so a pre-existing file at `-o` survives failure
(see `path_guard.rs` docs). `bench-merge -o important.pbf` destroys
`important.pbf` before the first byte is read, and the merge then writes
directly to the final path. The remove error is also swallowed. **Fix:**
`guarded_output` like the rest, or bench into a scratch path the command
owns. **Enforceable:** a `disallowed_methods` rule on `fs::remove_file` in
`cli/` would have caught this; a test that `bench-merge` leaves a
pre-existing output intact on failure.

### 4.5 `paths_alias` errors abort the run for non-aliasing reasons
`paths_alias` takes care to stay silent on NotFound for the input, but a
`canonicalize(output)` or `metadata` failure on an existing output (e.g.
permission denied on a parent) aborts with a bare io error that names no
path, from a check that was only meant to detect aliasing. **Fix:** add the
path to the error. **Enforceable:** covered by 3.4's wrappers.

### 4.6 `export` alias-checks twice
`validate_input_output_aliases` covers `Export { output: Some }`, and
`run_export` repeats the check with a slightly different message
("export output ... aliases input"). `run_export` also hand-rolls the
`OutputFile` dance that `guarded_output` exists for. **Fix:** route
through `guarded_output`; drop the second check. **Enforceable:** no.

---

## 5. Policy invented per call site

### 5.1 Flag-combination policy: refuse, warn, or ignore
For "this flag has no effect in this mode", the CLI does all three:
refuses (`-j` with `-R`, `--distrust-metadata` outside the scanning
report, `-v` + `-j` on diff, `inspect -j`/`--force` outside `--nodes`),
warns (`tags-filter -R -t`, `getid -t` without `-r`), and silently ignores
(`--direct-io` on `check`, on `getid -I`, `--force` on `bench-merge`
hard-wired `true`). Validation is also split between clap attributes and
hand-written `if x { return Err(...) }` ladders in `main` (sixteen such
lines across `cat`, `tags-filter`, `diff`, `check`, `inspect`). The clap
ones show up in `--help` and usage errors exit via clap's path; the
hand-written ones exit `2` with a different message format.
**Fix:** one policy (refuse), expressed in clap (`conflicts_with`,
`requires`, `ArgGroup`) or, where it depends on a runtime fact like input
kind, in one validation function per command. Consider splitting modal
commands into subcommands (`diff text` / `diff osc`, `tags-filter pbf` /
`tags-filter osc`, `cat` / `cat dedupe`) so the invalid combinations stop
existing. **Enforceable:** subcommand split makes them unrepresentable.

### 5.2 Output-publish policy implemented three ways
`guarded_output` (most commands), a hand-rolled copy in `run_export`, a
multi-file variant with preflight in `run_extract_config`, and none at all
in `bench-merge` and `build-geocode-index` (output directory, written in
place; a failed build leaves a partial index directory under the user's
name - `PathGuard::dir` exists and is not used here as far as the CLI
shows). **Fix:** an `Outputs` type that owns file, multi-file and
directory publication. **Enforceable:** if every `run_*` takes an
`&OutputSink` instead of a `&Path`, the bypass is not expressible.

### 5.3 `WriteOpts` field defaults set by hand per arm
Fourteen dispatch arms build `WriteOpts` and `HeaderOverrides` with the
same four lines; commands without `--io-uring`/`--force` write `false` by
hand. Clap `#[command(flatten)]` of one `WriteArgs` struct would produce
`WriteOpts` directly. Then the library functions themselves still take
`compression, direct_io, io_uring, force` positionally (`cat::cat`,
`repack::repack`, `degrade::degrade`, `getid::getid`, `extract::extract`,
...), so `WriteOpts` is unpacked back into the adjacent-bool shape the
struct was created to avoid, one hop later. `too_many_arguments` is denied,
and fourteen functions carry `#[allow(clippy::too_many_arguments)]` to get
round it. **Fix:** a library-level `WriteOptions` taken by every command.
**Enforceable:** yes - remove the `allow`s and let the existing lint hold.

### 5.4 Arena cap set per command (see also 6.2)
Nine commands each decide `mallopt(M_ARENA_MAX, 2)` independently, each
with its own `unsafe` block and comment; `time_filter` documents a
different rule for its two paths. Policy, per call site.

---

## 6. Ambient dependencies

### 6.1 Clock, `/proc`, nproc and the environment read from production logic
- `debug.rs` reads `BROKKR_MARKER_FIFO` lazily on first marker, from
  whichever thread emits first, and caches it in a process-global
  `OnceLock`. Tests cannot inject a sink; the protocol line is only
  testable via the private `Line` type.
- `read_page_faults` (`debug.rs`) and `read_peak_rss_kb` (`main.rs`) are
  two hand-written `/proc` parsers in two crates; `micros_since_process_start`
  is a third. All silently return zero/None on failure.
- `available_parallelism()` read directly at ~20 sites (1.1).
- `diff --format osc --update-timestamp` reads the wall clock inside the
  library (outside scope; noted because it makes the output untestable for
  equality).
**Fix:** a `Host` / `Env` value (workers, page-size, marker sink) built
once in `main` and handed to commands. **Enforceable:** `disallowed_methods`
for `std::env::var`, `available_parallelism`, `SystemTime::now` outside
the module that builds it.

### 6.2 The library mutates the process allocator
`libc::mallopt(libc::M_ARENA_MAX, 2)` is called inline, under `unsafe`, in
`check_refs`, `verify_ids` (x2), `cat`, `repack`, `degrade`, `renumber`,
`time_filter` and the geocode builder. The comment in `check_refs` says
"Scoped to this command"; it is not - `mallopt` is process-global and
never restored, so a library consumer that calls `pbfhogg::check::refs`
once has its allocator capped for the rest of its life, and in the CLI the
setting depends on which command ran. `debug.rs` meanwhile offers a
`set_mmap_threshold` wrapper that nobody calls, and no `set_arena_max`
wrapper that everybody would. `brokkr.toml` captures `MALLOC*` env vars,
suggesting `MALLOC_ARENA_MAX` was once the knob.
**Fix:** allocator tuning belongs to the binary (`main`), set once, or via
env; the library must not touch it. **Enforceable:** `disallowed_methods`
(`libc::mallopt`) in the library crate.

---

## 7. Shared state and locking

### 7.1 `writer_*` counters: global static, emitted once by convention
`main` calls `pbfhogg::write::emit_writer_counters()` once after dispatch;
the comment explains that per-flush emission double-counted. The safety
of the counters rests on "every command path passes through `main`". A
library consumer, or a test that runs two commands in one process, gets
cumulative totals across both. **Fix:** counters owned by the writer
instance and returned in stats, aggregated by the caller. **Enforceable:**
by ownership.

### 7.2 Marker FIFO `OnceLock` captures env on first use
If anything emits a marker before `main` sets up (static initialisers,
tests that set the env var after a first emission), the `None` is cached
forever. Not a bug today; a structural hazard for any test that wants to
capture markers. Covered by 6.1's fix.

### 7.3 `OutputFile` scratch name uniqueness relies on pid + process-local counter
Two processes in the same directory are distinguished by pid; fine. A
fork-without-exec child would share the counter state and pid namespace
differs - not realistic here. Noting only that the uniqueness is
structural (pid, seq) and correct; no finding.

---

## 8. Unbounded growth

### 8.1 `inspect tags` holds every distinct (key, value) pair in memory
`tags_count` accumulates a global `FxHashMap<String, FxHashMap<String,
u64>>` over the whole file, then filters by `--min-count` / `--max-count`
afterwards, then copies the key into every `TagCount`. At planet scale the
distinct-value set (names, addresses, `source:date`) is hundreds of
millions of strings; the per-blob change in the doc comment fixed the
per-worker copy but not the global one. `--min-count 1000` does not bound
memory at all. **Fix:** count-min sketch or a two-pass approach when
`min_count > 1`; at minimum a documented memory bound. **Enforceable:** an
`--alloc` brokkr bench row as a regression guard.

### 8.2 `check --show-ids` collects every missing ref
`result.missing_refs` is a Vec of every missing reference, printed or
serialised at the end. `--max-errors` (default 100) bounds `verify_ids`
and `check_index` but is not passed to `check_refs` at all
(`check_refs(path, check_relations, show_ids, direct_io)`), so on an
extract with millions of dangling refs `--show-ids` grows without bound
and `--max-errors` silently does not apply to the refs check. **Fix:**
pass `max_errors` through. **Enforceable:** by signature (options struct
shared by all three checks).

### 8.3 `getid --verbose-ids` prints every requested ID to stderr
`print_requested_ids` writes one line per kind with every ID, `-I
big.osm.pbf` makes that the whole ID space of a file. Minor, and opt-in.

---

## 9. Code that is no longer load-bearing

- `debug::malloc_trim` and `debug::set_mmap_threshold`: no caller in
  `src/`, `cli/`, or tests (searched). Their doc comments describe
  investigations ("Use to fight the documented cross-thread retention
  pattern"), not users. `debug` is a `pub` module, so an external caller is
  possible in principle; nothing in the project is one. Dead.
- `debug::emit_mallinfo2` callers carry "DIAGNOSTIC (2026-04-11 round 3)"
  provenance and fire in `extract`, `check_refs`, `time_filter` on every
  run. They are live (they emit counters) but tied to a closed
  investigation; whether brokkr still reads `MI_*` / `checkrefs_*_arena`
  needs a brokkr-side check before removal.
- `BROKKR_TEST_PLATFORM = "1"` in `brokkr.toml`'s `platform` profile:
  nothing in `tests/`, `src/` or `cli/` reads it (searched). Either dead or
  read by brokkr itself; worth confirming and either deleting or adding a
  comment saying who consumes it.
- `capture_env = ["PBFHOGG*", ...]` in `brokkr.toml`: no code reads any
  `PBFHOGG*` variable. Captures nothing.
- `IndexTypeArg::Dense` hidden value: a deliberate compatibility pointer
  for a removed mode. Fine as a decision; it has no end date. Pre-1.0 it
  could go.
- `TagSortArg` aliases `count` / `name`: compatibility with "the
  hand-written parser this replaced". Same remark.
- `commands/mod.rs`: section header "Element type filter" sits above
  passthrough-flush helpers (the type filter moved to `owned.rs`), and a
  stray doc line "Map Osmosis sentinel -1 to 0 (protobuf default for
  absent) in dense node" is fused onto `clean_metadata`'s doc comment; the
  function it described (`map_sentinel`) lives in `owned.rs` now.
- `check/verify_ids.rs`: `#[allow(clippy::too_many_lines)]` sits above the
  doc comment of `warn_unindexed_triple_scan` (a 10-line function), not on
  `verify_ids_streaming_parallel` where it was presumably meant. The allow
  is dead where it is.
- `getid::parse_ids` and `parse_ids_from_file` (no-default-type wrappers):
  the CLI uses the `_with_default_type` variants; check tests before
  deleting.
- `pbfhogg::commands::getparents::getparents` vs `getparents_with_min_blobs`
  (see 2.4).
- `src/lib.rs` crate doc says `pbfhogg = "0.4"`; the crate is 0.5. Reword
  to not carry a version.
- `Cargo.toml` `test-hooks` comment says it is "Turned on by `brokkr check`
  via `--all-features`"; `brokkr.toml` turns it on with an explicit feature
  list and its own comment says `--all-features` is not used.
- `ExitWithCode`'s `Display` ("exit with code N") is never shown on any
  intended path; it exists only to satisfy `Error`. Goes away with 4.3.

---

## 10. Host assumptions and writes outside owned locations

### 10.1 `bench-write` writes to `/dev/null` by absolute path
Both writer modes open `/dev/null`. On a non-Unix host this fails with a
bare io error. `PbfWriter::to_path` on `/dev/null` also exercises the
pipelined writer against a character device (no `pwrite` offsets, no
fsync semantics), so the "pipelined" number does not measure the real
file writer path. **Fix:** `std::io::sink()` for sync; a scratch file in
a caller-supplied directory for pipelined. **Enforceable:** text rule on
`"/dev/null"` under `src/` and `cli/src/`.

### 10.2 `/proc` parsing assumed Linux and glibc
`debug.rs` gates on `target_os = "linux"` for `mallinfo2` / `malloc_trim` /
`mallopt`, which are glibc functions; on musl Linux these are absent or
no-ops, and `libc::mallinfo2` will fail to link (musl does not provide
it). The same applies to the nine inline `mallopt` calls. **Fix:** gate on
`target_env = "gnu"`. **Enforceable:** a `--target x86_64-unknown-linux-musl`
build in a sweep.

### 10.3 Scratch files dropped beside the user's output
`OutputFile` writes `.pbfhogg-tmp-<pid>-<n>.<name>` into the destination
directory. That is the correct place for an atomic rename, and the choice
is documented. The consequence is that a SIGKILL or OOM-kill (common at
planet scale) leaves a hidden multi-GB file in the user's directory that
nothing ever cleans up, because `Drop` does not run. **Fix:** on startup,
or in the `OutputFile` constructor, report (not delete) stale
`.pbfhogg-tmp-*` siblings whose pid is not alive. **Enforceable:** test.

### 10.4 Tests write into `target/`
`path_guard.rs` tests (and `merge_changes`, `direct_reader` tests) create
scratch under `CARGO_MANIFEST_DIR/target`. `target/` belongs to cargo;
`cargo clean` destroys it mid-run, and brokkr's per-worktree
`CARGO_TARGET_DIR` means these tests write into the main tree's `target/`
even when building elsewhere. The comment explains the reason (avoid
`/tmp`, keep scratch in the repo). **Fix:** a dedicated, git-ignored
`.scratch/` (or the brokkr scratch dir, exported as an env var by the
sweep) owned by the test harness. **Enforceable:** text rule on
`join("target")` under `src/` and `tests/`.

### 10.5 `inspect` / `check` fall back to invented names
`run_check` uses `"input.osm.pbf"` when the path has no UTF-8 file name and
emits that into JSON output as the file name - a wrong answer rather than
a lossy one. `inspect`'s header path uses `to_string_lossy` instead. Two
policies. **Fix:** `to_string_lossy` everywhere.

### 10.6 `detect_input_kind` heuristics
`tags-filter` sniffs the first 4 bytes: gzip means OSC, `<` means OSC, a
BE u32 in 1..1000 means PBF, otherwise falls back to extension. A gzipped
non-OSC file, or an OSC with a BOM or leading whitespace, takes the
extension fallback, and an unknown extension defaults to PBF. Note the
fallback list includes `.osc.bz2`, which the OSC reader does not support
(no bzip2 dependency), so a `.osc.bz2` input is detected as OSC and then
fails inside the parser with whatever the XML reader says about bzip2
bytes. **Fix:** refuse ambiguous input with a message naming
`--input-kind`; drop `.bz2`. **Enforceable:** test.

---

## Lateral findings

- `--max-errors` default 100 is documented as "per check" but `check_refs`
  never receives it (8.2) - user-visible.
- `diff -q` `conflicts_with = "output"` but the `--format osc` arm requires
  `--output` and separately rejects `--quiet` by hand; the clap conflict
  already handles that, so the hand-written check is unreachable.
- `run_extract`'s `(Some, Some)` and `(None, None)` region arms are
  unreachable: clap's `group = "area"` plus `required_unless_present`
  already enforce exactly-one... except `area` is not marked `required`, so
  `(None, None)` is reachable and handled at run time instead of by clap.
  Mark the group required.
- `Inspect` has `file: Option<PathBuf>` with `required = true` plus
  `subcommand_negates_reqs`, then `file.ok_or("Input PBF file is
  required")` - a second required check that clap already does.
- `inspect tags` takes its own `file` positional, so `pbfhogg inspect
  x.pbf tags y.pbf` parses with two different files and silently uses the
  second.
- `has_indexdata` trusts the first data blob; `inspect --indexed` reports
  "yes" for a file whose head is indexed and tail is not. The doc says so;
  the CLI help ("Check if PBF has blob-level indexdata") does not.
- `bench_write_loop` turns relation member role decode errors into `""`
  (`m.role().ok().unwrap_or_default()`).
- `lib.rs`: `pub mod debug` makes the brokkr FIFO protocol, `/proc`
  readers and allocator pokes part of the public API surface. Decision 0012
  keeps CLI bridges `doc(hidden)`; `debug` is not even hidden.
- `decisions/` is a fifth document folder not covered by the `reference/`
  / `docs/` / `notes/` layout in AGENTS.md, and source cites it
  (`path_guard.rs`). It needs a place in that table.
