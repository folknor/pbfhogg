# ADR-0002: Negative input IDs rejected project-wide

Date: 2026-04-24
Status: Accepted

## Context

Cluster 1 of the 0.3.0 bug sweep consolidated four findings about
negative OSM IDs:

- `renumber/wire_rewrite.rs:519-524` - the relation-member-ref path
  lets a negative ref flow through `resolve` unchanged and bumps the
  orphan counter, rather than rejecting like the node and way paths
  already do (commit `ab01438`).
- `diff/parallel.rs:138-142` + `derive_parallel.rs:136-142` - the
  shard planner builds thresholds via raw `i64` compare while the
  element-merge hot path uses canonical `osm_id_cmp` ordering;
  mixed-sign inputs would silently mis-shard.
- Two sibling findings in the same files for single-sided emit and
  `merge_up_to` bounds, same root cause.

`DEVIATIONS.md` already claimed pbfhogg "rejects negative input IDs"
as an intentional osmium divergence, with the rationale that renumber
uses `IdSet` bitsets indexed by unsigned IDs and supporting negatives
would need new data structure work. But the claim overstated coverage:
the reject only applied at the renumber node and way entry points.

An audit of libosmium and osmium-tool confirmed osmium goes the other
direction - negatives are affirmatively supported as JOSM-staging IDs
via a canonical `id_order` (`0 -> negatives by abs value -> positives
by abs value`) documented in `include/osmium/osm/object_comparisons.hpp`
and `osmium-tool`'s `renumber` / `sort` man pages.

Three options were on the table:

- **(a) Reject at input boundaries everywhere** - reject negatives
  wherever they enter the pipeline, not just at `IdSet` sites.
- **(b) Full osmium-style support** - adopt canonical `id_order`,
  split / widen `IdSet`, interleave renumbered output per osmium's
  convention.
- **(c) Document positive-only project-wide, hard-reject at `IdSet`
  entry points, `debug_assert` at shard-planner sites.**

## Decision

Option **(c)**. Treat "all production PBFs are positive-only" as a
project-wide invariant. Enforce it with a hard reject at every site
where a negative ID could flow into `IdSet` (renumber node / way /
relation-member paths); gate the latent shard-planner sites with
`debug_assert!` on descriptor `min_id >= 0` at planner entry. Do not
redesign `IdSet` or canonicalize the shard-planner compares today.

## Alternatives considered

- **(a) Reject at input boundaries everywhere.** Matches osmium least
  of all three options. We already reject at input for renumber; the
  other commands (diff, derive, sort, cat, getid, extract, etc.)
  don't have a natural input-boundary hook where a blanket "no
  negatives allowed" error would fit, and adding one would penalize
  every command for a problem that only two of them actually have.
- **(b) Full osmium-style support.** Matches the reference
  implementation and would unblock JOSM staging-file workflows. Real
  implementation cost: `IdSet` either split into `positives` /
  `negatives` sub-bitmaps with per-site routing, or widened to a
  signed offset-mapped index; shard planners switched to
  `osm_id_cmp`; renumber output numbering interleaved per `id_order`;
  a second regression suite against JOSM sample files. No user has
  asked for it. Reverse the decision if a real ask surfaces.
- **(c)** *[chosen]* - matches the existing `IdSet` design, ratifies
  what commit `ab01438` already established for renumber, and covers
  the latent shard-planner sites at zero runtime cost. The
  `debug_assert` catches misuse in tests; release builds trust the
  upstream chain.

## Consequences

- `renumber/wire_rewrite.rs` - added an unconditional `old_abs_id <
  0` reject at the relation-member-ref path, mirroring the node and
  way checks from commit `ab01438`.
- `diff/parallel.rs::plan_shards` and
  `derive_parallel.rs::plan_shards` - added `debug_assert!` on
  descriptor `min_id >= 0` at planner entry. One assertion subsumes
  the four downstream raw-compare sites (threshold build,
  single-sided emit, merge_up_to bound) because all of them are
  correct for positive-only inputs.
- `DEVIATIONS.md` - section renamed to "Negative input IDs rejected
  project-wide" and expanded with osmium's `id_order` design,
  pbfhogg's two enforcement classes, the `IdSet` rationale, the
  migration path if we ever reverse to (b), and osmium's own
  symmetric gap at `command_derive_changes.cpp:184`.
- `CHANGELOG.md` - broadened the pre-existing `renumber` negative-ID
  bug entry to cover both triggers (stale indexdata; inconsistent
  input with negative relation member refs).
- `TODO.md` - all four Cluster 1 items marked landed.
- **Follow-up triggered by:** any user report of needing JOSM
  staging-file interop. If that lands, reopen this ADR as
  `Superseded by NNNN` and migrate to (b).

## Cross-references

- `DEVIATIONS.md` > "Negative input IDs rejected project-wide" -
  full behavior comparison with osmium, `IdSet` rationale, migration
  path.
- Commit `ab01438` (2026-04-23) - the earlier unconditional-reject
  landing this ADR builds on.
- Commit `f6834de` (2026-04-24) - the Cluster 1 landing.
