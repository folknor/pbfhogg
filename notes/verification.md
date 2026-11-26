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

## VER-160 - Most commands' `--direct-io` wiring has no CLI witness

`crates/cli/src/lib.rs` wires `direct_reads` into each command's options by hand,
and the CLI direct cells are what witness that the flag reaches the library. Only
check, cat, sort, add-locations-to-ways and apply-changes have one (`cli.rs`,
`cli_cat.rs`, `cli_sort.rs`, `cli_add_locations_to_ways.rs`,
`cli_apply_changes.rs`; `cli_output_guard.rs` for apply-changes). getid,
getparents, tags-filter, time-filter, repack, degrade, diff (and its OSC form),
inspect, inspect tags, extract, renumber and build-geocode-index all have a direct
read path in `crates/cli/src/io_reach.rs`, but no CLI cell passes `--direct-io` to
them. The library fault cells pin those commands' direct lists, but only from a
`WriteOptions` the test builds itself, so a CLI arm that dropped the flag would
pass every test. `direct_io_help_and_feature_refusal_cover_every_command` checks
help text and feature refusal, not that the flag reaches the run. Derived from the
code; not observed.

## VER-161 - `process_nofile` runs every arm under one 20 s watchdog

`crates/lib/tests/process_nofile.rs` runs all its scenarios in one libtest test,
`descriptor_limit_scenarios`: the six injected counts, the held-descriptor cases and
every charge-witness arm (External and Auto buffered, External O_DIRECT, External
io_uring). The io_uring arm's `uring_admission` arms `TEST_HANG_BOUND` (10 s) plus
its report grace over an unbounded `locked_memory_lease()` wait and the writer's
run. Neither bound is set against what the earlier arms already used, so on a host
whose memlock limit admits the writer, a lease held by a concurrent io_uring cell
in another test binary can push the whole test past `brokkr check`'s fixed 20 s
watchdog. The `tunables.rs` const assertions bound each wait alone, not their sum
within one test. Not observed: on a memlock-refusing host the arm refuses quickly,
and the gate has been green. Derived from the code.

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
