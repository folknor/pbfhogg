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
  20.2 s in total).

## Payoff of link 1 (matched A/B, 2026-11-21, plantasjen)

User ruling: the item does not close, and no further link lands, until each
link proves it pays off. Link 1 was measured as A = verifier from 4f484d00
checked out uncommitted, B = 16e70d27, denmark, PASS-line check wall, one run
per cell, each arm switch rebuilt outside the timed wall. Plan and verdict
sparred with codex.

- `sort`, balanced order BA, AB, BA: B 13.1 / A 18.1, A 18.1 / B 13.0,
  B 13.0 / A 17.9 s, so -27.6 %, -28.2 %, -27.4 %. `osmium cat` (2 runs)
  B 7.0-7.1 s vs A 11.7-11.9 s; `opl merge`, `pbfhogg sort` and the cached
  `osmium sort` unchanged. Pays: the gate (more than 3 % in every pair) is
  cleared.
- `cat-dedupe`, order A, B, A, B (two cells queued on the brokkr lock while
  the tree was switched, so the third cell's arm is inferred, not proven):
  totals 2m55 / 2m50 / 2m50 / 2m44; `osmium cat` (11 runs) A 45.4 / 45.1 s,
  B 40.8 / 40.7 s; `opl merge` drifted down through the sequence in both
  arms. It holds one two-miss call (four order scans, six one-miss
  comparisons against a shared decoding), so the decode stage drops about
  10 % and the check-wall gain is unproven. Recorded as a mixed workload,
  not a win. It did not clear the every-pair gate.
- Verdict: link 1 stays, on the `sort` result. Later links each need their
  own matched A/B on the checks they convert, measured the same way (the
  tree switched only while no queued brokkr command is waiting).

## Landed in link 2

Sparred with codex (profile `read`) to consensus; the plan's shape A (all
listed checks) was narrowed to the checks with seconds to win.

- `Harness::pbfhogg_group(Vec<(PbfArgv, PathBuf)>) -> Result<Vec<Ended>>`:
  a private dir per member, `run_group` observing `Run::revalidate` at each
  leader exit (the serial `Harness::pbfhogg` observes nothing), then
  revalidate and checkpoint after settlement; one step wall per label.
  A group abort is the error of the whole group, so callers judge each
  group before starting the next.
- `tunables::PBFHOGG_GROUP` = 4, bounded by summed member memory, not
  cores; callers chunk.
- diff: group 1 = indexed verbose, indexed plain, strip old, strip new,
  consumed in that order; group 2 = the two unindexed diffs.
- cat --dedupe: the self merge stays serial before the derives (an empty
  derived operand ends the check and must not pre-empt the self merge's
  verdicts); the other five merge as chunks of 4 then 1, each chunk judged
  in case order before the next starts. Merge messages now print at
  judgement, after the group.
- `dedupe_fixture.rs`: the fake merge wrote one shared trace file by
  read-modify-write, which concurrent merges race; each merge now writes
  `trace-<label>` and the driver assembles them in case order against the
  same `TRACES`.
- Not converted, no measurable payoff: getparents, check-ids, extract,
  tags-filter, inspect-tags, check-refs (each pbfhogg run a second or two at
  most). derive-changes' indexed and unindexed OSC producers are independent
  and worth up to about 8 s, but the raw-pair preparation errors and the
  raw member's result must be held until the original parity point so they
  cannot suppress the roundtrip findings; left for a later link.

## Payoff of link 2 (matched A/B, 2026-11-21, plantasjen)

A = e6207da8, B = the link uncommitted (switched by stash while nothing was
queued), denmark, PASS-line check wall, warm summaries (one B warm-up per
check not counted), order B, A, A, B.

- diff: B 32.9 / A 47.5 / A 47.5 / B 32.8 s, -30.7 % and -30.9 %. `pbfhogg
  diff` (4 runs) 19.1 s vs 33.4-33.5 s; `pbfhogg degrade` (2 runs) is
  reported at its group's wall, 1.6-1.7 s.
- cat --dedupe: B 41.3 / A 1m21 / A 1m21 / B 41.5 s, about -49 % in both
  pairs. `pbfhogg cat` (6 runs) 23.5 s vs 1m3s.
- Verdict: pays; both checks clear the every-pair gate by a wide margin.

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

### Link 2 - landed (see "Landed in link 2")

- Open candidate: derive-changes' two OSC producers, under the error
  deferral named there, with its own matched A/B.

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
