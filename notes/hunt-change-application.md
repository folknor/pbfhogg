# Hygiene hunt: change application

Scope: `src/osc/*`, `src/commands/apply_changes/*`, `src/commands/merge_changes/`.
Read against HEAD `955664d`. Findings are not ranked. Each carries an
"Enforce" line saying whether the fixed shape can be held mechanically.

Existing enforcement worth knowing before reading the "Enforce" lines: the
workspace clippy table in `Cargo.toml` denies `unwrap_used`,
`too_many_arguments`, `too_many_lines`, `cognitive_complexity` and the cast
lints, but there is **no `clippy.toml`**, so `disallowed-methods`,
`disallowed-types` and `disallowed-macros` are unused, and the restriction
lints `print_stderr` / `print_stdout` are not on. Several fixes below are
one `clippy.toml` entry each. Also: `too_many_arguments` is denied but
`#[allow]`ed at `worker_loop` (15 parameters), `handle_candidate` (20),
`emit_create_for_output`, `flush_remaining_upserts`, `emit_gap_creates`,
`handle_type_transition`, `process_item`, `dispatch_variant` and
`emit_create_local`. A denied lint with nine local exemptions in one module
does not enforce anything there.

---

## Bugs and surprises found on the way (flagged first because they matter most)

### B1. `--locations-on-ways` does not update coordinates on unchanged base ways

`reference/cli-reference.md` documents `--locations-on-ways` as "Preserve and
update way-node locations through the merge". Only ways that the OSC itself
touches get fresh coordinates:

- `NodeLocationIndex::build_from_diff` puts only the refs of **OSC ways** into
  `needed_set` / `locations`.
- `rewrite_block_parallel` writes a surviving base way through
  `write_base_way_local_with_locations`, which copies the old `lat_data` /
  `lon_data` bytes unchanged.
- Way blobs with no diff overlap take the splice fast path (scanner comment:
  "Way/Relation blobs are still eligible for the splice fast-path under LOW").

So if the OSC moves node N and does not touch way W, which references N, W
goes out with N's old position. A delete of N leaves W carrying N's last
coordinate. Neither case is detected or counted, and `loc_missing` cannot see
it because it only counts refs from OSC ways. osmium's `apply-changes -l`
works out locations again from the merged node set. `reference/pipeline.md`
says the production loop no longer uses `--locations-on-ways` (altw runs
again after each merge), which is probably why nobody has hit this, but the
flag is still shipped and documented as updating. Fixing it means building
the set of base ways that reference any modified or deleted node, which needs
a reverse index or a full way pass. Then those way blobs have to go through
the rewrite path instead of the splice path. Otherwise the doc has to say
"OSC ways only".

Enforce: a cross-validation test against `osmium apply-changes -l` on a diff
that moves a node referenced only by an untouched way. A unit-sized fixture
is enough. No lint can see this.

### B2. Under LOW, a way-phase rewrite with no published loc_map silently strips LocationsOnWays

`handle_candidate` reads `loc_map_handle.and_then(|h| h.get().cloned())`. If
that is `None` (barrier protocol broken, or a future routing change),
`rewrite_block_parallel` falls through to `write_base_way_local` and drops
inline coords from every base way in the blob. The `--force` guard in
`rewrite.rs` exists because exactly this silent strip was reachable. The
guard closes one route to it but leaves the fallback in place. With
`locations_on_ways == true` and a non-node kind, a missing loc_map should be
an error.

Enforce: give the rewrite a `LocMode` enum (`Off` / `Required(&Map)`) instead
of `Option<&Map>`, so "LOW on but no map" cannot be represented.

### B3. Node-coordinate extraction errors are swallowed and come back with no cause

`streaming.rs` `handle_candidate`: `extract_node_tuples(...).is_ok()`. On
`Err`, which the comment says happens for non-dense `Node` messages, the
whole blob's coordinates are dropped and nothing is logged. The fallout shows
up later as `cannot write LocationsOnWays: N referenced node(s) could not be
resolved`. That message names no blob, no node id and no cause, and points
the operator at `--ignore-missing-nodes`. With that flag the result is a
silent `(0,0)`. The comment claims it matches `node_locations.rs` prefill
semantics, but that prefill no longer exists (`node_locations.rs` header).
The precedent being cited is gone.

Enforce: make the extractor handle plain `Node` groups, or fail with the blob
seq and offset. A test with a non-dense node blob under LOW would pin it.

### B4. `loc_node_blobs_scanned` is not what it says

`rewrite.rs` sets `final_stats.loc_node_blobs_scanned =
worker_counters.blobs_processed`. That counter is bumped for every candidate
and every owned passthrough of every kind (`handle_candidate` and
`handle_owned_passthrough` both increment it). The stats doc says "Passthrough
node blobs decompressed for coordinate extraction". The printed number
includes way and relation rewrites and `--direct-io` passthroughs.

Enforce: a dedicated counter, incremented only on the extraction branch,
plus an assertion in an existing LOW CLI test.

### B5. `blobs_skip_decompress` and `blobs_scan_only` are dead stats that still get printed

Both `MergeStats` fields are never incremented anywhere (grep:
initialisation and `print_summary` only). `print_summary` still prints
"(N index-hit, 0 scan-only, 0 skip-decompress)" and adds
`blobs_skip_decompress` into `total_blobs`. They are left over from the
removed batch-classify pipeline, and they are `pub`, so library users see
them too.

Enforce: delete them. To stop this kind of decay, build `MergeStats` only
through `merge_from` / per-variant constructors so a field nobody writes
shows up as dead code. That works once the struct stops being `pub`
field-by-field.

