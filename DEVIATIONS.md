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
byte-identical to `osmium export`. Positions follow RFC 7946's longitude,
latitude order and lie in its WGS 84 range (a node or way with an emitted
position outside it is skipped and counted, as osmium skips and counts an
invalid location), and Polygon rings are closed, counterclockwise and of
non-zero area. Full RFC 7946 geometry validity, such as freedom from
self-intersection, is not checked. osmium output is not a golden byte-level
oracle for this command.

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
untagged nodes are kept if they appear in this set. Members are matched by
their signed ID, so a member `n-7` keeps node `-7` only; osmium's
`--keep-member-nodes` folds each member onto its absolute value
(`positive_ref()`), and its man page says the option does not work with
negative IDs. The pass is skipped when `--keep-untagged-nodes` is set (all
nodes are kept anyway) unless `--inject-prepass` needs its member ways.

## add-locations-to-ways: missing nodes are an error

**osmium behavior:** Fails on missing node coordinates unless
`--ignore-missing-nodes` is passed. With it, a ref whose node is missing is
given libosmium's undefined location, replacing any inline coordinate it
carried, which its PBF writer stores as `2147483647` on
each axis under the default framing.

**pbfhogg behavior:** The same by default. `--ignore-missing-nodes` writes
every unresolved ref as `(0, 0)`; that is pbfhogg's own placeholder, not
osmium's output. Missing nodes used to be always tolerated, `(0, 0)` was
substituted, and the count appeared in the summary line as `missing
locations`.

**Why the deviation was withdrawn:** the substituted `(0, 0)` is Null Island, a
real coordinate off the coast of Ghana. The output is well-formed PBF and the
numbers are plausible, so no reader downstream can tell a placeholder vertex from
a genuine one - the summary line on stderr was the only signal, and it does not
survive a pipeline. Absence has to be reported as absence. `--ignore-missing-nodes`
still produces exactly the old output for callers who want it, and the summary
line is unchanged.

## add-locations-to-ways and apply-changes: a node at two coordinates is refused

**osmium behavior:** No policy. `add-locations-to-ways` stores every node
record in its location index; a duplicate ID at a different coordinate is
resolved by the index type (the dense index keeps the last record in stream
order, the sparse one returns whichever the unstable sort leaves first).
`apply-changes --locations-on-ways` keeps the first base record it meets.

**pbfhogg behavior:** A node ID the command uses to resolve a way location
that occurs more than once at different decoded coordinates (decimicrodegrees)
fails the run with invalid data, under every `--index-type`; nothing is
published. `--ignore-missing-nodes` does not waive it. Identical duplicates
are accepted, and duplicates of a node no way needs are not examined. For
`apply-changes`, only the base records looked up for the ways the OSC creates
or modifies are compared, and a node the OSC upserts takes its coordinate from
the OSC, so a conflicting base pair under it is not examined. Every examined
record is held to the geographic domain.

**Rationale:** Neither record is evidence that the other is wrong, and stream
order is not evidence either. Picking one wrote a coordinate that depended on
the index type and, for the sparse index and `apply-changes`, on which worker
ran last, so the same input gave different geometry from run to run. A refusal
depends only on the records, so every index strategy and every schedule refuses
the same inputs. The parity holds for correctly indexed input: a blob whose
indexdata hides it from the external index's node walk is outside it.

## apply-changes: missing LocationsOnWays nodes are an error

**osmium behavior:** Under `--locations-on-ways`, a reference of a way from
the change file whose location osmium finds neither among the change file's
nodes nor among the base nodes it looks up keeps the location its `<nd>`
carried. For an ordinary OSC `<nd>`, which carries none, that is libosmium's
undefined location, which decodes to `2147483647` on each axis under the
default framing. There is no warning and no option to fail. A reference to a
node the change deletes may be resolved from the delete's own `lat`/`lon` or
from the base's record of the node.

**pbfhogg behavior:** The command fails and does not publish its output. It
fails once the base's nodes have been read, before any way is written, and
the error counts the distinct unresolved nodes. A
reference from an OSC way to a node the OSC deletes counts as unresolved too.
`--ignore-missing-nodes` writes every unresolved reference as `(0, 0)` and
publishes the output; that is pbfhogg's own placeholder, not osmium's output.

**Rationale:** Null Island is a valid coordinate, so a placeholder-bearing
output cannot distinguish missing data from real data. Failure by default
matches `add-locations-to-ways` and keeps the lossy behavior explicit. The
escape hatch matters for incremental extracts, where a regional base plus a
full-planet change stream legitimately references nodes outside the region.

Only the references of ways the OSC creates or modifies are resolved against
the base's node records. A surviving base way is not: it keeps its inline
coordinates except where the OSC supplies a new one (next section), so it has
nothing to resolve. Node records for the references of changed ways must come
from the OSC or from the base, so a base made by `add-locations-to-ways`
without `--keep-untagged-nodes` can fail on an ordinary daily diff: a changed
way can reference unchanged untagged nodes whose records are in neither file.

## apply-changes: --locations-on-ways takes new locations from the OSC only

**osmium behavior:** `osmium apply-changes --locations-on-ways` builds a
location index from the change files: every version of every node in them,
deletes included (a delete has a location only when it carries `lat`/`lon`).
During the base's node pass it adds a base node's location when some way
version in the change files references the node and the index lookup returns
no location. Writing every way, untouched base ways included, it overwrites
each reference whose node has a defined location in the index and leaves the
others as they were. The index has ordering defects: during the base node
pass it is searched before it has been sorted, so an entry can be missed and
a base location added beside it, and after the sort at the base's node-to-way
transition a node with several defined locations resolves to the smallest
(longitude first), not to its latest version. The sort runs only when a base
way follows a base node, so on a base with no nodes or no ways the index is
never sorted and every lookup, for a node with a single version too, searches
an unsorted index.

**pbfhogg behavior:** The OSC holds one final state per node. A surviving
base way, whether in a blob the merge rewrites or one it would copy, takes
the OSC coordinate for each existing inline coordinate whose node the OSC
creates or modifies; a way with no coordinate arrays is left without them.
It keeps every other inline coordinate and is never resolved against base
node records. Ways the OSC creates or modifies take the OSC coordinates too,
and resolve their other references as in the previous section. A reference
in a surviving way to a node the OSC deletes keeps its coordinate and is
counted in the summary (`refs to deleted nodes kept`), in the way blobs the
run searched or rewrote; a diff that creates or modifies no node leaves
untouched way blobs unsearched.

**Where the coordinates differ:**

- A surviving way referencing a node the OSC deletes: osmium may write the
  delete's coordinate or the base node's; pbfhogg keeps the way's own and
  counts the reference. The OSM API refuses to delete a node a way still
  uses, so on a base matched to its replication diffs this does not occur.
- A surviving way whose inline coordinate for node N disagrees with the
  base's record of N, when the OSC leaves N unchanged and some way in the OSC
  references N: osmium overwrites the coordinate with the base record;
  pbfhogg keeps it. On a base whose inline coordinates agree with its node
  records the two are the same.
