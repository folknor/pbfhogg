# ADR-0020: Seal an invocation by one exchange that never waits

Date: 2026-10-30
Status: Accepted

## Decision

`pbfhogg::host::Invocation::seal()` ends an invocation: it closes
admission, takes the final snapshot of the invocation's counters, and ends
the call stream of the invocation's own call sink. It never waits and never
retries. A caller that needs final values (a harness writing a call
document, a host reporting a run) seals once the call has returned and
seals again after it has dropped what it still holds.

- **Three roles, three mechanisms.** The `Invocation` is a cloneable control
  handle: identity, the cancellation token and the reads
  (`for_each_counter`, `for_each_publication`, `stream_account`). Holding or
  leaking one keeps nothing open. Recording needs a capability: a `Call`
  guard, held by every public entry that can record for its extent
  (commands, methods of library values, `cancel`, the end-of-run emitters),
  or a `Permit`, held by a value that records outside a public call (a
  writer and its threads, a block iterator's pipeline, a decompression pool
  kept alive by blocks, a bound output) past its last recording. Values of
  a census kind (`session`, `writer`, `output`, `buffer`) that crossed to
  the caller hold a census ticket until they cease to exist. The recording
  interface is reachable only by borrowing a capability, so the compiler
  rejects a recorder that holds none.
- **One transition word.** One atomic holds the lifecycle state (Open,
  Finalizing, Sealed, Failed), a sticky saturation flag and the live guard
  and permit counts. A public entry mints its guard by one exchange that
  requires Open, so admission after a seal began is refused with
  `ErrorKind::Sealed` (`cancel` returns `CancelOutcome::Ended` and trips
  nothing). A capability minted under a live one cannot be refused: a
  counted capability keeps a count above zero and an uncounted one the
  saturation flag, and the seal's exchange needs neither. A count at its
  field's maximum sets the flag in the same Open-conditioned exchange
  instead of counting, and a saturated invocation can never seal.
  Retirement is `Release`; the seal's exchange is `Acquire`.
- **The seal is one exchange from Open with everything zero to
  Finalizing.** On failure it classifies the word it observed, in order: a
  guard alive is `Refused(InProgress)` (what a seal from inside a callback
  of a call gets); Finalizing is `Refused(Finalizing)`; Sealed returns the
  stored snapshot; Failed is `Refused(FinalizationFailed)`; saturation is
  `Refused(Saturated)`; with permits alive, the census under a non-blocking
  lock: `Outstanding(census)` when an issued value is alive,
  `Refused(Unticketed)` when none is (a permit the caller cannot release,
  inside a leaked value or a forgotten panic payload), `Refused(Busy)` on
  contention.
- **The word is the barrier.** Every emission into the sink and the tap
  runs while the capability its recorder was borrowed from is alive, and
  that capability retires after the emission returned. So when the
  exchange succeeds, every callback into the sink has returned and none can
  start: no in-flight count is kept.
- **Finalization runs no caller code, in two regions split by one commit
  point.** Before it, the winning thread takes the snapshot (every counter
  `for_each_counter` visits, as owned data); an unwind there marks the
  invocation Failed, writes no sealing record and leaves the stream open.
  The commit point stores the prepared snapshot; from there the invocation
  ends Sealed on every path. The call sink stops admitting ordinary
  records, writes `@@name=value` for every snapshot counter (saturated at
  `i64::MAX`, zeros included), then one terminal record, `!sealed drops=N`,
  or `!failed drops=N` once any write of the sink failed outright or the
  sealing emissions unwound. After the terminal write only atomic stores
  run: the account keeps `N` when the terminal went out whole, counts it
  (`N + 1`) when it was dropped, and is failed when its write failed
  outright, so the account always agrees with a terminal record that
  reached the FIFO. Then the word is set Sealed. Concurrent sealers see
  Finalizing until then.
- **The invocation owns its call sink.**
  `Invocation::with_call_sink(CallSink, Option<Arc<dyn InstrumentSink>>)`
  takes a non-`Clone` `CallSink` by value, so a seal ends the stream of the
  one invocation it belongs to. The optional tap sees every ordinary record
  after the sink and nothing at sealing; a panicking tap is latched off on
  its own. `MarkerFifo::call` refuses an id it has already issued, and the
  call stream refuses the forms reserved for ending it (a marker name
  starting `!`, a counter name starting `@`). Generic sinks
  (`Invocation::with_sink`) have no sealing semantics.
- **Liveness of drop is the caller's.** Dropping a block iterator whose
  pipeline is blocked in the caller's own `Read` blocks until that read
  returns.

The protocol this serves is brokkr's library harness protocol
(`brokkr man library`, The artefacts and The call stream).

## Alternatives considered

- **A draining seal** that closes admission and then waits for calls and
  workers. Rejected: the library owns no asynchronous work that outlives
  its owner (library pools are joined, writers wait for their framing), so
  the drain waited for nothing it could not refuse instead, and brought
  every waiting hazard with it: a lost wakeup, starvation, deadlock on an
  executor worker, nested admission and a callback marker.
- **Sealing by unique ownership** (`seal(self)` and `Arc::try_unwrap`).
  Rejected: a forgotten panic payload holding a handle vetoes it forever; a
  handle count is not the census of values the caller holds; and a failed
  `Weak` upgrade cannot tell sealed from dropped.
- **No library seal**, the caller deciding when values are final.
  Rejected: nothing would guarantee final values or end a call stream.
- **An in-flight emission count in the call sink**, waited for after the
  sink stops admitting. Rejected: the word already orders every emission
  before the exchange, so the count would add work to every emission and
  prove nothing new.
- **A generic sink ended by the harness, or a `sealed` hook on
  `InstrumentSink`.** Rejected: neither can prove which invocation a sink
  belongs to.
- **Dropping a caught sink panic payload** to release a capability it may
  hold. Rejected: its destructor is caller code and could panic again; the
  permit it holds is reported truthfully as `Unticketed` instead.