### B6. The drain's "byte-budget reorder buffer" has no budget

`descriptor.rs` module doc ("merged into a byte-budget reorder buffer"),
`DrainItem::byte_cost` doc ("for the reorder buffer's byte-budget
backpressure") and `streaming.rs` both describe backpressure. `run_drain`
only **records** `bytes_in_buffer` into a high-water counter. It always
pulls the next item off the channel and inserts it into the `BTreeMap`. When
one worker stalls on a large seq N, the drain keeps draining the 256-slot
channel, so the scanner and other workers never block, and every
`Rewritten { framed_chunks }` produced after N piles up in memory. In
practice the limit is how far ahead the scanner can get, and that is the
whole file. See item 8.

Enforce: real admission control. Stop `recv` while
`bytes_in_buffer > budget && !buffer.contains_key(next_seq)`, or have
workers take a budget permit before decompressing, the same way the writer
already uses `writer_permit_wait_ns`. A fault-hook test (`test-hooks` delay
at seq N) can assert the high-water bytes stay under the budget.

### B7. Scanner "bytes high water" counters measure the largest single item, not bytes in flight

`run_scanner`: `*bytes_hw_fast = (*bytes_hw_fast).max(approx_cost)`. The
comment says "Track rough bytes in flight for the high-water counters".
`merge_scanner_to_drain_bytes_high_water` /
`merge_scanner_to_workers_bytes_high_water` are therefore "largest descriptor
seen" (about 64 bytes plus tagdata). Anyone reading the sidecar to size the
channels gets a meaningless number.

Enforce: none mechanical. Rename or delete the counters.

### B8. `merge-changes --allow-version-regression` is a silent no-op without `--simplify`

The CLI arg has no `requires = "simplify"`. In `merge_changes`, the flag
reaches only `write_simplified`. The streaming path never checks versions,
so the flag is accepted and does nothing. The help text talks about
`--simplify` but does not stop the combination.

Enforce: clap `requires = "simplify"`, so a test of the bare combination
expects a usage error.

### B9. `BlobOrderGuard` reports the wrong id for negative IDs

`merge_join.rs` `BlobOrderGuard::observe` formats `prev_last.1` as an id.
`prev_last` is an `osm_id_key`, and for negative IDs `.1` is
`id.saturating_neg()`, so a guard trip after blob `-5..-3` says "already
reaching id 5". The drain's twin guard (`check_output_order`) stores the real
id next to the key for exactly this reason. Both guards implement the same
watermark rule (see item 1). Merging them fixes this too.

### B10. `push_info_attrs` turns a negative timestamp into a nonsense date

`osc/write.rs`: `format_epoch_secs((milli_timestamp / 1000).cast_unsigned())`.
A pre-1970 or corrupt negative timestamp becomes about year 584 billion with
no error. The parse side (`parse_rfc3339_utc`) refuses unparsable
timestamps, so the two ends of the round trip disagree on what a valid
timestamp is.

Enforce: `cast_sign_loss` is denied, but `cast_unsigned()` is a method, not
an `as`, so the lint does not fire. A `disallowed-methods` entry for
`i64::cast_unsigned` outside a vetted helper would catch it.

### B11. Test scratch dirs: one fixed name in the system temp dir, deleted at start

`osc/parse.rs` `make_test_dir` and `osc/write.rs` (around the `temp_dir`
call) use `std::env::temp_dir().join("pbfhogg_osc_test_{suffix}")`. There is
no pid and no random part, and `remove_dir_all` runs first. The recent commit
`b353de8` made the test sweeps run in parallel (`all`, `consumer`,
`minimal`). Two sweeps running the same test delete each other's directory
mid-test. That is a flake waiting to happen, and it breaks the project rule
against `/tmp`. Meanwhile `merge_changes/mod.rs` tests use
`CARGO_MANIFEST_DIR/target/merge-changes-{name}-{pid}` and never clean it up,
and `path_guard.rs` tests use `tempfile` inside `target/`. That is three
policies for one need. Two of them write into `target/`, which belongs to
cargo, and that may not even be the real target dir when brokkr sets
`CARGO_TARGET_DIR` per sweep. `cargo clean` deletes them, and they pile up
between cleans (the pid-named ones are never removed).

Enforce: one shared `test_scratch()` helper (tempfile-backed, under a
project-owned ignored directory such as `.scratch/`, or under
`CARGO_TARGET_TMPDIR`, which cargo provides to integration tests for exactly
this). Add `clippy.toml` `disallowed-methods = ["std::env::temp_dir"]`.

### B12. Stale docs that describe a pipeline that no longer exists

- `apply_changes/mod.rs` module doc: "Phase 1: Parallel classify [rayon
  pool] / Phase 2: Sequential inline assign / Phase 3+4". That is the removed
  batch loop. The current pipeline is scanner, then workers, then drain.
- `apply_changes/classify.rs` doc: "In that case the blob is passed through
  raw, and the creates are emitted afterward by the gap-create logic."
  `streaming.rs` now forces a rewrite whenever `inline_upserts` is non-empty,
  precisely because passing the blob through raw broke sort order.
- `stream_output.rs` doc: "These run on the main thread as slots are drained".
- `rewrite.rs` doc comment on `merge`: "The legacy batch-loop merge() is
  replaced by..." (historical narration).
- `diff_ranges.rs`: orphan comment "osc_member_type_to_member_type removed ...
  (see osc.rs)". `osc.rs` does not exist.
