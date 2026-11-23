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

## VER-150 - extract --clean has no test of its output

Reported by: spar accounting window 1 (lateral from the cat --clean version
dense-claims work).

`extract --clean` routes dense nodes through `clean_metadata` then
`add_node_dense` at two call sites of its own (`extract/common.rs`, for the
simple and complete strategies, and `extract/smart.rs`), and cleaning also
turns off the simple strategy's bbox raw passthrough and its single-pass
route. No test checks what any extract strategy writes under `--clean`:
`crates/cli/tests/cli_extract.rs` never passes `--clean`, the library tests
use cleaning only in `fault_extract_input.rs` (which checks input pinning,
not output metadata), `cancel.rs` says cleaning is not exercised, and
`pbfhogg-verify extract` does not run `--clean`. The shared rule is
unit-tested in `crates/lib/src/owned.rs` through `clean_metadata` directly,
so extract's wiring, the passthrough switch-off, and the claims of a cleaned
versioned dense node (kept zeros written plain) go unwitnessed. A CLI test
per strategy, mirroring the `cli_cat.rs` clean tests, would cover it.

## VER-151 - No test pins that a direct-I/O output is truncated to its logical size

Reported by: spar accounting window 1 (scout lateral from the DirectWriter
fail-stop work).

`DirectWriter` writes whole aligned buffers with `O_DIRECT`, zero-padding the
last one, and its final flush truncates the file back to the bytes actually
written (`set_len` to the logical size in `write/direct_writer.rs`).
`roundtrip_direct_io` in `crates/lib/tests/roundtrip.rs` (a
`PbfWriter::to_path_direct` roundtrip) carries the comment "Verify exact
file size - no padding left over", but asserts only that the file is not
empty. A regression that left the alignment padding in place would still
pass, since a reader may stop before trailing zeros. The test should compare
the file length with the bytes written (or with a buffered write of the same
blocks), as its comment says it does.

## VER-152 - DirectWriter under PbfWriter::new is driven by no test

Reported by: spar accounting window 1 (scout lateral from the DirectWriter
fail-stop work).

`DirectWriter` is public and can be handed to the synchronous
`PbfWriter::new(writer)`, but its only in-tree caller is
`FileWriter::direct_in`, under the pipelined direct constructors; no test or
command wraps it in `PbfWriter::new`. On that route `PbfWriter::flush` keeps
no error of its own (its `flush_error` is set only for a pipelined writer),
so what a failed and retried flush reports depends entirely on
`DirectWriter`'s own fail-stop state (a failed writer refuses later writes
and flushes). That contract is unit-tested inside `direct_writer.rs`, but
not through `PbfWriter::new`, where `finalize_writer` runs on every flush. A
test writing blocks through `PbfWriter::new(DirectWriter::create(...))`,
including a failed-flush case under the test hooks, would cover the route.

## VER-153 - The OPL relation reader refuses canonical order with negative IDs

Reported by: scout lateral from the inspect ordering work.

`crates/verify/src/verify_opl.rs` refuses an osmium-written OPL relation
whose ID is numerically at or below the previous one ("does not ascend").
Canonical OSM order (ADR-0028, osmium's `id_order`) puts `-1` before `-7`
before `1`, so a correctly sorted file with negative relation IDs would be
refused for descent. No verified dataset holds negative IDs today, so it has
not fired. The comparison should be strict canonical ascent; the verify
crate does not link pbfhogg, so it needs its own copy of the order.
