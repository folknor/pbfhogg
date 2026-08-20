# Bug hunt: change stream (`src/osc/`, apply-changes, merge-changes, diff, time-filter)

Hunt date 2026-08-12. Single Opus hunter. Findings only, unverified. Ranked by
severity as the hunter ranked them.

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
