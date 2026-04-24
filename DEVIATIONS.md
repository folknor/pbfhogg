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

## Negative input IDs rejected project-wide

See [decisions/0002](decisions/0002-negative-ids-rejected-project-wide.md)
for the decision record (rationale, alternatives considered, migration
path). This section documents the resulting behavior difference from
osmium.

**osmium behavior:** Treats negative IDs as first-class. libosmium
defines a canonical `id_order` comparator
(`include/osmium/osm/object_comparisons.hpp:87-110`) with the order
`0 → negatives by absolute value → positives by absolute value` and
documents it as "the same ordering JOSM uses"
(`include/osmium/osm/object.hpp:494`). `osmium renumber`'s man page
(`osmium-tool/man/osmium-renumber.md:23`) explicitly states
*"Negative IDs are allowed, they must be ordered before the positive
IDs"*; `osmium sort`'s man page documents the same negative-first
output order. The libosmium CHANGELOG calls out: *"These changes
extend this ordering to negative IDs which are sometimes used for
objects that have not been uploaded to the OSM server yet."* JOSM
interop is a designed feature, not a tolerated edge case.

**pbfhogg behavior:** Rejects negative input IDs project-wide.
Production PBFs (planet, Geofabrik extracts, applied OSC streams) are
positive-only, and several code paths rely on that invariant.

**Sites enforcing the invariant:**

- `renumber` - **hard reject** at every entry point where a negative
  id could flow into an unsigned `IdSet` operation. The node path
  checks `old_id < 0` before `set_atomic`
  (`src/commands/renumber/wire_rewrite.rs` `reframe_dense_with_new_ids`),
  the way path checks `old_way_id < 0` before `set`
  (`reframe_ways_with_new_ids`), and the relation-member-ref path
  checks `old_abs_id < 0` before `resolve`
  (`rewrite_relations_with_new_ids`). All three return an error
  naming the offending id. The check is unconditional, not
  indexdata-gated: a PBF whose per-blob indexdata advertises
  `min_id >= 0` while the payload contains negatives still errors
  cleanly rather than panicking in `chunk_for_atomic` or silently
  dropping bits.
- `diff` / `derive-changes` parallel shard planners
  (`src/commands/diff/parallel.rs::plan_shards`,
  `src/commands/diff/derive_parallel.rs::plan_shards`) -
  **`debug_assert!` only**. Threshold comparisons inside the
  planner and the shard hot path (`emit_side`, `merge_decoded`,
  `merge_up_to`) are raw `i64` compares rather than `osm_id_cmp`.
  For positive-only inputs the two agree; mixed-sign inputs would
  silently mis-shard. Release builds rely on the upstream chain
  (read → renumber/apply-changes → diff) never producing a
  mixed-sign PBF; debug builds flag the violation at planner entry.

**Rationale:** `IdSet` is the load-bearing data structure in
renumber - a bitmap indexed by unsigned id supporting `O(1)`
rank-based lookup and `O(n/64)` cross-worker merges. Supporting
negatives would mean either splitting each bitmap by sign (double
the bookkeeping, double the merge cost) or widening to a signed
offset-mapped index (extra indirection on the hot path). Neither
pays off against the actual demand, which is zero: no user has asked
for JOSM-staged input, and the canonical workflow for such data is
"upload to OSM, then re-extract." The shard-planner invariant
piggy-backs on the same justification: production upstreams never
introduce negatives, so the planners can use raw `i64` compare
without a canonical-compare layer.

**Migration path to osmium-style support.** If a consumer does need
JOSM-interop, the work is:

1. Introduce an `osm_id_cmp` (canonical `id_order`: 0 → negatives by
   abs value → positives by abs value) used everywhere an ordering
   decision is made; current uses of raw `<` / `>` / `!=` on `i64`
   ids audited and switched.
2. `IdSet` either split into `positives` / `negatives` sub-bitmaps
   routed by sign at every call site, or widened to a signed index
   with the capacity cost that implies.
3. Renumber output numbering interleaves per `id_order` so the
   emitted sequence matches osmium's expectations.
4. Shard planners drop their `debug_assert` and switch threshold
   compares to `osm_id_cmp`.
5. DEVIATIONS.md entry inverts: claim alignment with osmium instead
   of deviation.

This is ~several days of work plus thorough regression testing
against JOSM sample files. Reverse the decision only on a real user
ask.

**Osmium's own gap.** `osmium derive-changes` has a symmetric latent
bug at `osmium-tool/src/command_derive_changes.cpp:184`: after
ordering-based merging by `operator<` (which is canonical), the
"same id?" check uses raw `it1->id() != it2->id()` rather than the
absolute-value comparator. Mixed-sign inputs can mis-trigger there
too. Our debug_assert at least catches the violation loudly; osmium
does not. The pbfhogg finding is not pbfhogg-unique - it's an
ecosystem-wide gap.

**Impact:** Users with JOSM-local staging data must resolve negative
IDs before running pbfhogg commands that touch the invariant sites.
This affects only hand-edited files that haven't been uploaded.
