# Hygiene hunt: geocode index

Scope: `src/geocode_index/` (format, reader, builder: admin, interp, pass1,
pass1_5, pass2, pass3, strings), plus the CLI entry point in
`cli/src/main.rs` and the docs that restate it, where the contract leaks out.

Findings are grouped by the ten questions. Every one says whether the fixed
version could be held mechanically. Unranked on purpose.

---

## 1. One value, one owner

### 1.1 Build defaults are spelled in four places

`17 / 14 / 10 / 500 / 75 / 1000` (street level, coarse level, admin level,
max admin vertices, fine radius, coarse radius) live in:

- `BuildConfig::default()` in `builder/mod.rs`
- the clap `default_value = "..."` strings on `Command::BuildGeocodeIndex` in
  `cli/src/main.rs`
- `reference/cli-reference.md` and `docs/cli/commands.md` (plus the
  generated copies under `docs/.vitepress/dist/`)
- `format.rs` test fixtures (`valid_header_bytes`, `header_roundtrip`), which
  are fine as test data but read like a fourth source.

The CLI does not use `BuildConfig::default()` at all - it builds the struct
field by field from clap values - so the library default and the CLI default
are independent and only agree by coincidence today. A library user and a
CLI user get the same index only while nobody edits one side.

Enforceable: yes. Expose the defaults as `pub const` on `BuildConfig` (or a
`BuildConfig::DEFAULT_*` set) and have clap use
`default_value_t = BuildConfig::DEFAULT_STREET_LEVEL`. A unit test can assert
`BuildConfig::default()` equals the clap-parsed empty-args config. Docs can
only be held by a doc test or a generated table.

### 1.2 The level range `0..=19` is restated in help text and docs

`MAX_BUILD_S2_LEVEL = 19` in `pass3.rs` is the owner, and `validate()` reads
it. But the clap help strings ("0-19") and `cli-reference.md` ("0-19") hard-code
it. Raising the budget changes validation and leaves the help lying.

Enforceable: help text can be built with `concat!`/a `const_format` string, or
a CLI test can assert the help mentions the constant's value.

### 1.3 The S2 format limit `30` is a bare literal in `Header::from_bytes`

`street > 30 || coarse > 30 || admin > 30` and the error string "0..=30" are
literals. There is no `MAX_S2_LEVEL` constant in `format.rs` next to
`MAX_BUILD_S2_LEVEL`; the relationship between the two (format limit vs build
budget, documented at length in comments) is held only in prose.

Enforceable: one `pub const S2_MAX_LEVEL: u8 = 30` in `format.rs`, plus a
`const _: () = assert!(MAX_BUILD_S2_LEVEL <= S2_MAX_LEVEL);`.

### 1.4 Admin level vocabulary: `2..=10`, `11` = postal, `12` array size

The meaning "admin level 11 is a postal-code boundary" is invented in
`pass1.rs` (`else { 11 }`), never named, and then relied on by:

- `reader.rs`: `best_by_level: [...; 12]` twice (`into_result` and
  `search_admin_ranked`), with `level < 12` / `level >= 12` guards
- `format.rs` doc "Admin level 2-11" and `ReverseResult.admin` doc "(2-11)"
- `admin.rs` `admin_vertex_cap` match arms (`0..=2`, `3`, `4`, `5`, `_`), so
  postal boundaries silently take the local cap.

There is no consumer-visible way to tell a postal polygon from an admin one
other than knowing the magic 11. If anyone widens accepted admin levels to
`2..=11` (OSM does use 11), real level-11 admin boundaries collide with
postal codes in the same slot and the "smallest area per level" ranking
mixes them.

Enforceable: yes - a `pub const POSTAL_ADMIN_LEVEL: u8` and
`ADMIN_LEVEL_SLOTS: usize` in `format.rs`, or better an explicit `kind` byte in
`AdminPolygon` (byte 25 is reserved and zero today - it is the obvious home,
and it is a format bump). With a kind field the type makes the collision
unrepresentable.

### 1.5 Interpolation type encoding `0/1/2` lives as literals at both ends

`pass2.rs` maps `"even" => 1, "odd" => 2, _ => 0`; `format.rs` documents
"0=all, 1=even, 2=odd" in a comment; `reader.rs` matches `0 => ..., _ => ...`.
The reader treats any nonzero value (including garbage) as even/odd, and
odd vs even is never distinguished by the reader at all (both use the
step-2 rounding, relying on `start_number` parity). An unrecognised byte off
disk is accepted silently.

