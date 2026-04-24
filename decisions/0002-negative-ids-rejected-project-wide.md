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
unsigned-indexed and no user has asked for JOSM interop. The full
behavior comparison, `IdSet` rationale, and reversal migration path
live in `DEVIATIONS.md > "Negative input IDs rejected project-wide"`.

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

If a real ask for JOSM staging-file interop surfaces, reopen this
ADR as `Superseded by NNNN` and migrate to alternative (b).
