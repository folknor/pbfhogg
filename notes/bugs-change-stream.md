# Bug hunt: change stream (`src/osc/`, apply-changes, merge-changes, diff, time-filter)

Hunt date 2026-08-12. Single Opus hunter. Findings only, unverified. Ranked by
severity as the hunter ranked them.

## `apply-changes` can emit an unsorted PBF while stamping `Sort.Type_then_ID`

Where: `src/commands/apply_changes/streaming.rs` (`if !overlaps` ->
`FalsePositive`) plus `src/commands/apply_changes/drain.rs` `process_item` /
`dispatch_variant` (the "Cursor-rule invariant" comment).

Mechanism. The scanner flags a blob as a candidate when
`DiffRanges::range_overlaps` is true - which includes pure creates whose IDs fall
inside the blob's ID span. The worker then runs `block_overlaps_diff`, which only
checks whether an element actually in the block matches a diff ID. A pure create
in the blob's interior fails that check, so the blob is shipped as `CopyRange` /
`OwnedBytes` passthrough and, per the documented cursor rule, the drain does not
advance the upsert cursor. `handle_gap_creates` runs before the blob using that
blob's own `osm_first`, so it only emits upserts with `id < blob.min_id`. The
interior create is therefore emitted on the next blob's gap-create pass (or at
the type transition), i.e. after elements with larger IDs.

Concrete failure. Base node blob covering ids 10, 20, 30; OSC creates nodes 5,
15, 35. Output order becomes 5, 10, 20, 30, 15, 35. `build_header_bytes`
unconditionally writes `hb.sorted()`, so the result is a PBF that claims
Type_then_ID but is not. Everything downstream that trusts that flag
(apply-changes on the output, `derive-changes`, `block_pair_merge_phase`, `sort`
fast paths) then silently produces wrong answers rather than erroring.

Already exercised and not caught: `tests/cli_apply_changes.rs::
merge_gap_creates_between_blobs` uses exactly this shape (base 10/20/30, creates
5/15/35) but asserts only `nids.contains(&id)` - never order. The hunter expects
the current output of that test is already misordered.

Fix direction (structural, not minimal): the "passthrough does not advance the
cursor" rule is the bug. Either (a) the worker must force a rewrite whenever
`upsert_slice(...)` for the blob's range is non-empty, regardless of
`block_overlaps_diff` - the precise check should be `overlaps ||
!inline_upserts.is_empty()`; or (b) the drain must emit gap creates against the
blob's `[min,max]` interior, which it cannot do without splitting the
passthrough. (a) is the right shape. Add an order assertion to the gap-create
tests and to `brokkr verify apply-changes`.

Confidence: high.

## OSC arena offsets are `u32` - silent corruption past 4 GiB

Where: `src/osc/compact.rs` `arena_append_node/way/relation` all
`#[allow(clippy::cast_possible_truncation)]` and return `arena.len() as u32`;
`src/osc/parse.rs` indexes are `FxHashMap<i64, u32>`.

Once a per-kind arena exceeds 4 GiB the returned offset wraps and the index
points at an arbitrary earlier record. No check anywhere. `get_node` / `get_way`
then hand back a different element's bytes, and `apply-changes` writes them into
the output with the wrong ID's data - silent corruption, no panic, no error. Node
records are 49 B plus tags, so the cliff is ~85 M node changes: reachable for a
catch-up/monthly diff or a large `load_all_diffs` accumulation. Either widen to
`u64` or hard-error on `arena.len() > u32::MAX` in the append functions.

Related, same file: `StringInterner::resolve` does `self.table[id as usize]` - a
corrupted intern id panics instead of erroring.

Confidence: high.

## OSC coordinate parsing silently teleports nodes to Null Island

Where: `src/osc/xml_parse.rs::handle_elem_start`:

```rust
let lat = parse_f64_attr(e, b"lat").unwrap_or(0.0);
...
state.current_lat = (lat * 1e7).round() as i32;
```