Enforceable: a `#[repr(u8)] enum InterpolationType` in `format.rs` with
`TryFrom<u8>`, validated in `Reader::validate_structure`.

### 1.6 Bucket record layout is a second, hand-rolled format

`pass3.rs` defines `BUCKET_RECORD_SIZE = 15`, `ENTRY_TYPE_*`, and
`write_bucket_record` / `parse_bucket_file` with manual byte offsets, outside
`format.rs`. It is scratch, not on-disk index format, so living elsewhere is
defensible - but it duplicates the exact serialization pattern `format.rs`
exists to own, and Stage B's `match e.etype { ..., _ => {} }` silently drops
an unrecognised entry type (see 4.6).

Enforceable: a `BucketRecord` struct with `to_bytes`/`from_bytes` alongside
the others and an exhaustive enum for the entry type.

### 1.7 Eight file names threaded through `FusedCellAssignmentParams`

`FusedCellAssignmentParams` takes `fine_cells_file`, `fine_street_entries_file`
... `coarse_interp_entries_file` as `&str` parameters, and `run_stage_b` takes
four more. Every call passes the `format::FILE_*` constants; there is no
second caller. The indirection makes the file-name set look configurable
when it is fixed by the format, and makes the fine/coarse pairing a property
of argument order (swap two `&str`s and it still compiles).

Enforceable: replace with a `GeoTreeFiles { cells, street, addr, interp }`
const pair `FINE_TREE` / `COARSE_TREE` in `format.rs`, used by both builder
and reader (the reader's `search_level` and `validate_geo_tree` take the same
four-mmap quadruple positionally - same hazard on the read side).

### 1.8 The file-set count "19" is hard-coded in prose