- `osc/parse.rs` `load_all_diffs`: the doc says "Load all .osc.gz diffs" and
  the error text says "none is named `<sequence>.osc.gz`, which is the only
  naming this loader recognises". `parse_sequence_number` also accepts plain
  `<seq>.osc`. So the error message states something false about the code
  that raises it.
- `scanner.rs` `send_err` comment: "The caller (merge) will still observe the
  scanner's return Ok". It returns `Err`.
- `streaming.rs` `run_workers` comment refers to
  `rewrite::merge_with_overrides`, and so does `drain.rs`. No such function
  exists; it is `rewrite::merge`.
- `osc/parse.rs` struct doc: "40-60% less memory than the old `DiffOverlay`".
  The comparison target no longer exists.

Enforce: none mechanical beyond a `rustdoc` intra-doc-link check. Writing
the references as `[`rewrite::merge`]` links would turn the
`merge_with_overrides` case into a broken-link warning under
`-D rustdoc::broken_intra_doc_links`.

---

## 1. One value, one owner

### 1.1 Element-kind vocabulary spelled at least eight times, with two `ElemKind` types

- Two different enums are both called `ElemKind`: `crate::blob_meta::ElemKind`
  (apply-changes, merge_join) and `crate::osc::xml_parse::ElemKind`
  (merge-changes, tags-filter OSC). They have the same variants and are not
  convertible.
- Kind to `"node"/"way"/"relation"`: `xml_parse::ElemKind::tag`,
  `drain::kind_name`, `merge_join::kind_name`,
  `merge_changes::check_version_advance` (inline match),
  `merge_changes::write_change_element` (inline match for deletes),
  `CompactDiffOverlay::push_*` / `delete_*` (string literals `"node"` etc.
  passed as `kind: &str`), `compact.rs` `arena_append_*` (literals),
  `xml_parse::kind_for` / `parse_member_type` (reverse direction),
  `osc/write.rs` `member_type_str` and the `BytesStart::new("node")` element
  writers.
- Kind to ordinal: `drain::kind_ordinal`, `merge_join::kind_order`,
  `merge_changes::ChangeElement::key` (`(0, id)`, `(1, id)`, `(2, id)`),
  `compact.rs` `member_type_to_byte` / `byte_to_member_type` (0/1/2 again,
  for member types), plus `merge_join::kind_type_char` (`n/w/r`).

One `ElemKind` with `as_str()`, `ordinal()` (or a derived `Ord`; the drain
comment says it "deliberately does not derive `Ord`" so the order cannot be
silently reordered, which a unit test pinning the order handles better than
three hand-written copies) and `FromStr` would serve all of them.
`check_version_advance` in the overlay takes a `&str` kind only to format an
error. It should take the enum.

Enforce: delete `osc::xml_parse::ElemKind` so the one type is the only
spelling. A text rule in `brokkr check` (or a clippy `disallowed-names` for
local `fn kind_name`) can stop new copies of the helper.

### 1.2 Version-advance rule implemented twice, and the copies already disagree

- `CompactDiffOverlay::check_version_advance` (apply-changes): compares
  against the max of the live record's version and a **versioned tombstone**
  map. A delete is a version claim, and after `delete v9`, an unversioned
  change, then `modify v8`, the v8 is still refused.
- `merge_changes::check_version_advance` (`--simplify`): compares only
  against the **immediately previous change for the key** in
  `last_by_object`. After `delete v9`, `modify` (no version), `modify v8`, the
  previous change has no version, so v8 is accepted. The same three-file
  input is refused by `apply-changes` and accepted by
  `merge-changes --simplify`. That is a real divergence, not a prediction.
- The error texts are separate prose blocks with different remediation
  advice (`--allow-version-regression` vs
  `CompactDiffOverlay::allow_version_regression(true)`).

The `xml_parse.rs` module doc is a long argument that "two code paths
answering the same question differently is the recurring defect". The
version gate is that defect, one layer down. Fix: a `VersionFloor` type
(live max plus tombstone max per `(kind, id)`) owned by `osc`, used by both
the overlay and the simplify dedupe.

Enforce: extend `tests/osc_parser_parity.rs` (it already exists for parse
parity) with a version-gate parity case fed through both commands.

### 1.3 Metadata validation differs between the two sinks

`parse_owned_metadata` (merge-changes, tags-filter OSC) parses only
`version`. `timestamp`, `changeset`, `uid` and `visible` are kept as raw
strings. `arena_meta` (apply-changes) parses `timestamp` as RFC 3339,
`changeset` as i64, `uid` as i32, and maps `visible` to "is it exactly
`false`". So `uid="abc"` or `timestamp="yesterday"` is a hard error in
`apply-changes` and passes silently through `merge-changes`, which re-emits
it. `visible="False"` is visible to apply-changes and re-emitted verbatim by
merge-changes. The doc on `scan_meta_attrs` claims "ONE leniency decision,
shared by every sink". That covers *presence* only, not *validity*.

Fix: parse once into a typed `OscMeta` (`Option<i32>`, `Option<i64>` epoch,
`Option<i64>`, `Option<i32>`, `Option<String>`, `Option<bool>`). The owned
sink formats it back on emit. `osc/write.rs` already has `push_info_attrs`
taking exactly those typed `Option`s.

Enforce: one typed metadata struct, with the stringly `OwnedMetadata` fields
removed so unvalidated text cannot be stored.

### 1.4 Member-role parsing: two spellings, both swallow errors

- `CompactSink::member`: `parse_str_attr(e, "role").unwrap_or_default()`
  turns *every* error (a malformed attribute, a bad entity in the role) into
  `""`.
