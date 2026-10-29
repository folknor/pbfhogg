# Library harness: design and landings

Status: proposed design, revision 3, under spar. Nothing here is built yet.

Written against `reference/technical-implementation-spec.md`. Spawned from
the bz tickets pbfhogg#30 to #38 (filed by brokkr 2026-10-29) and the
release gate "Library use is verified" in `notes/pre-release.md`.

The binding contract is brokkr's library harness protocol, version 1:
`brokkr man library`, source `~/Programs/brokkr/docs/commands/library.md`,
with its invariants in `~/Programs/brokkr/reference/library-internals.md`.
pbfhogg ADRs that bind this work: ADR-0016 (the library does not own the
process), ADR-0017 (outputs are published by the library), ADR-0019
(cancellation is a caller token checked where the run already passes).

## Settled

- The harness is a new workspace member `harness/` (package and bin
  `pbfhogg-harness`, `publish = false`) beside `cli/`. The repo is not
  converted to a `crates/` layout.
- Nothing works around brokkr through `Cargo.toml` (no `default-members`).
  Packages are selected by brokkr's sweeps.

## Survey: what the code does today

1. **Threads that outlive a call.** Only the rayon pools the library
   builds itself: the per-read decode pool in `src/read/pipeline.rs` (its
   comment: workers are not joined on drop, so they may outlive the read)
   and the merge-changes `Parallelism::Fixed` pool in
   `src/commands/merge_changes/mod.rs`. The session types
   (`PipelinedBlocks`, `PbfWriter`, the write pipeline, `parallel_writer`)
   join their own `std` threads on drop.
2. **Counters that move after return.** A `PooledBuffer`
   (`src/read/decompress.rs`) holds an `Arc<DecompressPool>`, which holds
   an `Invocation` clone. When the last `Bytes` over it drops, the buffer
   returns to the pool, and an oversize, full or poisoned pool counts a
   `pipeline_decompress_pool_*` drop into the invocation.
3. **The publication decision.** It is a bare `cancel.check()?` at the
   top of `Output::publish`, `OutputDir::publish` and `OutputSet::publish`,
   followed by `publish_all`. Publications of one invocation are
   serialized by `Invocation::publication_lock` from `PUBLISH_START` to
   `PUBLISH_END`, which is taken after the decision.
4. **Output binding.** An output is bound to no invocation at
   construction (ADR-0019: outputs are built before a run's token need
   exist). Each member's `Completion` records the invocation its writing
   session opened under, at the `Reserved -> Writing` transition.
   `OutputSet` members are lent out individually through `get` and `iter`,
   so two members can be opened under different invocations. An empty
   `OutputSet` is accepted, and so is publishing it. `publish` on an output
   nobody wrote refuses `Unfinished`.
5. **Holders that escape a call.**
   - Sessions: `PipelinedBlocks`, `PbfWriter`, `OutputWriter`, `DirWriter`.
     `ElementReader` is `Clone`.
   - `PbfWriter::into_inner` returns `W`, which on the synchronous path
     can be a `FileWriter` whose later writes record writer metrics.
   - `RunScratch` escapes `create(host)` and reports diagnostics at
     cleanup.
   - Bound outputs keep their `Completion` after their writer is gone.
6. **About 40 internal sites clone the invocation** (`write/writer.rs`,
   `output.rs`, `write/parallel_gzip.rs`, `read/decompress.rs`,
   `read/input.rs`, `osc/xml_parse.rs` and others).
7. **Generic inputs.** `ElementReader::new` takes any `Read` and no
   `Host`; it carries its own default token, and `.invocation(inv)`
   rebinds it. A `PipelinedBlocks` background thread can block forever in
   `BlobReader::next` on a caller-supplied stream.
8. **Executors outside a Host.** `PbfWriter::to_path` takes an
   `Executor` directly, under a fresh invocation of its own, and its drop
   drains permits cooperatively when run on that executor's worker.
9. **End-of-run emitters.** `emit_read_counters`, `emit_writer_counters`
   and `report_writer_degradations` are public. A textlint names the CLI
   as their only caller in this repository; it cannot constrain external
   callers.
