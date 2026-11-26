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

## VER-133 - Geocode antimeridian and same-level paths the real-data check does not reach

`pbfhogg-verify geocode` judges all four answer classes, and its admin model now assembles
rings by an independent oracle of the builder's node-id rules. Two paths stay out of reach
on the configured datasets:

- The model refuses data beyond 170 degrees of longitude or 85 of latitude, and any member
  edge spanning half a turn, as a verifier error. The check runs over every complete, located
  boundary relation of the decoding, not only those near queries, so a region extract holding
  one far-flung national boundary errors whole. Unwrapping longitude touches every class
  (address, street, interpolation, building span, admin containment, bands, boxes, grid
  cells); it can be built and tested on a fixture through `geocode --input`. Judging real data
  across the seam (the Fiji entry in notes/todo.md, bz pbfhogg#75) additionally needs Fiji in
  the maintainer-owned verify dataset list.
- Same-level competition is judged only by chance: germany judged one query among several
  containing areas (2026-11-26). A unit test pins the ranking judgement on nested rectangles;
  witnesses aimed at real overlaps would need an overlap search over edge crossings and
  interiors, not only vertices, since two areas can overlap with no vertex of either inside
  the other.
