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

## VER-135 - sort's required LocationsOnWays drop has no real-data witness

sort's declaration verdict is exact (`checks::sort::sort_way_locations` predicts the
blob planner, decoding every blob pass 1 would scan), but every drop branch (overlap
component, internally unsorted or restructured way blob on a `LocationsOnWays` input)
is witnessed by unit tests only: no configured dataset both declares
`LocationsOnWays` and makes sort rewrite a way blob. No producer makes one:
re-encoding `degrade` drops the declaration, and `cat A A` keeps it but duplicates
every identity, which the strict element comparison and order check refuse. A
witness needs overlapping way blobs with disjoint identities that keep inline
coordinates and true indexdata.

## VER-018 - Direct-route transports are attributed only where a fault cell drives the route

Every `fault_*_input` reference run asserts the exact transport of each pinned
open (reference/testing.md "Platform-dependent tests"). Residue:

- Direct routes no fault cell drives go unattributed: extract's unsorted, Smart
  and multi-extract routes (only sorted single extracts run direct), and sort and
  add-locations-to-ways, whose fault files have no direct route at all.
- CLI direct cells assert only that some direct open happened (`>0`).
- io_uring stays outside the gate's requirement (the host rule forbids an rlimit
  precondition), so on a default-memlock host its cells skip and only the opt-in
  `full_registered_pool_instrument` proves the pool.
- Refusal predicates are the only route to a backend skip by convention. A
  source-policy rule could confine backend-refusal inspection to the designated
  helpers; it could not prove an early return exercised the backend.

## VER-133 - Geocode admin and antimeridian paths the real-data check does not reach

`pbfhogg-verify geocode` judges all four answer classes; on the configured datasets two
paths stay out of reach:

- A relation whose outer or inner ways do not assemble uniquely (an endpoint shared by three
  or more member ends) leaves its whole member box unjudged at its level. On germany that is
  r16239 and r17511 at level 2 and r52343 at level 4 (southern Bavaria, the run prints them).
  Localising to the branching group's box is sound for containment, but the builder refuses
  the whole relation when its ring-search budget runs out, which only a branching group can
  cause. A conservative placement bound over the group's member count (about 531,804
  placements for seven members, under the 2^20 extra allowance) could certify small groups
  safe and localise them; whether these relations' groups are small enough is not yet known.
- The model refuses data near the antimeridian; unwrapping longitude would let an
  antimeridian dataset (the Fiji entry in notes/todo.md) be judged. No eligible dataset
  crosses it (the verify dataset list is denmark and germany).

Same-level competition is judged only by chance (germany judges one or two queries among
several containing areas); witnesses aimed at overlaps would need an overlap search over edge
crossings and interiors, not only vertices, since two areas can overlap with no vertex of
either inside the other.
