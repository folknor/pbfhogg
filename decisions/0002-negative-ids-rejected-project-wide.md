# ADR-0002: Negative input IDs rejected project-wide

Date: 2026-04-24
Status: Accepted

## Decision

Treat "all production PBFs are positive-only" as a project-wide
invariant. Enforce it with a hard reject at every site where a
negative ID could flow into `IdSet` (renumber node / way /
relation-member paths); gate latent shard-planner sites in `diff` and
`derive-changes` with `debug_assert!` on descriptor `min_id >= 0` at
planner entry. Do not redesign `IdSet` or canonicalize the
shard-planner compares today.

osmium goes the other way - negatives are affirmatively supported as
JOSM-staging IDs via a canonical `id_order` (0 → negatives by abs
value → positives by abs value). We diverge because `IdSet` is
unsigned-indexed and no user has asked for JOSM interop.

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
- `TODO.md` - all four negative-ID findings in the policy-clustered
  bug sweep marked landed.
- **Follow-up triggered by:** any user report of needing JOSM
  staging-file interop. If that lands, reopen this ADR as
  `Superseded by NNNN` and migrate to (b).

## Cross-references

- `DEVIATIONS.md` > "Negative input IDs rejected project-wide" -
  full behavior comparison with osmium, `IdSet` rationale, migration
  path.
- Commit `ab01438` (2026-04-23) - the earlier unconditional-reject
  landing this ADR builds on.
- Commit `f6834de` (2026-04-24) - negative-ID enforcement landing.
