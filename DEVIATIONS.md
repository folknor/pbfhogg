# pbfhogg deviations from osmium

Intentional behavioral differences from osmium. These are deliberate design
choices, not bugs.

## export: area detection and property model

**osmium behavior:** Area classification uses osmium's configurable export
rules, and property emission depends on its attributes configuration.

**pbfhogg behavior:** `export` always emits `@id` and `@type`, with the
remaining OSM metadata behind `--metadata`. Tags colliding with emitted
reserved names are suppressed. Closed ways become polygons only through the
fixed area-key list and `area=yes` or `area=no` overrides defined by
[ADR-0010](decisions/0010-geojson-export-format-and-area-heuristic.md).

**Impact:** Feature classification and property sets are intentionally not
byte-identical to `osmium export`. Coordinate order and GeoJSON validity still
follow RFC 7946, but osmium output is not a golden byte-level oracle for this
command.

**Rationale:** Stable identity properties and a compact, documented area rule
make the initial streaming format predictable without importing osmium's
configuration surface.

## add-locations-to-ways: relation-member nodes always preserved

**osmium behavior:** Requires `--keep-member-nodes` flag. Without it, untagged
nodes referenced as relation members are silently dropped, breaking relation
geometry.

**pbfhogg behavior:** Untagged relation-member nodes are unconditionally kept.
No flag needed.

**Rationale:** Dropping relation-member nodes is virtually never desired. It
breaks multipolygon boundaries, route relations, and any relation that references
nodes directly. osmium's opt-in flag is widely considered a footgun. The cost of
the extra relation-scanning pass is negligible (relation blobs are a small
fraction of the file, filtered via `BlobFilter::only_relations()`).

**Implementation:** A third pass scans relation blobs to collect node IDs
referenced as members into an `IdSet` bitset. During the write pass,
untagged nodes are kept if they appear in this set. The pass is skipped entirely
when `--keep-untagged-nodes` is set (all nodes are kept anyway).

## add-locations-to-ways: missing nodes are an error (no longer a deviation)

**osmium behavior:** Fails on missing node coordinates unless
`--ignore-missing-nodes` is passed.

**pbfhogg behavior:** The same. This used to be a deviation - missing nodes were
always tolerated, `(0, 0)` was substituted, and the count appeared in the summary
line as `missing locations`.

**Why the deviation was withdrawn:** the substituted `(0, 0)` is Null Island, a
real coordinate off the coast of Ghana. The output is well-formed PBF and the
numbers are plausible, so no reader downstream can tell a placeholder vertex from
a genuine one - the summary line on stderr was the only signal, and it does not
survive a pipeline. Absence has to be reported as absence. `--ignore-missing-nodes`
still produces exactly the old output for callers who want it, and the summary
line is unchanged.

## apply-changes: missing LocationsOnWays nodes are an error

**osmium behavior:** Under `--locations-on-ways`, an unresolved way-node
reference is written with `(0, 0)` as its coordinate and no warning.

**pbfhogg behavior:** The command fails and does not publish its output. Pass
`--ignore-missing-nodes` to request the osmium-compatible `(0, 0)` fallback.

**Rationale:** Null Island is a valid coordinate, so a sentinel-bearing output
cannot distinguish missing data from real data. Failure by default matches
`add-locations-to-ways` and keeps the lossy behavior explicit.

The check covers the ways apply-changes writes locations for, which is the
scope of the next section: a base way the OSC does not touch is not checked.

## apply-changes: --locations-on-ways updates only the ways the OSC touches

**osmium behavior:** `osmium apply-changes -l` recomputes way-node locations
from the merged node set, so every way referencing a moved node gets the new
position.

**pbfhogg behavior:** Ways the OSC creates or modifies get locations from the
merged nodes. A base way the OSC does not touch is copied through with the
locations it already carried, even when the OSC moves or deletes a node it
references; a deleted node leaves its last position in such a way. Rerun
`add-locations-to-ways` on the output for a full refresh.