- A node with several versions in the change, or one an OSC way also
  references: pbfhogg uses the final state; osmium's choice depends on the
  index defects above (on a base with no nodes or no ways, any moved node).
- A surviving way with no coordinate arrays: osmium writes arrays anyway,
  with the index's location where it has one and the undefined location
  elsewhere; pbfhogg leaves the way without them.

pbfhogg also refuses a base whose header does not declare `LocationsOnWays`;
osmium does not check.

**Cross-validation:** pbfhogg does not repair a coordinate that was already
stale for a node the OSC does not carry, and osmium repairs one only in the
second case above, so compare outputs from a base whose inline coordinates
agree with its node records. A blob pbfhogg rewrites is written in the
default framing, so coordinates and timestamps from a source block with
non-default `granularity`, offsets or `date_granularity` are converted to
decimicrodegrees (truncated toward zero, as libosmium) and whole seconds
(rounded down, where libosmium truncates toward zero, so a negative
sub-second timestamp lands one second earlier than osmium writes it) there;
blobs copied unchanged keep their own framing. The summary's refresh counts cover the way blobs the
run searched or rewrote, not the whole file.

**Rationale:** The OSC is the only source of new coordinates, which is what
incremental maintenance of a consistent file needs. A delete under a live way
is a mismatch between base and OSC; it is left as it was, and counted where
the run searched the way, rather than resolved with a coordinate chosen for
a node the output no longer holds.

## diff: content equality vs version ordering

