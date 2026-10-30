# Library harness: design and landings

Status:
- Decisions A (seal) and B (publications and cancel) are settled by
  design spar, 2026-10-29, against the protocol as amended in brokkr
  `3c59ce8` and `75dc76f`, and both have landed (L1, L3, L4).
- Decision C (the owned call sink) was argued in the earlier rounds and is
  carried as it stands.
- The independent prerequisites have landed: `0e5807c7`, `02192dc3`,
  `d100ae8f`, `ce104e1e` and `d2d8836a`.
- Every landing is in, and the harness certifies: `brokkr library --all
  --dataset denmark` passes every scenario and every required capability
  on the clean tree of the #39 commit, which is also L8's (#37)
  re-certification under digest `v2`. Nothing in the landing table
  remains.

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

**What to build, in order:** the landing table at the end of this note.
L5 (the scenarios), L6 (#35, capability ids and declarations) and L7
(#36, the reserving descriptor provider) have landed; see the landing
list, as has #34's caller-pool framing proof. What remains is
certification itself, blocked on the two required capabilities no scenario
covers yet (#39).

L8 (#37, digest `v2`) has landed on the pbfhogg side: plain `pbfhogg
digest` prints the full form, `xxh128 v2`, in `v1`'s four-line grammar,
and `--form content` keeps `v1` (diff's equality, which brokkr#8's verify
cache needs). brokkr accepts both since brokkr#60 (`98bddee`). What `v2`
covers is in `docs/cli/commands.md` "digest": beyond brokkr#60's list it
also hashes exact nanodegree coordinates and the declared prepass fields.
Its residue is the re-certification (a certification under `v1` holds
for `v1` only), which rides on the same `brokkr library --all` run as
certification: the L6 sweep compared every counterpart under `v2` and all
passed, but the sweep as a whole did not certify.
- L0 (irreversible unbacked replacement) and L1 (#33, Decision B) have
  landed; see the landing list.
- L2, the harness skeleton, has landed; see the landing list. Since L4 it
  reports real seals.
- L3, Decision A's capabilities, and L4, the seal itself (Decision A's
  states and finalization, and Decision C), have landed; see the landing
  list. ADR-0020 is the seal's contract.

**bz state:**
- **pbfhogg#31 and #32** are closed by L4.
- **#34** is closed by its caller-pool framing proof
  (`executor.caller-pool.v1`). **#39** owns the proofs
  of `host.page-cache-keep.v1` and `counters.own-snapshot.v1`. #35 (L6),
  #33 (L1) and #36 (L7) are closed. #37 (L8) is closed; its
  re-certification is the certification row below.
- **#30 and #38** are closed.
- **brokkr#58, #59 and #60 are closed** (brokkr `2f407c3`, `6074751` and
  `98bddee`):
  - isolation timing reads the CLI command interval;
  - refusal counterparts refuse `exit: 64`;
  - CLI equality accepts digest `v1` and `v2`.
- **brokkr#61** (digest has no measured brokkr command) is open. Until it
  lands, the cost of digest `v2` on the success path is unmeasured.
- **brokkr#62** (filed at L2) is open: the call-stream defect "a name
  emitted twice" reads as forbidding repeated ordinary markers and
  counter samples. Since L6 every call claims `stream.sealed.v1`,
  including `nofile-altw-external`, whose ordered pools emit their
  running totals once per run; the installed brokkr passed that stream.
  The text is still ambiguous, and a stricter brokkr would fail it.

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
3. **The publication decision** (L1) is made in `publish_unit`
   (`src/output.rs`) under the invocation's publication log
   (`src/publication.rs`), inside the execution lock that runs through
   completion; `Invocation::cancel` decides pending units under the log.
4. **Output binding** (L1).
   - An output is bound to no invocation at construction; `bind` consumes
     it into a bound owner and enlists it (ADR-0019).
   - A bound set's members inherit its invocation and are lent as
     `&BoundOutput`.
   - An empty `OutputSet` is refused at construction.
   - Placement is `Staging::{Beside, Dir, DirWithBackups}` (`d100ae8f`).
5. **Holders that escape a call.**
   - Sessions: `PipelinedBlocks`, `PbfWriter`, `OutputWriter`, `DirWriter`.
     `ElementReader` is `Clone`.
   - `PbfWriter::into_inner` returns `W`, which can be a `FileWriter` that
     records through its writes.
   - `RunScratch` reports diagnostics at cleanup.
   - Bound outputs keep their `Completion` after their writer is gone.
6. **Access to invocation state** (since L3).
   - `Invocation` is `Arc<InvocationState>`, and `InvocationState` is
     private. The handle carries identity and reads only.
   - Every mutation is on the crate-private `Recorder`, reached only by
     borrowing a `Call` or a `Permit`; `metrics()` borrows from that
     capability. `src/` holds no `Weak` and no `Arc` of a metrics type.
   - Internal holders store a `Permit` where they record outside a call and
     the handle where they only read.
   - The sink's caught panic payloads are forgotten, so a payload holding
     an `Invocation` (or a permit) is retained forever.
7. **Generic inputs.** `ElementReader::new` takes any `Read` and no
   `Host`. A `PipelinedBlocks` background thread can block forever in
   `BlobReader::next` on a caller-supplied stream.
8. **Executors outside a Host.** `PbfWriter::to_path` takes an
   `Executor` directly, under a fresh invocation of its own.
9. **End-of-run emitters.** `emit_read_counters`, `emit_writer_counters`
   and `report_writer_degradations` are public. A textlint names the CLI
   as their only caller in this repository.
10. **Sink delivery** (since L4).
    - `Invocation::with_sink` takes any `Arc<dyn InstrumentSink>`, with no
      sealing semantics.
    - `Invocation::with_call_sink` owns a non-`Clone` `CallSink` and an
      optional tap; a seal ends the sink's stream.
    - `MarkerFifo::call` is fallible and issues an id once per FIFO.
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

**Settled by L3's implementation spar** (what the code does; the items
above that L3 did not settle are listed as residue in the landing list):
- **The proof.** `Recorder` (crate-private, never owned outside the
  capability types, not `Clone`) carries every mutation: `marker`,
  `counter`, the sink and its latch, `metrics`, the report-once keys, the
  publication log and execution lock. `Call` and `Permit` deref to it; the
  handle has none of it. `Diagnostics::emit_once` takes a `Recorder`.
- **Commands** enter: `let host = &host.enter()` at every public command
  gives an `Entered` whose `invocation` field is the command's `Call`,
  shadowing `Host::invocation`; internal helpers take `&Entered`. A nested
  public command takes the `Entered` as `&Host` and enters again.
- **The word**: 2 bits state (only Open exists), a sticky saturation bit,
  30 bits guards, 31 bits permits. A mint at a full field sets the bit and
  yields an uncounted capability that never decrements. Minting is
  infallible in L3; L4 makes `call` fallible at the mint sites.
- **Internal walks never hold a call across a caller's stream.** Public
  `BlobReader::next` mints; the library's walks use `next_blob` under their
  enclosing capability, and `run_pipeline` borrows the caller's `Recorder`
  through teardown. A `PipelinedBlocks` thread holds a permit of its own to
  its last act; the session holds the census ticket and records nothing.
- **Cold recordings under read-only holders** (failed `fadvise`, the
  unrecognised-indexdata warning, the unindexed-filter note) mint a nested
  call on the spot; every such site runs under an enclosing capability.
- **Issuance by actual visibility.** Public constructors, `new_in`,
  `BoundOutput::pbf_writer`, public `bind` and `bind_to`, `PipelinedBlocks`
  and blocks returned by `next` or passed to the
  `for_each_block_pipelined` callback are issued; commands bind through
  `sealed::Bind::bind_unissued`, which takes an unconstructible
  `sealed::Internal` key, write through `pbf_writer_unissued` and
  `PbfWriter::with_permit`, and read through `for_each_own_block_pipelined`.
  An already-bound value keeps its one ticket. `for_each_pipelined` lends
  elements only and issues nothing.
- **Admission is on the value's invocation**, and a public method that can
  tear the value down keeps its call through that teardown (`into_inner`
  and a refused `bind_to` move `self` into a local declared after the
  call).
- **Drop order.** A block's ticket is declared after its buffer, an
  owner's ticket after its `Enlisted`, a writer's after its permit:
  recordings and decisions happen before the count retires. `Enlisted`
  mints its permit, then enlists, then becomes the guard.
- **The census list** is `session`, `writer`, `output`, `buffer`.
  `OutputWriter`, `DirWriter`, an idle `ElementReader` or `BlobReader`, a
  `RunScratch` and the never-issued `FileWriter` hold no permit and no
  ticket. `OutputWriter`'s `Write` methods record nothing and take no call.

**Settled by L4's implementation spar** (one codex session, 2026-10-30):
- **Two mint kinds, by what the minter holds.** `Invocation::call` (from a
  bare handle) is fallible, `ErrorKind::Sealed` once the state is not
  Open; `Host::enter` returns a `Result`. `Recorder::call`, `permit` and
  `ticket` (from a borrowed live capability) are infallible, which is
  sound because a live counted capability keeps a count above zero and an
  uncounted one the saturation flag. The handle has no `permit` or
  `ticket`. `Call::detached` is a call on a fresh invocation nobody else
  holds (throwaway runs, tests). Cold sites that hold only a handle and
  return unit skip their recording when refused.
- **Word.** Open is 0, so the seal is `compare_exchange(0, FINALIZING)`
  (`AcqRel`/`Acquire`). Saturation is set inside the Open-conditioned
  exchange, closing the window where a minter that saw a full field set
  the flag after a seal had won.
- **The word is the in-flight barrier**; the call sink keeps no in-flight
  count, only a state load so an impossible late ordinary record is
  ignored and counted nowhere.
- **Commit point.** The snapshot (as `Arc<Snapshot>`) is built inside the
  pre-commit `catch_unwind`; an unwind there stores Failed. Storing it is
  the commit; after it the stream is ended and Sealed stored on every path.
  The sealing emissions and the `SealStep::Stream` hook run under a second
  `catch_unwind` (an unwind there makes the terminal `!failed`); the
  terminal state is decided before its write, and the write's result only
  adds to it: whole keeps `N`, a soft drop gives `N + 1`, a hard failure
  gives `failed` and `N + 1`. Only atomic stores follow (drops, then the
  state with `Release`).
- **Surface.** `Output::bind`, `OutputDir::bind`, `OutputSet::bind`,
  `into_blocks_pipelined` and `PbfWriter::new_in` return `Result`;
  `BlobReader::next` and `PipelinedBlocks::next` yield `Sealed` once and
  end. The emitters return unit and do nothing when refused.
  `stream_account()` is `None` without a call sink. Reserved forms are
  refused on the tagged stream only (untagged `@@x=1` is a counter named
  `@x`). The `MarkerFifo::call` registry is per opened FIFO.
- **`Line::emit`** tells a whole line, a soft drop (overflow, `EAGAIN`,
  `EINTR`, a short write) and a hard failure apart; any hard failure in the
  sink's life ends the stream `failed`.

## Decision B: publication decisions and cancel (#33) (landed as L1)

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
   into destruction. Landed as L0 (the rollback used to remove the
   destination after an `AfterInstalled` failure). Keeping that behaviour would need a protocol effect for destructive
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
    - unbacked replacement is irreversible (ADR-0017 carries it since
      L0).

**Settled by L1's implementation spar** (what the code does, for the
items that list left open):
- **Entry points** take `impl BindOutput` / `impl BindOutputSet` (sealed):
  an `Output` binds to the host's invocation at entry, a `BoundOutput`
  bound elsewhere is refused (`InvalidArgument`) and, dropped, decides
  `refused_error` in its own log. `IndexOutput` stays opaque and unbound;
  the geocode entry binds it.
- **Abandonment guard:** an `Enlisted` (invocation, unit id) held only by
  the bound owner. Its drop runs the one pending-to-decided transition,
  which is a no-op on a decided unit, so publish needs no forget.
- **Allocation and unwind:** the pending entry carries the row's members
  from bind; a decision reserves first, then moves the entry into the row
  without allocating or calling out, so a poisoned log is recovered.
  Refused rows are born finished. The authorized execution runs under
  `catch_unwind`; the row is finalized from the transitions made, then the
  execution lock is released, then the unwind resumes.
- **Effects:** per target, three flags set right after each successful
  syscall and before any step observer: moved aside, put back, new
  generation at the destination. Installed if the new generation is
  there, else restored if moved aside and put back, displaced if moved
  aside only, else none. The fault matrix is in
  `tests/publication.rs`.
- **Re-entry:** a publication attempted on the thread executing one of the
  same invocation (a hook, the sink, a step observer) is refused
  (`InvalidArgument`, `refused_error`), or keeps its earlier decision if a
  cancel already made one. Never waited on.
- **The decision's checkpoint** (`on_check` test hook) runs before any
  lock; the decision reads the token under the log lock.
- **Tickets and permits** (L3): the unit is decided before its permit and
  ticket retire. L1 mints neither; bind, publish, the `Enlisted` drop and
  `cancel` are the sites L3 gates.

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
  | `fixed-resources-executor` | fixed `Resources` and a caller-built `Executor::Pool`; a second, raw-input cat proves the writer framed on that pool |
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
    call whose outcome is cancelled gets none; one cancelled after its
    decision returns ok with its publication complete, and keeps an
    `output` counterpart.
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
- **Isolation scenarios** (brokkr#58): its counterpart must
  `expect: "output"`, and it may have no generated roles. brokkr rejects
  both when it reads the describe document. Before `entry_us`, setup must
  not read dataset contents, start input I/O or build readers or other
  data derived from the datasets for the call. Opening and preparing the
  inputs belong inside the call. Under `--bench`, brokkr alternates timed
  library and CLI runs from a page cache verified cold.
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
- L1, Decision B (#33): the commit that carries this line. Bound owners,
  the per-invocation publication log, admission serialized through
  completion, invocation-wide cancel with `CancelOutcome`, effects from
  namespace transitions, `for_each_publication`, ADR-0017 and ADR-0019
  amended. Its implementation choices are listed under Decision B. For
  L2 and L5: a row's `members` carry destinations, not roles; the harness
  maps each destination to its role. For L4: the log is snapshotted by
  `for_each_publication`'s copy, pure reads into owned data. Residue:
  none in L1's scope; the L3 capability sites are named under Decision B.
- L0, unbacked replacement is irreversible (Decision B item 9),
  `2ffed89c`. `Installed` records what it displaced
  (`Displaced::Nothing`, `Backup` or `Unbacked`, observed at the rename);
  rollback never touches an `Unbacked` install, and publication reports it
  as `RollbackIncomplete` with a note saying the new output remains
  published, plus "durability unconfirmed" only when a directory it
  changed failed to sync. Only the `AfterInstalled` fault reaches it today.
  ADR-0017 carries the rule; ADR-0019 has no rollback text and its
  amendment stays with L1. For L1: an `Unbacked` install that is kept is
  effect `installed`. For L5: a lone replaced file failing after its
  rename now ends holding the new content, not the seed, so a
  `seeded-replace` scenario expecting a refusal judged on final state
  (brokkr#59) must not arm that fault on a lone file.
- L2, the harness skeleton (Decision E): the commit that carries this
  line. Member `harness/` (bin only, unit tests in the bin, executable
  tests in `harness/tests/all.rs` under the CLI's promise guard mounted
  from `cli/tests/guard.rs`), `repeat-cat`, the wiring, and a per-sink
  drop count on `CallSink` (`CallSink::dropped`, folded into the stream
  account at L4). Gate: `brokkr library
  repeat-cat --dataset denmark` fails on exactly `a/brokkr.seal` and
  `b/brokkr.seal`; both `brokkr.cli` results pass under digest `v2`.
  That expectation holds for a digest-compatible (sorted) input; an
  unsorted snapshot adds `brokkr.cli` failures. Implementation rulings:
  - Seal reported `refused` (reason names pbfhogg#31), snapshot null,
    stream `open` with the sink's real drop count. No `stream` or
    `covers` declarations until L6, so no `brokkr.stream` result.
  - Each call gets a fresh `CallSink`, invocation (`with_sink`) and
    host: `host::Fixed` with the machine's parallelism and no memory
    figure, `NoProcessActions`, `PageCachePolicy::Keep`,
    `Executor::Ambient`. Setup starts rayon's global pool before the
    first call; the context reports its size (`executor_threads`), so
    `RAYON_NUM_THREADS` cannot pair two workloads. The harness raises
    soft `RLIMIT_NOFILE` to hard at start (skipping `setrlimit` when
    already there), refuses if it cannot, and reports the read-back
    value per call. Nothing volatile (memory, free descriptors) enters
    a plan or context.
  - Plans spell every option resolved (compression `zlib:6`, backend
    `buffered`, every clean flag, every header override), never
    "default".
  - Runner order: entry_us, the call (outputs built inside it, staged
    `DirWithBackups(private dir)`), return_us, the three emitters, the
    seal attempt, seal_us, the host dropped, the collector closed, then
    publications and drops read, then the call document. A late
    diagnostic is counted, never kept, and fails the scenario-level
    `call-diagnostics-closed` assertion every scenario carries: a
    tripwire, not the guarantee.
  - Destinations map to roles by exact path equality with the call's
    bound outputs; an unmapped one refuses the run. Bindings are checked
    before any call: exactly the roles, inputs canonical regular files,
    outputs absent under resolvable parents, no output sharing a
    resolved path with another role (inputs may share).
  - `outcome.kind` is a typed match over `ErrorKind` (`other` for a
    future kind); a future `CancelOutcome` variant refuses the run
    rather than be misreported.
  - The request is read by a strict parser (duplicate keys, `-0`,
    fractions, trailing bytes refused). Refusals exit 2 with
    `refusal.json` and no output.
  - The hotpath guard exists only in `--test-harness` with
    `HOTPATH_OUTPUT_PATH` set and drops after `scenario.json`.
    `repeat-cat` has two calls, so it is not `hotpath: true`; profiling
    is first exercised by an L5 single-call scenario.
  - Residue for L4 (L3 did not need the stub): replace it with `seal()`,
    `with_call_sink` and the stream account (the drop count moves into
    it). For L5: a `hotpath` scenario's setup may not build generated
    inputs with library writers. Lateral: `Cargo.lock` is gitignored, so
    the resolved dependency graph is not part of a measurement's
    identity.

- L3, Decision A's capabilities: the commit that carries this line. Control
  handle, `Recorder` behind `Call` and `Permit`, the transition word (Open
  only), census tickets, the pool permit; admission always succeeds. Its
  implementation choices are listed under Decision A. Gate: `brokkr check`
  with `tests/cancel.rs` (capabilities back to zero after every command at
  every tripped checkpoint; no command issues its own values; sort's nested
  `sort_with` mints its own guard) and `tests/capabilities.rs` (a block
  iterator parked in the caller's stream holds a permit and its session
  ticket and no guard; exhausted, the ticket only; kept blocks the pool's
  permit and one ticket each; a writer nested in a caller's writer; bound
  outputs issued once, and decided by a cancel before their counts
  retire). Cost: one matched denmark pair of `brokkr write` (call guards on
  `BlobReader::next` and the `PbfWriter` write methods), no cost; numbers in
  `reference/performance.md`. Residue for L4:
  - every mint site (`Invocation::call` and `permit`, `Host::enter`, the
    cold nested mints) becomes fallible or provably nested; the cold
    sites take a handle today and should borrow their enclosing `Recorder`
    once refusal exists;
  - `seal()` reads saturation before the zero counts (saturated refuses);
  - the publication-ordering obligation: retirement is already `Release`,
    the finalizing CAS must `Acquire`;
  - forgotten sink panic payloads that hold a permit veto seal forever, as
    item 8 says.
  Lateral: no brokkr command issues blocks in bulk, so the census lock per
  issued block has no bulk measurement (see `reference/performance.md`).
- L4, the seal (Decisions A and C, #31 and #32): the commit that carries
  this line. `Invocation::seal` with `SealOutcome` / `SealRefusal` /
  `Census` / `Snapshot`, the four-state word, fallible admission
  (`ErrorKind::Sealed`, `CancelOutcome::Ended`), `with_call_sink` and the
  tap, the owned non-`Clone` `CallSink` with its `StreamAccount`, fallible
  `MarkerFifo::call`, the reserved forms, ADR-0020 (ADR-0019 amended). Its
  implementation choices are listed under Decision A. The harness reports
  real seal, snapshot and stream values. Gate: `brokkr check` with
  `src/host.rs` capability tests (classification, saturation, the exchange
  racing retirements and nested calls), `src/debug.rs` sink tests (a
  dropped terminal record, a vanished reader, an unwind before the
  terminal, ids issued once, reserved forms), `tests/seal.rs` (concurrent
  sealers refused until the account is frozen, a seal from a callback
  refused, the in-flight barrier, a panicking tap, a panic before and after
  the snapshot), `tests/capabilities.rs` (an outstanding buffer released,
  then a retry) and the harness's `repeat_cat_runs_to_the_protocol` (every
  snapshot counter's sealing emission and the terminal record on the
  FIFO); `brokkr library repeat-cat --dataset denmark` passes (both
  `brokkr.seal` results pass, streams "pass (not judged)"). Cost: one
  matched denmark pair of `brokkr write`, no cost; numbers in
  `reference/performance.md`. Residue:
  - for L5: a tap that holds its own invocation (the cancel scenarios
    trip it from the tap) forms an `Arc` cycle; hold it in a slot the
    harness clears after the call, as `tests/seal.rs` does. `cancel_json`
    still refuses `CancelOutcome::Ended`, which has no protocol spelling;
    no scenario cancels after a seal;
  - for L6: brokkr#62 decides whether a claimed stream with repeated
    ordinary names can pass.
- L7, the reserving descriptor provider (#36): the commit that carries
  this line. `ProcessActions::fd_allowance` returns an owned
  `FdReservation` (the `FdAllowance` plus an optional release run on
  drop); `host::FdPool` is the reserving provider, one `Arc` shared by
  every host of the concurrent calls, granting `min(wanted, free)` of a
  fixed caller budget as a host grant. Rulings (sparred):
  - Release belongs to the reservation the library holds, not to the
    provider's lifetime: a per-call provider released on `Host` drop
    exhausts the pool for any caller reusing one `Host`, and the harness
    drops its host only after the emitters and seal.
  - Placement: `cat` holds it to the end of the run; `cat --dedupe` holds
    it in `merge_pbf_to` across both passes (admission moved out of
    `plan_merge`, which would have released it before pass 2); altw
    external holds it to the end of stage 1, whose shard files are closed
    when pass A returns. A refusal drops a short grant at once.
  - The budget covers the asked-for demand only. Everything else every
    concurrent call opens (inputs outside a request, outputs' reservation
    descriptors, scratch, external stages 2 to 4, commands that never ask)
    is the caller's to fit under the soft limit less the budget, summed
    over the calls that run at once. The pool never reads the limit or
    counts descriptors, so grants depend on the budget and outstanding
    reservations only; first come, first served, never waiting.
  Gate: `brokkr check` with `src/host.rs`
  `fd_pool_grants_conserve_the_budget` (grants held at once across
  threads) and `tests/fd_pool.rs` (a retained host across two cats, held
  at `CAT_SCAN_START` and whole after each; a short grant refused and a
  failed run both return theirs; dedupe still charged at
  `MERGEPBF_PASS2_START`; external capped to one worker by the pool and
  released by `EXTJOIN_STAGE1_END`, refusing below one worker). No
  measurement: one request per run, not per blob. Residue for L5:
  - the harness still gives every call `NoProcessActions`; concurrent-cat
    shares one `FdPool` across both calls' hosts, sized for both cats'
    full requests (distinct inputs plus `CAT_EXTRA_FDS` each), reports
    its budget in each context (a plan integer, so not volatile), and
    proves overlap by parking, not by thread start order;
  - the nofile scenarios are single-call on `NoProcessActions` at a
    lowered soft limit; the harness can only raise today, so lowering
    (before library work, hard limit untouched, read back and reported,
    never while calls run) lands with them;
  - `nofile-cat-refused` at soft 1024 needs about 1100 distinct path
    spellings: 1000 asks for 1008, which fits when no more than 16
    descriptors are open, so "about 1000" does not refuse by arithmetic;
  - `nofile-altw-external`'s three stage-1 workers need fixed resources
    giving `cpu_cap` of at least 3 (parallelism 5 or more, less the 2
    reserved), not just the lowered limit.
- L5, the remaining scenarios (#34): the commit that carries this line.
  Every scenario of Decision E's table besides `repeat-cat`. The runner
  takes a per-call spec (parallelism, executor, process actions, overwrite
  stance, tap) from which both the host and the context are built, runs
  calls on threads of their own where the plan runs them at once, applies a
  per-scenario soft `RLIMIT_NOFILE` policy (raise to hard, or set exactly,
  hard untouched) before any library work, runs setup for generated roles,
  and starts rayon's global pool only for scenarios that use it. The
  protocol types gained generated and seeded roles, `hotpath`, `isolation`,
  `scratch` and refusal counterparts. Rulings (sparred):
  - `concurrent-cat` reads two generated inputs of one and three blocks.
    Overlap is proven by parking in the caller's `ProcessActions`, not in
    the sink (which must not block): each call's wrapper around the shared
    `FdPool` waits at a two-party gate after its grant, and the second
    arrival reads the pool empty while both are held (a timeout is sticky).
    Each wrapper tracks its own grant's release, which its tap reads at
    `CAT_SCAN_START`, so cat's retention is checked, not only the pool's
    charge. Counter equality is checked on each call's tap; the executable
    test checks the tagged FIFO counters.
  - The cancel scenarios cancel from the tap, on the calling thread. The
    `InstrumentSink` contract now names `Invocation::cancel` as the one
    call a callback may make into its own invocation (it takes only the
    publication log's lock, under which nothing runs but bookkeeping). The
    tap holds the invocation in a slot; the runner releases every tap after
    the seal attempt, before the host drops, on every path.
  - `cancel-post-decision` returns ok with its publication installed and
    has an `output` counterpart.
  - `nofile-cat-refused` names 1100 raw spellings of one input (`./`
    segments), which cat pins one each, and has no counterpart: the CLI
    raises its soft limit when a command asks.
  - `nofile-altw-external` runs at fixed parallelism 8 (a CPU cap of 6), so
    the descriptor cap, 3, binds; the assertion fails, naming the
    allowance, on a launch holding enough descriptors to leave fewer than
    832 available.
  - `seeded-replace`'s refused call is refused when its output is built,
    so it records no publication; no fault is armed.
  - `isolation-cat` is also the `hotpath` scenario (profiled once,
    `--hotpath --force`, with a valid report).
  Gate: bare `brokkr library` (the listing; with `--dataset` brokkr
  requires a scenario or `--all`), then each of the nine with
  `brokkr library SCENARIO --dataset denmark`, all passing. Residue:
  - `fixed-resources-executor` proves only that the call never initialised
    rayon's global registry (`build_global` succeeds afterwards). That the
    writer framed on the caller's pool is not shown: a framing path that
    ignored the executor would pass. It needs an observation point in the
    library; until then L6 must not let this scenario cover a
    caller-pool-execution capability.
- L6, capability ids and declarations (#35): the commit that carries this
  line. `[pbfhogg.library.require]` in `brokkr.toml` names the required
  capabilities; `CallDecl::stream` and `AssertionDecl::covers` carry the
  declarations, validated as capability ids, and the unit test
  `declarations_match_the_require_table` keeps them and the table in step,
  listing the required ids nothing covers with their tickets. Rulings:
  - Every call claims `stream.sealed.v1`. Choosing stream claims by which
    calls survive every reading of brokkr#62 would have shaped the
    declarations around it, so the altw external call claims it too.
  - Required ids no scenario proves stay required, so certification
    classifies them unsupported instead of passing without them:
    `executor.caller-pool.v1` (#34; `fixed-resources-executor` covers only
    `executor.global-pool-unused.v1`), `host.page-cache-keep.v1` and
    `counters.own-snapshot.v1` (#39). The altw below-320 refusal is not
    required: the release gate's list of harness checks does not name it.
  - Assertions were strengthened before covering anything: `own-counters`
    requires each call's exact cat counter inventory once each (the claim
    is about those counters only); the concurrent outcomes also require
    the grant held at `CAT_SCAN_END`; `a.three-workers` also requires
    `extjoin_num_shard_workers` 3; `nofile-cat-refused` now names 1100
    spellings of a path that does not exist and requires that
    `CAT_SCAN_START` never passed, so an input opened before admission
    would have failed with not-found instead of the descriptor refusal.
  Gate: `brokkr library --all --dataset denmark` ran all ten scenarios
  passing, 13 capabilities passed and the three above unsupported: not
  certified, as intended.
- #34, the caller-pool framing proof: the commit that carries this line.
  The premise was half wrong: the old `fixed-resources-executor` call,
  `cat --clean version`, never puts framing on the executor at all. Its
  clean pass frames on cat's own classify workers (`frame_blob_pipelined`)
  and hands the writer finished frames (`write_raw_owned`), so no proof
  built on it could pass. That is the documented Executor scope (cat's
  worker sets are not the executor), not a library defect. Rulings:
  - The observation is a library counter,
    `writer_payload_framed_on_executor_items`, added in the pipelined
    framing job beside `writer_payload_framed_items` when framing
    succeeded and `Executor::on_worker` holds on the framing thread (for
    `Pool`, membership of that very pool; rayon compares registries, so
    another pool or a nested `install` cannot pass). The sync writer never
    counts. Equality with the framed total is the proof; a path that frames
    inline, on another pool or on a library thread gives framed above zero
    and fails. The check must stay beside the framing call: moved into a
    wrapper that delegates framing, it would observe the wrapper.
  - Rejected: caller-side CPU accounting of the pool threads (statistical;
    `writer_frame_ns` excludes compression; cannot show every payload),
    test-hooks counters (brokkr builds the harness without them), and an
    Executor trait for job counting (proves dispatch, not where framing ran).
  - The scenario gains call `b`, `cat --type node,way,relation --force` on
    the `raw` variant, driven off the pool: every blob decoded on the calling
    thread and framed through the writer. `b.framing-on-caller-pool`
    requires no blob passed through, some decoded, framed above zero and
    equal to framed-on-executor; it alone covers `executor.caller-pool.v1`.
    Call `a` stays, covering only `executor.global-pool-unused.v1`; nothing
    asserts its pool idle, since moving cat's framing onto the executor
    would break no contract.
  - Same capability id: its meaning did not change.
  Bench: `reference/performance.md` "Framing placement counter", no cost.
  Gate: `brokkr library fixed-resources-executor --dataset denmark` passes
  (11 results), both counterparts equal under digest `v2`.
- #39, the page-cache `Keep` and own-snapshot proofs: the commit that
  carries this line. Both premises held; no library defect was found
  (no eviction under `Keep`, no counter recorded into another call's
  invocation). Rulings:
  - The `Keep` observation is a library invocation counter,
    `fadvise_input_evictions`, counted at the crate's one
    `posix_fadvise` for the new `Advice::EvictInput` (scratch eviction
    keeps `Advice::DontNeed`), as issued, before the syscall, so a failed
    eviction still counts. It is attributable to the call that asked,
    not read off residency. The variant split labels input eviction; it
    does not enforce it by type: the claim rests on it plus the audit
    that `FileReader::evict_consumed_prefix` is the only input eviction.
  - Rejected: a `ProcessActions` eviction hook replacing the policy (a
    breaking surface that observes no more than the counter, every host
    then owning `posix_fadvise`), and a per-reader count flushed at reader
    drop (lost when a reader outlives the seal; not at the work).
  - New scenario `page-cache-keep`: one generated 3-block input,
    `digest` twice, call `keep` on `Keep`, then call `evict` on `Evict`
    (`cli` null). The one covering assertion, `keep.evicts-nothing`,
    requires keep's sealed count `0` and evict's exactly `4` (one per
    frame, header included), so a dead counter cannot pass the
    capability. `cat` cannot be the contrast: its raw-frame walk never
    evicts under either policy. `CallSpec` carries the policy; contexts
    and plans spell it from the host, no longer as a literal.
  - `concurrent-cat` gains `a.own-snapshot` and `b.own-snapshot`
    (`counters.own-snapshot.v1`): each sealed snapshot's
    `writer_payload_raw_items` equals its input's data blocks (1 and 3)
    and its `writer_bytes_written` equals its installed output's size.
    This shows those counters uncontaminated, not every counter family:
    a decompression counter both cats leave at zero is not judged, and
    timings, high waters and thread-local scratch peaks follow from no
    input.
  - Same capability ids: neither meaning changed.
  Bench: `reference/performance.md` "Input eviction counter", no cost.
  Gate: `brokkr library --all --dataset denmark` certifies, 11 of 11
  scenarios and 16 of 16 capabilities passed.

## Out of scope

- brokkr's side. Everything this design needed from brokkr has landed
  (brokkr `3c59ce8`, `75dc76f`, `6074751`, `2f407c3`, `98bddee`); the open
  brokkr items are listed under bz state above.
- A `crates/` relayout.
