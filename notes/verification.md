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

## VER-144 - Source-policy rules re-parse every file with syn, in debug

Measured 2026-11-18 on bygg (debug `brokkr test`): the single real-tree
audit test took 18.8 s of brokkr's fixed 20 s per-test watchdog before the
ID sign rule landed, and timed out in the parallel gate after it. Per rule:
loop_waits 4.1 s, host_independence 3.5 s, environment 3.5 s, test_modules
2.3 s, statics 1.5 s, library_text 1.4 s, corpus load 0.7 s; each rule
parses the files it needs with syn on its own. The audit is now one test
per rule over a shared corpus, so no single test owns the sum, but the
slowest rules still parse the same files repeatedly. Parsing each file
once in `Corpus` (lazily, shared by every structural rule) would cut the
total; a single rule growing past the watchdog is the residual risk.

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

## VER-155 - verify extract fails pbfhogg when osmium dies on a negative way ref

Reported by: spar partner during the OPL reader signed-ID work.

osmium's complete_ways and smart strategies record a matched way's outside
node refs signed into an unsigned set
(`research/osmium-tool/src/extract/strategy_complete_ways.cpp`,
`strategy_smart.cpp`), so an input whose matched way references a dangling
negative node (no negative element ID, which `checks/extract.rs` refuses
before any run) can make osmium exit "Out of memory" with no output. The
negative-reference refusal reads only completed osmium outputs, so this run
reaches `settled_reference` as an unaccepted exit, `Error::Failed`, and
the check reports FAIL (exit 1) against a pbfhogg that answered correctly.
Classifying it as the verifier's error needs the reference-run failure
contract (`crates/verify/src/error.rs`, `reference/verify.md` Results) to
distinguish an osmium limit from an incomplete reference, the open question
TOOL-006 also raises; a local stderr match in extract alone was rejected as
an exception to that contract. No verified dataset holds negative refs.
