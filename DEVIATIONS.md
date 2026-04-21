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

## add-locations-to-ways: missing nodes tolerated by default

**osmium behavior:** Fails on missing node coordinates unless
`--ignore-missing-nodes` is passed.

**pbfhogg behavior:** Missing nodes are always tolerated. A `(0, 0)` coordinate
is substituted and the total count is reported in the summary line as
`missing locations`. No flag needed.

**Rationale:** Missing nodes are normal when processing extracts - ways near
extract boundaries reference nodes outside the extract. Failing by default
forces every user to discover and pass `--ignore-missing-nodes`, which is the
right behavior in virtually all cases. The substituted `(0, 0)` is consistent
with the Null Island sentinel used by `DenseMmapIndex` (see CORRECTNESS.md).

## apply-changes: permissive missing-element semantics

**osmium behavior:** `osmium apply-changes` validates that OSC operations
reference elements consistent with the base PBF. Modify on an absent ID and
way refs to absent nodes are user-facing errors unless explicitly overridden.

**pbfhogg behavior:** Every missing-element case is tolerated silently. No
flag needed.

| OSC op | Element state in base | pbfhogg outcome |
|---|---|---|
| `<create>` on existing ID | present | Silent overwrite (treated as modify). Base record is replaced with the OSC record. |
| `<modify>` on absent ID | absent | Silent insert (treated as create). OSC record is written. |
| `<delete>` on absent ID | absent | Silent no-op. |
| way/relation ref to absent node | absent from base AND from OSC | Under `--locations-on-ways`: `(0, 0)` sentinel coord and the `loc_missing` counter in the summary increments. Without `--locations-on-ways`: the ref is written bare, no coordinate lookup attempted. |

**Rationale:** Identical to the ALTW entry above. The motivating
workload is incremental-extract - region-extracted base PBF plus a
full-planet daily OSC, then re-extract by bbox. Such pipelines
routinely reference OSC elements (nodes outside the region, ways
whose refs extend outside the region) that are not in the base.
Failing by default would force every such user to discover and pass
an opt-out flag, which is the right behavior in virtually all cases.

The `(0, 0)` coordinate sentinel under `--locations-on-ways` is
consistent with the Null Island convention used elsewhere in the
codebase (see CORRECTNESS.md "Null Island ambiguity in dense mmap
index"); ways referencing nodes exactly at Null Island are
indistinguishable from ways referencing absent nodes. This affects
zero real-world nodes (nearest land ~570 km).

**Implementation:** Upsert semantics are anchored in
`src/commands/apply_changes/rewrite_block.rs` (the walker treats
`diff.get_node/way/relation(id)` hits as replacements regardless of
whether the ID was in base). Delete no-ops arise naturally - the
`deleted_nodes` / `deleted_ways` / `deleted_relations` sets are only
consulted while walking base elements, so a delete of an absent ID
has nothing to skip. The `(0, 0)` fallback under
`--locations-on-ways` is at
`src/commands/apply_changes/element_writes.rs` (search
`locations.push((0, 0))`); the corresponding counter is
`MergeStats::loc_missing` in `src/commands/apply_changes/stats.rs`.

**Test coverage:** `tests/apply_changes_invariants.rs` pins the
three non-ALTW scenarios (create-on-existing, modify-on-missing,
delete-on-missing). The ALTW `(0, 0)` fallback is covered by the
Denmark byte-equal cross-validation against osmium.

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

**pbfhogg behavior:** `diff --format osc` produces a perfect roundtrip - applying
the derived OSC to the old PBF reproduces the new PBF exactly.

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

## renumber: orphan-reference handling

**osmium behavior:** When a way ref or relation member points to an object not
present in the input, osmium assigns a **new** sequential id to the orphan
target via its `id_map::m_extra_ids` overflow table. These ids continue past
the last in-input id for each type, so a Denmark run with 6,616,526 ways emits
orphan way refs as 6,616,527, 6,616,528, ... in the order they're first
encountered. Guarantees contiguous new-space output, at the cost of assigning
ids to "phantom" objects that don't exist in the output.

**pbfhogg behavior:** Orphan refs pass through with their old id. The output
contains a mix of new-space ids (for in-input targets) and old-space ids
(for orphans).

**Cross-validation:** Denmark: 306 relations differ, all in their `member`
list only. No nodes, ways, or other relation fields differ. The 306 affected
relations are all transboundary admin boundaries, route relations, and TMC
(Traffic Message Channel) segments - all expected to have cross-border
member references. Total match: 59,151,976 / 59,152,282 elements (99.9995%).

**Rationale:** Both policies are defensible. pbfhogg preserves the original
id space boundary (a ref to old id `X` always means "the thing that had
id X in the input" - whether or not X is in the output), while osmium
produces contiguous new-space output at the cost of introducing ids that
reference nothing. pbfhogg's choice is the lower-surprise default: the
output never contains ids for objects that aren't in the output.

Users who need osmium-compatible orphan handling can add a followup
`--orphan-policy {preserve,assign}` flag.

**Impact:** Downstream tools that assume output ids are contiguous in
`[start_id, start_id + N)` must tolerate orphan refs outside that range.
Tools that only chase live references don't hit the orphan ids at all
(because no output element has the orphan id).

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

## renumber: negative input IDs rejected

**osmium behavior:** Handles negative IDs (JOSM editor-local staging
identifiers) transparently, assigning them new sequential IDs like any
other element.

**pbfhogg behavior:** Rejects negative input IDs with an error. Negative
IDs are JOSM editor-local staging identifiers that are resolved before
upload to OSM - they never appear in production planet extracts or
Geofabrik downloads.

**Rationale:** The renumber implementation uses `IdSet` bitsets indexed
by unsigned ID for O(1) rank-based lookup. Negative IDs would require
either a separate data structure or offset mapping. Since negative IDs
are never present in real-world inputs, the complexity isn't justified.

**Impact:** Users with JOSM-local staging data must resolve negative IDs
before renumbering. This affects only hand-edited files that haven't been
uploaded.