10. **Sink delivery.** `Invocation::with_sink` takes any
    `Arc<dyn InstrumentSink>`; nothing stops two invocations sharing one.
    `Invocation::deliver` wraps each callback in `catch_unwind` and
    latches the sink off after a panic. `CallSink` is `Clone`, unreleased
    (387daff5), and `MarkerFifo::call` is infallible.
11. **Certification.** brokkr certifies only when every scenario passed
    and every required capability passed
    (`~/Programs/brokkr/src/pbfhogg/library/certify.rs`). Its digest parser
    accepts `xxh128 v1` only
    (`~/Programs/brokkr/src/pbfhogg/library/digest.rs`).

## Decision A: `Invocation::seal()`

What #31 asks for: Open, Draining and Sealed states. `seal()` is callable
from any clone. It closes admission, then drains the admitted calls and
the workers they own, without ever waiting for executor pool threads. It
refuses with no state change (`SealFromWithin`) when called from a worker
of a pool the drain needs, or from inside an admitted operation or a
callback. It has no deadline of its own. With caller-held blocks
outstanding it returns `Outstanding(n)` and stays Draining, so it can be
retried. The final snapshot exists only once Sealed.

### Candidates

- **A1, the ticket as written:** counters for admitted operations,
  workers and blocks, an admission guard at every public entry, workers
  counted through `exit_handler`. Wrong: completeness rests on an
  inventory, and `exit_handler` fires before a worker is torn down.
- **A2, kinds by holder type** (revision 2): a `Call` or `Held` token
  chosen by what holds it. Wrong: one reader or writer runs under either
  kind depending on what drives it. A `PipelinedBlocks` background thread
  calling `BlobReader::next` minted a `Call`, so seal waited on a caller
  stream that would never close.
- **A3, seal only observes.** Rejected: work admitted after the snapshot
  changes it.
- **A4, the proposal:** kinds by what drives the work, explicit internal
  contexts, and one atomic lifecycle word.

### A4: what counts

A **call** is one outermost public entry in progress: a public function
or a public method on a library value, from entry to return. Nothing
else is a call: not a background thread, not a job, not a destructor,
and not a public function the library calls internally (internal code
does not go through the public wrappers; see below).

A **lease** is everything else that can still change the invocation
after the calls that made it returned. Each of these holds exactly one
lease, from creation to drop:
- a session (`PipelinedBlocks`, `PbfWriter`, `OutputWriter`, `DirWriter`,
  each `ElementReader` bound to the invocation and each clone of one);
- a live `PooledBuffer`;
- a bound output unit (Decision B), until it is published or dropped;
- a `FileWriter` that escaped through `into_inner`;
- a `RunScratch`.

The background threads and jobs of a session run under its lease and
mint nothing of their own.

The `L4` survey completes this list: every type that holds an
`Invocation`, its metrics, a `Diagnostics` from an invocation's `Host`, or
a `Completion`, and can outlive the call that made it. The compiler finds
the first two once the field type changes (below); the last two are
found by grep and recorded in the landing.

### A4: explicit internal contexts

- Library internals never hold the public `Invocation`. They take a
  crate-private context, `&Work`.
  - `Work::Call` is lent by an outermost public wrapper for the extent of
    its call.
  - `Work::Lease` is the lease of the session or value the code runs
    under.
- `metrics()`, `marker()`, `counter()`, the library's own `check()` and
  sink delivery are methods on `Work`, never on `Invocation`.
- Every public function and public method is a thin wrapper over an
  internal implementation that takes `&Work`. The wrapper enters the call
  (below), and library code calls the implementations, never the public
  wrappers. That is what keeps a command's internal construction of a
  `PbfWriter`, or a background `BlobReader::next`, from counting as a
  call. A textlint checks that `src/` calls no public wrapper; clippy's
  `disallowed_methods` names each one.
- Minting a lease needs a `&Work`, so a lease can be minted only under a
  call in progress or under another live lease.

### A4: the user-callback marker

The one place a thread-local is still needed is caller code that runs
inside a call: an `ElementReader` closure, a sink, a `DiagnosticSink`, a
custom `Read` or `Write`. Every such callback runs under a thread-local
marker naming the invocation. The marker is used twice:
- An outermost public entry made on a thread that carries the marker is
  a nested call. It counts nothing new and is not refused while Draining,
  because the call around it is still in progress.