**osmium behavior:** Matches elements by `(type, id, version, timestamp)` ordering,
so same-id elements with different versions are reported as separate left/right
entries rather than as a modification. When two elements do match, equality is a
CRC over content plus `timestamp` (always) and `changeset/uid/user` (unless
`--ignore-attrs-*` flags are passed). Both behaviors can produce spurious diff
output when inputs have mismatched or absent metadata
([osmium-tool#93](https://github.com/osmcode/osmium-tool/issues/93)).

**pbfhogg behavior:** Compares elements field by field - node coordinates, tags,
refs, members. Metadata (version, timestamp, changeset, uid, user) is ignored entirely.
Two elements with the same type+ID are "same" if and only if their content is
identical. Inline `LocationsOnWays` way coordinates are not content by default,
while osmium's CRC folds each way node's location in when the file carries it;
`diff --locations` adds them, at osmium's 1e-7 degree resolution, for inputs
that both declare `LocationsOnWays`.

**Cross-validation:** 14-element discrepancy out of 59.1M on Denmark. These are
elements where osmium's version-based comparison disagrees with content comparison
- e.g., same version number but different coordinates, or different versions with
identical content.

**Rationale:** Content equality is deterministic regardless of metadata completeness.
It answers "did anything actually change?" rather than "which version is newer?"

## Change stream: version precedence is enforced, not merely positional

**osmium behavior:** `osmium apply-changes` (on a data file) and `osmium
merge-changes --simplify` resolve conflicts by version, not by position:
both reverse the change list and stable-sort it with libosmium's
`object_order_type_id_reverse_version` (type, ID, descending version, then
descending timestamp when both timestamps are valid), and keep the first
change per object. So osmium keeps the highest version, then the later
timestamp, and only among full ties the later change in input order.
`apply-changes` merges the sorted changes with the base under the same
order, so a base object with a higher version than every change survives.
Feeding diffs out of sequence order therefore does not downgrade objects;
osmium silently keeps the highest version, with no check and no warning.

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
later change wins by position, where osmium prefers the later timestamp
and falls back to position only on equal timestamps; the floor does not move, so a
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
`apply-changes` also holds the changes to the base it rewrites: where a
change replaces or deletes a base element that claims a version, the
highest version the changes claimed for that object (deletes included,
so `modify v9` then an unversioned delete still claims 9) must not be
lower than the base element's, else the run is a hard error naming both;
osmium keeps the base element silently. The check is on the collapsed
change set, not each change in turn: base v10 with changes v9 then v11
applies v11. Equal versions apply (the change wins, where osmium compares
timestamps), and either side claiming no version passes. A final change
below the base arises only when, for that object, the change set lies
wholly before the base - stale diffs, or a content-derived patch back to
an older snapshot, which needs the opt-out below. `merge-changes
--simplify` emits final records only, so a simplified change set can no
longer show a claim its final record does not carry.
The default `merge-changes` mode is untouched - it preserves the full
change stream in input order by design and resolves nothing, so there is
nothing to regress.

Opt into a purely positional rule (the last change seen wins, whatever
version it or the base claims; osmium has no such mode) with
`apply-changes --allow-version-regression`,
`merge-changes --simplify --allow-version-regression` (the flag requires
`--simplify`, the only merge-changes mode that compares versions), or
`CompactDiffOverlay::allow_version_regression(true)`.

**Rationale:** A change whose version is lower than one already seen means
the inputs are out of order. osmium hides that by keeping the highest
version; pbfhogg treats it as an operator error worth stopping for, since
an out-of-order input set usually means a diff was missed or misnamed, and
a success exit code on it lets nothing downstream notice. The overlay
already stored the version, making the check nearly free. Rejecting by default rather than warning follows the
project rule that a command must not report success having done something
other than what was asked. The escape hatch exists because replaying a
change stream backwards is a legitimate, if rare, thing to want.

## OSC input: the root level is stricter than libosmium

pbfhogg's OSC reader follows libosmium's section and object-child structure
(sections only under the root, only objects in sections, only `tag`, `nd`,
`member` and `bbox` / `bounds` in objects; see CORRECTNESS.md), but refuses
three things libosmium's XML reader accepts at the root level:

- **An object directly under `<osmChange>`**, outside any `<create>`,
  `<modify>` or `<delete>` section. libosmium reads it as a plain object;
  pbfhogg cannot know which change it was meant to be and refuses rather than
  guess.
- **A root other than `<osmChange>`.** libosmium also accepts `<osm>`, which
  is a data file, not a change file.
- **A root-level element with element children**, such as a `<changeset>`
  carrying `<tag>` or `<discussion>` children. libosmium parses changesets
  (and osmium's `apply-changes` / `merge-changes` then ignore them); pbfhogg
  has no changeset model, tolerates an unrecognised root-level element only as
  a childless leaf, and refuses any element nested inside it rather than
  silently discard structured content. A childless `<changeset/>` or
  `<bounds/>` is accepted and ignored.

## OSC input: coordinates with an exponent round from their exact value

pbfhogg's OSC reader stores a node coordinate as its exact decimal value
rounded half away from zero to decimicrodegrees. For a plain decimal that is
libosmium's answer too (`39.00704275` is `390070428` in both). With an
exponent the two differ: libosmium keeps at most eight fraction digits of the
mantissa and then applies the exponent, so `0.000000001e2` reads as 0 there
and as 1 in pbfhogg. The accepted spellings also differ: pbfhogg takes what
Rust's `f64` parser takes (a leading `+`, an exponent `+`, any digit count),
which libosmium refuses in part. The geographic range check is made on the
`f64` reading of the text, so a token long enough to defeat that parser's
exponent handling (hundreds of kilobytes) can be refused although its exact
value is in range.

## cat --dedupe: which copy survives, and repeats inside one input

`cat --dedupe` stands in for `osmium merge`, but they choose differently
among copies of one (type, ID, version). pbfhogg keeps the copy from the
earliest input, then the earliest frame, then the earliest position, and
never reads the timestamp or any other field. osmium merge orders its queue
by type, ID, version and then timestamp (when both are valid), and writes the
last copy of an equal run, so of two copies with differing timestamps it
keeps the later one, and with equal timestamps the choice is unspecified.
osmium also refuses an input holding the same ID and version twice, or a
descending version; pbfhogg accepts both, drops the repeat where its blob is
in an overlap run and passes it through elsewhere. Where every copy of an
object is identical, the two answers agree; osmium merge is not an oracle
for anything else.

## sort: records of one ID and version keep input order

osmium sort orders by type, ID, version and then timestamp when both
records carry a valid one, with a stable sort, and pbfhogg's `sort` orders by
type, ID and version alone: of two records of one ID and version it keeps
the order they came in (earlier blob first, then earlier position), never
reading the timestamp. The two agree wherever no ID repeats a version, which
is every snapshot and every well-formed history file. libosmium's comparison
is not a strict weak order there either (a record without a timestamp ties
with two that differ), so its own result for such records depends on the
input order.

## merge-changes and tags-filter OSC: a deleted node's location is both coordinates or neither

Both commands keep a delete's body as osmium does: tags, way node refs,
relation members and a deleted node's `lat` / `lon` are written back out.
They differ from osmium only in which locations they accept on a deleted
node. pbfhogg reads a delete's coordinates by the rule a created or modified
node's are read by, and refuses a deleted node that carries `lat` without
`lon` (or the reverse), an empty or unparsable value, or a value out of
range. libosmium parses the same attributes, but its writer emits a location
whenever either coordinate is defined (`Location::is_defined`), so a half
location in the input can come out as a different location in the merged
change. A re-emitted
change that is silently different from its input is the outcome pbfhogg
refuses. `apply-changes` keeps only a delete's identity and version claim,
so the location cannot change its answer; it accepts the same file.

A created or modified node whose `visible` reads exactly `false` is the
object's deleted state (apply-changes records it as a delete), so the same
rule covers it: no location is accepted, both coordinates are re-emitted,
and half a location or an unusable value is refused where apply-changes
accepts it. The node stays in its input section with its `visible`
attribute; libosmium's writer moves a hidden object into `<delete>`.

## merge-changes and tags-filter OSC: an explicit empty metadata attribute is written back

**osmium behavior:** libosmium holds a user as a plain string with no
absence, so `user=""` and no `user` read the same, and its XML writer omits an
empty user. It refuses an empty `uid`, `changeset` or `timestamp` on reading
(`string_to_ulong`, `parse_timestamp`), so no osmium command re-emits one.

**pbfhogg behavior:** the re-emitters write every metadata attribute a kept
change carried, empty included (an empty `visible` excepted, which every
consumer reads as absent). An empty `user=""` is written back, because
apply-changes records it as a blank-user claim distinct from no user, and
dropping it would change what the change claims once re-emitted. The
re-emitters do not validate `uid`, `changeset` or `timestamp`, so an empty one
passes through as malformed nonempty text does; apply-changes refuses it
directly and after re-emission alike.

## OSC input: an empty `visible` is read as absent

**osmium behavior:** libosmium's `set_visible` accepts only `true` and
`false`; `visible=""` throws, so osmium refuses the change file.

**pbfhogg behavior:** every OSC consumer reads `visible=""` as the attribute
absent: no visibility claim, and not on its own metadata. pbfhogg does no
strict true/false validation of `visible` (only `false` hides), and the
re-emitters never see an empty `visible` as present, so reading it as absent is
what keeps a change's claims the same applied directly and after
`merge-changes` or OSC `tags-filter`.

## OSC input: a DOCTYPE internal subset is refused

**osmium behavior:** libosmium hands the document to Expat, which parses an
internal DTD subset; libosmium refuses only entity declarations ("XML
entities are not supported"), so an internal subset holding only, say, an
`ATTLIST` declaration is read.

**pbfhogg behavior:** every OSC consumer refuses a `<!DOCTYPE>` that carries
an internal subset (a `[` outside its quoted literals), whatever it declares.
pbfhogg applies nothing from a DTD, but a declaration's values (an attribute
default, an entity value) are part of the document and can hold a character
reference XML 1.0 forbids; checking them would need a second grammar, for
input that neither pbfhogg's nor osmium's OSC writers produce. `<!DOCTYPE osmChange>` and an external
identifier (never loaded) are read as before.

## derive-changes: changes are decided by content, not version

**osmium behavior:** `osmium derive-changes` walks both inputs in
type/id/version/timestamp order (timestamps take part only when both are
valid). An object only in the old file is written as a delete, except where
the next new object has the same ID and a later kind: its "same id?" check
compares IDs without kinds, so old `n7 v1` against new `w7 v1` writes the
create of `w7` and no delete of `n7` (osmium 1.19.1). One only in the
new file is written as a create or modify, and two objects with the same type, id and
version (and timestamp, where both are valid) count as unchanged whatever
their content. So a content change that kept its version and timestamp is
missed, and a new version or timestamp with identical content is written as a
change.

**pbfhogg behavior:** `diff --format osc` decides by content - coordinates,
tags, way refs and relation members, exactly what `diff` compares (defined in
`crates/lib/src/content.rs`) - plus visibility, and ignores the rest of the
metadata. Applying the derived OSC to the old
PBF therefore gives a file whose content equals the new PBF's, so a `diff` of
the result against the new PBF reports no differences. Creates and modifies
carry each element's metadata (version, timestamp, changeset, uid, user, and
`visible="false"` when the element is hidden; an explicit `true` is not
written) and deletes carry its version, the same whether or not the inputs have indexdata;
`derive_then_apply_preserves_metadata` in `crates/lib/tests/roundtrip_invariants.rs`
pins that the metadata of a changed element survives the roundtrip.

What that check cannot see is metadata, and anything else outside the content
definition (inline way coordinates, the file header). An element whose only
change is its metadata (a version bump with identical content) is not a change
at all: it gets no OSC entry, and the applied result keeps the old file's
metadata for it. Visibility is the exception: an element whose `visible`
claim changes (visible to hidden, or back) with identical content is written
as a modify of the new record, carrying the new side's version. A hidden one
is printed `visible="false"`, which apply records as the object's deletion
with that version, rather than as a delete, which would carry only the old
version and let an older stale version resurrect it; an unhidden one restores
the object. A hidden record unchanged on both sides gets no entry. The content
roundtrip holds for snapshots of visible objects: a hidden record in the new
file that is new, changed or newly hidden is written as a hidden create or
modify, so the result lacks a record the new file still carries, and a
visibility-only change is now subject to every refusal of the modify writer
(an unrepresentable coordinate, timestamp or string). A further margin: a dense node with no version (stored
as version 0) prints only its non-zero metadata fields, because a zero there
cannot be told apart from the padding a node without metadata gets (see
CORRECTNESS.md, "Version 0 is not a version claim either"), so a zero-valued
field on such a node does not survive the roundtrip.

An ID that occurs more than once on either side (canonical order admits
it) is compared occurrence by occurrence, as `diff` does. A derived OSC
states each object once and applying it rewrites every occurrence, so a
create, modify or delete of such an ID is refused rather than written: no
OSC maps old `[n1 a, n1 b]` to new `[n1 a, n1 c]`. osmium orders repeats by
version and timestamp and writes what its walk finds. Repeats unchanged on
both sides are not a change and pass.

**Cross-validation:** pbfhogg writes a delete for every object only the old
file holds, matching by kind and signed ID; osmium does too except in the
same-ID, later-kind case above. A derived OSC is a snapshot delta, so neither
reproduces changes a replication diff for the same window also carries, such
as an object created and deleted between the two snapshots.

**Rationale:** Content equality is deterministic regardless of metadata
completeness, and matches what `diff` reports; see "diff: content equality vs
version ordering" above.

## extract: per-strategy membership against osmium

The inclusion rules themselves are stated in `docs/cli/commands.md`
"### extract". For a bbox region they match osmium extract's
(osmium-tool `strategy_*.cpp`) except in the two places below; everything
else - which nodes are inside, which ways and relations are matched, the
recursive parent relations of complete_ways and smart, smart's completion of
direct node and way members and their way nodes - is the same element set on
input without negative IDs (where osmium folds IDs onto their magnitude, see
"Negative IDs: pbfhogg answers by the signed ID").

**osmium behavior (simple):** A relation is written when its *first* node or
way member is in the extract; a later member does not count. Its manual says
"all relations referencing any nodes or ways already included", and osmium
fixed the identical early return for ways (a way starting outside the region)
but not for relations.

**pbfhogg behavior (simple):** A relation is written when *any* member node is
in the region or any member way is in the extract, as osmium documents and as
its complete_ways and smart strategies do.

**osmium behavior (smart):** Completes relations of `type=multipolygon` only,
unless given `-S types=...`.

**pbfhogg behavior (smart):** Completes `type=multipolygon` and
`type=boundary`, judged by the first `type` tag as osmium judges it. The output
equals `osmium extract -s smart -S types=multipolygon,boundary`. There are no
`-S` options: `types`, `complete-partial-relations`, `tags` and complete_ways'
`relations=false` are not offered.

**Impact:** Simple can hold relations osmium omits, each with its first node or
way member outside the extract and a later one inside. Smart holds the member
ways of boundary relations that reach into the region, and their nodes, which
osmium's default omits; on a city box that is the country, state and municipal
boundary rings around it. Measured on bygg, 2026-11-03, on the
denmark-20260220 and germany-20260224 extracts: denmark (box
12.4,55.6,12.7,55.8) simple 127 extra relations, smart 394 extra ways and 56307
extra nodes; germany (Berlin box 13.0,52.3,13.8,52.7) simple 628 extra
relations, smart 3030 extra ways and 219769 extra nodes; no other membership
difference in any strategy, and the smart outputs byte-identical (as OPL) to
osmium's with `types=multipolygon,boundary`.

**Rationale:** The simple rule follows osmium's documented behaviour rather than
the early return it already fixed for ways. Boundary completion is pbfhogg's
smart contract since the strategy was added: an administrative area cut at the
extract edge is as broken a polygon as a multipolygon is, and pbfhogg's own
`build-geocode-index` and `export` assemble `type=boundary` relations as areas.

**Cross-validation:** The equality above holds for bbox regions and for
polygon regions osmium reads the same way (see the next section for the
polygon input pbfhogg reads differently). A bbox edge token written with an
exponent is quantized from its exact value, where libosmium truncates the
mantissa after eight decimals before applying the exponent;
`0.000000001e2` is column 1 here and 0 there. Polygon membership was
measured on bygg, 2026-11-03, against osmium on denmark-20260220 with four
municipality boundaries exported from the same file (Copenhagen with its
Frederiksberg hole, Frederiksberg, Aarhus, Odense; 814 to 9694 vertices, every
vertex a node location) and on germany-20260224 with the Berlin boundary:
complete_ways and smart (`-S types=multipolygon,boundary`) write exactly
osmium's element sets, single and through `--config`, and simple differs
only by the relations described above.

## extract: polygon input beyond osmium

Polygon membership itself is osmium's (`docs/cli/commands.md` "### extract"
states the rule): vertices rounded as libosmium rounds them, rings oriented as
osmium orients them, a node on a vertex inside, the rest by osmium's integer
crossing test with one parity over every ring of a geometry. The differences
are in which polygon input is accepted and what it means.

**osmium behavior:** A FeatureCollection contributes its first feature only.
An open GeoJSON ring is used as given, without a closing segment. Polygons
crossing the antimeridian are documented as unsupported; an edge from 179 to
-179 is taken as spanning the globe. A coordinate array must hold exactly two
numbers, and a coordinate is checked after rounding, so `180.00000001` is
accepted as 180. A ring of zero signed area, such as a symmetric bow-tie, is
accepted.

**pbfhogg behavior:** Every feature of a FeatureCollection is read, and
features are unioned; the rings within one feature's Polygon or MultiPolygon
share osmium's single parity. An open ring is closed by repeating its first
position, and its orientation is judged on the closed ring. A geometry with
an edge spanning more than 180 degrees of longitude is unwrapped: each member
polygon's outer ring is unwrapped from its first vertex, each hole is moved by
whole turns into its own outer ring's longitude window, members are unioned
instead of sharing a parity, and a node is tested at its own longitude and one
turn east and west; the region's box spans every longitude. A crossing ring
whose unwrapped path does not close (it circles a pole) or that spans 360
degrees is refused. A coordinate array may carry a third number (altitude),
which is ignored. A coordinate outside the WGS84 domain before rounding, and
a ring of zero signed area, are refused.

**Impact:** A FeatureCollection of several features selects their union
where osmium selects the first. An open ring selects what its closed form
encloses; osmium's result for it has no closing edge. A polygon crossing the
antimeridian selects both sides of it. The refusals are errors, never a
different extract.

**Rationale:** A FeatureCollection of several regions is far more often
meant as their union than as its first member. An open ring is a common
GeoJSON slip, and closing it is the only reading with an interior. Splitting a
region at the antimeridian, as osmium's manual asks, is a burden pbfhogg can
carry for the user. A ring of zero signed area is a line, a point, or a
self-crossing ring whose lobes cancel (a symmetric bow-tie still has an
even-odd interior); refusing it treats that cancellation as the input
mistake it nearly always is, rather than extracting from it. Refusing a
coordinate outside the domain keeps one domain check for every coordinate
pbfhogg reads. None of this changes a polygon osmium supports, as
a closed, non-crossing single geometry.

## extract: multi-extract config path roots

**osmium behavior:** A relative top-level `directory` is resolved from the
process working directory. `--directory` is prepended to each configured
`output` by string concatenation, including absolute output strings.

**pbfhogg behavior:** A relative top-level `directory` is resolved from the
config file's directory. With `-d`, relative outputs keep their configured
subpaths. An absolute output under the configured directory keeps its relative
subpath under the override; an absolute output outside that root is reduced to
its filename under the override. "Under" is a lexical comparison by whole
path components, with any `..` in the configured directory's spelling kept as
written; the CLI makes a relative `--config` absolute first, so the comparison
is never between an absolute output and a relative root. The kept subpath must
name a file below the override: one holding `..`, or one that is empty (an
empty `output`, `.`, or the configured directory itself), is refused before
any work. Containment is lexical: a symlinked directory inside the override is
followed. A library caller of `parse_extract_config_with_output_directory`
passing a relative config path has an absolute output refused, since only the
caller knows the working directory.

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

**pbfhogg behavior:** Orphans are named by a per-kind bijection of the
absent ids onto the ids outside the renumbered range. Take every 64-bit id in
canonical order (`0`, then `-1, -2, ...`, then `1, 2, ...`); an orphan's
position among the ids absent from the input, counting from 0, is placed in
that order before the live range when it is smaller than the start's own
position, and after the live range otherwise. On positive input renumbered
from 1, an orphan X becomes X plus the number of input elements of its kind
above X, so an orphan past every input id keeps its id. Nodes, ways and
relations are mapped independently, with no extra pass. Two refs to the same
orphan stay equal, distinct orphans stay distinct and keep their canonical
order, an orphan never names an element in the output, and no id overflows,
whatever its sign or size. Recovering the old id needs the input's ids of
that kind. Relation members of an unknown type (anything but node, way or
relation once the type is read as int32) keep their id unchanged.

**Cross-validation:** `brokkr run --with pbfhogg pbfhogg-verify -- renumber`
proves every difference from osmium's output, record by record, to be an
orphan name and nothing else: both sides put every object at its renumbered
ID, every orphan ref lies outside its kind's live range on both sides, and
the orphan names correspond one to one per kind. On Geofabrik extracts only
relations differ - the members of cross-border relations (admin boundaries,
routes, TMC segments) - since the extracts carry complete ways. The check
does not compare pbfhogg's orphan mapping against the input's ids.

**Rationale:** Passing orphans through with their old id, the earlier policy,
let an old id equal the new id of a different element, so a reference
silently pointed at the wrong object. A later shift, `start + count + X`,
collided for a negative orphan and overflowed near the largest id. osmium's
sequential assignment needs per-orphan state in first-encounter order; the
bijection needs only the rank index the live ids already use, so it is
stateless and deterministic under parallel rewriting.

**Start IDs follow osmium's rule.** A positive `--start-id` counts up and a
negative one counts down (`-1, -2, ...`, which is canonical order, so the
output stays sorted), and 0 means 1, as in osmium. pbfhogg accepts both `i64`
endpoints as starts, which osmium's ID parser rejects, and refuses a start
whose last assigned id would leave the 64-bit range; osmium does not check
that, and its offset arithmetic can overflow near either end before the last
id does. (A rewritten block that grows past the format's size cap is refused
too, whatever the start's sign.)

**Impact:** Downstream tools that assume output ids are contiguous in the
live range must tolerate orphan refs outside it, below it as well as above
it. Tools that only chase live references don't hit the orphan ids at all
(because no output element has the orphan id). Orphan ids also differ from
osmium's, which numbers orphans on past the live range in the order it meets
them.

## check-refs: occurrence-counting vs unique-counting for missing references

**osmium behavior:** For missing node refs in ways (`Nodes in ways missing`)
and for missing relation-to-relation members, osmium reports the total number
of broken references (occurrences): every way node ref whose node is absent
counts, however often the same node is referenced. For node and way members of
relations it counts each missing ID once.

**pbfhogg behavior:** Reports unique missing IDs as the primary count for every
class. For missing node refs in ways and for relation-to-relation members the
occurrence count follows in parentheses when it differs from the unique count:
`Missing node refs in ways: 1 (2 references)`,
`Missing relation members: 706 (777 references)`, and JSON carries
`missing_node_ref_occurrences` and `missing_relation_member_occurrences`. The
`FAILED (N missing references)` total sums the unique counts.

**Impact:** Both tools find the same set of missing IDs. The difference is
presentational: for those two classes osmium's count corresponds to pbfhogg's
occurrence (parenthesized, or the bare count when they are equal) count, not
the primary count, and the verifier compares exactly that. Node and way members
of relations agree.

## Negative IDs: pbfhogg answers by the signed ID

See [ADR-0028](decisions/0028-negative-ids-are-handled-like-any-other-id.md)
for the rule: an object's identity is its kind plus its signed ID, and every
command handles a negative ID like any other. This section records where
osmium's answer differs.

**osmium behavior:** Treats negative IDs as first-class in its ordering.
libosmium defines a canonical `id_order` comparator with the order
`0 → negatives by absolute value → positives by absolute value` and
documents it as "the same ordering JOSM uses". `osmium renumber`'s man page
states *"Negative IDs are allowed, they must be ordered before the positive
IDs"*; `osmium sort`'s man page documents the same negative-first output
order. The libosmium CHANGELOG says: *"These changes extend this ordering to
negative IDs which are sometimes used for objects that have not been
uploaded to the OSM server yet."* Several commands, however, look IDs up by
their absolute value (`positive_id`, `positive_ref`), so `n-7` and `n7`
are one key to them; the paragraphs below name them.

**pbfhogg behavior:** Every command orders, selects, joins, ranks and
renumbers a negative ID by its signed value. `diff` and `diff --format osc`
order and cut by canonical keys (`crates/lib/src/commands/diff/units.rs`),
`sort` and `cat --dedupe` order canonically, and `inspect --show` takes
signed query IDs. `inspect --extended`'s `objects_ordered` judges
canonical order like `osmium fileinfo`, except that a repeated ID within a
kind is unordered here, where fileinfo reports it as ordered with
`multiple_versions`; `inspect --id-ranges` monotonicity is a numeric report
field. Per command:

**renumber numbers live objects as osmium does.** It renumbers negative
node, way and relation IDs, refs and members by canonical rank, as osmium's
`id_order` numbering does, requiring each kind in canonical order as
osmium's `CheckOrder` does. Orphan naming differs (see "renumber:
orphan-reference handling"), and so do the limits of the start, described
there.

**check is exact.** `check --refs` tracks and resolves negative node IDs
(and, with `--check-relations`, way and relation IDs) by their signed
value, as osmium's `check-refs` does with its second ID set for
negatives; no difference remains except that osmium stores `0` in that
second set and pbfhogg in the first, which changes no answer, and that
osmium cannot represent `abs(i64::MIN)`. `check --ids` judges order, and `--ids --full`
duplicates, by canonical order and signed ID, as libosmium's `CheckOrder`
and `osmium fileinfo -e` do, so an ordered file holding negatives passes.
`check --locations`, which osmium has no equivalent of, gives negative
nodes slots like any other.

**getid (including `--invert`) and getparents are exact.** They accept a
negative ID from every source (positional IDs, `--id-file`, `--id-osm-file`;
a bare `-7` under `--default-type` needs no `--`), and select, remove,
follow (`--add-referenced`: way refs, relation members, relations in
relations) and find parents by the exact signed ID; any `i64` is accepted,
both extremes included. osmium's `getid`, `removeid` and `getparents` refuse
a negative text ID ("This command does not work with negative IDs", also
in their man pages), refuse `LLONG_MIN` and `LLONG_MAX` as illegal IDs, and
match every other ID, reference and member by its absolute value
(`positive_id`, `positive_ref`): `--id-osm-file` with `n-7` selects `n7`,
`removeid n7` also removes `n-7`, and `getid -r` follows a reference to
`-N` as `N`. pbfhogg answers by the signed ID in each case.

**tags-filter is exact.** Default (reference-including) mode selects a
negative direct match, way ref, relation member, member way and nested
relation by its exact signed ID. osmium's `tags-filter` records every
matched and referenced way and relation ID, and every way node ref and
member ref, by its absolute value (`positive_id`, `positive_ref`), so a
matched `w-7` also writes `w7` and a ref to `n-7` also writes `n7`; its
`-R` mode consults no ID set and is exact, like pbfhogg's.

**Default tags-filter refuses what selection by ID cannot answer.**
osmium's manual says default mode works correctly on history files only
with `-R`, and enforces nothing: it writes every version of a matched way
or relation ID (nodes it tests per record). pbfhogg refuses a PBF
declaring `HistoricalInformation` before the first pass, and an
undeclared repeated ID whose records disagree about the expressions after
the second pass (the matching records counted in pass 1 differ from the
records pass 2 calls direct); repeats that agree are written in full.

**extract is exact.** Every strategy, single and through `--config`,
selects what its rules select (`docs/cli/commands.md` "extract") by the
exact signed ID: a negative node inside the region, a way or relation
matched through one, and where the strategy adds them, a matched way's
outside nodes, a smart relation's member ways and nodes, and parent
relations. osmium's
complete_ways and smart record selected IDs by absolute value: with `n7`
inside the region and `n-7` outside it, both write both nodes (osmium
1.19.1, where simple writes `n7` only), and both look parent relations up
by absolute value too. osmium's simple strategy matches
node refs and members by absolute value, so a way whose only refs are
`n-2` outside the region is written when `n2` is inside. osmium's
complete_ways and smart record a matched way's outside node refs unfolded
in an unsigned set, so a negative one stands for an ID near `2^64`; on
osmium 1.19.1 a matched way referencing one outside node `n-5` makes the
run fail "Out of memory".

**add-locations-to-ways is exact with both index types.** It joins a
negative node ref to its node, keeps an untagged negative relation-member
node, and under `--inject-prepass` marks a negative member way and pins a
negative shared node, each by the exact signed ID, whichever index runs,
the external one included (it keys IDs `<= 0` by magnitude in a second
key family of its buckets). osmium joins negative refs through a second
index keyed by absolute value, which is exact for the join except at
`i64::MIN`, whose negation overflows; its
`--keep-member-nodes` retention records members by absolute value
(`positive_ref()`), so a member `n-7` also keeps an untagged `n7`, and its
man page says the option does not work with negative IDs. pbfhogg keeps
only `n-7`.

**apply-changes --locations-on-ways is exact.** pbfhogg collects the refs
of changed ways, the change file's node locations and the base's node
locations by signed ID, and looks each ref up by its signed ID. osmium's
`apply-changes --locations-on-ways` keys its location index by absolute
value (`positive_id`, `positive_ref`) for the refs it collects, the change
file's nodes, the base fallback and the final lookup, so a way referencing
`n-7` can take `n7`'s location. (Which surviving base ways osmium refreshes
differs as well, for every sign: see "apply-changes: --locations-on-ways
takes new locations from the OSC only".)

**merge-changes --simplify orders canonically.** Its survivors are written
in kind then canonical ID order inside each action group; osmium sorts by
the same object order without the action groups (see `docs/cli/commands.md`
"merge-changes").

## getid and getparents: bare numeric IDs require an explicit type

osmium's `getid` and `getparents` default bare numeric IDs to nodes. In
pbfhogg, `--default-type` is optional and has no implicit value: a bare number
in command arguments or an ID text file is rejected unless the caller selects
node, way or relation explicitly. This is deliberate. A bare number carries
no element kind, and silently assigning it to node can select a different
object than the caller intended. The same parser serves both commands.
`parse_id_spec` in `crates/lib/src/commands/getid/mod.rs` owns this rule; the CLI
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

## apply-changes: the base URL survives, the position does not

`osmium apply-changes` (`command_apply_changes.cpp`) writes a fresh header:
the writing program and the user's `--output-header` values, no replication
field at all.

`pbfhogg apply-changes` drops the replication timestamp and sequence number
as osmium does - the output is past them, and an OSC carries no sequence
number to name the new position - but keeps the base URL. The URL names the
feed, not a position in it, and stays the output's provenance. It matters to
updaters that fall back to the newest object timestamp when the header has
no position: `pyosmium-up-to-date` and `osm2pgsql-replication init` (from
its import properties) then stay on the base's feed instead of switching a
regional extract to their planet-feed default. An explicit
`--output-header osmosis_replication_base_url=` drops it as osmium would.

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
`--type` filter. Osmium's `tags-count` `add_matcher` hands the entire
expression to `get_tag_matcher` and has no type-prefix syntax; there,
`w/highway` is treated as a tag-key pattern. Use `--type way` for the
equivalent kind restriction in osmium. The pbfhogg parser is shared from
`crates/lib/src/tag_expr.rs` by `crates/lib/src/commands/tags_count/mod.rs`; the behavior is
described in the CLI reference.

`inspect tags` otherwise counts what osmium's `tags-count` counts, by its two
filters (`research/osmium-tool/src/command_tags_count.cpp`): no expression and
an expression without `=` count keys, an expression with `=` (`key=*`
included) counts key=value pairs, and a tag both select counts in both rows.
The rows differ from osmium's in three places:

- **Quoting.** Osmium wraps the key and value in double quotes and doubles
  any quote inside, escaping nothing else, so a tab or newline in the text
  breaks its row framing. pbfhogg writes each as a JSON string literal
  (backslash escapes, the CLI reference has the rules), so a row always
  splits on tabs into two or three fields. The counts and the row order are
  osmium's; a byte comparison of the two outputs differs only on text holding
  a quote, a backslash or a control character.
- **`-e` with no positional expression.** Osmium decides "no expression" from
  the positional arguments alone, before it reads the `-e` file, so
  `osmium tags-count in.pbf -e file` counts every key and the file's
  expressions on top. pbfhogg treats the file's expressions and the
  positional ones as one list and counts every key only when that list is
  empty (an empty or comments-only file with no positional expression
  included), so `-e file` alone counts what the file names, as the same
  expressions typed on the command line would.
- **Keys holding a NUL byte.** Osmium names a key row `key` and a pair row
  `key` NUL `value` and sorts by those strings; pbfhogg sorts by key, then
  value, a key row first. The two orders agree for every key without a NUL
  byte (osmium cannot hold one, as its strings are NUL-terminated); with one,
  osmium's rows are ambiguous and pbfhogg's are not.

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

## getid: missing objects are reported, not an exit status

`getid -r` follows references as osmium's does
(`osmium-tool/src/command_getid.cpp`, `find_referenced_objects`): relation
members recursively through relations in relations, the node and way
members of every relation reached, then the nodes of every way requested or
pulled in, and `--remove-tags` strips every pulled-in object of any kind
that was not itself requested. Selection and tag stripping match osmium's
output on real data; the differences are in what a missing object does.

**osmium behavior:** every object still unfound at the end, requested or
pulled in by `-r`, counts toward `Did not find N object(s)`, and any such
object makes the command exit 1 (`return m_work_with_history ||
no_ids(m_ids)`), unless `-H`. A relation whose member lies outside an
extract therefore fails the run.

**pbfhogg behavior:** `getid` exits 0 whenever it wrote its output, found
or not. `-v, --verbose` reports the requested IDs that were not written,
requested ones only: an object `-r` reached that the file lacks is not a
miss (`print_missing_ids` in `crates/cli/src/lib.rs`, fed by `record_found` in
`crates/lib/src/commands/getid/mod.rs`, which records requested IDs of every kind).

**Rationale:** on any extract, members cut off at the border are the normal
case, so an exit status that fails on them reports the extract, not the
query; and the requested-ID list is the one a caller can act on. A negative
member reference is followed by its signed ID where osmium abs-folds it, see
"Negative IDs: pbfhogg answers by the signed ID".

## PBF decoding: a repeated Info or DenseInfo is refused, not merged

Protobuf merges repeated occurrences of a singular embedded message field:
scalars set later override, scalars set only earlier survive, and repeated
fields concatenate. pbfhogg does not implement that merge for an element's
`Info` or a dense group's `DenseInfo`. A second non-empty occurrence is a hard
error naming the field; an empty occurrence merges to nothing and is ignored.
This matches the refusal of a `PrimitiveGroup` carrying two `DenseNodes`
messages. No producer emits either shape, keeping the last occurrence silently
lost the earlier copy's fields, and a merge on the read paths would put a case
nobody produces on every one of them.

`cat --repair` is the one place the merge exists. It validates every data
blob, and rewrites a block refused for this shape into the canonical encoding
of the merged message: an `Info`'s non-empty bodies concatenated (a later
field overrides, the Osmosis `-1` mapping applied to the merged value), and a
`DenseInfo`'s columns joined one segment per column in occurrence order, its
other fields kept after them. Each occurrence must frame cleanly on its own
first, and the rewritten block is validated as any block is, so merged columns
longer than their group are refused. libosmium merges differently and is not
the reference here: its `Info` user comes from the last occurrence alone, and
each `DenseInfo` column from its last segment.

## PBF decoding: a repeated StringTable is refused, not merged

`PrimitiveBlock.stringtable` follows the same rule. Protobuf would merge two
occurrences by concatenating their `s` entries. pbfhogg refuses a second
occurrence whose body is non-empty, and ignores an empty body in either
position, in the decoder, the indexdata scanner (indexdata generation,
`cat --reindex`, `check --index`), renumber's wire splice and both
add-locations-to-ways way reframers. Blobs copied through without being
read (an indexed blob in plain `cat`, a relation blob in
add-locations-to-ways) are not inspected. Every block accepted therefore reads as
its protobuf merge. Earlier pbfhogg kept the last occurrence, which resolved
tags against the wrong strings with no diagnostic. libosmium refuses a further
occurrence once its table holds any entry, so the two differ at the edges: a
populated table followed by an empty one is accepted here and refused by
libosmium, and an occurrence carrying only unknown fields (non-empty body, no
entries) followed by a populated one is refused here and accepted by
libosmium. `cat --repair` has no rewrite for this shape.

## PBF decoding: a Blob with more than one payload field is refused

`Blob` carries its payload in one of `raw`, `zlib_data`, `lzma_data`,
`OBSOLETE_bzip2_data`, `lz4_data` and `zstd_data`. Protobuf would read a blob
holding several of them as the last one present. pbfhogg refuses any blob
carrying two payload occurrences, empty or not and the same field or not, with
an error listing the exclusive fields, at the one Blob parser every read path
shares.
They are alternative encodings of one payload, and two earlier pbfhogg parsers
disagreed about such a blob, so one blob decoded to two different blocks
depending on the read path; refusing removes the ambiguity rather than
choosing a winner. libosmium answers a third way as it walks the fields: it
returns on the first `raw` it meets, throws on an `lzma_data` or `zstd_data`
(and on `lz4_data` when built without LZ4), skips bzip2, and
otherwise keeps the last supported compressed field. No known writer produces
the shape. `reference/truncation-handling.md` records the
trade.

## add-locations-to-ways: nodes in a non-default-framed way blob are refused

A way blob is spliced, not rebuilt: its way coordinates are written in
decimicrodegrees and its `granularity`, `lat_offset` and `lon_offset` are
dropped, so the output block is in the default framing whatever the input's
(its `date_granularity` and timestamps are kept). A node in the same blob is
copied verbatim and would be misread under the dropped framing, so a way blob
in a non-default coordinate framing that also holds a `Node` or a non-empty
`DenseNodes` is refused, though protobuf and the PBF format allow it. osmium
rebuilds every block and accepts it; `repack` rebuilds the input one kind per
block (plain `cat` copies an indexed blob whole and does not). Neither the
pbfhogg nor the osmium writer produces a non-default framing.

## PBF decoding: a packed array is read from its one data-bearing occurrence

Protobuf lets a packed repeated field (`Way.refs`, `Relation.memids`, a
`DenseNodes` or `DenseInfo` column, an element's `keys` and `vals`, and the
rest) appear several times, packed or as individually tagged varints, and
defines the value as the concatenation of every occurrence in field order.
pbfhogg reads at most one occurrence that carries data: an empty occurrence is
ignored in either position, so every accepted shape reads as its protobuf
concatenation, and a second data-bearing occurrence is a hard error naming the
message and field. That refusal is the deviation from protobuf; the read path
is built on one borrowed slice per array, which cannot express a join, and no
known writer splits an array. It covers the element decoder and the scanners
routed through it (way and node scanners, indexdata generation, apply-changes
refresh, `check --locations`), renumber's wire splice and both
add-locations-to-ways way reframers. Because each unpacked value is its own
occurrence, unpacked support is exact for a one-element array and an error
beyond it. renumber also refuses the unpacked form of `DenseNodes.id`,
`Way.refs`, `Relation.memids` and `Relation.types` outright, since its splice
needs one packed byte range. `reference/truncation-handling.md` records why.

libosmium keeps the last occurrence of each packed field, empty or not, so the
two readers also differ on input pbfhogg accepts: an empty occurrence after a
data-bearing one erases the column for libosmium. A way whose `refs` end in an
empty occurrence has its node list here and none in osmium; an emptied
`Relation.memids`, `roles_sid` or `types` gives the relation no members there;
an emptied `DenseNodes.id` gives the group no nodes, and an emptied
coordinate column makes libosmium refuse the block. renumber and
add-locations-to-ways leave such an empty occurrence in place in what they
write, so their output carries the same shape. osmium comparisons on such
input can therefore disagree, check-refs and geocode among them.

## PBF decoding: required features are matched exactly

Like libosmium, pbfhogg refuses a HeaderBlock declaring a required feature
other than `OsmSchema-V0.6`, `DenseNodes` and `HistoricalInformation`, and
the refusal names the feature. libosmium compares with
`strncmp(known, feature, feature.size())`, which admits any proper prefix of
a known name (`Dense`, the empty string) and a known name followed by a NUL
and further bytes. pbfhogg compares exactly and refuses those. Unknown
optional feature names are accepted. Every HeaderBlock decode refuses, and
every command that reads elements or derives metadata from data blobs
decodes the header first, `inspect` included, as `osmium fileinfo` does.
The public indexdata and tagdata probes (`has_indexdata`,
`has_indexdata_in`, `has_tagdata`, and so `inspect --indexed`) decode the
header too, though they read no data payload, so no `inspect` mode answers
for a file the readers refuse. The one path that never decodes the header is
`degrade`'s header-only passthrough: it copies the OSMData payload bytes and
keeps the required-feature declarations in the header it forwards. A
command's internal indexdata precondition reads only framing and leaves the
header to the command's own reader, so a command may refuse missing
indexdata before it reaches the header refusal.

## PBF decoding: the header bbox appears at most once, complete

`HeaderBlock.bbox` (field 1, length-delimited) may appear once, and that
occurrence must carry all four of its `required` coordinates as varints; a
correctly typed zero counts, and a coordinate of another wire type is an
unknown field that does not. A missing coordinate, an empty occurrence and a
second occurrence of any kind are refused, by every HeaderBlock decode and
by the verifier's own header reader. Earlier pbfhogg read a missing
coordinate as 0 and let a later occurrence replace an earlier one, so a
complete box followed by an empty one read as `(0, 0, 0, 0)`.

Protobuf would merge repeated occurrences field by field, and libosmium
decodes each occurrence alone, refusing an incomplete one, and keeps them
all as a list (`Header::box` is the first, `osmium sort` joins them). The
two disagree whenever a repeat carries data, so pbfhogg reads neither way
and refuses. It therefore refuses legal protobuf that splits the box across
occurrences, and two complete boxes libosmium accepts; no known writer emits
either. A single complete box reads the same occurrence in both readers, but
not always the same value: libosmium truncates nanodegrees to its
100-nanodegree grid, normalises an inverted box by extending it with its
corners, and treats a coordinate equal to `i64::MAX` as absent, while
pbfhogg keeps the four stored values exactly. `degrade`'s header-only
passthrough does not decode the header, so `--strip-bbox` there removes a
malformed bbox without refusing it.

## PBF decoding: a group holding several element fields is read in kind order

The PBF format puts one kind of element in each `PrimitiveGroup`, but nothing
on the wire enforces it, and a group can carry plain `Node`, `DenseNodes`,
`Way` and `Relation` fields together, interleaved in any order. libosmium
decodes such a group in field order. pbfhogg reads it, without refusal, in
kind order: the group's dense nodes, then its plain nodes, then its ways, then
its relations. Each field keeps its own elements in wire order, and groups and
blocks stay in file order, so the two readers differ only inside such a group
and only when its fields are out of that kind order (a plain node before the
`DenseNodes` field, say). Every pbfhogg read path yields the same order
(`PrimitiveBlock::elements`, `for_each_element`, and `ElementReader`'s
sequential and pipelined callbacks), and every command inherits it: plain
`cat`, which rebuilds such a block into single-kind blocks, writes the dense
nodes before the plain nodes that preceded them on the wire
(`crates/cli/tests/cli_cat.rs::passthrough_reencode_writes_a_plain_before_dense_group_dense_first`,
`crates/lib/tests/read_paths.rs::mixed_field_group_is_read_in_kind_order_by_every_path`).
A command that checks element order (the `time-filter` history path,
`check --ids` on unindexed input) judges the kind-ordered sequence.

Protobuf gives the relative order of distinct field numbers in one message no
meaning, so neither order is the file's in a sense a conforming parser must
keep; following the wire would add a per-field state machine to the hottest
read path for a shape no known producer writes. Refusing such groups was also
rejected: libosmium reads them and plain `cat` is the repair recipe for blocks
no single index describes.

## inspect --extended: metadata coverage at the edges of fileinfo

The metadata census uses the value tests of `osmium fileinfo -e`
(libosmium's `detect_available_metadata`) on every object, plain and dense,
whatever its version. It differs only at the edges: a user index past the
string table counts as no user where libosmium refuses the file; timestamps
are judged in milliseconds, not truncated seconds; a user name is non-empty by
byte length, not by its first byte; versions and changesets libosmium refuses
are accepted and judged by value; an empty input reports `(none)` for "All
objects have"; and the timestamp range ignores zero timestamps. CORRECTNESS.md
"Metadata coverage in inspect --extended" gives the rule.

## Re-encoding: absent metadata fields stay absent

libosmium's object model has no absent metadata field: decoding reads an
absent version, timestamp, changeset or uid as 0 and an absent user as empty,
and the PBF writer then emits whichever fields its `add_metadata` option
selects, so an osmium round trip writes zeros the input never claimed.
`osmium cat --clean` likewise sets the cleaned fields to zero and writes them.
pbfhogg keeps the presence of each field of a plain `Info` through every
re-encoding command: an absent field is written absent, a present zero stays
a present zero, and `--clean` zeroes only the fields an element carries.
The Osmosis `-1` version and changeset are read field by field as absent,
as libosmium reads them, and the element's other fields are kept. A node
whose metadata is complete with a version of at least 1 is written dense with
all six columns; absent visibility on such a node becomes visible, the PBF
default, and a changeset of `-1` a dense 0. Any other node with metadata is
written dense with exactly its present fields as columns only where the
dense slot claims what its plain `Info` would (a DenseInfo column cannot
leave one node's field out, a versioned slot claims `visible` true without
its column, and a slot below version 1 claims only values no padding holds),
and only when its run is smaller dense; otherwise it is written as a plain
`Node`. Dense input nodes are re-encoded dense as decoded, with the
DenseInfo columns their group carried (a group with all five core columns
is written with all six); the Osmosis `-1` version and changeset are
written as 0, which is how libosmium reads them and how pbfhogg's metadata
outputs read them.

## PBF decoding: a short DenseInfo column is refused

libosmium's PBF writer emits each DenseInfo column independently under
`add_metadata` (`add_metadata=version+timestamp` writes two columns; a
history file written without metadata carries `visible` alone), and pbfhogg
reads any such subset as libosmium does: an absent column reads as 0, absent
`visible` as true, an absent user as the empty name. Where they differ is a
present column shorter or longer than the id array: libosmium tests each
column for emptiness per node, so it reads the missing tail as absent, while
pbfhogg refuses the block. Re-encoding keeps a partial-column group's
columns, dense, without adding any.