- `OwnedSink::member` (merge-changes) and tags-filter OSC:
  `parse_str_attr_optional(e, "role").unwrap_or_default()`.
  `parse_str_attr_optional` itself `.flatten()`s attribute errors and
  `.ok()`s normalisation errors.

Neither distinguishes an absent role (legitimately `""`) from an unreadable
one. Fix: `parse_str_attr_opt -> ParseResult<Option<String>>`.

Enforce: delete `parse_str_attr_optional`. Its return type is the thing that
throws errors away.

### 1.5 `OwnedSink::member` has a dead `MemberType::Unknown` branch

`parse_member_type` never returns `Unknown` (it errors on unknown strings),
so the `Unknown(v) => return Err(...)` arm in merge-changes can never run,
and its message ("unknown relation member type value") differs from the live
one. CompactSink instead goes through `member_type_to_byte`, which also
handles `Unknown`. Make `parse_member_type` return a three-variant
OSC-specific type and the dead arms vanish.

### 1.6 The `<osmChange version="0.6">` prelude/postlude is written at six sites

`merge_changes` (single-input path, `build_prelude_bytes`,
`build_postlude_bytes`), `tags_filter/osc.rs`, `diff/derive.rs`, and the
`osc/write.rs` tests. The root name already has an owner, `xml_parse::ROOT`,
on the parse side. The writer side has none, and the version string `"0.6"`
and the XML decl are repeated each time.

Enforce: an `OscDocumentWriter` in `osc/write.rs` that owns decl, root,
section switching (`emit_change`'s open/close action logic is also something
every OSC writer needs) and close. Once it exists, a text rule in
`brokkr check` can flag `BytesStart::new("osmChange")` outside `osc/`.

### 1.7 OSC output gzip policy: level, indent and `.gz` detection repeated

- `flate2::Compression::fast()`: `merge_changes` (twice), `tags_filter/osc.rs`,
  and `write/parallel_gzip.rs` (which wraps it in a private
  `compression_level()` fn with a doc explaining why, the only owner, and
  it is private).
- `Writer::new_with_indent(_, b' ', 2)`: twice in `merge_changes`, plus the
  other OSC writers.
- `output.to_str().is_some_and(|s| s.ends_with(".gz"))`: three times in
  `merge_changes` alone. A non-UTF-8 path silently writes plain XML into a
  `.gz` name.
- The input side sniffs gzip magic (`open_osc_reader`). The output side
  trusts the extension. That asymmetry is fine, but the rule should live in
  one function.
- `merge_changes` builds its own multi-member gzip (per-worker `GzEncoder`
  buffers concatenated) next to `write/parallel_gzip.rs`, which exists to do
  exactly that and says "The only in-crate consumer is `diff --format osc`".
  Two implementations of parallel multi-member gzip.

Enforce: `OscOutput::create(path)` returning the writer, used everywhere.
Then add `disallowed-methods` for `flate2::Compression::fast` outside it.

### 1.8 Worker-count defaults: three different fallbacks for one question

- apply-changes: `available_parallelism() - 2`, min 2, **fallback 4**,
  `Some(1)` rejected.
- merge-changes: `available_parallelism()`, **fallback 8**, `Some(0)` means
  auto.
- The CLI maps `0` to `None` separately in `run_merge` and
  `run_merge_changes`. The library also treats `Some(0)` as auto, so the rule
  "0 means auto" lives in four places.
- `reader.rs`, `scan/classify.rs` and roughly fifteen other
  `available_parallelism()` call sites outside scope each choose their own
  subtraction and fallback.

Enforce: `crate::threads::default_workers(reserve: usize) -> usize`, plus
`disallowed-methods = ["std::thread::available_parallelism"]` everywhere
else.

### 1.9 `DESCRIPTOR_OVERHEAD = 64` defined twice

`descriptor.rs` `DrainItem::byte_cost` and `scanner.rs` `scanner_emit_cost`,
both function-local consts. `scanner_emit_cost` re-implements `byte_cost` for
the `ScannedBlob` variants. Fix: one `BlobDescriptor::byte_cost`.

### 1.10 Arena layout offsets spelled by hand on both sides

`compact.rs` writes the metadata block field by field in
`arena_append_meta`, and `read_meta` reads it back at hand-written offsets
`+1, +5, +13, +21, +25`. `META_LEN = 29` is a hand sum, and `NODE_META_OFF =
20` / `WAY_META_OFF = 16` repeat the record-header layout that
`arena_append_node` / `_way` lay out by sequence. Nothing ties them
together. A field change can leave the reader reading the neighbouring
field.

Enforce: derive the offsets from one `const` layout table, or use `zerocopy`
/ `bytemuck` `#[repr(C, packed)]` header structs, so a mismatch is a type
error. A round-trip unit test per field is the cheaper alternative.

### 1.11 Owned and borrowed OSC element writers duplicated

`osc/write.rs` has `write_node_xml` / `write_way_xml` / `write_relation_xml`
(owned) and `write_dense_node_xml` / `write_borrowed_node_xml` /
`write_borrowed_way_xml` / `write_borrowed_relation_xml` (borrowed), each
spelling the same element shape. `write_tags_xml` and
`write_borrowed_tags_xml` are identical apart from the iterator type. The
metadata emit is also duplicated: `OwnedMetadata::push_attrs` (strings) and
`push_info_attrs` (typed). A fix in one has to be repeated in the other, and
the comments show this has already happened at least once (the
"presence, not truthiness" fix).

Enforce: write each element once over a small trait (`id`, `coords`,
`tags()`, `refs()`, `members()`, `meta()`) that both owned and borrowed
types implement.