**Rationale:** Most way blobs are spliced into the output without decoding.
Finding every base way that references a changed node needs a node-to-way
lookup across the whole file and would send those blobs through the rewrite
path, a cost the splice exists to avoid. The behaviour is documented rather
than approximated.

## diff: content equality vs version ordering

**osmium behavior:** Matches elements by `(type, id, version, timestamp)` ordering,
so same-id elements with different versions are reported as separate left/right
entries rather than as a modification. When two elements do match, equality is a
CRC over content plus `timestamp` (always) and `changeset/uid/user` (unless
`--ignore-attrs-*` flags are passed). Both behaviors can produce spurious diff
output when inputs have mismatched or absent metadata
([osmium-tool#93](https://github.com/osmcode/osmium-tool/issues/93)).

**pbfhogg behavior:** Compares elements field by field - coordinates, tags, refs,
members. Metadata (version, timestamp, changeset, uid, user) is ignored entirely.
Two elements with the same type+ID are "same" if and only if their content is
identical.

**Cross-validation:** 14-element discrepancy out of 59.1M on Denmark. These are
elements where osmium's version-based comparison disagrees with content comparison
- e.g., same version number but different coordinates, or different versions with
identical content.

**Rationale:** Content equality is deterministic regardless of metadata completeness.
It answers "did anything actually change?" rather than "which version is newer?"

## Change stream: version precedence is enforced, not merely positional

**osmium behavior:** `osmium apply-changes` and `osmium merge-changes`
resolve conflicts positionally - the last change seen for an object wins,
whatever version it claims. Feeding diffs out of sequence order downgrades
objects to an older state and exits 0. osmium offers no check and no
warning.

**pbfhogg behavior:** An incoming change whose `version` is lower than the
highest version ever claimed for that object, by any create, modify or
delete read earlier in the same run (across all input files), is a hard
error naming both versions. This applies to `CompactDiffOverlay`
accumulation (so `apply-changes` and `osc::load_all_diffs`) and to
`merge-changes --simplify`, which share the rule and its error text. A change
that carries no `version` makes no claim: it is always admitted, and neither
raises nor resets the floor, so an unversioned change between v9 and v8 does
not let the v8 through.

A change whose `version` equals that highest claim is admitted, and the
later change wins by position, as in osmium; the floor does not move, so a
lower claim after it is still refused. Equal versions are ordinary input,
not a symptom: overlapping replication diffs repeat boundary changes, and
regional extract diffs (Geofabrik's, and `diff --format osc` without
`--increment-version`) delete an object leaving the region at the version
it already had, so `modify v12` in one diff and `delete v12` in the next is
a normal sequence, as is the object re-entering unchanged as `create v12`.
Version alone cannot order changes that share a number, and comparing their
content cannot either - a leaving delete may carry the full metadata or the
version alone - so the gate protects numerical version precedence and
leaves ordering within one version to input order. Two consequences follow:
an equal-version conflict (two different `v12` payloads) is resolved by
position rather than refused, and inputs listed backwards across a pair of
diffs that differ only in equal-version changes (`delete v12` then
`modify v12`) are not detected; a real backwards listing is still refused by
every object whose version advanced between the two diffs.
The default `merge-changes` mode is untouched - it preserves the full
change stream in input order by design and resolves nothing, so there is
nothing to regress.

Opt back into osmium's behavior with
`apply-changes --allow-version-regression`,
`merge-changes --simplify --allow-version-regression` (the flag requires
`--simplify`, the only merge-changes mode that compares versions), or
`CompactDiffOverlay::allow_version_regression(true)`.

**Rationale:** Positional precedence is defensible when the operator
controls the order, and indefensible as a silent default when they do not.
Regressing an object to an older version is data corruption that produces a
plausible-looking output file and a success exit code, so nothing
downstream can detect it; the overlay already stored the version, making
the check nearly free. Rejecting by default rather than warning follows the
project rule that a command must not report success having done something
other than what was asked. The escape hatch exists because replaying a
change stream backwards is a legitimate, if rare, thing to want.

## derive-changes: lossless delete roundtrip

**osmium behavior:** `osmium derive-changes` loses deletes when generating an OSC
from two PBFs. In Denmark cross-validation, osmium's OSC is missing 1243 deletes
that are present in the original diff.

**pbfhogg behavior:** `diff --format osc` produces a perfect roundtrip - applying
the derived OSC to the old PBF reproduces the new PBF exactly. As of
2026-07-10 this includes element metadata: derive emits the full
attribute set (version, timestamp, changeset, uid, user) and
apply-changes carries OSC metadata into its output, so the roundtrip is
metadata-lossless, not just content-lossless (pinned by
`derive_then_apply_preserves_metadata` in `tests/roundtrip_invariants.rs`).

**Rationale:** Not a design choice - osmium simply cannot represent certain deletes
when the deleted element is absent from both input files. pbfhogg's content-equality
diff captures all three change types (create, modify, delete) correctly.

## extract: relation inclusion criteria differences

**osmium behavior:** In complete-ways and smart strategies, osmium applies its own
heuristics for which relations to include and which additional nodes/ways to pull
in for relation completeness.

**pbfhogg behavior:** extract has expected differences in relation inclusion criteria
across all three strategies. Cross-validation shows 99.99% node/way match. In smart
mode, pbfhogg includes more way-referenced nodes while osmium includes more relations.

**Impact:** For the vast majority of use cases the output is equivalent. Edge cases
near extract boundaries may see slightly different relation membership. The node/way
coverage is effectively identical.

## extract: multi-extract config path roots

**osmium behavior:** A relative top-level `directory` is resolved from the
process working directory. `--directory` is prepended to each configured
`output` by string concatenation, including absolute output strings.

**pbfhogg behavior:** A relative top-level `directory` is resolved from the
config file's directory. With `-d`, relative outputs keep their configured
subpaths. An absolute output under the configured directory keeps its relative
subpath under the override; an absolute output outside that root is reduced to
its filename under the override.

**Impact:** The same config can write to different locations under the two
tools. In pbfhogg, `-d` keeps absolute outputs below the override directory
instead of treating the configured absolute path as a concatenated suffix.

**Rationale:** The config file owns its relative resource and output paths.
When `-d` is supplied, all configured outputs stay under the requested output
root, while paths already below the configured root retain their useful
subdirectory structure.

## renumber: orphan-reference handling

**osmium behavior:** When a way ref or relation member points to an object not
present in the input, osmium assigns a **new** sequential id to the orphan
target via its `id_map::m_extra_ids` overflow table. These ids continue past
the last in-input id for each type, so a Denmark run with 6,616,526 ways emits
orphan way refs as 6,616,527, 6,616,528, ... in the order they're first
encountered. Guarantees contiguous new-space output, at the cost of assigning
ids to "phantom" objects that don't exist in the output.

**pbfhogg behavior:** An orphan ref with old id X becomes `start + count + X`,
where `start` is that kind's start id and `count` the number of elements of
that kind in the input: the first id past the renumbered range, plus the old
id. Nodes, ways and relations are shifted independently, with no extra pass.
Two refs to the same orphan stay equal, the old id is recoverable as
`new - (start + count)`, and an orphan whose shifted id would pass the largest
64-bit id is an error. An orphan id therefore never names an element in the
output. Relation members of an unknown type (anything but node, way or
relation once the type is read as int32) keep their id unchanged.

**Cross-validation:** `brokkr verify renumber` on a Geofabrik-style extract
finds equal element counts and no node or way that differs; every difference
is in a relation. Those are attributed to orphan member refs - the members of
cross-border relations (admin boundaries, routes, TMC segments) - but the
harness counts relation diffs against a sanity threshold rather than checking
each one is an orphan ref.

**Rationale:** Passing orphans through with their old id, the earlier policy,
let an old id equal the new id of a different element, so a reference
silently pointed at the wrong object. osmium's sequential assignment needs
per-orphan state in first-encounter order; the shift is stateless,
deterministic under parallel rewriting, and keeps the old id recoverable.

**Start IDs below 1 are refused.** osmium accepts a negative start ID and
counts downward from it; pbfhogg requires every `--start-id` value to be at
least 1, since its outputs are positive-only like its inputs (see "Negative
input IDs rejected project-wide" below), and refuses before reading the input.

**Impact:** Downstream tools that assume output ids are contiguous in
`[start_id, start_id + N)` must tolerate orphan refs above that range.
Tools that only chase live references don't hit the orphan ids at all
(because no output element has the orphan id). Orphan ids also differ from
osmium's, which numbers orphans `count + 1, count + 2, ...` in the order it
meets them.

## check-refs: occurrence-counting vs unique-counting for missing references

**osmium behavior:** For missing relation-to-relation members, osmium reports the
total number of broken references (occurrences).

**pbfhogg behavior:** Reports unique missing IDs as the primary count, with the
occurrence count in parentheses when it differs from the unique count:
`Missing relation members: 706 (777 references)`. Also reports
`missing_relation_member_occurrences` in JSON output.

**Impact:** Both tools find the same set of missing IDs. The difference is
presentational - pbfhogg distinguishes "how many distinct IDs are missing" from
"how many references point to missing IDs." Users comparing numeric output between
the two tools should be aware that osmium's count corresponds to pbfhogg's
occurrence (parenthesized) count, not the primary count.

## Negative input IDs rejected project-wide

See [decisions/0002](decisions/0002-negative-ids-rejected-project-wide.md)
for the decision record (rationale, alternatives considered, migration
path). This section documents the resulting behavior difference from
osmium.

**osmium behavior:** Treats negative IDs as first-class. libosmium
defines a canonical `id_order` comparator with the order
`0 → negatives by absolute value → positives by absolute value` and
documents it as "the same ordering JOSM uses". `osmium renumber`'s
man page explicitly states *"Negative IDs are allowed, they must be
ordered before the positive IDs"*; `osmium sort`'s man page
documents the same negative-first output order. The libosmium
CHANGELOG calls out: *"These changes extend this ordering to
negative IDs which are sometimes used for objects that have not been
uploaded to the OSM server yet."* JOSM interop is a designed
feature, not a tolerated edge case.

**pbfhogg behavior:** Rejects negative input IDs project-wide.
Production PBFs (planet, Geofabrik extracts, applied OSC streams) are
positive-only, and several code paths rely on that invariant.

`diff` and `diff --format osc` are not among the sites below: they
accept negative IDs and order them by the canonical `id_order`. Their
parallel shard planners compare thresholds as raw `i64`, which agrees
with `id_order` only for non-negative IDs, so an indexed input holding
any negative ID takes the sequential block-pair merge for the whole
comparison instead (`src/commands/diff/shard.rs`).

**Sites enforcing the invariant:**

- `renumber` - **hard reject** of any negative node, way or relation
  id, and of any negative way ref or relation member ref, in the
  rewriters of `src/commands/renumber/wire_rewrite.rs`
  (`reframe_dense_with_new_ids`, `reframe_ways_with_new_ids`,
  `reframe_relations_with_new_ids`). Each returns an error naming
  the offending id. The check is unconditional, not
  indexdata-gated: a PBF whose per-blob indexdata advertises
  `min_id >= 0` while the payload contains negatives still errors
  cleanly rather than panicking in `chunk_for_atomic` or silently
  dropping bits.
- `getid` - **hard reject** at parse time. `parse_id_spec` in
  `src/commands/getid/mod.rs` rejects any negative id parsed from a
  CLI argument or `--id-file` text spec, returning an error naming
  the offending id and its kind (node/way/relation). osmium's
  `getid` rejects identically; its man page is explicit that the
  command does not work with negative IDs. The reject covers CLI
  args and id-spec text files; ids harvested from a separate PBF
  via `--id-osm-file` are not validated (the source PBF's contents
  are treated as opaque data, matching the project-wide stance that
  passthrough commands like `cat`/`sort`/`inspect` do not validate
  PBF payloads). The blob-prefilter (`IdSet::any_in_range` at
  `src/idset.rs`) clamps `min_id < 0` to 0 so a *target* PBF whose
  indexdata range straddles zero correctly screens in its positive
  portion - without the clamp the unsigned cast wraps and the blob
  is silently skipped, dropping legitimate positive matches.
  Negatives in the target PBF are still absent from output because
  `IdSet::set` is a silent no-op on negatives (no positive request
  id can match a negative element id).
- `add-locations-to-ways` (both backends) - **silent**, by
  inheritance from `IdSet`. Relation-member node IDs are collected
  into an `IdSet` that decides which untagged nodes to keep; the
  external backend's stage 4 also consults
  `relation_member_node_ids.any_in_range(min_id, max_id)` to skip
  node blobs that contain no relation-referenced nodes. The
  `any_in_range` clamp covers the straddling-blob case as for
  `getid` above; the per-id silent no-op on `IdSet::set` (and the
  matching abs-fold absence on the pbfhogg side) is the residual
  divergence below.

`inspect --show` accepts signed query IDs. It is a read-only lookup into the
PBF payload, not an ID set passed through `IdSet`; the shared typed-ID parser
preserves the sign and `getid` applies its own rejection afterward.

**Residual divergence (tags-filter, add-locations-to-ways):**
The shared root is `IdSet::set` in `src/idset.rs`, a silent
no-op on negative ids, where osmium's equivalent paths abs-fold via
`object.positive_id()` to collapse `-N` and `+N` into a single bit.

- `tags-filter`'s parallel-classify path silently **drops**
  negative-id ways from the output, where osmium's `tags-filter`
  silently **abs-converts** them. Pinned as the current state by
  `cli/tests/cli_negative_id_invariants.rs::tags_filter_handles_mixed_sign_ids`.
- `add-locations-to-ways` silently **omits** relation-member
  retention for any relation member whose id is negative (the member
  id is dropped at `IdSet::set` time, so the write pass doesn't know
  to keep the corresponding untagged node).
  osmium's `add-locations-to-ways` abs-folds the member ref via
  `member.positive_ref()`, collapsing `-N` and `+N` member refs
  into the same retention decision.

Both behaviors are silent; neither matches the project-wide reject
stance, and neither matches osmium's abs-fold. Promoting either to
a fourth hard-reject site is not planned: osmium's own behavior is
also a silent miscount (just a *different* silent miscount), so
neither tool offers a clean precedent for forced rejection here. A
user who needs deterministic behavior on mixed-sign input must run
`renumber` first to surface the negatives as a hard error.

**Rationale:** `IdSet` is the load-bearing data structure in
renumber - a bitmap indexed by unsigned id supporting `O(1)`
rank-based lookup and `O(n/64)` cross-worker merges. Supporting
negatives would mean either splitting each bitmap by sign (double
the bookkeeping, double the merge cost) or widening to a signed
offset-mapped index (extra indirection on the hot path). Neither
pays off against the actual demand, which is zero: no user has asked
for JOSM-staged input, and the canonical workflow for such data is
"upload to OSM, then re-extract."

**Migration path to osmium-style support.** If a consumer does need
JOSM-interop, the work is:

1. Route every ordering decision through `osm_id_cmp` in
   `src/osm_id.rs` (canonical `id_order`: 0 → negatives by abs value
   → positives by abs value), which sort, dedupe, merge-join and diff
   already use; the remaining raw `<` / `>` / `!=` on `i64` ids
   audited and switched.
2. `IdSet` either split into `positives` / `negatives` sub-bitmaps
   routed by sign at every call site, or widened to a signed index
   with the capacity cost that implies.
3. Renumber output numbering interleaves per `id_order` so the
   emitted sequence matches osmium's expectations.
4. DEVIATIONS.md entry inverts: claim alignment with osmium instead
   of deviation.

This is ~several days of work plus thorough regression testing
against JOSM sample files. Reverse the decision only on a real user
ask.

**Osmium's own gap.** `osmium derive-changes` has a symmetric latent
bug: after ordering-based merging by `operator<` (which is
canonical), the "same id?" check uses raw `it1->id() != it2->id()`
rather than the absolute-value comparator. Mixed-sign inputs can
mis-trigger there. pbfhogg's diff routes negative-ID inputs away from
its raw-compare shard planners instead; the gap is not pbfhogg-unique,
it is ecosystem-wide.

**Impact:** Users with JOSM-local staging data must resolve negative
IDs before running pbfhogg commands that touch the invariant sites.
This affects only hand-edited files that haven't been uploaded.

## getid and getparents: bare numeric IDs require an explicit type

osmium's `getid` and `getparents` default bare numeric IDs to nodes. In
pbfhogg, `--default-type` is optional and has no implicit value: a bare number
in command arguments or an ID text file is rejected unless the caller selects
node, way or relation explicitly. This is deliberate. A bare number carries
no element kind, and silently assigning it to node can select a different
object than the caller intended. The same parser serves both commands.
`parse_id_spec_inner` in `src/commands/getid/mod.rs` owns this rule; the CLI
leaves `default_type` unset unless the user supplies `--default-type`.

pbfhogg also accepts named typed forms such as `node/123`, `way/456` and
`relation/789`, alongside osmium's compact `n123`, `w456` and `r789` forms.
This is an additive spelling accepted by both commands and their text ID files.

This does not affect IDs collected from `--id-osm-file`, where each element's
kind is present in the PBF. The CLI reference documents the opt-in behavior.

## cat: multi-input header is reconciled, not blanked

`osmium cat` writes a default-constructed `osmium::io::Header` whenever it is
given more than one input file - in `command_cat.cpp::run`, only the
`m_input_files.size() == 1` branch copies the reader's header. Every input's
bounding box, replication timestamp, replication sequence number and
replication base URL is discarded.

`pbfhogg cat` reconciles instead. It reads every input's header and keeps only
what the inputs collectively support:

- **bbox**: the union, and only when *every* input declares one. An input
  without a declared box has unknown extent, so the union is unknown and the
  field is omitted.
- **replication base URL, and `source`**: kept when every input agrees on the
  value.
- **replication timestamp and sequence number**: kept only when every input
  carries the same timestamp, sequence number *and* base URL. They name a
  position in one feed; the same sequence number in two feeds is not one
  state.
- **optional features**: not sorted (a concatenation of several inputs is
  not in type-then-ID order, however each input is ordered);
  `LocationsOnWays` when every input declares it, since every way blob is
  copied with its coordinates; never the two `pbfhogg.*` prepass features,
  which a second input can falsify.
- **`HistoricalInformation`**: the logical OR, since it declares that history
  metadata *may* be present.

Both designs obey the same rule - a header must never claim more than the data
supports - and both can only ever understate. osmium applies it bluntly;
pbfhogg applies it per field. The practical difference is that a `cat` of two
adjacent extracts from the same replication source keeps a usable bbox and base
URL here and loses them under osmium.

This holds for every cat path, `--dedupe` included, which also requires each
input to carry exactly one OSMHeader, as its first blob.

This is a deviation from *osmium*, not from pbfhogg's own prior behaviour,
which was worse than either: the cat paths built the output header from the
first input alone, so the output advertised one input's bounding box over
data drawn from all of them.

## time-filter: the sequence number does not outlive the replication state

`osmium time-filter` (`command_time_filter.cpp`) lowers the output's
`osmosis_replication_timestamp` to the cutoff when the input's is later, and
keeps the rest of the input header, sequence number included. That sequence
number names the input's replication state, which the snapshot is no longer
at: an updater resuming from it would skip every diff between the cutoff and
the input's state.

`pbfhogg time-filter` lowers the timestamp the same way and drops the
sequence number whenever it cannot show the output is still at the input's
state: when it lowered the timestamp, and when the input has no timestamp to
compare against. A cutoff at or after the input's timestamp keeps both.

## tags-filter and inspect tags: expression-syntax edge cases

The matcher is built to match `osmium-tool/src/util.cpp::get_string_matcher`
and `get_tag_matcher` rule for rule, including the order in which the `!`
inversion marker is tested relative to whitespace stripping - the marker counts
only when it is the last character of the raw key, so `highway !=x` inverts
while `highway ! = x` means the literal key `highway !`. The edges below
deviate deliberately.

`inspect tags` and `tags-count` deliberately reuse the full tags-filter
expression parser, including its `n/`, `w/` and `r/` element-type prefixes.
Thus `w/highway` counts only `highway` tags on ways, subject to any additional
`--type` filter. Osmium's `tags-count` `add_matcher` sends the entire expression
to its tag matcher and has no type-prefix syntax; there, `w/highway` is treated
as a tag-key pattern. Use `--type way` for the equivalent kind restriction in
osmium. The pbfhogg parser is shared from `src/tag_expr.rs` by
`src/commands/tags_count/mod.rs`; the behavior is described in the CLI
reference.

**An empty key is an error, not an empty-string match.** `get_string_matcher`
returns `equal{""}` for an empty key, so osmium accepts `=value` and matches
tags whose key is the empty string. pbfhogg rejects `=value` and `!=value` with
`empty key in expression`. A PBF may technically carry an empty key, but an
expression that produces one is far more likely to be a typo than an intent,
and an error is recoverable where a silent zero-match is not.

**An unrecognised type prefix is treated as part of the key, not an error.**
`get_filter_expression` splits on the first `/` anywhere in the expression and
hands the prefix to `get_types`, which throws on any character outside
`n`, `w`, `r`, `a`. So `addr:full/name` is a hard error under osmium. pbfhogg
applies the type prefix only when the text before the `/` is non-empty and
every character of it is one of `n`, `w`, `r`, and otherwise treats the whole
string as the tag expression, so `addr:full/name` filters on that literal key.
The empty prefix is part of the same rule: osmium reads `/name` as the key
`name` applied to all types, while pbfhogg filters on the literal key `/name`.
One case is an error instead: a prefix that is valid only once its spaces are
stripped (`  w/highway`, `w /highway`) is refused, as osmium refuses it,
rather than becoming a literal key that matches nothing.

**A blank line in an expressions file is skipped.** Both tools cut a line at
its first `#`, remove one trailing `\r`, and strip only spaces from each half
of the expression, so a tab stays part of the key. osmium then turns a line
left empty (or holding only spaces) into an empty-key matcher; pbfhogg skips
it, consistent with refusing an empty key above.

**There is no area (`a`) type.** osmium's `get_types` accepts `a` and routes
matching expressions to a separate area filter. pbfhogg has no area concept in
`tags-filter`, so `a` is not a recognised prefix character and an expression
using it falls through to the key-literal rule above.

## getid: ID files tolerate a trailing carriage return

The ID-file reader follows `osmium-tool/src/id_file.cpp::read_id_file`: strip
spaces from both ends, cut at the first space or `#` (so `n123 # note` and
`n123 extra` are accepted), skip a line left empty, and reject anything else
the ID parser does not accept, including a tab. One difference: pbfhogg
removes one trailing `\r` before reading the line, so a CRLF file works.
osmium sheds the `\r` only when a space or `#` cut happens to remove it, so it
rejects `n123\r` and `n123#c\r` while accepting `n123 #c\r`, and rejects a
line holding only `\r`, which pbfhogg skips.
