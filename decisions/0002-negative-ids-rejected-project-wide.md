# ADR-0002: Negative input IDs rejected project-wide

Date: 2026-04-24
Amended: 2026-04-26 - getid hard-reject site added (`1cc7c2b`,
osmium parity); `IdSet::any_in_range` straddling-blob clamp added
(`16e3694`); `IdSet` silent-inheritance sites for tags-filter and
altw-external documented.
Amended: 2026-10-22 - the diff and derive-changes shard planners are
no longer an enforcement site: an indexed input holding any negative
ID takes the sequential block-pair merge, which orders by `osm_id_cmp`,
instead of relying on a `debug_assert` and mis-sharding in release.
Amended: 2026-10-25 - the sequential block-pair merge is gone; every
indexed comparison runs one engine over ID-range shards, and an input
holding a negative ID is planned as one unbounded shard of that engine,
which still orders by `osm_id_cmp`.
Amended: 2026-10-26 - `add-locations-to-ways --index-type external`
recorded as a hard-reject site for negative node IDs. It already refused
a node blob whose range is wholly negative; since `1db71d8a` its
metadata scan refuses any node blob whose indexdata range reaches below
zero, because stage 2 partitions and walks node IDs as unsigned values
and a mixed-sign range silently turned present nodes into missing ones.
The silent `IdSet` inheritance for negative relation-member refs is
unchanged.
Amended: 2026-10-26 - `check --locations` recorded as a hard-reject site
for negative way refs. Its slot store is keyed through an `IdSet`, so a
negative ref could not be compared, and a check that skipped it would
pass a stale coordinate unseen; it refuses the input instead. Negative
node records it is not asked to compare are still counted and
domain-checked.
Amended: 2026-10-27 - `check --refs` recorded as a hard-reject site for
every negative ID it compares (node, way under `--check-relations`,
relation, way ref, relation member). Its sets are `IdSet`s, so it used
to count a negative element as untrackable and report each reference to
it as missing; the untrackable counts are gone with the reject.
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
- **Hard reject of negative node ranges** in
  `add-locations-to-ways --index-type external` (amended 2026-10-26).
  Its metadata scan refuses a node blob whose indexdata range holds a
  negative ID, before any join work, naming the sparse index as the
  backend that joins such input. A cost of one compare per blob in a
  header-only scan.
- **Hard reject of negative way refs** in `check --locations`
  (amended 2026-10-26): a verifier cannot certify a comparison it
  skipped.
- **Hard reject of compared negative IDs** in `check --refs` (amended
  2026-10-27), for the same reason. The rejects sit where a set lookup
  or insertion has already failed, so valid input pays nothing, except
  relation-to-relation members, which are deferred without a lookup
  and take one sign compare each.
- **Routing, not enforcement**, for the shard planners in `diff` and
  `derive-changes` (amended 2026-10-22). Their threshold compares are
  raw `i64`, which order negative IDs wrongly, so the shard planner
  (`src/commands/diff/shard.rs`) plans any indexed comparison holding
  a negative ID as one shard with no window. This replaced a
  `debug_assert!` that left release builds to mis-shard silently.
- **Silent inheritance** through `IdSet::set`'s no-op-on-negative
  semantics for `tags-filter` (silently drops negative-id ways) and
  `add-locations-to-ways --index-type external` (silently omits
  retention for negative relation-member refs). Both differ from
  osmium, which abs-folds negatives via `positive_id()` /
  `positive_ref()` at the call site - itself a silent miscount,
  just a different one. Neither is promoted to a further
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
  signed offset-mapped index; renumber output numbering interleaved per `id_order`;
  a second regression suite against JOSM sample files. No user has
  asked for it. Reverse the decision if a real ask surfaces.
- **(c)** *[chosen]* - matches the existing `IdSet` design, ratifies
  what commit `ab01438` already established for renumber, and covers
  the latent shard-planner sites at zero runtime cost. The
  `debug_assert` catches misuse in tests; release builds trust the
  upstream chain.

If a real ask for JOSM staging-file interop surfaces, reopen this
ADR as `Superseded by NNNN` and migrate to alternative (b).
