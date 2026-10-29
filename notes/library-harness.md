# Library harness: design and landings

Status:
- Decisions A (seal) and B (publications and cancel) are settled by
  design spar, 2026-10-29, against the protocol as amended in brokkr
  `3c59ce8` and `75dc76f`. Their specification obligations go to the
  implementation spar.
- Decision C (the owned call sink) was argued in the earlier rounds and is
  carried as it stands.
- The independent prerequisites have landed: `0e5807c7`, `02192dc3`,
  `d100ae8f`, `ce104e1e` and `d2d8836a`.

Written against `reference/technical-implementation-spec.md`. Spawned from
the bz tickets pbfhogg#30 to #38 (filed by brokkr 2026-10-29) and the
release gate "Library use is verified" in `notes/pre-release.md`.

The binding contract is brokkr's library harness protocol, version 1:
`brokkr man library`, source `~/Programs/brokkr/docs/commands/library.md`,
with its invariants in `~/Programs/brokkr/reference/library-internals.md`.
Version 1 has never been used, so it is amended in place, with no new
protocol number. Its harness rules beyond library.md are in bz brokkr#59.
pbfhogg ADRs that bind this work:
- ADR-0016: the library does not own the process.
- ADR-0017: outputs are published by the library.
- ADR-0019: cancellation is a caller token checked where the run already
  passes.

## Handoff: building this through the loops

For an orchestrator picking this up cold. Everything in this section came
from the 2026-10-29 session and is recorded nowhere else.

**Standing rulings from the user:**
- **brokkr first.** pbfhogg never works around a weakness in brokkr's
  design. If the better fix is in brokkr, stop, write it up (bz in project
  brokkr, or a reply for the brokkr developer), and wait for it to land.
- **Protocol v1 is amended in place,** with no new protocol number: it has
  never been used.
- **Design passes settle shape only.** Every "for the implementation
  spar" list and obligations list below is input to the implementer's
  spar, not to another design pass. Do not reopen Decisions A or B in
  design without new evidence.
- **Measure as little as possible.** A benchmark is needed only when a
  change adds work on a success path, and then it is one matched pair on
  denmark (`--variant indexed --bench 1`, interleaved against a
  `--commit` baseline), never planet. A cost above noise comes back to the
  user instead of landing.
- **The roster** used for `orchestrate resolve spar`: a Sonnet scout
  (harness subagent), an Opus implementer (harness subagent), a Sonnet
  green agent, and a codex spar partner on profile `read`, the only
  read-only codex profile. One item per implementer. Confirm the roster
  with the user again, as that procedure requires.