`builder/mod.rs` comment ("a directory of 19 files"), AGENTS.md
("19-file on-disk format") and `cli-reference.md` ("a directory of 19
files"). Nothing enumerates the set: the 19 `FILE_*` constants are separate
items, and the reader's `open` lists them a second time by hand. Per the
house rule this should be reworded rather than re-counted.

Enforceable: `pub const ALL_FILES: [&str; N]` in `format.rs`; the reader opens
from it, the builder's invalidation/cleanup can use it, and prose can say
"every file in `format::ALL_FILES`".

### 1.9 Empty-file mmap trick implemented twice

`Reader::open`'s `mmap` closure and `pass2.rs`'s `mmap_file` closure both do
"zero-length file -> anonymous read-only mmap". Pass 2's comment even says
"same as Reader". Two copies of one platform workaround.

Enforceable: one `fn mmap_or_empty(path) -> io::Result<Mmap>` in `format.rs`
or a crate util; a grep rule for `map_anon()` outside it.

### 1.10 Record decoders duplicated between builder and reader

`interp.rs` has `read_addr_point_mmap` / `read_node_at`; `reader.rs` has
`decode_addr_point`, `record_at`, `read_record`, `two_node_distance`. Same
"index * SIZE, bounds-check, from_bytes" logic in two modules, slightly
different (the builder's uses unchecked `offset + SIZE`, the reader's uses
`checked_add`).

Enforceable: generic `fn record<T: Record>(data, index) -> Option<T>` with a
`Record` trait (`SIZE`, `from_bytes`) in `format.rs`. That trait also removes
the three near-identical entry iterators (see 9.3).

### 1.11 Thread-count rule copied

`pass1_5.rs` re-implements `available_parallelism().saturating_sub(2).max(1)`
with fallback 4, which is exactly `scan::classify::resolve_thread_count`.
Pass 2 calls `parallel_classify_phase(..., None, ...)` and gets the shared rule;
Pass 1.5 has its own copy. Pass 3 and admin assembly use the global rayon
pool, which has a different rule again (all cores).

Enforceable: make `resolve_thread_count` `pub(crate)` and call it; a grep
rule for `available_parallelism` outside it.

### 1.12 `M_ARENA_MAX = 2` prelude at nine sites

`builder/mod.rs` plus `check/refs.rs`, `check/verify_ids.rs` (twice),
`repack`, `renumber`, `time_filter`, `cat`, `degrade`. The comment in
the geocode copy says "same prelude used by ..." - the duplication is known
and narrated rather than removed. See also 6.1 / 10.1 for why it should not
be in library code at all.

Enforceable: one `crate::alloc_policy::cap_arenas()` with the rationale, and
a grep rule for `M_ARENA_MAX` elsewhere.

---

## 2. Values nobody can find, change, or trust

### 2.1 The real tunable set is scattered and uninjectable

What a person tuning a build would want, and where it actually lives:

| Knob | Location | Injectable? |
|---|---|---|
| S2 levels, radii, vertex cap | `BuildConfig` | yes |
| Excluded highway classes | `pass2::EXCLUDED_HIGHWAYS` | no |
| Highway vs `GeocodeTagLiterals::standard()` in Pass 1.5 | `scan::way` | no (see 2.2) |
| Accepted admin levels `2..=10` | `pass1.rs` literal | no |
| Per-level vertex multipliers 16/8/4/2/1 | `admin::admin_vertex_cap` | no |
| Hole ownership samples `5` | `admin::hole_inside_outer` | no |
| Interp endpoint "exact" tolerance `1` e7, search = street cell + 8 neighbours | `interp.rs` | no (search scope is tied to `street_level`, not to a radius) |
| Bucket count 256, chunk sizes 100k/500k/100k, Stage B window 8 | `pass3.rs` | no |
| Per-segment sample budget 2^24 | `pass3.rs` | no |
| Decode threads | `pass1_5.rs` / `classify` | no |

Nothing answers "what can I tune here". The first block is policy (what
counts as a street, an admin boundary) and belongs in `BuildConfig` or a
`GeocodePolicy`; the second block is mechanism and belongs in one `const`
block at the top of `builder/mod.rs` with the rationale.

Enforceable: partially. Moving them is a one-off; a test that builds with a
non-default `GeocodePolicy` is what keeps the injection point from rotting.

### 2.2 Street classification is defined twice and can disagree

Pass 1.5 decides which ways' nodes to keep via
`scan::way::GeocodeTagLiterals::standard()` and its `is_street /
is_building_addr / is_interp` flags. Pass 2 re-decides the same thing in
`classify_way_into` with its own tag match and `EXCLUDED_HIGHWAYS`. If Pass
1.5 is narrower than Pass 2 on any rule, Pass 2 classifies a way as a street
whose nodes were never collected: `referenced_nodes.get(nid)` is false,
`filter_map` drops them, and the street silently loses vertices or vanishes
(`coords.is_empty()` return). If Pass 1.5 is wider, memory is wasted. I did
not read `scan/way.rs` in full, so whether they have already diverged is
unverified - but nothing ties them together, and the two definitions are in
different modules with different representations (byte literals vs `&str`
match).

Enforceable: yes. One classification table consumed by both (Pass 2 can use
the same flags off the wire-format scan, or both call a shared
`classify(tags) -> GeocodeFlags`), plus a property test that runs both over
the same blobs and asserts Pass 2's set is a subset of Pass 1.5's.

### 2.3 Header radii are not validated on read

`BuildConfig::validate` rejects non-finite or negative radii, but
`Header::from_bytes` accepts any `f32`. A NaN radius in a header (other
producer, corruption) makes `fine_max_dist_sq` NaN and every `dist_sq <
max_dist_sq` false: the index opens and answers "nothing here" everywhere.
The level checks were mirrored into the reader; the radius checks were not.

Enforceable: one `validate_radius` fn shared by builder and `Header::from_bytes`,
with a test that pokes bytes 16..24 like the existing level tests do.

### 2.4 `max_admin_vertices` in the header is write-only

It is written into the header and parsed back, but nothing in the reader
uses it, and since 2.1's per-level multipliers it no longer describes the
stored polygons (a country holds 16x it). It is metadata that misleads.

Enforceable: drop it, or record the effective per-level caps.

---

## 3. One channel, one implementation

### 3.1 Library code prints progress with `eprintln!`

`build_geocode_index` is a library function (`pbfhogg::geocode_index::builder`)
and writes ~15 lines to stderr unconditionally ("Pass 1: Relations...",
counts, "Done in", the smoke-test WARNING). A library consumer cannot
silence it, redirect it, or attach identifiers. The CLI then prints its own
summary line from `BuildStats`, so the counts are printed twice in two
formats. The `hotpath`-feature block prints `key=value` lines which is a
third format on the same stream.

Enforceable: yes. Progress through a callback/`Progress` trait on
`BuildConfig` (or the crate's existing output path if one exists for other
commands), and a clippy `print_stderr = "deny"` on the library crate with
`#[allow]` only in the CLI. That lint is the mechanical rule.

### 3.2 Significant events with no output

- Missing node references: `classify_way_into` drops refs whose node is
  absent (`filter_map`), silently shortening streets and opening admin rings.
  No counter, no line. On an extract this is the common case at the border.
- Admin ways that never resolved (in `needed_admin_ways` but absent from
  `way_geom`) - only the relation-level "dropped" count surfaces, not which.
- Dropped relations are counted, but no relation ID is kept anywhere
  (`RawAdminRelation` has no `id` field), so "412 admin relations dropped"
  cannot be acted on.
- Unresolved interpolation ways: count only, no IDs (no OSM way ID is
  retained past Pass 2 for any record).
- Negative node refs skipped in Pass 1.5, silently.

Enforceable: counters via `crate::debug::emit_counter` are cheap and already
used (`pass2_node_blobs`). Carrying the OSM ID for diagnostics is a design
choice; a capped sample list of IDs is enough.

### 3.3 Smoke-test result is a warning nobody can act on

`"WARNING: smoke test query returned no address or street match"` - no
coordinate, no index path. And the smoke test is a stderr line, not a
`BuildStats` field, so the CLI cannot fail on it and tests cannot assert it.

Enforceable: `BuildStats.smoke_test: SmokeOutcome`; then a test can pin it.

---

## 4. Errors

### 4.1 Error type is `Box<dyn Error>` from strings throughout

Builder and reader both use `Result<T, Box<dyn Error>>` with `format!(...)
.into()`. Pass 1.5 converts worker errors to `String` (`e.to_string()`),
discarding the `ErrorKind`; Pass 2b converts `io::Error` to `String`
(`merge_err = Some(e.to_string())`). The crate has a boxed `ErrorKind`
pattern in `error.rs`; this module does not use it, so callers cannot match
on "corrupt input" vs "disk full" vs "bad config".

Enforceable: a `GeocodeError` enum (or crate `ErrorKind` variants) and the
`Result` alias pointing at it makes string errors a type error.

### 4.2 Errors that shed their subject

- `pass1_5`: "pread at {offset}: {e}" - no file path. Decompression errors
  carry no blob offset at all.
- Pass 2b write errors: `e.to_string()` - no file name (which of the four
  output files?).
- Pass 2a out-of-domain node: names the node, good, but aborts the whole build
  (4.4).
- `run_stage_b` / `write_admin_data` / `File::create(...)?` everywhere: a
  bare `io::Error` with no path.
- `Reader::open`: `std::fs::read(dir.join(FILE_HEADER))?` is bare (the other
  18 opens add the path; the header, the one most likely to be missing, does
  not). "geocode header too short" also has no path.
- Reader validation errors name the kind and offset but not the record index
  that held the bad offset, nor the directory.

Enforceable: a helper `fn create(path) -> Result<File, GeocodeError>` that
attaches the path; a grep rule against bare `File::create(` in the module.

### 4.3 `--force` means two unrelated things

`config.force` is passed to `require_indexdata(..., config.force, ...)` (skip
the indexdata requirement) and also gates overwriting an existing index. An
operator who wants to rebuild in place must also opt out of the indexdata
check, and vice versa. On non-indexed input that opt-out leads into 4.5.

Enforceable: split into `overwrite: bool` and `allow_unindexed: bool` in
`BuildConfig` and two CLI flags. The type makes the conflation impossible.

### 4.4 One bad node aborts a planet build

Pass 2a sets `state.error` for any node outside the coordinate domain and the
merge then fails the build. That is a refusal, not a panic, which is right
for corrupt files - but it is the only data-quality rule in the builder that
aborts; missing refs, bad roles, unparseable levels are all skipped. One
policy for "bad element" would be skip-and-count everywhere, or fail
everywhere under a `--strict`.

Enforceable: a single `on_bad_element` policy enum on the config.

### 4.5 Panic on operator-controlled input in Pass 1.5

The comment in `run_pass1_5` says it outright: with `--force` on input whose
node blobs lack indexdata, `max_node_id` is under-reported (0 if no blob has
indexdata) and `IdSet::set_atomic` "would panic via its diagnostic path". A
node ID larger than any indexdata `max_id` (a lying or stale indexdata
block) does the same. The negative-ID guard was added; the too-large guard
was not. This takes out a worker thread inside `thread::scope`, which
propagates as a panic out of the library.

Enforceable: `set_atomic` should return `Result` (or the scan should check
`r <= max_node_id` and record an error). Alternatively refuse `--force`
without indexdata for this command, since Pass 1.5 cannot work without it.

### 4.6 Silent drops that should be errors

- Stage B: `match e.etype { ..., _ => {} }` drops records with an unknown
  type. Since only this module writes the buckets, an unknown type is a
  builder bug and should be an error.
- `pass3` Stage A: `if let Some(rec) = rec` / `if let (Some(n1), Some(n2))`
  silently skip a street record or segment that is out of bounds in a file
  the builder just wrote. Same reasoning.
- Reader: entry lists are validated for length but not for content
  (`way_index < street_way_count`, `segment_index + 1 < node_count`,
  `addr idx < addr_point_count`, `poly_idx < admin_polygon_count`). Out-of-range
  values are skipped at query time (`continue`) - the very "safe is not
  correct" failure the `validate_way_geometry` doc comment argues against. A
  `segment_index` equal to `node_count - 1` is worse than skipped:
  `two_node_distance` reads the next node in the file, which is the first
  node of the next way, and returns a plausible but wrong segment.

Enforceable: the reader checks belong in `validate_structure` (4.7 notes the
cost); the builder cases can be `debug_assert!` + error.

### 4.7 Open-time validation is O(index size)

`validate_structure` walks every record of every file at `open`, which at
planet scale faults in many GB and makes `Reader::open` take seconds to
minutes. The builder's smoke test then repeats the whole walk. That is a
legitimate trade (correctness over open latency) but it is undocumented as
a cost and has no opt-out (`open_unchecked` / `open_trusted` for an index the
caller just built or checksummed). A content checksum in the header would
make this O(1) to trust.

---

## 5. Policy invented per call site

### 5.1 Overflow policy differs per counter

Some counts use checked conversion with a "bump FORMAT_VERSION" error
(per-cell u16 counts, admin offsets, vertex counts). Others wrap silently in
release builds:

- `addr_point_count += 1`, `street_way_count += 1` (u32) in Pass 2
- `interp_way_count as u32`, `admin_polygons.len() as u32` in the header
- `total_cells += 1` (u32) in Stage B, `cell_ids.len() as u32` in admin index
- `StringPool::intern`: `self.data.len() as u32` - past 4 GiB of strings the
  offsets wrap and every later string points at the wrong text, with no
  error. All of `name_offset`, `street_offset`, etc. are u32.
- `chunk_start + STREET_CHUNK as u32` can overflow near `u32::MAX`.
- `poly_idx as u32` in `admin_cells_for_polygon` (the INTERIOR_FLAG guard
  catches > 2^31 later, so this one is covered).

Enforceable: yes. `clippy::cast_possible_truncation` is already on and is
being `#[allow]`ed at function level across `pass3.rs`, `admin.rs`,
`interp.rs`, `mod.rs`. Narrowing those allows to expression level, plus
`clippy::arithmetic_side_effects` on this module, would force each site to
choose. A typed `CountU32::try_increment()` is the structural answer.

### 5.2 Three copies of the length-prefixed list iterator

`U32EntryIter`, `SegmentEntryIter`, `AdminEntryIter` in `reader.rs` have
identical `new` bodies (four early returns each) and near-identical `next`.
`binary_search_cells` and `binary_search_admin_cells` are the same function
twice. `validate_list` is a fourth implementation of the same framing. If
the count width is ever bumped to u32 (the error messages all suggest it),
that is five edits.

Enforceable: one `ListIter<T: Record>` and one `binary_search<T: Record +
Keyed>`; the framing width becomes one const.

### 5.3 Stage A street and interp loops are copy-pasted

The fused fine/coarse cover closure (including the `coarse_seen[4]` dedup)
appears verbatim twice in `bucketed_cell_assignment_fused`, and the bucket
write loop three times. The `coarse_seen` dedup only remembers 4 coarse
cells; a segment crossing more than four coarse cells re-emits the fifth
onward for each fine sample, writing duplicate coarse entries (inflating
per-cell counts toward the u16 limit; the reader tolerates duplicates). Any
fix to that has to be made twice.

Enforceable: extract `fn cover_way_fused(...)` and a `BucketSink` - pure
refactor; nothing mechanical keeps it from re-forking.

### 5.4 Ring-closure assumption differs between producer and consumers

`admin_cells_for_polygon` covers edges with `poly.vertices.windows(2)`,
which never covers the closing segment last -> first of each ring unless the
ring repeats its first vertex. Whether it does depends on `assemble_rings` and
`simplify_ring` output in `geo.rs` (other scope), and nothing in the format
says whether rings are stored closed. `geo::point_in_ring` handles implicit
closure (there is a test for it), so readers and the edge cover may disagree
on an open ring: the closing edge's cells are not edge cells and only get
coverage if the flood fill reaches them.

Enforceable: state "rings are stored closed" (or open) in `format.rs`, assert
it in `write_admin_data`, and check it in `validate_structure`.

---

## 6. Ambient dependencies

### 6.1 `mallopt` mutates process-global allocator state from a library call

`build_geocode_index` calls `libc::mallopt(M_ARENA_MAX, 2)` and never
restores it. The comment says "Scoped to this command; other pbfhogg paths
unaffected" - that is true for the CLI, false for a library consumer whose
whole process (a server that builds then serves) keeps two arenas
afterwards. It is also invisible to tests.

Enforceable: move to `cli/src/main.rs` (per-command, before dispatch), or
make it an explicit `BuildConfig.cap_malloc_arenas` the caller opts into. A
grep/lint rule banning `mallopt` under `src/` keeps it out.

### 6.2 Thread count from `available_parallelism`

See 1.11. No injection point, so tests run with whatever the host has and
cannot exercise the single-worker or many-worker orders deterministically.

### 6.3 Wall clock

`Instant::now()` for "Done in" is display only; fine.

---

## 7. Shared state and locking

### 7.1 `CoordMmapShared` safety rests on an unchecked input property

The `unsafe impl Sync` and `write_coord` contract require "no two concurrent
calls pass the same rank", justified by "sorted PBF guaranteeing every node
ID appears in at most one blob". Nothing checks the input is sorted or
deduplicated: `require_indexdata` checks indexdata, not
`Sort.Type_then_ID`, and `--force` skips even that. A PBF with a duplicated
node across two blobs (a naive concatenation, an un-deduped merge) makes two
workers write the same slot concurrently - a data race, i.e. UB, not merely
"last writer wins". `write_coord`'s bounds check is `debug_assert!` only, so
in release an out-of-range rank is an out-of-bounds write.

Enforceable: yes, structurally. Either use `AtomicU64` slots with relaxed
stores (same codegen on x86-64, no UB, removes the `unsafe impl`), or check
`is_sorted()` at entry and refuse. Make the bounds check a real `assert!` or
a returned error - it is one compare per node.

### 7.2 Overwriting a live index truncates files under readers' mmaps

`--force` removes the header and then `File::create`s every data file in
place. Any process with a `Reader` open on that directory has those files
mmapped; truncation turns its next page access into `SIGBUS`, and a
`read_string` on changed content hits the `expect("Reader::open validated
every referenced string")` panic. The reference doc frames invalidation as
the honest substitute for atomic replace, but it only protects against
readers that open later, not readers already open - which is the normal case
for a geocoding server being refreshed.

Enforceable: build into a sibling scratch directory and `rename` the
directory (atomic when the target does not exist; for replace, rename old
aside, rename new in, then delete old - readers keep their unlinked inodes).
Or write each file to `name.tmp` and `rename` over, which keeps old mmaps
valid. The reader-side `expect` should become an error regardless.

### 7.3 Test-hook statics are process-global

`ADMIN_PIP_CALLS` in `reader.rs` and `PANIC_AT_STREETS_WAY_IDX` in `pass3.rs`
are statics shared across every `Reader` and every build in the test
process. `reset_admin_pip_calls(&self)` takes `&self` but resets the global,
which reads as per-instance. Two tests counting PIP calls in parallel
interfere; correctness rests on test scheduling.

Enforceable: hold the counter on the `Reader` (behind the feature) and the
panic hook on the config; or mark those tests `serial::` per the tier model.

---

## 8. Unbounded growth

### 8.1 Admin flood fill is bounded only by `admin_level`, and validation permits 19

`MAX_BUILD_S2_LEVEL` bounds per-segment sampling. It does not bound the
interior flood fill in `admin_cells_for_polygon`, whose work and memory
(`visited`, `queue`, `entries`) grow with polygon area / cell area, i.e. 4x
per level. At the default 10 a country is ~10^5 cells; `--admin-level 19`
passes validation and asks for ~10^10 cells for Russia: a hang and an OOM,
the exact failure mode the segment budget was introduced to prevent. Postal
and level-2 polygons are covered at the same level as municipalities.

Enforceable: a separate, smaller `MAX_ADMIN_S2_LEVEL` derived from a
cell-count budget over the largest legal polygon (computable the same way
the segment budget is), checked in `validate`, with a test like
`max_build_level_is_exactly_the_sampling_budget_boundary`. The structural
fix is the region coverer already tracked in ADR-0015.

### 8.2 All intermediate state in RAM, acknowledged

`way_geom` (every admin way's full geometry), `admin_polygons`, `interp_ways`,
`StringPool` (string bytes plus a second owned `String` per entry in the
`FxHashMap`, i.e. every string stored twice), and `admin_cell_entries` all
grow with input. The doc comment acknowledges planet limits. The
`StringPool` double storage is the cheap one to fix (hash into `data` by
offset).

### 8.3 Stale bucket directories accumulate inside the index directory

Scratch lives at `output_dir/.buckets-level{N}`. The stale sweep only removes
directories for the current run's two levels. A SIGKILLed build at levels
18/15 followed by a build at 17/14 leaves `.buckets-level18` /
`.buckets-level15` (up to tens of GB at planet) in the index directory
forever, and nothing reports it. See 10.3 for where they should live.

---

## 9. Code that is no longer load-bearing

### 9.1 `#[cfg(feature = "geocode-reader")]` around the smoke test is always true

`builder` is gated on `commands`, and `commands = ["dep:serde_json",
"geocode-reader"]` in `Cargo.toml`. Inside the builder the reader feature is
therefore always on; the cfg is a switch with one value. It suggests a
configuration (builder without reader) that cannot be built.

### 9.2 `first_addr_lat_e7` / `first_addr_lon_e7` exist only for the smoke test

Carried through both Pass 2 merge closures and `Pass2Output` solely so the
smoke test can query one point. Reading the first record from
`addr_points_mmap` at smoke-test time gives the same coordinate without
threading state through production code.

### 9.3 `pass3::parse_polygon_rings` is a one-line forwarding wrapper

Wraps `format::parse_polygon_rings(vertices.iter().copied())`; exists only
because of argument shape. `format::parse_polygon_rings` itself is used only
by pass3 (the reader calls `parse_rings` directly).

### 9.4 `collect_candidates` forwards to `search_level` verbatim

Its own doc says it is "kept only to make the two public call sites
readable". It adds nine positional arguments of surface for no behaviour.

### 9.5 `SlimInterpWay` "house number 0" limitation is moot

The sentinel comment (repeated in `pass2.rs` twice and `interp.rs`) says a
real interpolation from house 0 is indistinguishable from unresolved. But
`find_endpoint_house_number_mmap` returns `None` for `hn == 0`
(`if hn > 0`), and the reader rejects `start == 0 || end == 0`. House 0 can
never resolve, so the ambiguity cannot arise; the three comments describe a
risk the code already decided. Either the doc should say "house number 0 is
unsupported" once, or a `resolved` flag should be added - not three
paragraphs of known limitation.

### 9.6 Stale comment: "zero-sentinel semantics"

`classify_way_into`'s doc says "the zero-sentinel semantics (nodes at
lat==0 && lon==0 are dropped) match the previous sequential path". That was
fixed - `coord_pack` makes Null Island representable, as the comment 70
lines above in the same file explains. Read as live, this doc says the
opposite of what the code does.

### 9.7 `GeocodeLevels` CLI struct mirrors `BuildConfig` field-for-field

`cli/src/main.rs` defines `GeocodeLevels` to avoid positional transposition,
then copies each field into `BuildConfig`. Passing a partially built
`BuildConfig` (or having clap derive `Args` on a config struct directly)
removes one hop and closes 1.1 at the same time.

### 9.8 `BuildStats` vs header counts

`BuildStats` and `Header` carry the same counts; `BuildStats` could be
derived from the header (plus `dropped_admin_relations`).

---

## 10. Where production code assumes it runs, and what it touches

### 10.1 glibc assumed under `target_os = "linux"`

`libc::mallopt` is gated on Linux, not on glibc. On musl Linux `mallopt`
exists as a stub that does nothing (or is absent depending on version), so
the anti-fragmentation rationale silently does not apply; on Linux with
jemalloc/mimalloc as global allocator it is a no-op. Silent degradation,
nobody notices until planet RSS.

Enforceable: gate on `target_env = "gnu"`; better, see 6.1.

### 10.2 `std::os::unix::fs::FileExt` makes the builder Unix-only

`pass1_5.rs` imports `std::os::unix::fs::FileExt` unconditionally. The
builder therefore does not compile off Unix, while the feature docs present
`commands` as a portable feature. The `minimal`/`consumer` sweeps only run
on this Linux host, so nothing would notice. Fine if Unix-only is intended -
then it should be stated and `cfg`-gated at the module, not discovered at
compile time.

### 10.3 Scratch written into the output directory

`.buckets-level{N}` directories are created inside `output_dir`, which is
the user's index directory and, per 7.2, may be live. The scratch shares a
filesystem with the output by design (disk pressure), but it does not need to
share the directory: a sibling `output_dir.with_extension("building")` or a
caller-supplied `scratch_dir` in `BuildConfig` keeps the index directory
containing exactly the format's file set. The code also `remove_dir_all`s any
pre-existing directory with that name in the user's directory without
checking it is ours.

Enforceable: `BuildConfig.scratch_dir: Option<PathBuf>`; `validate_structure`
could additionally reject unexpected entries in the index dir (cheap with
`ALL_FILES`, 1.8).

### 10.4 64-bit assumed

`interp.rs` `read_node_at` says "u64->usize: Linux 64-bit only" and casts.
`coord_array_len = referenced_count as usize * 8` truncates on 32-bit. The
reader is careful (`usize::try_from`); the builder is not. Wrong answers, not
refusals, on 32-bit.

Enforceable: `compile_error!` on `target_pointer_width != "64"` for the
builder, which states the assumption and makes it a refusal.

### 10.5 No reader-only build is ever compiled

`geocode-reader` without `commands` is the configuration a geocoding service
would use, and it is advertised in `geocode_index/mod.rs`. No
`[[check]]` sweep in `brokkr.toml` enables it alone (`minimal` has no
features, `consumer` has `commands`). Like the old untested
`default-features = false` claim the `minimal` sweep comment describes.

Enforceable: yes - one more `[[check]]` sweep with
`features = ["geocode-reader"]`, scoped to `-p pbfhogg`.

---

## Lateral findings

- **Antimeridian handling is otherwise consistent.** `geo::point_in_polygon`
  is antimeridian-aware, so the flood fill and the reader agree with
  `hole_inside_outer`. Not a finding; recorded so the next pass doesn't
  re-check.
- **`interpolation_resolvable` recomputes way length per candidate.** Every
  interp segment hit inside the radius walks the whole way's nodes to compute
  total length, and the winning one does it again in `interpolated_number`.
  Resolvability could be precomputed into the format (a flag in byte space
  `InterpWay` doesn't have, or the `resolved` flag of 9.5).
- **Interp endpoint search is scoped by cell, not distance.** Endpoint
  matching scans the street-level cell and its 8 neighbours. At level 17
  that is a ~450 m window; at `--street-level 19` it shrinks to ~100 m and
  endpoints silently stop resolving. The search scope should be a radius,
  independent of the index cell level.
- **Building centroid is a vertex mean** including the duplicated closing
  vertex, so it is biased toward the first vertex and can fall outside
  concave buildings. Same issue the admin seed fix addressed.
- **`--force` invalidation leaves the old data files** in place when a build
  then fails before Pass 2's `File::create`s - harmless since the header is
  gone, but `.buckets-*` from 8.3 sit alongside.
- **`docs/.vitepress/dist/` is checked in** and contains generated copies of
  the CLI defaults (1.1). If it is meant to be a build artifact it is a
  fifth copy that only a regeneration keeps in step.
