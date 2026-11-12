# Hardcoded waits: design and slice plan

Transient. The item came from the maintainer in conversation (no findings
entry): inventory every hardcoded wait and fix it systematically. A wait on
something our code controls ends on a notification; a time value survives
only as a deadline bounding a wait that normally ends on an event.

## Settled rulings

- No wait polls. The fault plan's waits are rung only: `Wake`,
  `TEST_HOOK_TICK` and the polled form are gone. `FaultPlan::wait_for`
  wakes only on a ring, so its condition reads only rung state (`Watched`
  fields, the token, outside state whose writer rings). This is a documented
  rule, not a typed one: a closure cannot prove what it reads, and a
  restricted view type would not stop captures.
- Outside state a hook wait reads rings the plan from its writer, in hook
  builds only, as `Invocation::cancel` does: the altw router rings after
  `abort` (it holds a hook-build permit), the merge-changes `StopOnDrop`
  rings after `Ledger::stop`. A production metric a fault test waits on gets
  a `Watched` twin fed by an `on_*` hook beside it
  (`reorder_filled_high_water`, `writer_thread_exits`).
- One primitive for a test's own state: `pbfhogg_test_scratch::Bell`, the
  plan's protocol (snapshot ring count, evaluate without the lock, sleep
  only while unchanged, one absolute deadline, `FnMut` condition). Store,
  then ring. A conjunction over monotonic values waits on one bell with one
  deadline.
- A library slow path a unit test observes rings `counters::TestBell`,
  zero-sized with an empty `ring` outside `cfg(test)`. Existing increments
  stay where and as they were (the ordered pool's unconditional tallies,
  classify's test-only ones); the ring is added after each. Release builds
  are unchanged in behaviour and cost.
- A loop that is the test's own work is not a wait: the seal race makes a
  fixed number of attempts, joins its workers under a `HangBound`, and a
  final attempt must seal. The dead-writer tests take the framing barrier
  (`wait_for_framing`) and then require the next write to fail.
- A throttle that paces a producer is replaced by a structural bound where
  one exists: the fused early-error test runs one decode thread with
  read-ahead and decode-ahead of one, so the reader cannot drain the input.
- An elapsed-time assertion after an event is dropped when the event alone
  is the evidence (the stuck pause parks for good, so the exit code proves
  the second signal ended the run).
- `timed_waits` refuses sleeps and the spin primitives
  (`thread::yield_now`, `hint::spin_loop` by path) outright, with no
  residue list.

## Slices

1. Test-machinery polls. Landed with this document: fault-plan waits
   rung only, router and ledger ring, the shared bell, `TestBell`, every
   `yield_now` and `spin_loop` loop, `eventually`, `CountingRead`, the seal
   retry, the FailedWrite busy loop, the CLI elapsed assertion, RESIDUE
   removed, spin rule added. About 20 files.
2. Production polls: `DRAIN_RECV_TIMEOUT`, `GZIP_REORDER_PERMIT_POLL`,
   `EXECUTOR_IDLE_RECV_TIMEOUT` (the cooperative executor waits in
   `write/pipeline.rs::recv_permit`, `merge_changes/stream.rs` and
   `Ledger::acquire`). Measured path: a matched planet A/B per changed
   command, any cost above noise goes to the maintainer. Each site is asked
   whether its other side can notify (rayon's `yield_now` returning Idle
   cannot; a channel or condvar we own can). About 5 files plus
   `reference/performance` records; its size is the measurement, not the
   code.
3. Skipped by the maintainer: the ENOSYS `waitid` fallback
   (`TEST_CHILD_POLL` in `crates/lib/tests/common/process.rs`, kernels
   without `pidfd_open`).

Outside the spelling rule and still open to review: `Instant`-deadline loops
and `try_recv` spins with no primitive call in them.
