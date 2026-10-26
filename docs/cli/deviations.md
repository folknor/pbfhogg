# pbfhogg deviations from osmium

Intentional behavioral differences from osmium. These are deliberate design
choices, not bugs.

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
numbers are plausible, so nothing downstream can tell a placeholder vertex from a
genuine one - the stderr summary was the only signal, and it does not survive a
pipeline. `--ignore-missing-nodes` still produces exactly the old output for
callers who want it, and the summary line is unchanged. See
[Correctness](/guide/correctness).

## apply-changes: missing LocationsOnWays nodes are an error

**osmium behavior:** Under `--locations-on-ways`, a way reference osmium
cannot resolve keeps the location its OSC `<nd>` carried, normally none:
libosmium's undefined location, which decodes to `2147483647` on each axis.
There is no warning and no option to fail.

**pbfhogg behavior:** The command fails and does not publish its output. A
reference from an OSC way to a node the OSC deletes counts as unresolved too.
`--ignore-missing-nodes` writes every unresolved reference as `(0, 0)` and
publishes the output; that is pbfhogg's own placeholder, not osmium's output.

**Rationale:** Null Island is a valid coordinate, so a placeholder-bearing
output cannot distinguish missing data from real data. Failure by default
matches `add-locations-to-ways` and keeps the lossy behavior explicit. The
escape hatch matters for incremental extracts, where a regional base plus a
full-planet change stream legitimately references nodes outside the region.

Only ways the OSC creates or modifies are resolved against node records, so a
base made by `add-locations-to-ways` without `--keep-untagged-nodes` can fail
on an ordinary daily diff: a changed way can reference unchanged untagged
nodes whose records are in neither the base nor the OSC.

## apply-changes: --locations-on-ways takes new locations from the OSC only

**osmium behavior:** Overwrites each reference of every way it writes,
untouched base ways included, wherever its change-derived location index has
a location: the change file's nodes (deletes that carry `lat`/`lon`
included), plus base locations it adds, under conditions, for nodes that
ways in the change reference. The index has ordering defects: during the
base node pass it is searched before it has been sorted, so an entry can be
missed and a base location added beside it, and after the sort at the base's
node-to-way transition a node with several defined locations resolves to the
smallest (longitude first), not to its latest version. The sort runs only
when a base way follows a base node, so on a base with no nodes or no ways
every lookup searches an unsorted index, for a node with a single version
too. A way without coordinate arrays is written with them.

**pbfhogg behavior:** A surviving base way (one the OSC neither modifies nor
deletes), whether its blob is rewritten or copied, takes the OSC coordinate
for each existing inline coordinate whose node the OSC creates or modifies. It keeps every other inline coordinate, including one
for a node the OSC deletes, which is counted in the summary for the way
blobs the run searched or rewrote; a way without coordinate arrays is left
without them.

**Impact:** On a base whose inline coordinates agree with its node records
and a matching replication diff, both tools write the same coordinates
(osmium's index defects aside). They differ for a surviving way referencing a
deleted node (osmium may write the delete's or the base node's coordinate)
and for one whose inline coordinate disagrees with the base's node record
(osmium may overwrite it from the record).

## diff: content equality vs version ordering

**osmium behavior:** Uses version/timestamp ordering to determine which element is
"newer." Can produce wrong output when inputs have mismatched or absent metadata
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

## derive-changes: lossless delete roundtrip

**osmium behavior:** `osmium derive-changes` loses deletes when generating an OSC
from two PBFs. In Denmark cross-validation, osmium's OSC is missing 1243 deletes
that are present in the original diff.

**pbfhogg behavior:** `diff --format osc` roundtrips content: applying the
derived OSC to the old PBF gives a file whose content - coordinates, tags, way
refs and relation members, exactly what `diff` compares - equals the new PBF's,
so a `diff` of the result against the new PBF reports no differences. Creates
and modifies carry each element's metadata (version, timestamp, changeset, uid,
user) and deletes carry its version, the same whether or not the inputs have
indexdata.

What that check cannot see is metadata. `diff` ignores it, so an element whose
only change is its metadata (a version bump with identical content) is not a
change at all: it gets no OSC entry, and the applied result keeps the old
file's metadata for it. A further margin: a dense node with no version (stored
as version 0) prints only its non-zero metadata fields, because a zero there
cannot be told apart from the padding a node without metadata gets (see
[Correctness](/guide/correctness#version-0-is-not-a-version-claim-either)), so
a zero-valued field on such a node does not survive the roundtrip.

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

## renumber: orphan-reference handling

**osmium behavior:** When a way ref or relation member points to an object not
present in the input, osmium assigns a **new** sequential id to the orphan
target via its `id_map::m_extra_ids` overflow table. These ids continue past
the last in-input id for each type. Guarantees contiguous new-space output,
at the cost of assigning ids to "phantom" objects that don't exist in the output.

**pbfhogg behavior:** An orphan ref with old id X becomes `start + count + X`
for its kind: the first id past the renumbered range, plus the old id. Two
refs to the same orphan stay equal, the old id is recoverable by
subtraction, and an orphan id never names an element in the output. (Passing
orphans through unchanged, the earlier policy, let an old id collide with a
new one.)

**Cross-validation:** element counts match osmium and no node or way differs;
every difference is in a relation, attributed to orphan member refs.

**Impact:** Downstream tools that assume output ids are contiguous in
`[start_id, start_id + N)` must tolerate orphan refs above that range.
Tools that only chase live references don't hit the orphan ids at all.

## renumber: negative input IDs rejected

**osmium behavior:** Handles negative IDs (JOSM editor-local staging
identifiers) transparently, assigning them new sequential IDs like any
other element.

**pbfhogg behavior:** Rejects negative input IDs with an error. Negative
IDs are JOSM editor-local staging identifiers that are resolved before
upload to OSM - they never appear in production planet extracts or
Geofabrik downloads.

**Impact:** Users with JOSM-local staging data must resolve negative IDs
before renumbering.