- `seal()` on a marked thread is refused with `SealFromWithin`.

The library's own threads carry no marker, because library code never
re-enters through public wrappers.

### A4: one lifecycle word

`InvocationState` gains one `AtomicU64` packing the state (Open,
Draining, Finalizing, Sealed), the call count and the lease count. Every
transition is one CAS on that word:
- **Enter a call:** state Open, then calls + 1. On a marked thread
  (nested) nothing changes and the entry proceeds. Otherwise, in
  Draining, Finalizing or Sealed it is refused with a new
  `ErrorKind::Sealed`.
- **Leave a call:** calls - 1, and notify when the count reaches 0.
- **Mint a lease:** leases + 1. Allowed in Open or Draining only; a
  `&Work` exists in Draining only under a live call or lease, so no
  minting reaches Finalizing.
- **Retire a lease:** leases - 1, after the value's last recording and
  callback (a returning buffer records its pool metrics, and then
  retires).
- **Begin sealing:** Open to Draining.
- **Finalize:** Draining to Finalizing, only when calls = 0 and
  leases = 0, checked in the same CAS. This removes the
  observe-then-claim race.

### A4: `seal()`

1. **Refuse** with `SealFromWithin`, changing nothing, when the calling
   thread carries this invocation's marker. Also refuse when the thread
   is a rayon worker (`rayon::current_thread_index().is_some()`) and the
   call count is above 0. Waiting there could starve work that needs the
   worker's pool, and no inventory of the pools that work needs is
   complete:
   - under `Executor::Ambient` the pool is chosen at submission;
   - an executor handed straight to `PbfWriter::to_path` passes through no
     `Host`.

   With no call in progress seal never waits, so it is allowed on any
   thread. This is a deliberate narrowing of the supported calling
   contexts. The protocol is compatible with it: a refusal is reported as
   `seal: refused`.
2. **Close admission:** Open to Draining. Calls now only end, so the
   count only falls and seal cannot be starved.
3. **Drain:** wait (condvar, no deadline) until calls = 0.
4. **Finalize:** try the finalize CAS.
   - If leases are still above 0, return `Outstanding(n)` and stay
     Draining. The protocol's `outstanding_blocks` field reports `n`.
     Retrying starts at step 3.
   - If another thread is Finalizing, wait until Sealed and return its
     snapshot.
   - If the CAS wins: take the snapshot (immutable, behind an `Arc`), end
     the call stream (Decision C), then store Sealed, notify and return.

While Draining a caller can only drop what it holds: every public entry,
the methods of its held sessions included, is refused, while drops
retire leases as usual. So `Outstanding` means "you still hold these;
drop them and retry". A writer not finished before seal cannot be
finished after it: dropping it abandons its output, decided
`refused_error`. This is the contract: seal ends the invocation.

From Finalizing on, nothing is admitted, marked or not, and once Sealed
the invocation delivers nothing to any sink.

### A4: library-built pools join for real

The decode pool and the merge-changes fixed pool are built with rayon's
`spawn_handler`, which spawns `std` threads and keeps their
`JoinHandle`s. The owner drops the rayon pool and then joins every
handle, including after a partial build failure. An `exit_handler` latch
is not used, because it fires before the worker's stack and thread-locals
are torn down.

## Decision B: publication decisions and cancel (#33)

**B1, the ticket's CAS word,** loses to B2 because B2 also has to carry
the publication log.

**B2, a mutex-guarded log per invocation (proposed).**

- **Units.** One publication unit per `Output`, `OutputDir` or
  `OutputSet`; an `OutputSet` creates one unit and shares it with its
  members. An empty `OutputSet` is refused at construction
  (`InvalidArgument`), since it would be a publication of nothing.
- **Binding.** A unit is Unbound, then Bound(invocation). The first
  writing session opened on any member binds it; a lease (Decision A) is
  minted then. A session opened under another invocation is refused at
  open, before anything is written (`InvalidArgument`). So every member
  of a set, and every write of an output, is under one invocation, and
  the unit is pending in exactly that invocation's log.
