# Hygiene hunt: filtering and spatial

Scope: `src/commands/extract/`, `src/commands/export/`, `src/commands/spatial.rs`,
`src/geo.rs`, `src/commands/tags_filter/` (PBF and OSC), `src/tag_expr.rs`,
`src/commands/time_filter/`, `src/commands/getid/`, `src/commands/getparents/`,
plus how the CLI (`cli/src/main.rs`) drives them.

Existing enforcement worth knowing before reading the "enforceable by" lines:
the workspace clippy table in `Cargo.toml` denies `unwrap_used`,
`cognitive_complexity`, `too_many_arguments`, `let_underscore_must_use`,
`unwrap_in_result`, `float_cmp` and friends. It does not deny `expect_used`,
`print_stderr`, `print_stdout`, `panic` or `unreachable`, and there is no
`clippy.toml`, so `disallowed_methods` / `disallowed_types` are unused. The
`minimal` / `consumer` / `all` sweeps in `brokkr.toml` enforce feature gating.
Every "a lint could hold this" below means one of those unused levers.

---

## 1. One value, one owner

### 1.1 Decode-thread count formula, six hand copies in scope

`available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(4)`
is spelled at `extract/common.rs` (pread_execute), `extract/simple.rs` (twice),
`extract/multi.rs` (twice) and `tags_filter/mod.rs` (pass-2 worker pool). The
same file set has other rules for the same question that already disagree:

- `tags_filter` `jobs: None` documents "rayon default (`available_parallelism()`)"
  - no minus-two - for the classify passes of the very same command.
- `cli/src/main.rs` `run_diff` / `run_derive_changes` resolve `jobs == 0` to
  `available_parallelism().unwrap_or(1)` - fallback 1, not 4, no minus-two.
- `scan/classify.rs` (the shared helper these commands also call) has its own.

So one `tags-filter` run uses two different thread-count rules in two passes,
and the `--jobs` knob reaches only one of them. Fallback-on-error is 4 here, 1
there: on a host where `available_parallelism` fails (restricted cgroup /
`/proc` not mounted) the commands disagree by 4x.

Fix: one `fn decode_threads(override: Option<usize>) -> usize` in the read layer,
taking the CLI `--jobs` everywhere. Enforceable: `disallowed_methods =
["std::thread::available_parallelism"]` in a `clippy.toml`, with a single
`#[allow]` at the owner.

### 1.2 Hand-rolled worker pool, five copies of the shape and its constants

`extract/common.rs::pread_execute`, `extract/simple.rs` (node and way classify),
`extract/multi.rs` (node write, way/relation write) and `tags_filter/mod.rs`
pass 2 each build the same machine: descriptor `sync_channel(16)`, result
`sync_channel(32)`, `Arc<Mutex<Receiver>>` shared by N workers, per-worker
`DecompressPool` + `BlockBuilder` + scratch, `pool_get_pub(.., data_size * 4)`
(the `* 4` decompression-size guess appears four times in scope),
`ReorderBuffer::with_capacity(32)`, and a consumer loop that is duplicated a
second time inside the same function for the post-drain tail (see
`pread_execute`: the `ConsumerItem` match body is written twice verbatim).
`scan/classify.rs` already owns `parallel_classify_phase` /
`parallel_classify_phase_abortable` with the same 16/32 constants - a third
owner of the numbers. `getparents` and `getid --id-osm-file` use the shared
helper; extract and tags-filter do not.

What has already diverged between the copies:
- `scan/classify.rs` and `getparents` walker check
  `data_offset + data_size > file_size` with a named error; the extract and
  tags-filter schedule scans do not, so a truncated file surfaces there as a
  bare `UnexpectedEof` from `read_exact_at` with no offset (see 4.2).
- `getparents` stops the scan on first error (`ControlFlow::Break`, with a
  comment explaining why this matters at planet scale); `pread_execute`
  returns `r?` from the consumer, which drops the receiver and lets the
  workers exit at their next send - fine - but the multi and tags-filter
  copies each made their own choice and nobody can tell from one site what the
  others do.