### 1.12 PbfWriter-flavour vs local-flavour element writes in apply-changes

`element_writes.rs`: `write_osc_way` and `write_osc_way_local` are the same
body apart from the capacity call. `write_osc_relation` and the inline
relation code in `rewrite_block.rs` (twice: `emit_create_local` and the
modify arm) are the same members-collect plus `add_relation`.
`stream_output::emit_create_for_output` and
`rewrite_block::emit_create_local` are the same match. Node creation
(`bb.add_node(osc.id(), osc.decimicro_lat(), ...)`) is spelled four times.
The `(0, 0)` fallback for a missing loc is in two places. Both `_stats`
parameters are unused.

Enforce: abstract the flush target (`trait BlockSink { fn ensure_*; }`
implemented for `&mut PbfWriter` and `&mut Vec<OwnedBlock>`) and write each
element once.

### 1.13 Sortedness watermark implemented twice

`drain::check_output_order` and `merge_join::BlobOrderGuard::observe` are the
same transitive-watermark rule. `BlobOrderGuard` even says "Same shape as the
drain's output guard". They already differ in what they keep (B9) and in
their error wording. One `OrderWatermark` type in `osm_id.rs` would serve
both, plus `sort` / `cat --dedupe`, which the drain comment says "already
use" the same rule.

### 1.14 Sidecar counter namespace is spelled ad hoc and is already wrong once

`xml_parse::parse_osc_path` emits `merge_changes_decompress_ns` for **every**
caller: apply-changes, tags-filter OSC and merge-changes. Brokkr therefore
attributes apply-changes' OSC decompress time to a merge-changes counter. On
multi-input merge-changes it is emitted once per file under one name from
parallel workers, so the max-wins view keeps only one file's time.
apply-changes uses `merge_*` (named after the internal function, not the
command), merge-changes uses `merge_changes_*`, and the markers are
`MERGE_*` vs `MERGECHANGES_*`. `MERGECHANGES_WRITE_FINISH_START/END` are
emitted back to back with nothing between them in both N>1 paths
(zero-width pairs kept for "shape compatibility").

Enforce: pass the counter prefix (or a typed `Counters` handle) into
`parse_osc_path` instead of hard-coding it. A const table of counter names
per command would let a test assert that names are unique across commands.

---

## 2. Values nobody can find, change, or trust

- **Tunables for apply-changes are in four files and none is configurable:**
  `DRAIN_CHANNEL_CAPACITY = 256` and `CANDIDATE_CHANNEL_CAPACITY = 64`
  (`rewrite.rs`), the 25 ms `recv_timeout` (an inline literal in
  `run_drain`, not even a named const), the worker heuristic (`rewrite.rs`),
  and `DESCRIPTOR_OVERHEAD` (two files). There is no reorder budget at all
  (B6). Nothing answers "what can I tune here". A `PipelineTuning` struct on
  `MergeOptions` with documented defaults would be the owner, and the same
  struct is the injection point for tests.
- **No injection point for the barrier timing.** The 25 ms poll is what
  keeps the scanner/drain/worker cycle from deadlocking (see 7.1). Tests
  cannot shrink it, stretch it to provoke the race, or replace polling with a
  deterministic step.
- **merge-changes tunables:** gzip level and indent (1.7), the chunking rule
  `group.len().div_ceil(worker_count)` in `write_simplified`, and the
  `inputs.len() <= 1` threshold that switches between the streaming and
  buffered strategies (the two produce different gzip member layouts). None
  is named or adjustable.
- **The `test-hooks` field on `MergeOptions` forces every struct literal to
  carry `#[cfg(feature = "test-hooks")] panic_at_blob_seq: None`.** The CLI
  builds `MergeOptions` field by field twice (`run_merge`, `run_bench_merge`)
  instead of using `..Default::default()`, so each new option is a
  three-site edit. The manual `Default` impl with
  `#[allow(clippy::derivable_impls)]` exists only because of that field.
  Moving test hooks to a separate `TestHooks` value passed to an internal
  entry point would remove both.
- **Configuration validated at the moment of use:** `-j 1` is refused only
  after the OSC has been fully parsed, DiffRanges built and the output
  writer created (`rewrite.rs` checks `jobs` after `writer_from_header_...`),
  so on a planet catch-up diff the refusal arrives after minutes of work. The
  `--force --locations-on-ways` refusal is up front, as it should be. The
  `-j` check belongs next to it, or in clap (`value_parser` range).
  `MergeOptions` should have a `validate()` called first thing.

---

## 3. One channel, one implementation

- **The library writes to stderr unconditionally.** `apply_changes::merge`
  prints "Parsing OSC diff", "Diff: ...", "Diff ID ranges: ...", the LOW
  needed-count line, and calls `final_stats.print_summary()` itself.
  `osc::load_all_diffs` prints a progress line per file plus a summary. A
  library consumer (elivagar, nidhogg) cannot silence any of it. By contrast
  `merge_changes` returns stats and leaves `print_summary` to the CLI. The
  crate has no logging facade, so apply-changes and merge-changes already
  follow two different conventions.
  Enforce: `clippy::print_stderr = "deny"` at the library crate level,
  `allow`ed in `cli/`. That single lint makes "the library does not print"
  structural.
- **`print_summary` is the operator-facing channel and it is a wall of
  `eprintln!`.** The "Blobs:" line is about 150 characters and prints three
  sub-counts, two of which are always zero (B5). The LOW line packs five
  numbers into one line.