- **Ordinals are assigned at decision**, so ordinal order is decision
  order.
- **The decision** happens at the top of `publish`. It is made in the
  unit's own log if the unit is bound, otherwise in the log of the
  invocation passed to `publish`.
  - A bound unit published under another invocation is decided
    `refused_error` in its own log. The token passed does not decide it;
    the unit's own token does.
  - Otherwise, under the log lock: tripped gives `refused_cancelled`; a
    readiness refusal (`check_ready`, such as `Unfinished` for an output
    nobody wrote) gives `refused_error`; anything else is `authorized`.
  - A unit `cancel` already decided returns `Cancelled` without a new
    entry.
- **`cancel()`** takes the log lock and sets `tripped`. Then:
  - If any unit is pending (bound and undecided), cancel decides every one
    `refused_cancelled` on the spot, assigning ordinals in the order they
    were bound, and returns `BeforeDecision { refused: k }` with the first
    of them.
  - Otherwise it returns `AfterDecision { authorized: k }` for the latest
    authorized unit, if there is one.
  - Otherwise `NoPublicationPending`.

  Every later decision sees `tripped` and refuses, so everything after
  `k` is refused. Checkpoints keep their relaxed load of `tripped`.
- **Dropped units.** A unit dropped while still pending is decided at
  drop: `refused_error`, or `refused_cancelled` if tripped. A decided
  unit keeps its decision. The drop then retires the unit's lease.
- **`installed`** is reported by `publish_all`, not inferred from the
  result. It is true when any new generation is visible at any
  destination when publication returns, so `Unsynced` is installed, and
  so is a rollback that left a member installed. `false` means no
  destination changed.
- **`Invocation::for_each_publication`** hands the harness its
  `publications` array.
- **Contract:** amend ADR-0019. The decision now includes the readiness
  refusals and is taken under the unit's own invocation, cancel decides
  pending units, and binding moves from the member to the unit.

## Decision C: ending the call stream (#32)

- **C1, generic sink plus harness-driven end.** Wrong: `with_sink` takes
  any `Arc<dyn InstrumentSink>`, so nothing proves which invocation a sink
  belongs to.
- **C2, a `sealed` hook on `InstrumentSink`** (revision 2). Wrong for the
  same reason: a shared sink, a forwarding sink inheriting the no-op, or a
  direct call to the public hook all break identity.
- **C3, the invocation owns its call sink (proposed).**
  - `Invocation::with_call_sink(CallSink, tap: Option<Arc<dyn InstrumentSink>>)`
    takes the `CallSink` by value. `CallSink` is no longer `Clone`, so
    exactly one invocation owns each one, by construction.
  - The tap is a caller observer that sees every ordinary marker and
    counter after the sink has taken it. It gets nothing at sealing, and
    a panicking tap is latched off on its own. The cancel scenarios trip
    their token from the tap, so no forwarding sink stands between the
    invocation and its `CallSink`.
  - `Invocation::stream_account()` returns the sink's `{state, drops}`
    for the artefact.
  - Generic `with_sink` stays for other sinks, which have no sealing
    semantics.

  **Ending the stream.** The finalizing seal thread is the only one that
  can end it, and it runs the steps in order:
  1. Flip the sink's cell from Open to Closing.
  2. Wait for in-flight ordinary emissions to reach 0. An ordinary
     emission holds the in-flight count across its whole path (name
     validation, drop accounting, stamping, writing), released on every
     path including unwind.
  3. Write every `@@name=value` line from the snapshot (saturated to
     `i64::MAX`, zeros included).
  4. Write the terminal record: `!sealed drops=N`, or `!failed drops=N`
     after a hard write error (anything but `EAGAIN`).
  5. Freeze the account as Closed.

  More rules:
  - **Identity.** The `MarkerFifo` shared state refuses an id it has
    already issued, so `MarkerFifo::call` becomes fallible.
  - **The terminal write.** If it is dropped, the account counts it:
    `drops` is N + 1, while the lost record said N, as library.md
    specifies.
  - **A panic.** This is library code, not a caller callback, so the
    `sink_failed` latch does not apply. If a panic happens anyway, it is
    contained, the account is frozen as `failed` and a drop is counted for
    the missing terminal record. The stream then reads as unevaluated,
    never as a false pass.
  - **After Closed,** ordinary attempts are ignored and counted nowhere.
  - **Drops.** `Line::emit` reports a drop. Refused names count as drops
    while Open. The reserved forms (a tagged marker starting `!`, a
    counter name starting `@`) are refused.