**What to build, in order:** the landing table at the end of this note,
run as L0, L1, L2, L3, L4, L7, L5, L6. L7 (#36) comes before L5 because
the concurrent scenarios need a reserving descriptor provider.

L8 (#37, digest `v2`) has landed on the pbfhogg side: plain `pbfhogg
digest` prints the full form, `xxh128 v2`, in `v1`'s four-line grammar,
and `--form content` keeps `v1` (diff's equality, which brokkr#8's verify
cache needs). brokkr accepts both since brokkr#60 (`98bddee`). What `v2`
covers is in `docs/cli/commands.md` "digest": beyond brokkr#60's list it
also hashes exact nanodegree coordinates and the declared prepass fields.
Its residue is the re-certification, which waits on a harness (L2 onward):
a certification under `v1` holds for `v1` only.
- L0 (irreversible unbacked replacement) stands alone.
- L1 is #33 per Decision B.
- L2, the harness skeleton, needs neither seal nor B.
- L3 and L4 are the seal (Decision A and C). They are the largest and are
  candidates for a chain under one design document, which is the user's
  call.

**bz state:**
- **pbfhogg#31** still describes the superseded general seal contract.
  Rewrite it to Decision A when L3 or L4 starts.
- **#32, #33, #34, #35 and #36** are open and map to the landings. #37
  (L8) is closed; its re-certification is L8's gate below.
- **#30 and #38** are closed.
- **brokkr#58** (library-against-CLI timing on `CLI_COMMAND_START` /
  `CLI_COMMAND_END`) and **brokkr#59's follow-ups** (refusing `exit: 64` in
  refusal counterparts, rewording its refusal docs) are with the brokkr
  developer. Nothing here waits on them.

**Practical facts:**
- **Read the protocol** through `brokkr man library` (the installed
  binary). Do not read brokkr's working tree while its developer is
  editing it, and tell spar partners the same.
- **The CLI contract document** is `docs/cli/commands.md`.
  `reference/cli-reference.md` does not exist.
- **`review` prompts** sent through `echo` must avoid backticks, quotes,
  braces and other shell-special characters, or the permission matcher
  refuses the call. Write long material into a file and point the partner
  at it.
- **Scratch for scouts, implementer reports and the rulings ledger** goes
  in `.agents/` (git-ignored). Use `rm -f` there, because a plain `rm`
  prompts.
- **Exit statuses:** usage errors are 64, runtime failures and refusals
  2, `check` violations 1, platform refusals 3, cancellation 130 and 143.
  A harness refusal counterpart declares `exit: 2`.

## Settled

- **The member.** The harness is a new workspace member `harness/`
  (package and bin `pbfhogg-harness`, `publish = false`) beside `cli/`.
  The repo is not converted to a `crates/` layout.
- **Nothing works around brokkr through `Cargo.toml`** (no
  `default-members`). Packages are selected by brokkr's sweeps.
- **brokkr first.** pbfhogg does not work around weaknesses in brokkr's
  design. Where the better fix is in brokkr, it lands there first.
- **Design passes settle shape only.** Specification gaps go to the
  implementation spar.

## Survey: what the code does today

1. **Library work does not outlive its owner.**
   - Library-built rayon pools are `joined_pool::JoinedPool`s
     (`02192dc3`): every worker has exited, thread-local destructors
     included, before the call ends.
   - Every pipelined `PbfWriter` ending waits for its framing jobs
     (`0e5807c7`).
   - Sessions join their own threads on drop.
   - The residue: `std::thread::scope` worker sets are waited for only to
     the end of their main functions, so their thread-local destructors
     can trail a call. No library thread-local records into an invocation
     (`notes/todo.md` "Executor residue").
2. **Counters that move after return.** A `PooledBuffer`
   (`src/read/decompress.rs`) holds an `Arc<DecompressPool>`, which holds
   an `Invocation` clone. When the last `Bytes` over it drops, the buffer
   returns to the pool, and an oversize, full or poisoned pool counts a
   `pipeline_decompress_pool_*` drop. The pool stores returned `Vec<u8>`s,
   not live buffers, so there is no cycle.
3. **The publication decision** is a bare `cancel.check()?` at the top of
   `Output::publish`, `OutputDir::publish` and `OutputSet::publish`,
   followed by `publish_all`. `Invocation::publication_lock` serializes one
   invocation's publications from `PUBLISH_START` to `PUBLISH_END`, after
   the decision.
4. **Output binding.**
   - An output is bound to no invocation at construction (ADR-0019).
   - Each member's `Completion` records the invocation its writing session
     opened under.
   - `OutputSet` members are lent individually, so two members can be
     opened under different invocations.
   - An empty `OutputSet` is accepted.
   - Placement is `Staging::{Beside, Dir, DirWithBackups}` (`d100ae8f`).
5. **Holders that escape a call.**
   - Sessions: `PipelinedBlocks`, `PbfWriter`, `OutputWriter`, `DirWriter`.
     `ElementReader` is `Clone`.
   - `PbfWriter::into_inner` returns `W`, which can be a `FileWriter` that
     records through its writes.
   - `RunScratch` reports diagnostics at cleanup.
   - Bound outputs keep their `Completion` after their writer is gone.
6. **Access to invocation state.**
   - `Invocation` is `Arc<InvocationState>`, and `InvocationState` is
     private.
   - `src/` holds no `Weak` and no `Arc` of a metrics type.
   - `metrics()` returns a borrow of `&self`.
   - About 40 internal sites clone the invocation.
   - `Invocation::deliver` forgets caught panic payloads, so a payload
     holding an `Invocation` is retained forever.
7. **Generic inputs.** `ElementReader::new` takes any `Read` and no
   `Host`. A `PipelinedBlocks` background thread can block forever in
   `BlobReader::next` on a caller-supplied stream.
8. **Executors outside a Host.** `PbfWriter::to_path` takes an
   `Executor` directly, under a fresh invocation of its own.
9. **End-of-run emitters.** `emit_read_counters`, `emit_writer_counters`
   and `report_writer_degradations` are public. A textlint names the CLI
   as their only caller in this repository.
10. **Sink delivery.**
    - `Invocation::with_sink` takes any `Arc<dyn InstrumentSink>`.
    - `CallSink` is `Clone` and unreleased.
    - `MarkerFifo::call` is infallible.
    - The CLI writes `CLI_COMMAND_START` / `CLI_COMMAND_END` around its
      dispatch (`ce104e1e`).
11. **Exit statuses.** Usage errors exit 64, and runtime failures and
    refusals exit 2 (`d2d8836a`). brokkr requires a counterpart's `exit`,
    from 2 to 127.
12. **Certification.** brokkr certifies only when every scenario passed
    and every required capability passed. Its digest parser accepts
    `xxh128 v1` and `v2` (brokkr#60) and treats a version mismatch
    between the library and CLI reports as a defect.

## Decision A: `Invocation::seal()` (settled)

Spar rounds 4 to 7, 2026-10-29, one codex session. This supersedes ticket
#31's general contract, so #31 is rewritten to it rather than worked
around.

### Candidates and why this one

- **#31 as written** (Draining, waiting on calls and workers,
  `SealFromWithin`, refusal on rayon workers) built a drain for work that
  no longer outlives its owner (survey 1). Every waiting hazard came with
  it: a lost wakeup, starvation, deadlock on executor workers, nested
  admission and a callback marker.
- **Seal by unique ownership** (`seal(self)` plus `Arc::try_unwrap`) lost
  for three reasons:
  - a forgotten panic payload holding a handle vetoes it forever;
  - a handle count is not the protocol's census ("every value the caller
    still held, each counted once");
  - a failed `Weak` upgrade cannot tell sealed apart from dropped.
- **No library seal** gives no guarantee of final values and nothing to
  end the call stream.
- **Chosen: three separate roles and a seal that never waits.** Identity
  and control, permission to record, and the census each get their own
  mechanism.

### The design

1. **Control handle.** `Invocation` is a cloneable handle carrying
   identity, the lifecycle status (Open, Finalizing, Sealed, Failed, held
   in the state) and the reads: `for_each_counter`,
   `for_each_publication` and `stream_account`. Holding one, or leaking
   one, never blocks seal. A read copies under its lock and calls the
   caller's visitor after releasing it.
2. **Capabilities gate every mutation.** Every mutation of invocation
   state needs a `&Call` or a `&Permit` whose borrow lifetime is tied to
   it. That covers:
   - counters;
   - sink delivery and its failure latch;
   - report-once keys;
   - publication decisions, effects and abandonment;
   - cancellation's changes to the publication log;
   - deferred emitters.

   The compiler rejects a recorder without one, and that is the proof.
3. **Call guards.** Every public entry that can record takes a `Call` for
   its extent: commands, methods on library values, trait methods such as
   `Write::write` on a library writer, the end-of-run emitters, and
   `cancel`.
   - Minting is one CAS that requires Open and increments the count in the
     same step.
   - It is refused in Finalizing, Sealed and Failed (`ErrorKind::Sealed`;
     `cancel` returns `CancelOutcome::Ended`).
   - Nested and internal entries simply mint, since their enclosing
     capability keeps the state Open. There is no callback marker and no
     public-wrapper split.
   - A call under invocation A that reaches a value bound to B mints on B.
4. **Permits.** A value holds a permit exactly when it can record outside
   a public call, through background work or its own `Drop`, and holds it
   past its last recording and callback. Internal values hold permits
   too.
   - A `DecompressPool` holds one permit for its life. Every
     `PooledBuffer`'s existing `Arc` to its pool keeps that permit alive
     until the last buffer has returned and recorded. This adds no
     per-blob work on the invocation word.
5. **Census tickets** follow the protocol's `held` population. One ticket
   is minted per issued value identity: when a value the census covers
   first crosses the API boundary to the caller, returned or passed to a
   caller callback that can keep it. It retires when that value ceases to
   exist.
   - Nesting by the caller is irrelevant, so an issued `OutputWriter`
     inside an issued `PbfWriter` is two values.
   - Internally built values get none.
   - Buffers get a `buffer` ticket only when a block escapes: at receipt
     in `next`, or before caller code in a block-callback API. This is an
     explicit escape policy in the internal core.
   - Tickets are counted under one census mutex, which is bookkeeping
     only: no recording, callbacks, joins, generic destructors or other
     locks.
6. **One transition word.** One `AtomicU64` packs the state, the guard
   count and the permit total, and the counts saturate into refusal.
   `seal()` makes one strong `compare_exchange` from Open with guards and
   permits both 0 to Finalizing. On failure it classifies the word it
   observed, in this order:
   1. guards above 0: `Refused(InProgress)`;
   2. Finalizing: `Refused(Finalizing)`;
   3. Sealed: the stored result;
   4. Failed: `Refused(FinalizationFailed)`;
   5. permits above 0: the census under `try_lock`, which is
      `Outstanding(census)` with any ticket live, `Refused(Unticketed)`
      with none, and `Refused(Busy)` on contention.

   Seal never waits and never retries.
7. **Finalization runs no caller code.** The CAS winner runs these steps
   in order:
   1. Takes the snapshot and freezes the publication log, as pure reads
      into owned data. A panic here sets Failed: no sealing record has
      been written, so the stream stays `open` and seal reports `refused`.
   2. Ends the owned `CallSink`'s stream (Decision C).
   3. Freezes the stream account.
   4. Only then publishes Sealed with the stored result.

   Concurrent sealers get `Refused(Finalizing)` throughout, so no caller
   ever sees a successful seal with an open stream. After the snapshot,
   the invocation is sealed whatever happens to the stream: a failed
   stream on a sealed invocation is legal in the protocol and reads as
   unevaluated.
8. **Leaking is local.** A leaked control handle vetoes nothing. A leaked
   permit vetoes seal and is reported truthfully: under its kind if
   ticketed, otherwise as `Refused(Unticketed)`.

Against #31: "callable from any clone" holds. "Closes admission" holds,
by the Open CAS. "Drains calls and workers" is replaced by "refuses while
calls are in progress", because the library owns no asynchronous work
that outlives its owner. "Refuses from within" is `Refused(InProgress)`.
"Outstanding, retryable" is `Outstanding(census)`.

### Obligations for the implementation spar

These are specification gaps the design pass recorded rather than
resolved.

- **Committing to the terminal record.** Everything that can panic is
  prepared before the terminal write. After the terminal record is
  written, accounting and publication of the result go through
  operations that cannot unwind, and the terminal state is never changed
  afterwards. A missing terminal record is counted only if it really is
  missing.
- **Publication ordering.** Retiring a call or permit publishes its writes
  before decrementing (Release), and the finalizing CAS acquires them
  (Acquire), including through the decrement sequence by which the
  finalizer observes zero. The census mutex does not order unticketed
  recorders.
- **Ticket identity.** A consuming builder that returns the same value
  mints no second ticket. An already-ticketed inner value returned keeps
  its ticket. An unbound output gets its registration point when it binds.
  Retirement covers consuming operations and unwind.
- **Destructor discipline.**
  - A capability lives through `Drop::drop` and all later field
    destruction.
  - A local moved out (`PbfWriter::drop` takes its pipeline out) carries
    its own capability.
  - Each child that records independently owns a permit.
  - A partly built value holds its permit before its first recording or
    dispatch.
  - Retirement and finalization use private administrative authority.
  - The mutation interface is unreachable from the control handle,
    cached metric references, publication-log accessors and raw atomics.
- **The census list.** Confirm `session`, `writer`, `output` and `buffer`
  against the criterion. No permit and no ticket go to values that record
  only through methods: an idle `ElementReader`, a `FileWriter` whose
  drop-flush bypasses its metrics, a `DirWriter`, a `RunScratch`.
- **Cost.** The block APIs gain one census-mutex lock per escaped block at
  mint and at retire. When built, this is gated by one matched pair on
  denmark on a command that uses the block API.
- **Liveness of drop.** Dropping a `PipelinedBlocks` blocked in a caller's
  `Read` still blocks. Document it as the caller's obligation.

## Decision B: publication decisions and cancel (#33) (settled)

Design spar, 2026-10-29. The first proposal (one log per invocation,
binding at the first writer open, effects read from final `State`) lost
on four points, each verified:
- **Two members of an unbound set** opened under different invocations
  have no common lock to arbitrate which one binds.
- **Binding at the first writer open** misses a command that fails or is
  cancelled before opening its output.
- **Ordinal order is not filesystem order.** `publication_lock` is taken
  after the decision and the hooks, in `PublishSpan::start`.
- **Effects cannot be reconstructed from cleanup state.** A restored
  target and an untouched one both end `Staged`. And a lone file replaced
  without a backup, then rolled back, destroys the previous generation,
  which no protocol effect can describe.

The settled shape is the following. No brokkr change is needed.

1. **Binding is explicit and consumes the owner.**
   - `Output::bind(self, &Invocation) -> BoundOutput`, with the same for
     `OutputDir` and `OutputSet`. Members of a set are lent only after
     the whole set is bound, and inherit its invocation, which cannot
     change.
   - Construction can still come before the invocation exists, as
     ADR-0019 requires.
   - Two invocations cannot compete for one unit, because binding happens
     on an owned value.
   - Command entry points bind their outputs to the host's invocation at
     entry.
2. **Pending means enlisted.** Binding registers the unit as pending in
   its invocation's log: "this invocation has accepted responsibility for
   deciding this publication". It also mints the unit's permit and its
   `output` census ticket (Decision A). Opening a writer changes only
   session state.
3. **The log** is one mutex per invocation holding plain data: decided
   rows, the pending set and the next ordinal. It owns no outputs. It
   never covers hooks, sink delivery, filesystem operations or generic
   destruction.
4. **`BoundOutput::publish(self)`** uses the invocation it stores and
   mints a `Call` on it, so publishing under the wrong token cannot be
   written. A command handed an output bound to an invocation other than
   its `Host`'s still refuses it (`InvalidArgument`).
5. **Admission is serialized through completion.** In order:
   1. take the invocation's publication execution lock, held until the
      publication finishes;
   2. under the log lock, decide (tripped: `refused_cancelled`; a
      readiness refusal: `refused_error`; otherwise `authorized`), take the
      next ordinal and release the log lock;
   3. run the hooks and the publication;
   4. record the effects in the row and release the execution lock.

   So ordinal order is execution order within an invocation. Ordinal
   order for a role shared across invocations is the harness's rule
   (brokkr#59). `cancel` takes only the log lock, so it can decide pending
   units while an authorized publication runs. Whether a callback may
   re-enter publication is policy for the implementation spar.
6. **`cancel()` is invocation-wide**, as ADR-0019 already allows (a token
   for a group meant to stop together). Under a `Call` and the log lock it
   sets `tripped`, then answers:
   - pending units exist: it decides each `refused_cancelled` in bind
     order and returns `BeforeDecision { refused: k }` with the first;
   - none pending, but some unit authorized: `AfterDecision { authorized: k }`
     with the latest;
   - otherwise `NoPublicationPending`.

   K is one witness among the units it refused, not a summary of every
   concurrent command; the protocol checks only K and the refused suffix.
   Cancelling one command among several needs separate invocations.
7. **Abandonment is owned by the publication owner,** not by the shared
   `Completion`.
   - Dropping a bound owner that is still pending decides it
     (`refused_error`, or `refused_cancelled` if tripped), then retires its
     permit and ticket.
   - Dropping one set member does not abandon the set, and surviving
     writers do not become the owner.
   - Responsibility moves into `publish_all` and covers every early
     return and unwind.
8. **Effects are recorded from namespace transitions**, each successful
   rename or removal, and finalized after publication or rollback. They
   are never reconstructed from cleanup state:

   | Path | Effect |
   |---|---|
   | readiness, recheck or staged-sync failure before the namespace changes | none |
   | new generation removed, destination absent beforehand | none |
   | previous generation moved aside and put back | restored |
   | previous generation kept privately, destination absent | displaced |
   | new generation remains after a failed rollback | installed |
   | completed installation, `Unsynced` included | installed |

   A directory `TakenOut` with a backup is displaced, and without one it
   is none. A group gets an independent result for every member, untouched
   members after the failing one included. Every member of a refused
   decision is none.
9. **Unbacked replacement is irreversible.** A lone file replaced by
   rename-over, with no backup, is a commit boundary. A failure after it
   keeps the new generation installed (`installed`) and never rolls back
   into destruction. This is a pbfhogg fix: the current rollback can
   destroy the previous generation after an `AfterInstalled` failure.
   Keeping that behaviour would need a protocol effect for destructive
   loss, which is rejected.
10. **Reads.** `for_each_publication` copies the rows under the log lock
    and calls the visitor afterwards.
11. **Contract.** Amend ADR-0019 and ADR-0017:
    - explicit binding before work, with an immutable bound identity;
    - the decision includes readiness and is made under the unit's own
      invocation;
    - admission is serialized through completion;
    - cancel decides pending units;
    - effects are a reported result of publication;
    - unbacked replacement is irreversible.

**For the implementation spar:** the guard layout for abandonment, the
retirement order of tickets and permits, allocation and unwind handling,
the path-by-path transition and fault matrix for effects, and the
callback re-entry policy.

## Decision C: ending the call stream (#32)

- **C3, the invocation owns its call sink.**
  - `Invocation::with_call_sink(CallSink, tap: Option<Arc<dyn InstrumentSink>>)`
    takes a non-`Clone` `CallSink` by value.
  - The tap sees every ordinary record after the sink. It gets nothing at
    sealing, and a panicking tap is latched off on its own. The cancel
    scenarios trip their token from the tap.
  - Generic `with_sink` stays for sinks without sealing semantics.
  - The rejected alternatives, a generic sink with a harness-driven end
    and a `sealed` hook on `InstrumentSink`, cannot prove which invocation
    a sink belongs to.
- **Ending the stream** runs on the finalizing seal thread (Decision A
  item 7), in order:
  1. Flip the sink from Open to Closing.
  2. Wait for in-flight ordinary emissions to finish. Each holds the
     in-flight count across validation, drop accounting, stamping and
     writing, released on every path.
  3. Write every `@@name=value` line from the snapshot (saturated to
     `i64::MAX`, zeros included).
  4. Write the terminal record: `!sealed drops=N`, or `!failed drops=N`
     after a hard write error.
  5. Freeze the account.

  Once the account is Closed, ordinary attempts are ignored and counted
  nowhere.
- **Identity.** `MarkerFifo::call` becomes fallible and refuses an id it
  has already issued.
- **Drops.** A dropped terminal record is counted in the account (N + 1
  against the record's N). `Line::emit` reports a drop, and refused names
  count while Open. The reserved forms (a tagged marker starting `!`, a
  counter name starting `@`) are refused.

## Decision D: end-of-run counter emission order

The call returns; the harness calls `emit_writer_counters`,
`emit_read_counters` and `report_writer_degradations` (each a `Call`);
then it seals. Counters emitted before the snapshot can be below their
final values, which the protocol allows: only an ordinary counter above
its final value is a defect.

## Decision E: the harness member

- **`harness/src/protocol.rs`:** typed documents per `brokkr man library`.
  Every number is a JSON integer, and long text is cut at a character
  boundary with `truncated`.
- **`harness/src/runner.rs`:** per call, in order:
  1. A fresh `Invocation::with_call_sink(call_sink, tap)`, and a
     collecting `DiagnosticSink` that stamps each event's `at_us` from
     `now_us()`.
  2. `entry_us` (before the call's outputs and readers are constructed),
     then the call, then `return_us`.
  3. The end-of-run emitters (Decision D).
  4. `seal()`, then `seal_us`. The census is kept as taken at this
     attempt.
  5. Drop every value of the call, and wait for every callback
     attributable to it.
  6. Close and snapshot the call's diagnostics.
  7. Write the call document: the snapshot, `stream_account()`, the
     publications, the cancel result and the context.

  Setup's and the harness's own diagnostics go in `scenario.json`. Exit
  0, 1 or 2 from the results.
- **`harness/src/scenarios/`:** one file per family behind a `Scenario`
  trait (describe, plan, setup, run, assert):

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
  | `seeded-replace` | a `seeded: true` output replaced, and one refused under `Overwrite::Refuse` |
  | `isolation-cat` | one call, with `hotpath` and `isolation` set |

- **CLI counterparts.**
  - `expect: "output"` for calls expected to publish.
  - `expect: "refusal"` for refused or failing calls, with a required
    `exit` (2: pbfhogg's runtime refusal; never 64, a usage error). A
    cancelled call gets none.
  - `overwrite` is declared per counterpart.
- **Harness rules from brokkr#59:**
  - no call publishes content identical to its role's seed;
  - publications that share a role run in ordinal order, rollback
    included.
- **Staging and scratch.** Every output takes
  `Staging::DirWithBackups(private_dir)`, and every `RunScratch` is
  created in the private directory, so nothing lands in `roles/`. Whether
  a displaced backup survived is the harness's own assertion.
- **Generated inputs.** A `hotpath` scenario binds only dataset roles,
  because setup in a profiled run may do no library work.
- **Process policy.** The harness sets its own soft `RLIMIT_NOFILE` per
  plan before library work and never touches SIGPIPE. A panic reaches
  stderr, which fails the run.
- **Profiling.** The `hotpath` and `hotpath-alloc` features forward to the
  library. The guard exists only in `--test-harness` with
  `HOTPATH_OUTPUT_PATH` set, and is dropped after seal, before returning
  from `main`. The counting allocator lives in `harness/src/main.rs`.
- **Wiring:**
  - `[pbfhogg.library.harness]` in `brokkr.toml`;
  - the member selected by the `all`, `consumer` and `musl` sweeps;
  - the emitter textlints widened to admit it;
  - the #24 guards (no child process or descriptor reset, no file status
    flag change, the test-only seccomp guard) extended to `harness/`;
  - the library-only bans (env reads, setrlimit) not applied to it.
- **Tests.** Scenarios run in-process against generated fixtures, so the
  suite still passes with an empty `data/`.

## Landings, in order

Every landing that changes a lifecycle carries deterministic tests built
on `test-hooks` parking points, placed per `reference/testing.md`.
Measure only where a change adds work on a success path, with one matched
pair on denmark.

Landed independently, 2026-10-29:
- `0e5807c7`: framing jobs waited for at every writer ending.
- `02192dc3`: joined library pools.
- `d100ae8f`: `Staging::DirWithBackups`.
- `ce104e1e`: the CLI command interval (#38).
- `d2d8836a`: usage errors exit 64.

| # | Landing | Gate |
|---|---|---|
| L0 | Unbacked replacement is irreversible: a lone file replaced by rename-over is a commit boundary, so a later failure keeps it installed instead of rolling back into destroying the previous generation. Independent of the rest, and a fix on its own | `brokkr check`, with the `AfterInstalled` fault on a lone replaced file now keeping the new generation |
| L1 | #33 per Decision B: explicit `bind` on outputs, directories and sets, with command entry points binding at entry; the per-invocation log; admission serialized through completion; effects recorded from namespace transitions; invocation-wide cancel deciding pending units; `for_each_publication`; ADR-0017 and ADR-0019 amendments | `brokkr check`, with tests for: reversed creation and publication order; ordinal order equal to execution order with a hook parked; one row per set; a command failing before it opens its output recording `refused_error`; cancel parked against a decision from both sides; every effect, restored and displaced included, through publication faults; a group with a failing middle member; dropping one set member leaving the set pending |
| L2 | Harness skeleton: member, describe and run modes, runner, `repeat-cat` with an output counterpart, wiring, hotpath features. Seal is reported refused until L4 | `brokkr check`; `brokkr library`; `brokkr library repeat-cat --dataset denmark`, which fails on exactly the `brokkr.seal` results |
| L3 | Decision A's capabilities: control handle, `Call` and `Permit`, the transition word Open-only, census tickets, the pool permit. Admission always succeeds, so behaviour does not change | `brokkr check`, with tests for: guards and permits back to 0 after every command and drop; a held `PipelinedBlocks` blocked on a caller stream holding a permit and a ticket and no guard; a nested entry minting normally; `Refused(Unticketed)` impossible in a quiescent run |
| L4 | #31 and #32 as one contract: the seal states and classification, finalization order, `with_call_sink` and the tap, the non-`Clone` `CallSink` closing sequence and drop counts, fallible `MarkerFifo::call`, the reserved forms refused, and an ADR for the seal contract. The harness reports real seal and stream values | `brokkr check`, with tests for: concurrent sealers refused until the account is frozen; seal from a callback refused; the CAS racing a retirement and a nested call; an outstanding buffer released, then a retry; a panic before and after the snapshot; a panicking tap; a dropped terminal record; the in-flight emission barrier; block-API cost on one matched denmark pair. `brokkr library repeat-cat --dataset denmark` passes |
| L5 | The remaining scenarios | `brokkr library --dataset denmark`, then each scenario once |
| L6 | #35 capability ids and `covers` / `stream` declarations | `brokkr library --all --dataset denmark` certifies |
| L7 | #36 reserving descriptor provider (needed by the concurrent scenarios) | per landing |
| L8 | #37 full-fidelity digest `v2`. Landed: `digest` defaults to `--form full` (`v2`), `--form content` keeps `v1`; brokkr accepts both (brokkr#60). Remaining: certify again once a harness exists | `brokkr check` (passed at landing); `brokkr library --all --dataset denmark`, pending a harness |

## Out of scope

- brokkr's side, apart from the `v2` digest acceptance L8 paired with
  (landed, brokkr#60).
  That includes brokkr#58 (library against CLI timing) and #59's
  follow-ups.
- A `crates/` relayout.