- **Levels and identifiers:** the `warn_locations_on_ways_loss` warning, the
  progress lines and the summary all go to the same unlabelled stderr. A
  warning cannot be told apart from progress. The `-j 1` refusal is a
  ten-line paragraph. The version-regression errors are about five lines of
  prose each. These make up for the lack of structure with volume.
- **Significant events with no log line:** coordinate-extraction failure
  (B3); the forced rewrite of a false-positive blob because of inline
  upserts (counted nowhere, invisible in stats); the drain's idle barrier
  fire versus a type-transition fire (two triggers for one event, and the
  markers do not say which one fired); merge-changes N>1 versus N<=1
  strategy selection.

---

## 4. Errors

- **Everything in the apply-changes pipeline becomes
  `ErrorKind::Io(io::Error::other(string))`.** `streaming::io_err`,
  `drain::io_err` (a copy: "Mirrors the io_err helper in streaming.rs"), and
  inline `new_error(ErrorKind::Io(std::io::Error::other(e.to_string())))` at
  about eight drain sites. Decompress failures, protobuf parse failures,
  rewrite errors (including the overlay's own typed errors), protocol bugs
  ("barrier ran twice?", "coord_slots is None") and the unsorted-output
  refusal all reach the caller as `Io`. The original `ErrorKind` is
  stringified away, and **no message carries the blob seq, frame offset or
  base path**, so "decompression failed" names no subject. Fix: one
  `PipelineError { seq, frame_start, stage, source }` that keeps the
  `source` chain.
  Enforce: delete both `io_err` helpers, and a text rule against
  `io::Error::other(` in `commands/` outside `error.rs`.
- **merge-changes stringifies across the rayon boundary.** `do_work`,
  `build_simplify_stream` and the per-chunk closures map every error to
  `String` because `BoxResult` is not `Send`. The error kind, the
  `source()` chain and any typed parse error are lost. The fix is making the
  crate error type `Send + Sync`, which also removes the reason for the
  workaround.
- **Logic errors dressed as I/O:** "scanner: fast-path descriptor missing
  indexdata - should be unreachable", "misconfigured channels", "drain:
  received CopyRange item but use_copy_range is false",
  `flush_copy_range`'s non-feature stub. These are invariants that the
  channel wiring should make unrepresentable: `ScannerChannels` with
  `drain_tx: Option<_>` plus `use_copy_range: bool` are two values that must
  agree. An enum `Route::Splice { drain_tx } | Route::Owned` carrying the
  sender only in the splice variant removes three of these runtime checks.
- **Double error on a mid-stream fault is accepted in comments at three
  sites** (`rewrite.rs`, `drain.rs`, `streaming.rs`): the drain reports
  "Producer dropped a seq" first, and the real cause only shows at the scope
  join. But `drain::run_drain(...)?` returns early out of the scope closure,
  so the explicit `scanner_handle.join()` / `workers_handle.join()` never
  run when the drain fails. `std::thread::scope` then joins the remaining
  threads implicitly. A scanner or worker that returned `Err` has its error
  **dropped**. A thread that panicked makes `scope` itself **panic** after
  the join (std semantics for unjoined panicked scoped threads), and the
  worker pool's inner scope turns any worker panic into exactly that. So the
  documented behaviour, "both errors surface, the real one second", is
  wrong both ways: the real cause is either discarded or turned into a
  process panic. Fix: run the drain, then always join both, then pick the
  root cause through the shared first-error slot that the comment calls
  unneeded. The existing `panic_at_blob_seq` hook test should assert which
  message the caller actually gets.
- **Swallowed:** coordinate extraction (B3); role errors (1.4); `user` and
  `role` decode errors in `osc/write.rs` (`info.user().ok()`,
  `m.role().unwrap_or("")`) drop or blank the field when re-emitting a
  corrupt PBF string-table entry, so `diff --format osc` quietly emits
  `role=""` for a broken role; `StringInterner::resolve` and
  `CompactTagIter` return `""` on an out-of-range id or invalid UTF-8,
  which the comment justifies as "keeps a read-side accessor from
  panicking". That turns arena corruption into silent empty tags in the
  output PBF. The arena is crate-written and immutable, so a
  `debug_assert!` plus an error-returning iterator would be honest.
  `barrier_tx.send` failure is ignored deliberately (documented).
  `std::fs::metadata(p).ok()` in counter emission is harmless.
- **Aborts on input:** none found. The `expect`s in `merge_join.rs` follow a
  `peek`/`is_some` check, and those in `compact.rs` are slice-length
  conversions on fixed slices. The `streaming.rs` `panic!` is behind
  `test-hooks`.

---

## 5. Policy invented per call site

- **Cleanup on the error path:** `merge_changes` builds its own
  `OutputFile`-or-`PathGuard` choice based on `output.exists()` (a long
  comment explains why). `apply_changes::merge` does nothing, so a library
  caller gets a partial PBF on failure, and it never checks whether
  `output_pbf` is the same path as `base_pbf` (the writer would truncate the
  input it is about to `copy_file_range` from). The CLI wraps both in
  `guarded_output`. So there are three policies: CLI guard, merge-changes
  self-guard, and apply-changes unguarded. Fix: library entry points take
  an `OutputFile` (or do what merge-changes does through one shared helper),
  and refuse `same_file(base, output)`.
- **Channel-closed handling:** `scanner::send_err`, `streaming::send_drain`,
  the drain's disconnect handling and `barrier_tx` each decide on their own
  whether a closed peer is graceful or an error, with comments that
  contradict the code (B12).
- **Poison handling:** `.unwrap_or_else(PoisonError::into_inner)` at five
  sites. It is consistent, but each one quietly continues after a worker
  panicked while holding a coord slot, which is exactly when that slot's
  content is suspect.
- **Validation of values from storage:** indexdata `count` is trusted for
  stats on passthrough, `id_range` from indexdata is trusted for routing,
  and `infer_kind_and_range` is used only when indexdata is absent. A
  forged or stale indexdata `min/max` would route a blob as "no overlap" and
  splice it unchanged past a delete. That is the same trust question the
  read-path scope owns, but apply-changes is where it turns into silent data
  loss. Worth an explicit decision recorded in `reference/`.

---

## 6. Ambient dependencies

- `std::thread::available_parallelism()` called directly from both
  commands' production logic (1.8). That makes worker counts, chunking and
  `write_simplified`'s output layout (chunk boundaries become extra
  `<create>` sections) depend on the host. The merge-changes **output
  bytes** therefore differ between hosts with different core counts.
  Tests that compare bytes will flake across machines.