## Decision D: end-of-run counter emission order

The ticket's order: the call returns; the harness calls
`emit_writer_counters`, `emit_read_counters` and
`report_writer_degradations`; then it seals and takes the snapshot.
These emitters become public wrappers like any other: each is a call, so
it is refused once Draining begins. Counters emitted before the drain can
be below final, which the protocol allows (only an ordinary counter
above its final value is a defect). Proposed: keep the ticket's order.

## Decision E: the harness member

- **`harness/src/protocol.rs`:** typed describe, request, artefact and
  refusal documents. Every number is a JSON integer, and long text is cut
  at a character boundary with `truncated`.
- **`harness/src/runner.rs`:** per call, in order:
  1. A fresh `Invocation::with_call_sink(call_sink, tap)` and a
     collecting `DiagnosticSink`.
  2. `entry_us`, then the call, then `return_us`. The call spans
     everything it does: `entry_us` is taken before its outputs and
     readers are constructed.
  3. The end-of-run emitters (Decision D).
  4. `seal()`, then `seal_us`.
  5. Into the call document: the snapshot,
     `Invocation::stream_account()`, the publications, the cancel result
     and the context.

  Exit 0, 1 or 2 from the results.
- **`harness/src/scenarios/`:** one file per family behind a `Scenario`
  trait (describe, plan, setup, run, assert). From #34 item 6:

  | Scenario | What it does |
  |---|---|
  | `repeat-cat` | the same cat twice in a row |
  | `concurrent-cat` | two inputs of different sizes so swapped tags show, plus a lifetimes-intersected assertion |
  | `fixed-resources-executor` | fixed `Resources` and a caller-built `Executor::Pool` |
  | `cancel-pre-decision` | the tap trips the token on a mid-run marker |
  | `cancel-post-decision` | the tap trips the token on `PUBLISH_START`, which comes after the decision |
  | `nofile-cat-refused` | about 1000 generated inputs at soft 1024 |
  | `nofile-altw-external` | completes with three stage-1 workers |
  | `error-path` | a bad input gives an error outcome and no stderr |
  | `isolation-cat` | one call, with `hotpath` and `isolation` set |

- **CLI counterparts.** A `cli` counterpart is declared only for a call
  whose plan expects an `ok` outcome and an installed PBF output. brokkr
  requires the library output to exist as a regular file and the replay
  to exit 0, and it has no expected-refusal mode.
- **Generated inputs.** A `hotpath` scenario binds only dataset roles,
  because setup in a profiled run may do no library work. Non-profiled
  scenarios may generate fixtures with the library during setup.
- **Process policy.** The harness sets its own soft `RLIMIT_NOFILE` per
  plan before library work and never touches SIGPIPE. A panic reaches
  stderr through the default hook, which fails the run, and brokkr keeps
  the evidence.
- **Profiling.** The `hotpath` and `hotpath-alloc` features forward to the
  library. The guard exists only in `--test-harness` with
  `HOTPATH_OUTPUT_PATH` set, and is dropped after seal and before
  returning from `main` (never `process::exit`). The counting allocator
  lives in `harness/src/main.rs`.
- **Wiring:**
  - `[pbfhogg.library.harness]` in `brokkr.toml`;
  - the member selected by the `all`, `consumer` and `musl` sweeps;
  - the emitter textlints widened to admit `harness/`;
  - the #24 guards (no child process or descriptor reset, no file status
    flag change, the test-only seccomp guard) extended to `harness/`;
  - the library-only bans (env reads, setrlimit) not applied to it.
- **Tests.** Each scenario runs in-process against generated fixtures, so
  the suite still passes with an empty `data/`. brokkr remains the
  protocol validator.

## Landings, in order

