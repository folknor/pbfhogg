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

## VER-018 - Backend cells outside the pinned-cell driver can return unwitnessed

Every reference-and-replacement-matrix cell of the `fault_*_input` files now runs
through `crates/lib/tests/common/pinned_cell.rs`, which owns the reference, its exact
transport list and the matrix, in both read modes, so those cells cannot end early
without a witness. Residue:

- The backend cells outside the driver still rest on the convention that a skip goes
  through a refusal predicate and a success through a positive witness: the
  roundtrip, `host_contract` and `process_nofile` direct and io_uring arms, the
  library unit cells (`test_support::refused` users), the CLI direct cells, the
  apply-changes `direct_reads_reach_every_base_open` cell (whose `return` on a
  classified refusal also drops the later backends of its loop) and
  `fault_uring_writer.rs`. Extending the driver's shape to them means a driver per
  kind of cell (writer path constructors, CLI children), each owning its run and
  its witness.
- The repeated-input cells (`cat a a`, `diff a a`, `derive a a`) and the
  replaced-before-the-call caller-pin cells are hand-rolled and run buffered only;
  their direct transports are unpinned.

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