- `Instant::now()` scattered for phase timings (eight sites in `rewrite.rs`
  and `streaming.rs`, plus `TimedRead`). This only feeds counters, so it is
  harmless for logic, but the 25 ms `recv_timeout` makes the **barrier
  protocol itself** time-dependent (7.1).
- `output.exists()` in `merge_changes` picks the write strategy from the
  live filesystem state. That is a TOCTOU with no consequence today, but it
  is the kind of ambient check that makes the behaviour depend on what
  happens to be on disk.
- Base PBF opened five times by path (`require_indexdata`, `read_header`,
  `File::open` for copy, `HeaderWalker::open`, `run_workers`). A file
  replaced mid-run gives mixed reads. Open once and hand the `File` down.

---

## 7. Shared state and locking whose safety is not structural

### 7.1 The node to way barrier is a three-party protocol held together by polling and comments

Scanner, workers and drain coordinate through `last_node_seq_tx` (scanner to
drain), `barrier_tx` (drain to scanner), the `OnceLock` loc map, and per-worker
`Arc<Mutex<FxHashMap>>` coord slots. Deadlock freedom depends on: the drain's
25 ms `recv_timeout` re-polling `last_node_seq_rx`; the "idle barrier fire"
block in `drain.rs`, which `scanner.rs` warns "Do not remove ... without also
rewriting this wait"; workers flushing `local_coords` into their slot
**before** sending the DrainItem (a comment in `handle_candidate`); and the
scanner holding back way descriptors. The invariant is maintained by the
scanner and the drain, which only know about each other through comments
that cite each other. The `-j 1` refusal exists because one of the deadlock
cases is unhandled ("scanner blocks on a full candidate channel with no one
draining"). The structural fix: make the node phase a separate pipeline run
(scanner over node blobs, then join, merge slots, publish, then scanner over
way and relation blobs). The barrier then becomes a function return, the
timeout and idle fire disappear, and `-j 1` stops being a deadlock.
Alternatively, the drain can block on a `select` over both channels
(crossbeam), which removes polling but keeps the protocol.

Enforce: a structural rewrite. Until then, a `test-hooks` stress test that
delays the scanner's `last_node_seq` send past the drain's last node item
(the exact race the timeout guards).

### 7.2 Coordinate slots read by the drain while workers may still write

`barrier_publish_loc_map` drains every worker's slot when
`next_seq > last_node_seq`. A worker writes its slot before sending each node
DrainItem, and again in a final flush at loop exit (`worker_loop`). The final
flush is always empty for node blobs (each candidate flushes before sending),
but if any coordinate ever reached `local_coords` outside that ordering, the
final flush would land **after** the merge and be silently lost. Safety
depends on "flush before send" at one call site. Fix: return the
coordinates inside the node `DrainItem` instead of through a side channel.
That makes the ordering structural (the drain has the coordinates exactly
when it has the item) and removes the slots, their mutexes and the
slot-count check in `run_workers`.

### 7.3 `CompactDiffOverlay` invariants: `pub` fields and two writers

The comment above the mutators says the invariants ("a delete-set insert must
be paired with an index remove") are why mutation goes through methods "rather
than `pub(super)` fields". But `deleted_nodes`, `deleted_ways` and
`deleted_relations` are **`pub`**, and `CompactSink::begin_element` mutates
them directly (`self.overlay.deleted_nodes.remove(&id)`) instead of through
an overlay method. So deletion state has two writers: `delete_*` and the
sink, and the "un-delete on create" rule lives in the parser, not the
overlay. Any library user can also insert into `deleted_nodes` without
touching the index or tombstones. Fix: private fields, `is_deleted(kind,
id)` / `deleted_ids(kind)` accessors, and the un-delete done inside
`push_*`.

Enforce: making the fields private makes the bad spelling unrepresentable
outside the module.

### 7.4 `candidate_rx` behind a `Mutex` because `mpsc::Receiver` is not `Sync`

This works, but it is a lock held across a blocking `recv()`. Every idle
worker queues on the mutex while one blocks inside `recv`. The comment says
it is held "microseconds", which is true only when the channel is non-empty.
A crossbeam MPMC channel or `std::sync::mpmc` once stable removes the lock.

---

## 8. Unbounded growth

- **The drain's reorder buffer** (B6). There is no bound, and the comments
  claim one.
- **merge-changes N>1 streaming path** holds every input's fully
  re-emitted, compressed XML in memory (`Vec<(Vec<u8>, u64)>`) before
  writing any of it. Memory grows with the total output size. The
  `--osc-range` workflow at planet (7 OSCs, 26.3 M changes in the comment)
  holds the whole compressed output. An ordered drain that writes chunk i
  as soon as it and every earlier chunk are done would cap memory at about
  `jobs` chunks.
- **merge-changes `--simplify`** holds the whole `ChangeStream` (every
  change, owned strings) and then a `BTreeMap` of survivors, plus three
  grouped `Vec`s. This is inherent to "last change wins", but a key to
  input-position index would cut it to O(objects).
- **`CompactDiffOverlay` arenas are append-only.** A superseding change
  appends a new record and repoints the index, and the old bytes stay. With
  `load_all_diffs` accumulating a catch-up chain, a hot object edited in
  every diff costs one record per diff. Interned strings (including every
  distinct `user` name) are never freed. This is bounded by the input, but
  the input is "however many diffs you pass", and `heap_size_estimate`
  counts the garbage as live.
- **`MergeStats::blob_sizes`** is a `Vec<u32>` with one entry per blob
  (about 1-2 M at planet), cloned again in `print_summary` to sort. It is
  small in absolute terms, but it is unbounded in principle, and
  `frame_len as u32` truncates over 4 GiB (impossible for valid PBF frames,
  but unchecked). A fixed-bucket histogram would do the same job.
- **Per-worker `local_coords`** are filtered by `needed_set`, so they are
  bounded. That one is fine.

---

## 9. Code that is no longer load-bearing

- `MergeStats::blobs_skip_decompress` / `blobs_scan_only` (B5): never written,
  only printed. Grep confirms.
- `WorkerOutput` is a one-variant enum (`FalsePositive`) that exists only to
  host `into_drain_item`. Its doc explains that the other variants were
  removed. It should be a function on `BlobDescriptor`.
- `scanner.rs`: `let _ = last_node_seq_sent;` with the comment "set here for
  symmetry; not read after". It is not set there either; it is a no-op
  statement.
- `streaming.rs` `worker_loop`: `let _ = worker_id; // reserved for per-worker
  counter slots later`. The parameter is used only by the `test-hooks` panic
  message, so the reservation is speculative.
- `element_writes.rs` `write_osc_way(.., _stats)` and
  `write_osc_way_local(.., _stats)`: unused parameters threaded through every
  caller.
- `merge_changes::action_tag` is a one-line wrapper over
  `OscAction::tag`, and `type Action = OscAction` is an alias kept "for
  readability". Both are indirection with no other purpose.
- `osc::parse_osc_file` and `osc::load_all_diffs` are `pub`, with no
  production caller in the crate or CLI. `parse_osc_file` is used by tests
  and `tests/osc_parser_parity.rs`, so it is a supported library API.
  `load_all_diffs` is used only by its own unit tests. It is the one
  multi-diff accumulator in the crate, and no command uses it:
  `apply-changes` takes one OSC, and `merge-changes` is the multi-file
  path. It still shapes the overlay (the `usize` offsets comment cites "a
  `load_all_diffs` accumulation" as the reason). Either it is a library
  feature with a consumer (nidhogg?) that should be named in its doc, or it
  is dead. The code alone cannot tell which, so this needs the user's
  answer.
- `merge_join.rs` `MERGECHANGES_WRITE_FINISH_START/END` zero-width marker
  pairs in merge-changes (1.14): markers that carry no span.
- The `cfg(not(feature = "linux-direct-io"))` `flush_copy_range` stub
  returns an error that its own doc says is unreachable. It is needed only
  because `use_copy_range: bool` cannot prove the feature at the type level
  (see the `Route` enum in item 4).
- `rewrite.rs` `merge()` destructures `allow_version_regression: _` and then
  reads `opts.allow_version_regression` anyway. The destructure exists only
  to make the field list exhaustive, which is fine, but one of the two reads
  is redundant.

---

## 10. Where production code assumes it runs, and what it touches

- **Linux-only `copy_file_range` decides routing through a feature flag, not
  detection.** `use_copy_range = io_uring || !direct_io` under
  `linux-direct-io`. On a filesystem where `copy_file_range` fails or falls
  back (cross-device output, some FUSE or network filesystems), whatever
  `write_raw_copy` does is the only safeguard. That belongs to the write-path
  scope, but apply-changes chooses the route without asking.
- **`streaming.rs` imports `std::os::unix::fs::FileExt` unconditionally**
  (for `read_exact_at`), and nothing in `apply_changes` is `cfg(unix)`.
  `AGENTS.md` says `apply_changes` compiles in a no-features build, and the
  `minimal` sweep checks that, but only on Linux. On a non-Unix target the
  crate fails to build at all, not just this command. Worth a decision:
  declare the crate Unix-only (and say so in `Cargo.toml` / README), or put
  pread behind a small platform shim. Enforce: a `cargo check --target
  x86_64-pc-windows-gnu` sweep, if non-Unix is meant to work.
- **Core count:** see 6. merge-changes output bytes depend on it.
- **Test writes:** B11. The system temp dir with fixed names, and cargo's
  `target/` used as scratch by two different helpers. The owner should be
  `CARGO_TARGET_TMPDIR` (integration tests) or a project-owned ignored
  `.scratch/` with a tempfile per test. Not `target/`, which cargo owns and
  `cargo clean` wipes, and not the system temp dir, which the project rules
  forbid.
- **Library output placement:** `apply_changes::merge` writes `output_pbf`
  in place with no scratch sibling (5). merge-changes writes a sibling
  scratch through `OutputFile` (which keeps the suffix, so `.gz` detection
  still works on the scratch path, which I checked). Both write only where
  the caller pointed. No writes into other tools' directories found.
