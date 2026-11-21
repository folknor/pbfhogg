# PERF-066 design: concurrent launches in pbfhogg-verify

Source item: notes/performance.md "PERF-066 - The verifier runs one child at
a time". Sparred with codex (profile `read`) to consensus on 2026-11-21.
Every later link is built from this document alone.

## Landed in link 1

- `Lifecycle::run_group(&mut self, &[Launch], observe)` in
  `crates/verify/src/process.rs`; `Lifecycle::run` is a group of one. The
  only spawn site is `Lifecycle::spawn_all` (source-policy
  `host_independence` inventory updated).
- `ReferenceCache::execute_uncached_group` and `Harness::reference_group`:
  uncached reference runs as one group, judged per member, placed only
  after the whole group settled cleanly.
- `opl_compare::with_decodings` makes all missing operands of a call as one
  group (`acquire_missing`): one decode per distinct source and policy, a pin
  per operand, every operand path watched, every stamp rechecked after.
- Staging trees are launch-unique (`ReferenceCache::staging`, a per-run
  counter in the name), which also closed the reused `opl-compare.1`
  collision after an environment outcome.
- Tests: `verifier_lifecycle` roles `group=rendezvous-write,rendezvous-read`
  (two members meet on a FIFO, which only concurrent members can),
  `group=orphan,parked` and `group=overflow,parked` (each must abort while a
  sibling that never exits is still running).
- Denmark `sort` on plantasjen at the link-1 tree: PASS 18.8 s, `osmium cat
  7.1s (2 runs)` as one group wall (the earlier serial run at d8b9c548 was
  20.2 s in total; no matched A/B was run, so no speedup is claimed).

## Settled rulings (do not reopen)

1. Shape B, a barrier-settled launch group, is the lifecycle. All members are
   spawned from the one main thread (PR_SET_PDEATHSIG is keyed to the
   forking thread); the verifier stays single-threaded.
2. The barrier is where results become usable. No member output is read,
   placed or judged before the whole group settled.
3. Adoption cannot be attributed once several members live, so any adopted
   descendant taints the whole group: every member is ENVIRONMENT. With more
   than one member, the wait scans `/proc/self/task/*/children` at every
   wakeup bounded by `tunables::GROUP_ADOPTION_POLL` and aborts at once; an
   orphan never runs beside a long sibling.
4. Each PID is classified as it is seen against the shrinking set of
   unreaped leaders (PID reuse). A child seen only as a reaped exit in
   `reap()` counts as adopted (this closed a hole single launches had too).
5. Leader statuses are collected only by `Child::try_wait` on the clean path;
   on an abort the settle sweep owns reaping.
6. A member is registered the moment it is spawned, before any fallible
   setup, so a later failure settles it too.
7. Exited pidfds and EOF'd stderr pipes leave the poll set.
8. Abort order: `Unsettled` wins over everything, then `Interrupted`
   (checkpointed again after cleanup), then the abort's own error.
9. `observe` runs at each leader exit (callers pass `Run::revalidate`), and
   a full revalidation plus checkpoint follow the settled group before any
   placement. This keeps the "after every reference run" observation.
10. Every member is judged before any is placed. A later member's failed
    placement leaves earlier ones placed (evidence, as serial acquire did).
11. Members of one group must have disjoint write sets and must not read
    each other's outputs.
12. Step timing: one wall per label per group, every launch counted in the
    run count (reference/verify.md "Results").
13. Shape A (a per-launch subreaper shim re-executing the verifier) is
    deferred, not refused: it is needed only where a result must be consumed
    while an unrelated sibling still runs (geocode's chains). If built, the
    shim must stay alive to kill and reap its tree on cancellation, report
    settlement before output is released, and a shim lost before reporting
    forces a group-wide abort with the verifier's sweep as fallback. It gives
    separate reaping domains, never filesystem isolation.
14. From the implementation audit: a `try_wait` status is recorded before
    the fallible final drain; an abort signals every live leader through
    its held pidfd before the sweep; a path is watched before the stat that
    plans or picks its decoding, and every slot is rechecked against its own
    planned source; a checkpoint precedes the walk; a staging root is owned
    the moment it exists, and staging is never removed after `Unsettled`.
15. Threads with per-thread spawners are refused (PDEATHSIG per thread and
    `waitpid(-1)` races).

## Link plan

### Link 2 - pbfhogg launch groups in checks (medium-large; codex write-sol-low)

- Add `Harness::pbfhogg_group(&mut self, Vec<(PbfArgv, PathBuf)>) ->
  Result<Vec<Result<Ended, Error>>, Error>` beside `Harness::pbfhogg`: a
  private dir per member (`private_dir`), `self.life.run_group(..,
  &mut || self.run.revalidate())` (split the borrows as
  `execute_uncached_group` does), one step wall per label.
- Bound group size by a new tunable in `crates/verify/src/tunables.rs`
  (memory on germany, not core count: start at 4 and document why).
- Convert the independent sub-runs, consuming results in today's order so
  verdicts and messages print as before, and recording an earlier member's
  failure before a later member's error is raised: getparents' 8 runs,
  check-ids' 3 degrades then its 18 `check --ids`, diff's 4 diffs,
  extract's strategies per region, the tags-filter, inspect-tags and
  check-refs cases. Read each check before converting; one whose runs read
  each other's outputs stays serial (ruling 11).
- Fixture tests under `crates/verify/tests/*_fixture.rs` must still pass
  unchanged; add none unless a conversion changes a message.

### Link 3 - cached reference groups (medium; codex write-sol-low)

- A cached variant of `execute_uncached_group`: per member key, lookup and
  hit as `execute` does; misses launch together; each publishes with its
  own `sealed_checkpoint` immediately before its rename. Then let extract,
  multi-extract and the change family run their independent osmium
  references as groups.

### Link 4 - decide shape A for geocode (spar first)

- Only if geocode's two builds and two osmium chains still dominate after
  links 2 and 3. Otherwise record the refusal in the PERF-066 entry.

### Link 5 - green agent (Sonnet)

- `brokkr fmt`, plain `brokkr check` after the last edit, then
  `brokkr run --with pbfhogg pbfhogg-verify -- CHECK --dataset denmark` for
  each converted check (never `all`, never above denmark), commit, and close
  or narrow PERF-066 in notes/performance.md and delete this document when
  the plan is exhausted.