A `<node>` in a create/modify section with a missing, empty, or unparsable `lat`
/ `lon` is accepted and stored at (0,0). Applying such an OSC silently moves the
node to lat 0 / lon 0 - and under `--locations-on-ways` that (0,0) propagates
into every way referencing it (`NodeLocationIndex::build_from_diff` seeds from
`get_node` unconditionally). The `as i32` cast also saturates on out-of-range
values (`lat="1e30"` -> `i32::MAX`) and maps `NaN` to 0. A create/modify node
without coordinates should be a hard parse error; the leniency documented on
`parse_meta_attrs` is defensible for metadata, not for geometry.

Confidence: high.

## `time-filter` silently no-ops on metadata-stripped input

Where: `src/commands/time_filter/mod.rs::dense_timestamp` / `node_timestamp` /
etc., all `unwrap_or(0)`.

An element with no `Info` gets timestamp 0, which is `<= cutoff` for every
plausible cutoff, so `snapshot_gate` keeps it and counts it in
`versions_before_cutoff`. A PBF written without metadata (common: osmium
`--drop-metadata`, and several pbfhogg paths) therefore produces "time-filter
succeeded, N elements written" with zero filtering applied. The command should
detect absent timestamps and either error or report them distinctly rather than
folding them into "before cutoff". Same issue in the history path: a versionless
element becomes the selected "latest <= cutoff".

Confidence: medium-high.

## Two divergent OSC parsers, and the doc comment about them is false

`src/osc/xml_parse.rs::parse_meta_attrs` sets `META_FLAG_PRESENT` when any
metadata attribute is seen, and its doc says this mirrors
`merge_changes::parse_metadata`'s leniency. It does not:
`src/commands/merge_changes/mod.rs::parse_metadata` starts with `let version =
parse_i32_attr_optional(e, b"version")?;` and returns `None` - dropping
timestamp, changeset, uid, user and visible - for any element lacking `version`.
So the same input file yields different metadata depending on which command reads
it, and `merge-changes` silently strips metadata that `apply-changes` preserves.

More broadly: there are two full hand-rolled quick-xml OSC state machines
(`osc/xml_parse.rs` and `merge_changes/mod.rs::handle_start_like` /
`handle_start_like_streaming` - the latter itself duplicated twice with only the
sink differing). They disagree on metadata leniency, on unknown member types
(`osc/xml_parse.rs` errors; `merge_changes` errors; but `MemberType::Unknown` is
representable and silently truncated elsewhere), and on input framing (see the
gzip finding below). This should be one parser with a sink trait. Pre-1.0, just
do it.

Confidence: high.

## `apply-changes` cannot read an uncompressed `.osc`

`src/osc/parse.rs::parse_osc_file_into` wraps the file in `MultiGzDecoder`
unconditionally. `merge_changes::parse_osc_into` sniffs the `1f 8b` magic and
handles both. Feeding a plain `.osc` to apply-changes fails with a raw gzip error
rather than working (or saying something useful). Lift the magic sniff into the
shared parser.

Confidence: high, easy.

## `load_all_diffs` silently loads nothing

`src/osc/parse.rs::parse_sequence_number` requires exactly `<digits>.osc.gz`. Any
other naming (`.osc`, `changes-4705.osc.gz`, Geofabrik's
`000/004/705.osc.gz`) is skipped without a word, and the function returns an
empty overlay with a cheerful "Loaded 0 diffs" summary. It should error on a
directory where files were present but none matched.

Confidence: medium, low severity.

## `merge_join` trusts indexdata and blob ordering, and loses elements silently when they lie

`src/osc/merge_join.rs`:

- `next_blob_for_kind`: a blob whose `index.kind` sorts earlier than the current
  phase is `continue`d - silently discarded forever. A node blob appearing after
  the way blobs vanishes from the diff. Header sortedness is checked by callers,
  but a header can lie and `degrade` exists specifically to produce such files.
  This should be an error, not a skip.
