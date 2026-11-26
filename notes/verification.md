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

## VER-162 - The reader's exact half-turn tie rule is not certified

`pbfhogg-verify geocode` models a street or interpolation segment spanning exactly half a turn
of longitude as both half-world paths, each a candidate with its own snap, and accepts either
when their distances tie within the tolerance. The reader breaks an exact tie by the path running
east from the lesser `(lon, lat)` endpoint (`segment_distance` in the geocode reader), and no
check holds it to that rule: an answer taking the other path at a tie passes. Certifying it needs
a query constructed on an exact tie (the `geocode-seam` fixture is the natural home) and a
judgement of the path, not only of the distance.