Every landing that changes a lifecycle carries deterministic tests for
it, built on `test-hooks` parking points and placed per
`reference/testing.md`. `verify` proves output parity and cannot see a
lifecycle bug, so these tests are each landing's real gate. Thread-count
baselines are not used, because unrelated tests create threads too.

| # | Landing | Gate |
|---|---|---|
| L1 | #30 clock-anchor fix, stale notes, hotpath 0.30.1 audit pin (done, uncommitted) | `brokkr check` |
| L2 | #33: publication units and their binding state machine, refusal of empty sets, decision-time ordinals, cancel deciding pending units, `installed` out of `publish_all`, `for_each_publication`, ADR-0019 amendment | `brokkr check`, with tests for: reversed creation and publication order; one entry per `OutputSet`; a second member opened under another invocation refused at open; an unwritten output published; a bound unit published under another invocation; cancel parked against a decision from both sides; `Unsynced` and a failed rollback reporting installed; a pending unit dropped; a decided unit dropped; a failed session construction |
| L3 | Harness skeleton: member, describe and run modes, runner, `repeat-cat` with a CLI counterpart and no stream capabilities, `brokkr.toml` table, sweeps, textlints, #24 guards, hotpath features. Seal is reported refused, so brokkr fails each call's `brokkr.seal` and nothing else, while describe, layout, CLI equality and digest get exercised early | `brokkr check`; `brokkr library`; `brokkr library repeat-cat --dataset denmark`, which fails on exactly the `brokkr.seal` results |
| L4 | `src/work.rs`: `Work` contexts and the public-wrapper split across the API, the lease inventory, the lifecycle word with Open only, the user-callback marker, and textlint plus `disallowed_methods` against internal calls of public wrappers. Admission always succeeds, so behaviour does not change | `brokkr check`, with tests for: calls and leases back to 0 after every command returns and its values drop; a held `PipelinedBlocks` blocked on a caller stream counting one lease and no call; an escaped `FileWriter` and a `RunScratch` each holding a lease; a nested public entry from a callback counting nothing; `brokkr verify all --dataset denmark --variant indexed` |
| L5 | Library-built pools joined through `spawn_handler` | `brokkr check`, with tests for: a pool worker parked in a thread-local destructor keeping its owning call from returning until released; a partial pool build failure joining the threads already spawned; `brokkr verify all --dataset denmark --variant indexed`; `brokkr cat --dataset denmark --variant indexed --bench` neutral within noise (the join moves pool teardown inside the call) |
| L6 | #31 and #32 as one contract: Draining, Finalizing and Sealed in the lifecycle word, `SealFromWithin`, `Outstanding`, `with_call_sink` and the tap, the non-`Clone` `CallSink` with its closing sequence and drop counts, fallible `MarkerFifo::call`, the reserved forms refused, ADR-0020 (the seal contract). The harness reports real seal and stream values | `brokkr check`, with tests for: concurrent seal callers returning one snapshot only after the terminal record; seal from a callback, from the tap, and from a rayon worker with a call in progress refused; seal from a rayon worker with no call allowed; a nested entry while Draining accepted and a fresh one refused; Finalizing refusing a marked thread; the finalize CAS racing a lease retirement and a nested call; an outstanding buffer released and a retry; a held session reported outstanding; a panicking tap; the terminal write dropped; the in-flight emission barrier. `brokkr library repeat-cat --dataset denmark` passes |
| L7 | The remaining scenarios | `brokkr library --dataset denmark`, then each scenario once: `brokkr library SCENARIO --dataset denmark` |
| L8 | #35 capability ids in `[pbfhogg.library.require]` and `covers` / `stream` declarations | `brokkr library --all --dataset denmark` certifies |
| L9 | #38 CLI entry and return marker pair; #36 reserving descriptor provider | per landing |
| L10 | #37 full-fidelity digest: a `v2` canonical version. Cross-repo, landing together with brokkr's acceptance of `v2`; until then brokkr fails every counterpart naming the version. A stronger equality can fail a scenario, and certification requires every scenario to pass, so certify again after it lands | `brokkr check`; `brokkr library --all --dataset denmark` certifies again |

