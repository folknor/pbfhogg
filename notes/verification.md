# Verification findings

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.

## VER-130 - The descriptor gate's charges are asserted, not observed

Residue, unobserved under the current design: the io_uring arm of the charge witness
on any host whose memlock limit refuses the writer; the light reserve
`TEST_LIGHT_FD_RESERVE` (a count in the shared binary is process-wide and light use
scales with runner threads, so a check would fail on large hosts); a ticket's units
versus its simultaneous whole runs (the ticket checks live grants, and an external
join requests only stage 1, so two joins on one unit overlapping after stage 1 pass);
test-local providers that answer `fd_allowance` outright (`fault_cat_input.rs`
`Grant` and `Fixed`) are light by convention.

## VER-018 - Fault files drive some routes buffered only, and a backend cell can return unwitnessed

extract, sort, add-locations-to-ways and getparents now drive every route direct
with an exact transport list. Residue:

- Routes the other fault files drive buffered but never direct, so their direct
  transports are unpinned: getid (forced unindexed `Include`; `AddReferenced`
  node-only, and relation-without-way on indexed and unindexed); tags_filter
  (two-pass without a selected relation, forced unindexed two-pass, the no-probe
  inverted and metadata-distrust variants of two-pass and `-R`); cat (plain,
  enriched plain with either input unindexed, type-filtered, untyped clean,
  type-clean with the first unindexed, dedupe with the first unindexed);
  check_ids (forced unindexed, ways-only, both modes); degrade (every decode
  variant but strip-locations, every passthrough variant but strip-indexdata);
  derive (gzip OSC, mixed-sign, the multi-worker fallback layouts); diff (the
  sequential, parallel and mixed-sign `diff`/`diff_stats` combinations, the
  multi-worker fallback layouts); geocode (forced unindexed); inspect
  (decode-only through a caller pin, `inspect_header`); repack (forced
  unindexed, LocationsOnWays, absent relation kind); tags_count (forced
  unindexed ways-only).
- Two reference runs assert less than an exact list: inspect's node-stats cell
  checks only that a direct read happened, and degrade's passthrough reference
  does not arm the hook at all.
- A backend cell can return early without a witness or a classified refusal and
  pass; `PBFHOGG_TEST_REQUIRE` only bites when a refusal predicate is reached. A
  token rule on `PlatformUnsupported` would not close it (reference/testing.md
  says why). The shape that would: a guard armed before each route's attempt,
  resolved only by a checked positive witness or a classified refusal, failing
  the test when dropped unresolved; its acceptance test is an early-return
  mutation of one route inside a loop.

## VER-133 - Geocode admin and antimeridian paths the real-data check does not reach

`pbfhogg-verify geocode` judges all four answer classes; on the configured datasets two
paths stay out of reach:

- The admin model assembles rings by walking member endpoints by node id and leaves a
  relation unmodelled (its whole member box unjudged at its level) wherever the builder's
  node-id assembly could differ: an endpoint shared by three or more member ends, a one-node
  member, a way listed twice in a role, an interior node shared within a role, a coordinate
  revisit or a ring of fewer than three coordinates. The builder withholds a relation whole on
  any topology refusal, so localising a mask to one group's box is not sound. On germany the
  branching masks were r16239 and r17511 at level 2 and r52343 at level 4 (southern Bavaria,
  before the guards widened; the run prints the current list). Closing it needs an
  independent oracle of the builder's node-id topology rules (bridges dropped, lobes split at
  shared nodes, its refusal classes), so these shapes are judged instead of masked.
- The model refuses data near the antimeridian; unwrapping longitude would let an
  antimeridian dataset (the Fiji entry in notes/todo.md) be judged. No eligible dataset
  crosses it (the verify dataset list is denmark and germany).

Same-level competition is judged only by chance (germany judges one or two queries among
several containing areas); witnesses aimed at overlaps would need an overlap search over edge
crossings and interiors, not only vertices, since two areas can overlap with no vertex of
either inside the other.
