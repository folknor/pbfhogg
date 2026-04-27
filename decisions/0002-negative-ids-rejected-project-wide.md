# ADR-0002: Negative input IDs rejected project-wide

Date: 2026-04-24
Amended: 2026-04-26 - getid hard-reject site added (`1cc7c2b`,
osmium parity); `IdSet::any_in_range` straddling-blob clamp added
(`16e3694`); `IdSet` silent-inheritance sites for tags-filter and
altw-external documented.
Status: Accepted

## Decision

Treat "all production PBFs are positive-only" as a project-wide
invariant. Enforcement is layered by site:

- **Hard reject at input boundary** in `renumber` (every entry
  where a negative id could flow into an unsigned `IdSet` op:
  node, way, and relation-member-ref paths) and in `getid`
  (`parse_id_spec` for CLI args and `--id-file` text specs; ids
  harvested via `--id-osm-file <pbf>` are passthrough payload and
  not validated). The getid reject aligns with osmium's getid,
  whose man page documents the same restriction; it is the one
  enforcement site in this ADR that converges with osmium rather
  than diverging from it.
- **`debug_assert!`** on descriptor `min_id >= 0` at planner entry
  for the shard planners in `diff` and `derive-changes`. The
  threshold compares inside the shard hot path are raw `i64`;
  mixed-sign inputs would silently mis-shard. Release builds rely
  on the upstream chain (read → renumber/apply-changes → diff)
  never producing a mixed-sign PBF.
- **Silent inheritance** through `IdSet::set`'s no-op-on-negative
  semantics for `tags-filter` (silently drops negative-id ways) and
  `add-locations-to-ways --index-type external` (silently omits
  retention for negative relation-member refs). Both differ from
  osmium, which abs-folds negatives via `positive_id()` /
  `positive_ref()` at the call site - itself a silent miscount,
  just a different one. Neither is promoted to a fourth
  hard-reject site because osmium offers no clean precedent for
  forced rejection on these paths.

The shared `IdSet::any_in_range` blob prefilter clamps `min_id < 0`
to `0` and short-circuits on `max_id < 0`, so a target PBF whose
indexdata range straddles zero correctly screens its positive
portion. Without the clamp the unsigned cast wrapped past `max`
and the prefilter silently dropped the whole blob - affecting the
getid include/invert prefilter and the ALTW stage 4 relation-member
node-blob skip.

Do not redesign `IdSet` or canonicalize the shard-planner compares
today.

osmium goes the other way for the renumber/sort/diff cluster -
negatives are affirmatively supported as JOSM-staging IDs via a
canonical `id_order` (0 → negatives by abs value → positives by
abs value). We diverge because `IdSet` is unsigned-indexed and no
user has asked for JOSM interop. The full behavior comparison,
per-site enforcement table, `IdSet` rationale, and reversal
migration path live in `DEVIATIONS.md > "Negative input IDs
rejected project-wide"`.

## Alternatives considered

- **(a) Reject at input boundaries everywhere.** Matches osmium least
  of all three options for the renumber/sort/diff cluster. We do
  reject at input for renumber and getid (the latter aligning with
  osmium); the remaining commands (diff, derive, sort, cat, extract,
  etc.) don't have a natural input-boundary hook where a blanket "no
  negatives allowed" error would fit, and adding one would penalize
  every command for a problem that only a few of them actually have.
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