## Parked after spar round 3 (2026-10-29)

Work is parked here: what belongs in brokkr gets fixed there first.
Protocol v1 has never been used, so it is amended in place, with no new
protocol number.

### brokkr changes, first

1. **Expected-refusal counterparts.** CLI equality requires the replay to
   exit 0 and the library output to exist. A refused, cancelled or failing
   call therefore has no counterpart, and `nofile-cat-refused` and
   `error-path` lose the check that matters most: that the CLI refuses the
   same input and publishes nothing. Needed: a counterpart mode that
   expects a non-zero exit and an absent output.
2. **`seal.outstanding_blocks`.** The field is named for blocks, but what
   it counts is every held value: sessions, bound outputs and escaped
   writers. Rename it to a count of held values.
3. **`publications[].installed`.** A boolean cannot describe a failed
   rollback. An old destination can be moved to backup, installation can
   fail and restoration can fail, so no new generation is visible and yet
   the destination changed. Needed: a richer per-publication effect, such
   as none, new generation installed, or destination damaged.
4. **The diagnostics boundary.** library.md does not say up to when a
   call document's `diagnostics` must be complete. A `SummaryWriter` or an
   unbound `Output` dropped after seal can still report. State the
   boundary (captured up to `seal_us`), or require nothing after it.
5. **Drop "Protocol 1 is final"** from `docs/commands/library.md` and
   `reference/library-internals.md`. Nothing has produced v1 yet.

### Ticket #31's shape, not yet sparred

Most of round 3's difficulty comes from #31's general contract rather
than from what protocol v1 needs: seal callable from any thread and any
clone, concurrent sealers, waiting with no deadline, nested admission
while Draining, refusal on rayon workers. The protocol needs only that
the snapshot is final, the sink ended, and held values are reported. A
narrower seal deserves an argument of its own: no waiting on calls in
progress (`InProgress` is a refusal), with asynchronous library work
accounted for by leases. If it wins, #31 is rewritten, not worked around.

### pbfhogg design defects found in round 3 (verified)

- **The rayon refusal is checked before admission closes:** a call can be
  admitted between the check and the Open to Draining CAS. The refusal
  must be decided on the same observed word as the close.
- **Trait dispatch reaches public wrappers:** `FileOutputSink` writes
  through `FileWriter`'s public `Write`, and generic `W: Write` paths do
  the same. Internal cores or context-bearing adapters are needed for
  trait operations. `Clone` on `ElementReader` and `BlobReader` cannot
  return `Sealed`; choose removing it, a fallible clone, or allowing it
  while Draining.
- **The callback marker:** a callback under a lease has no surrounding
  call, so a nested entry must borrow the enclosing `Work`, not just an
  identity. Implicit caller code needs the marker too: `Display` through
  `write_fmt`, generic `R::clone`, and the destructors of generic `R` and
  `W` fields, which needs deliberate field teardown.
- **Diagnostics have no provenance:** `SummaryWriter`, and an unbound
  `Output`'s cleanup report, reach the sink after seal. Distinguish
  detached diagnostics from invocation-bound ones.
- **Lost wake:** the lifecycle word needs a mutex handshake with the
  condvar (or a validated park) for both waits, plus Acquire ordering
  before the snapshot.
- **Binding needs explicit ownership rules:**
  - Bound and pending must be one linearization point.
  - A lock order is needed across unit, `Completion`, log and lease.
  - The log must not own the units it observes, or abandonment can
    never happen.
  - Abandonment is tied to the publication owner's drop.
- **Liveness:** dropping a `PipelinedBlocks` blocked in a caller's `Read`
  blocks too. Document the caller's obligation, or let detached work keep
  its lease until it ends.
- **Control operations:** `seal`, `cancel`, `stream_account` and the
  publication log need explicit exemptions from admission.
- **The L5 bench gate is wrong:** default indexed `cat` takes the
  zero-decode passthrough. The gate must exercise the decode pool and the
  fixed merge-changes pool.

## Out of scope

- brokkr's side, apart from the `v2` digest acceptance L10 pairs with.
  That includes brokkr#58 (library against CLI timing).
- A `crates/` relayout.