- `element_merge_pair`: the in-range predicate is raw `element_id(e) <=
  merge_up_to`, while every ordering decision in the same function uses
  `crate::osm_id::osm_id_cmp`. For negative IDs (which `osm_id.rs` explicitly
  supports - 0, then -1, -2, ..., then positives) these two orders disagree, so
  the boundary cut is inconsistent with the merge order. Latent; positive-only
  production files are unaffected.
- `merge_decoded_pair`: if a blob's `index.max_id` understates its real contents,
  elements past the claimed max are dropped without consuming or erroring. No
  infinite loop (the hunter checked - `max > min(max)` cannot hold on both
  sides), but silent loss.
- `fill_buffer`'s Mixed-block path drops wrong-type elements with only a comment
  for defence.

Confidence: medium.

## `MemberId::Unknown` is written to OSC as `type="node"`

`src/osc/write.rs::write_relation_xml` and `write_borrowed_relation_xml` both do
`MemberId::Unknown(_, _) => "node"`. A relation member of an unknown type
silently becomes a node member - referential-integrity corruption in derived OSC.
Symmetrically, `compact.rs::member_type_to_byte` does `MemberType::Unknown(v) =>
v as u8`, truncating any value >= 256. Either error or round-trip faithfully; do
not invent a node reference.

Confidence: high, low severity.

## `push_info_attrs` drops legitimate zero-valued metadata

`src/osc/write.rs::push_info_attrs` gates each attribute on `> 0`. `changeset=0`,
`uid=0` (anonymous edits exist in old data), and `version=0` are silently
omitted - and when all of them are zero the element ships with no metadata at
all, so the parse side sets no `META_FLAG_PRESENT` and apply-changes writes it
without an info block. `reference/cli-reference.md` claims `--format osc`
"produces a lossless roundtrip - applying the derived OSC to the old PBF
reproduces the new PBF exactly". That claim is false at these margins.

Confidence: medium.

## Design-level: no version precedence anywhere in the change stream

Neither `CompactDiffOverlay` (later `parse_osc_file_into` call wins) nor
`merge-changes --simplify` (`BTreeMap::insert` in input order wins) ever consults
`version`. Precedence is purely positional. Consequences: `load_all_diffs` on a
directory with a gap or an out-of-order sequence silently produces a wrong
overlay; `merge-changes` with inputs listed out of order silently regresses
objects to older versions; applying the same OSC twice, or an older one after a
newer one, silently downgrades the PBF. This is arguably osmium-parity, but it is
undefended - there is not even a warning when an incoming change carries a
version <= the one it replaces. Given the overlay already stores
`ArenaMeta.version`, a "reject/warn on version regression" gate is cheap and
would catch the whole class of operator errors that `brokkr`'s `--osc-range`
makes easy to commit.

## Resource shape

- Arena garbage: every `push_node/way/relation` appends a fresh record and
  orphans the previous one; the index is overwritten but the bytes are never
  reclaimed. Accumulating N OSCs that repeatedly touch the same objects grows the
  arena by the sum of all versions, not the live set - and interacts directly
  with the 4 GiB cliff above.
- `merge-changes --simplify` is fully in-memory: `build_simplify_stream`
  materialises every `Change` (owned `String` tags, refs, members) for all inputs
  before dedupe. The code's own comment cites 26.3 M changes at planet 7-OSC.
  Since the dedupe key is `(u8, i64)` and only the last change per object
  survives, a two-pass shape (scan for last-occurrence offsets, then re-parse and
  emit) would cut peak RSS by roughly the whole stream. Worth a rewrite, not a
  tweak.
- Truncated `.osc.gz` at a gzip member boundary: `MultiGzDecoder` returns clean
  EOF, quick-xml returns `Eof`, and apply-changes happily writes a partially-
  applied PBF. Mid-member truncation does error. A trailing `</osmChange>` check
  in the parser would close this.
