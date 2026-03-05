# pbfhogg deviations from osmium-tool

Intentional behavioral differences from osmium-tool. These are deliberate design
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
referenced as members into an `IdSetDense` bitset. During the write pass,
untagged nodes are kept if they appear in this set. The pass is skipped entirely
when `--keep-untagged-nodes` is set (all nodes are kept anyway).

## add-locations-to-ways: missing nodes tolerated by default

**osmium behavior:** Fails on missing node coordinates unless
`--ignore-missing-nodes` is passed.

**pbfhogg behavior:** Missing nodes are always tolerated. A `(0, 0)` coordinate
is substituted and the total count is reported in the summary line as
`missing locations`. No flag needed.

**Rationale:** Missing nodes are normal when processing extracts — ways near
extract boundaries reference nodes outside the extract. Failing by default
forces every user to discover and pass `--ignore-missing-nodes`, which is the
right behavior in virtually all cases. The substituted `(0, 0)` is consistent
with the Null Island sentinel used by `DenseMmapIndex` (see CORRECTNESS.md).
