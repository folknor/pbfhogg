# tags-filter relation member resolution plan

## Current behavior (as of 2026-03-04)

- `tags-filter` with default mode (`omit_referenced = false`) does two passes.
- Pass 1 collects:
  - directly matched node IDs
  - directly matched way IDs
  - directly matched relation IDs
  - node refs of matched ways
- Pass 2 writes:
  - directly matched nodes
  - node refs of matched ways
  - directly matched ways
  - directly matched relations
- It does **not** include relation member closure (member nodes, member ways, member relations, or way-node deps pulled via member ways).

This matches the TODO item for osmium-tool#215.

## Proposed semantics contract

Applies only when `omit_referenced = false` (default mode).

1. Seed set
- Seed relations are relations that directly match the tag expressions.

2. Relation closure
- For every included relation, include all of its members:
  - member node IDs
  - member way IDs
  - member relation IDs
- Relation members are recursive:
  - if a member relation is discovered, include it and traverse its members.
- Cycles are handled by a visited-relation set; traversal terminates.

3. Way expansion from relation closure
- For every included way (whether directly matched or included from relation members), include all referenced node IDs.

4. Node inclusion
- Include nodes if any of the following hold:
  - node directly matches
  - node is referenced by an included way
  - node is a direct relation member

5. `-R` mode behavior
- With `omit_referenced = true`, behavior stays unchanged: only directly matched elements are emitted, no dependency expansion.

6. Output ordering and uniqueness
- Output remains standard stream order from pass 2 (nodes, then ways, then relations as encountered in file order).
- Elements are emitted at most once (ID-set based inclusion).

## Implementation outline

Pass 1 becomes a small fixpoint over relation membership:

1. During scan, collect:
- directly matched IDs (`matched_node_ids`, `matched_way_ids`, `matched_relation_ids`)
- way refs for directly matched ways (`way_dep_node_ids`)
- relation member adjacency:
  - `relation -> member nodes`
  - `relation -> member ways`
  - `relation -> member relations`
- way refs map:
  - `way -> referenced nodes` (needed for ways pulled in by relation closure)

2. After scan, run closure:
- Initialize queue with `matched_relation_ids`.
- BFS/DFS over relation adjacency:
  - add member nodes to node include set
  - add member ways to way include set
  - enqueue unseen member relations

3. Expand included ways to nodes:
- For every way in final way include set, add `way -> refs` nodes to node include set.

4. Pass 2 writes using final include sets.

## Test matrix

Add integration tests in `tests/tags_filter.rs`.

1. relation_match_includes_member_way_and_nodes
- Input: relation R matched by tag, with member way W; W refs nodes N1,N2.
- Expect: R, W, N1, N2 are emitted.

2. relation_match_includes_direct_member_node
- Input: matched relation R with member node N.
- Expect: R and N emitted.

3. relation_match_includes_nested_relations_recursively
- Input: R1 matched, member relation R2, R2 member way W and node N.
- Expect: R1, R2, W, N plus way-ref nodes.

4. relation_cycle_terminates_and_includes_each_once
- Input: R1 matched, R1 -> R2, R2 -> R1.
- Expect: both relations emitted once, no infinite loop.

5. relation_member_way_not_tag_matched_is_still_included
- Input: matched relation includes an untagged way.
- Expect: way included due to dependency, not dropped.

6. omit_referenced_keeps_current_behavior
- Same graph as tests above, but `-R`.
- Expect: only directly matched elements; no closure members.

7. mixed_direct_and_dependency_ids_not_double_counted
- Node/way both directly matched and dependency-reached.
- Expect: single output instance per element ID.

## Suggested stats additions (optional)

Current stats are match-oriented. For observability, consider adding:
- `nodes_from_relations`
- `ways_from_relations`
- `relations_from_relations`

Not required for correctness; can be follow-up.