- Abort-on-error, cancellation and `*_wait_ns` stall counters (the brokkr
  convention in AGENTS.md) exist on none of these hand pools, so `--stalls`
  is blind to extract and tags-filter pass 2.

Recommendation (big rewrite, worth it): one "pread-decode-reorder-consume"
engine in `scan/` that takes a schedule of `Decode | Passthrough` items, a
per-worker state factory, a block fn returning `crate::Result`, and a consumer
returning `ControlFlow`. Every command in scope becomes a closure pair.
Enforceable: `disallowed_methods` on `std::sync::mpsc::sync_channel` outside
`src/scan/` and `src/read/`, or a text rule in `brokkr check` that greps
`Arc::new(std::sync::Mutex::new(` under `src/commands/`.

### 1.3 Decimicrodegree scale: `E7` is private, and `1e-7` is spelled loose

`commands/spatial.rs` defines `const E7: f64 = 1e-7` with a long, correct
explanation that every boundary conversion must match the forward expression
exactly - then keeps it private. In scope, `extract/mod.rs`
(`contains_decimicro`, twice), `export/mod.rs` (Node and DenseNode arms) and
`geo.rs::e7_to_rad` each spell `as f64 * 1e-7` themselves; `coord_fmt.rs` uses
`/ 1e7`, which is exactly the "division is not the inverse" form the
spatial.rs comment warns about, and the geocode builder spells it a dozen more
times (outside scope). The `BboxInt::inside_bbox` exactness guarantee is only
true while every caller's conversion is bit-identical to `to_degrees`; nothing
holds that.

Fix: a `pub(crate) fn e7_to_deg(i32) -> f64` (and the matching `deg_to_e7_*`)
in one place - `geo.rs` is the natural home - used by extract, export, the
blob filters and coord_fmt. Enforceable: a text rule in a `brokkr check` sweep
rejecting `1e-7` / `1e7` literals outside that file; a newtype `E7(i32)` with a
`degrees()` method would make the bad spelling unrepresentable.

### 1.4 Coordinate domain validation written twice with different wording

`extract::parse_bbox` and `extract::parse_ring` each implement "finite and in
[-90, 90] / [-180, 180]". `xml_parse::coord_to_decimicro` (OSC, used by
`tags_filter/osc.rs`) is a third. The messages already diverge ("min_lat (x)
must be within..." vs "polygon latitude (x) must be within..."), and only the
bbox variant names which field. Fix: `fn validate_lat(name, v)` /
`validate_lon` in `spatial.rs`. Testable, not lintable.

### 1.5 The multi-extract config bbox round-trips through a string

`parse_extract_geometry` reads four `f64`s, `format!`s them into
`"a,b,c,d"`, and re-parses via `parse_bbox`. It works only because Rust's
float `Display` is shortest-round-trip; it also re-labels errors through the
string form. Fix: split `parse_bbox` into `parse_bbox_str` and
`Bbox::try_new(min_lon, min_lat, max_lon, max_lat)` and call the latter.

### 1.6 Tunables scattered where first needed

There is no answer to "what are the tunables of extract / tags-filter?". Found
in scope, each defined once at the use site:

- `extract/mod.rs::parse_extract_config`: `500` max extracts, literal, twice in
  one line (check and message). Undocumented in `reference/cli-reference.md`.
- `extract/multi.rs`: `const GRID_REGION_THRESHOLD: usize = 16` declared inside
  a function body.
- `extract/simple.rs`: `const BATCH_SIZE: usize = 64`.
- Channel depths 16 / 32 and reorder capacity 32 / 64 (see 1.2).
- `pool_get_pub(.., data_size * 4)` (see 1.2).
- `read/header_walker.rs::FULL_SCAN_ARM_MIN_BLOBS = 150_000` - this one is done
  right: one owner, and `getid` / `getparents` take it as an injected
  `min_blobs` (`getparents_with_min_blobs`) so tests can drive both arms. It is
  the in-repo precedent for every other item in this list.
- `geo.rs`: degenerate-segment epsilon `1e-30` spelled twice in production
  (`point_to_segment_distance_sq`, `simplify_ring`) and re-spelled in a test
  that reimplements the formula instead of calling it.

Fix: one `tuning` module (or per-command `Tuning` struct with `Default`) passed
in, the way `FULL_SCAN_ARM_MIN_BLOBS` is. Not lint-enforceable in general; a
text rule rejecting `const` items declared inside `fn` bodies under
`src/commands/` would catch the worst class.

### 1.7 "Element matches any expression" implemented four times

`tags_filter/mod.rs::element_matches` (bool triple), `tags_count`
(bool triple, same body), `tags_filter/osc.rs::matches_any` (a `char` 'n'/'w'/'r'
with a `_ => false` arm), and `export/mod.rs::matches_expressions` (a
`TypeFilter` used as the element kind, with "admitted" written as
`!kind.x || expr.x` conjunctions - a different formula that only agrees
because the callers pass single-kind filters). `tag_expr.rs` owns parsing and
`tag_matches` but not the any-expression/any-tag loop, so each consumer
re-decides the type-gate semantics. Fix: `Expression::admits(kind:
ElementKind)` and `fn any_matches(exprs, kind, tags: impl Iterator)` in
`tag_expr.rs`, with an `ElementKind` enum instead of chars or bool triples.
Enforceable by visibility: make `Expression::type_filter` and `matcher`
private to `tag_expr`, so no consumer can reimplement the loop.

### 1.8 Three ad hoc "element kind" encodings in scope

`char` 'n'/'w'/'r' (`getid::parse_id_spec`, then `unreachable!()` twice on the
fourth case; `osc.rs::matches_any`), bool triples (`element_matches`),
`time_filter::ElementKind` (a private enum), `blob_meta::ElemKind`,
`osc::xml_parse::ElemKind`, and `TypeFilter` used as a kind in export. A single
crate-level `ElementKind` would delete both `unreachable!()`s in `getid` (the
type proves the match exhaustive) and the `_ => false` in osc.rs.
Enforceable by the type.

### 1.9 List-file comment rules differ between two list formats

`getid::parse_ids_from_file_with_default_type`: `#` only at line start, `trim()`.
`tag_expr::read_expressions_file`: inline `#` stripped anywhere, `trim()`.
Two "one item per line with comments" readers, two rules. Worse, `trim()` in
`read_expressions_file` strips tabs, while `tag_expr::strip_spaces` documents at
length that osmium strips spaces only and "a tab stays part of the key there
and stays part of the key here". A tab-led key from an expressions file is
therefore stripped, contradicting the parity claim the module makes. Fix: one
`read_list_file` helper with the comment rule decided once, and `trim_matches(' ')`
plus `\r` only in the expressions path. Testable against osmium in
`brokkr verify tags-filter`.

---

## 2. Values nobody can find, change, or trust

- **Polygon antimeridian status is recomputed per point.**
  `extract::polygon_rings_contains` calls
  `geo::point_in_ring_with_antimeridian` for every candidate node, which
  re-runs `ring_crosses_antimeridian` (O(ring)) and, for crossing rings,
  allocates a fresh unwrapped `Vec` per point (`unwrap_ring_longitudes`).
  The function comment says "calls geo primitives directly to avoid per-point
  allocation" - true only for non-crossing rings. A property of the region is
  being treated as a property of the query. Fix: precompute a `PreparedRing
  { points, crosses, unwrapped }` once at `parse_geojson` time; the
  geocode reader (`point_in_polygon`) benefits equally. Not lintable; a
  benchmark on a Fiji/Chukotka polygon would show it.
- **`extract_multi` relative `directory` is CWD-relative, everything else is
  config-relative.** In `parse_extract_config`, `polygon_file` resolves against
  `config_dir`, `output` against `directory.unwrap_or(config_dir)`, but a
  relative `"directory"` string is used as-is, i.e. relative to the process
  working directory. The same config produces different output locations
  depending on where it is run from. Fix: join a relative `directory` onto
  `config_dir`. Testable.
- **The `-d` override silently ignores slots it cannot re-root.**
  `run_extract_config` rewrites `slot.output` only `if let Ok(relative) =
  slot.output.strip_prefix(old_dir)`; an absolute `output` in the config (or
  any path that does not strip) stays where the config put it with no message.
  The override is re-derived after the fact rather than passed into
  `parse_extract_config` as the resolve root. Fix: pass the override in and
  resolve once.
- **Config read at the moment of use.** The multi-extract config's
  `polygon_file`s are read eagerly at parse time (good). But in `tags-filter`
  the CLI rejects `--jobs` with `-R` up front (good, per the doc on
  `TagsFilterOptions::jobs`) while the library silently ignores it - a library
  caller gets no refusal. Same for `_direct_io` on
  `getid::parse_ids_from_pbf` (see 9) and `direct_io` on extract's pread
  passes (see 10).

---

## 3. One channel, one implementation

### 3.1 Every command prints its own summary with `eprintln!`

`ExtractStats::print_summary`, `TagsFilterStats::print_summary`,
`TagsFilterOscStats::print_summary`, `GetidStats::print_summary`,
`GetparentsStats::print_summary`, `TimeFilterStats::print_summary`,
`ExportStats::print_summary` all live in the *library* and write to stderr
directly. Mid-run progress lines are also library `eprintln!`s:
`[multi-extract] single-pass: ...`, `  node blobs: N passthrough, M decoded`,
`[getid] N blobs skipped ...`, `[getid --invert] ...`,
`[tags-filter] N blobs skipped by tag index`, `[tags-filter] pass 2: ...`,
`[N/M] Extracting to ...`. A library consumer (`default-features = false`)
cannot silence them, redirect them, or get them as data; and the bracketed
prefix convention is applied to some lines and not others. The CLI already
owns operator-facing text in places (`export_error` in `main.rs` explicitly
says "a Display impl in a library has no idea which front end is reading it")
- the same reasoning applies to every `print_summary`.

Fix: stats structs stay in the library with `Display` (or a `summary_lines()`),
printing moves to the CLI; progress goes through one `log`-style channel the
CLI configures. Enforceable: `print_stderr = "deny"` / `print_stdout = "deny"`
at the library crate with an `#[allow]` only in `debug.rs`; the CLI crate
keeps them.

### 3.2 Multi-extract prints scratch file names as if they were the outputs (bug)

`run_extract_config` redirects every `slot.output` to its `OutputFile` scratch
sibling (`.pbfhogg-tmp-<pid>-<seq>.<name>`, per `path_guard.rs`) before calling
`extract_multi`. Inside the library, both reporting paths print `slot.output`:
`extract/mod.rs` "`[i/N] Extracting to {}`" (sequential fallback) and
`extract/multi.rs` "`  [i] {name}: N elements (...)`" (single-pass), which uses
`slot.output.file_name()`. The operator sees dot-prefixed temporary names that
no longer exist after commit. The CLI then prints a second, correct summary
from `final_paths`, so single-pass runs report every region twice under two
names. Fix falls out of 3.1 (library stops printing); short of that, carry a
display name in `ExtractSlot`. Testable via `CliInvoker` stderr assertion.

### 3.3 Silent fallbacks where something significant happens

- `try_extract_multi_single_pass` returns `Ok(None)` - and the run silently
  falls back to N sequential full passes, an N-fold slowdown - on unsorted
  input, on a missing header, and on any `BlobReader` read error
  (`_ => return Ok(None)`). None of these is logged. The error case is also a
  swallow (see 4.1).
- `RegionGrid::build(..)` returning `None` (over budget) is `.flatten()`ed into
  the linear classifier with no message or counter.
- `--remove-tags` without `--add-referenced` is a CLI warning
  (`"Warning! ..."`); the library accepts the meaningless combination silently.

### 3.4 Observed counts mixed with indexdata claims in extract

`getid` deliberately separates `passthrough_claimed` from `*_written`
(its doc explains why: a lying indexdata would move the headline number with
no way to tell). `extract/common.rs::pread_execute` does the opposite:
`stats.nodes_in_bbox += count` for raw-passthrough blobs, where `count` is the
blob's `indexdata` claim. `multi.rs` does the same for its passthrough path.
Same class of number, two policies (see 5). Fix: adopt getid's split in
`ExtractStats`.

---

## 4. Errors

### 4.1 Swallowed

- `try_extract_multi_single_pass`: `match br.next() { Some(Ok(blob)) => ..., _
  => return Ok(None) }` turns an I/O or decode error on the first blob into
  "fall back to sequential". Sequential will probably fail again, but with a
  different error from a different site, and only after the fallback.
- `extract/common.rs` / `multi.rs` / `tags_filter` schedule scans:
  `let _ = walker.next_header()?.ok_or_else(...)?` - fine (it is only
  discarding the header metadata), but in `multi.rs` the header is read twice,
  once via `BlobReader` and again via `HeaderWalker`, and the first one is the
  one that decides sortedness. Two readers of the same fact.

### 4.2 Context shed

- **String errors through the block fn.** `pread_execute`'s `block_fn` returns
  `Result<_, String>`; the worker re-wraps it as `ErrorKind::Io(io::Error::other(e))`,
  and `flush_local` errors are stringified the same way. A wire-encoding or
  block-builder failure reaches the operator labelled as an I/O error with no
  blob sequence or offset. `getparents` does the same (`ClassifyResult =
  Result<_, String>`). The `(s, data_offset, data_size)` the worker holds at
  that moment is exactly the context that would make it actionable, and it is
  dropped.
- **Truncated input without the offset.** As in 1.2, extract and tags-filter
  workers `read_exact_at` without the bounds check that `scan/classify.rs` and
  `getparents` perform, so a short file reports a bare `UnexpectedEof`.
- **`export` options errors are re-typed.** `ExportOptions::new` wraps tag
  expression and bbox parse errors in `io::Error(InvalidInput, e.to_string())`.
  A user input error is classed as I/O in `ErrorKind`, so the CLI's exit-code
  mapping (if it distinguishes I/O) treats it as an environment failure.
- `parse_geojson` / `parse_extract_config`: `std::fs::read_to_string(path)?`
  and `serde_json::from_str(&data)?` propagate errors that do not name the
  file. A multi-extract config with 30 `polygon_file`s that fails to parse one
  says "expected value at line 1 column 1" and nothing else.
  `read_expressions_file` names its path; these two do not. Same policy,
  invented per site (see 5).

### 4.3 Aborts on input the caller controls

- `geo::assemble_rings` uses `.expect("nonempty ring")` / `.expect("search is
  nonempty")` in production. Both are true by construction today (ring is
  seeded from a non-empty segment, `search` checked by the loop condition), so
  this is not reachable - but `expect_used` is not denied, so nothing would
  stop a future edit making it reachable from OSM data. Rewriting with
  `let Some(..) else` costs nothing.
- `getid` `unreachable!()` x2 on the char prefix: unreachable by construction,
  but only because the char is an untyped encoding (1.8).

---

## 5. Policy invented per call site

- **Indexdata requirement.** `getid` and `removeid` refuse without indexdata
  unless `--force` (with two different hand-written messages); `getparents`
  calls `has_indexdata` only to pick an arm and never refuses; `getid
  --add-referenced` calls `require_indexdata` and then ignores the result;
  extract refuses for complete/smart only (per AGENTS.md list). Whether a
  missing index is an error, a warning or nothing is decided per command, and
  the AGENTS.md list of "commands requiring indexdata" is a hand-maintained
  copy of the decision. Fix: a declarative per-command policy table read by
  both the commands and the docs.
- **Error context for files** (4.2): some readers name the path, some do not.
- **Passthrough element accounting** (3.4).
- **Output-file guarding.** `cli/src/main.rs::guarded_output` claims "Every
  user-named `--output` goes through here"; `run_export` opens its own
  `OutputFile` by hand, `run_extract_config` builds a guard vector by hand, and
  `run_diff` (outside scope, flagged) writes `File::create(path)` directly with
  no guard - contradicting the claim. Within the library,
  `tags_filter/osc.rs::write_filtered_osc` and `multi.rs` call
  `File::create` on whatever path they are given, so a library caller gets
  truncate-then-fail semantics. Fix: commands take an already-guarded sink,
  never a `&Path` they create. Enforceable: `disallowed_methods =
  ["std::fs::File::create"]` under `src/commands/` with exemptions for the
  writer layer.
- **"Is this blob kind needed?"** is decided at least four ways in scope:
  `BlobFilter` (tags-filter, getparents pipelined), `needed_blob_kinds`
  (getparents walker), inline `match meta.index.kind` in multi.rs and
  extract/common.rs, and `spatial_blob_filter` (multi). Unindexed blobs are
  "include conservatively" in each, written each time.

---

## 6. Ambient dependencies

- `available_parallelism()` read directly from production logic in six places
  (1.1), with no injection point; tests cannot force the single-worker or
  many-worker interleavings of the reorder buffer.
- Process working directory: relative config `directory` (2), and
  `Path::new(".")` fallbacks for `config_path.parent()`.
- `std::process::id()` in scratch names (path_guard, outside scope) - fine.
- `time_filter` takes the cutoff as a parameter; no clock reads in scope. Good.

---

## 7. Shared state and locking

- `Arc<Mutex<Receiver>>` work queues (1.2): the mutex is held across a
  blocking `recv()`. Every idle worker parks on the mutex, not the channel,
  and wake order is whatever the futex gives. It is correct, but it is the
  pattern `crossbeam_channel` / a rayon scope removes, and it is copied five
  times. It also poisons silently (`unwrap_or_else(PoisonError::into_inner)`),
  so a panicking worker's partially consumed state is ignored rather than
  reported.
- `pread_execute` pre-inserts every passthrough item into the
  `ReorderBuffer` before any worker result arrives. Ordering correctness rests
  on the reorder buffer accepting an arbitrary number of out-of-window items;
  if `with_capacity(32)` ever became a hard ceiling (the "explicit ceiling"
  variant already exists in `reorder_buffer.rs`), this path would break on any
  input with more than 32 passthrough blobs. The invariant is not structural.

---

## 8. Unbounded growth

- **`geo::assemble_rings` is an unbounded backtracking search.** Each ring start
  runs a DFS over all endpoint candidates with full backtracking
  (`used[segment_idx] = false; ring.truncate(..)`). On a relation with many
  segments sharing endpoints (a grid-like admin boundary, or adversarial
  data), the work is exponential in the number of branching endpoints, and the
  `endpoint_map.get(..).cloned()` clones a candidate `Vec` per frame. The input
  is OSM relations (geocode builder), i.e. not under the operator's control.
  No step budget, no counter, no warning. Fix: a step cap with a logged
  give-up, or a deterministic non-backtracking assembler (osmium's assembler is
  in `research/libosmium/` as the reference).
- `tags_filter/osc.rs` holds the entire filtered change set in nine `Vec`s
  before writing. Bounded by input size, but planet-day OSCs can be large and
  there is no reason to buffer creates before modifies other than the
  section-grouped output. A streaming writer with three section spools would
  bound it.
- `extract::parse_geojson` / config / `getid` ID files: `read_to_string` of
  arbitrary size. Acceptable for operator inputs; flagged for completeness.
- `print_requested_ids` / `print_missing_ids` (CLI) print one id per line with
  no cap - a `-I planet.pbf` `--verbose-ids` run writes billions of lines.
- `multi.rs` keeps `3 * N` `IdSet`s (up to 500 regions, 1.6) with no memory
  estimate or refusal.

---

## 9. Code that is no longer load-bearing

- `getid::parse_ids_from_pbf(path, _direct_io: bool)`: the parameter is ignored
  (underscored), yet the CLI threads `write.direct_io` into it. Evidence: the
  body calls `build_classify_schedule(path, None)` which never sees it. Either
  wire it or delete it; today `--direct-io` silently does nothing on `-I`.
- `extract/common.rs::pread_execute`: `let _total_blobs = schedule.len();` -
  dead binding.
- `getparents_with_min_blobs` is `#[doc(hidden)] pub` "test instrument", yet
  the CLI calls it when `full_scan_min_blobs` is set, so it is a hidden
  production flag, not test-only. Either document the flag or move the
  injection into an options struct.
- `extract/mod.rs`: `use crate::geo::ring_crosses_antimeridian;` with the
  comment "used by tests and polygon_bbox_f64" - there is no
  `polygon_bbox_f64` in the file any more (the function is `bbox_from_polygons`).
  Stale comment.
- `#[allow(clippy::too_many_arguments)]` on roughly fifteen functions in scope
  (every extract strategy, every getid/getparents entry and dispatcher,
  `extract_multi`, CLI runners). The workspace denies the lint; the allows are
  the evidence that the `(input, output, compression, direct_io, force,
  overrides)` bundle wants to be one `RunContext` struct. A lint somebody paid
  for is being opted out of site by site.
- `extract/common.rs` comment cites "TODO.md history 2026-04-19"; `getid`
  cites "ADR-0006" (which lives under `decisions/`, so that one is fine). Code
  citing `TODO.md` is citing a transient document from durable source,
  against the AGENTS.md folder rule.

---

## 10. Host and location assumptions

- **`direct_io` silently partial.** `extract`'s pread passes open
  `File::open(input)` buffered regardless of `--direct-io` (comment explains
  the alignment reason); `multi.rs` writes through `PbfWriter::new(BufWriter)`
  regardless; `export` has no I/O flag and uses `ElementReader::from_path`.
  On a host where the operator chose O_DIRECT to protect page cache, these
  passes still fill it, with nothing logged. A flag that is honoured on some
  passes of a command and not others should either refuse or say so.
- **`use std::os::unix::fs::FileExt`** in `pread_execute` and the multi
  workers: the commands compile only on Unix. Not gated or documented as such
  at the module level.
- **`tag_expr.rs` test writes a fixed name under the system temp dir**
  (`std::env::temp_dir().join("pbfhogg_test_expr")`, then `remove_dir_all`).
  With the parallel test sweeps added in `b353de8`, two sweeps running this
  test at once share and delete each other's directory; it also writes outside
  the project, contrary to the project's no-`/tmp` rule. Every other test in
  scope uses `tempfile::tempdir()`. Fix: same. Enforceable:
  `disallowed_methods = ["std::env::temp_dir"]`.
- **Scratch output siblings** (path_guard) are written into the user's output
  directory as dot files - owned by the destination, correct choice, and
  cleaned on drop. But library entry points (`extract`, `tags_filter_osc`,
  `multi`) write wherever they are pointed with plain `File::create`, so a
  library caller gets none of that (5).
- Core count: see 1.1. A 2-core host gets 1 decode worker on extract (n - 2,
  min 1) but 2 on diff; an 8-core cgroup-limited container reporting host
  cores gets far more threads than it can run. Neither is detected or logged.

---

## Other things noticed

- `extract::Bbox` lives in the extract command, and `commands/spatial.rs`
  (the declared shared owner of spatial primitives) imports it from there,
  while `geo.rs` (library-level) owns ring math. Three homes for one subject;
  `Bbox`, `BboxInt`, the E7 conversions and prepared polygons belong together
  (in `geo.rs`, below the commands).
- `extract::polygon_rings_contains` and `geo::point_in_polygon` are the same
  function with different hole-slice types; the extract copy exists to avoid
  the `&[&[..]]` allocation. Changing the geo signature to take
  `impl IntoIterator<Item = &[(f64, f64)]>` deletes the copy.
- `bbox_from_polygons` widens to the full -180..180 for an antimeridian ring,
  which defeats blob-level spatial skip for that region entirely (correct, but
  silently a near-full scan). A split pair of bboxes would keep the skip.
- `parse_ring` accepts rings of any length, including 0-2 points and unclosed
  rings; `point_in_ring` then returns `false` for them. A polygon file with a
  degenerate exterior yields an empty extract with exit 0 - the exact
  "wrong answer reported as success" failure that `parse_bbox`'s comment
  says was fixed for bboxes.
- `export` builds a way's full geometry string before applying the bbox test,
  then discards it; it also reports `skipped_invalid_ways` for ways that would
  have been rejected by bbox anyway, so the counter depends on filter order.
