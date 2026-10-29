# ADR-0019: Cancellation is a caller's token, checked where the run already passes

Date: 2026-10-23
Status: Accepted (amended 2026-10-29: explicit binding, the publication
log, invocation-wide cancel; amended 2026-10-30: cancel after a seal,
ADR-0020)

## Decision

A run stops early only because its caller asked, through the
`pbfhogg::host::Invocation` token in `Host::invocation`, and it stops the way a
failure does: workers wind down, every guard drops (output staging and the
run's scratch directory are removed), nothing is published, and the call
returns `ErrorKind::Cancelled` (`Error::is_cancelled`).

- **The library never trips a token and never touches signals** (ADR-0016).
  `Invocation::cancel` is callable from any thread (not a signal handler: it
  takes the invocation's publication log lock, which nothing holds for longer
  than its own bookkeeping, and never waits for a publication in progress);
  tripping is permanent, so a token belongs to one invocation, or to a group
  meant to stop together. Cancel is invocation-wide: under the log lock it
  trips the token and decides every output still pending (bound and not yet
  decided) `refused_cancelled`, in bind order, and returns what it found -
  `BeforeDecision { refused }` with the first ordinal it refused,
  `AfterDecision { authorized }` with the latest authorized ordinal when
  nothing was pending (that publication completes regardless), or
  `NoPublicationPending`. Every decision after it refuses. Cancelling one
  command among several needs separate invocations. Once the invocation's
  seal has begun (ADR-0020) there is nothing left to cancel: `cancel`
  returns `Ended` and trips nothing, and binding an output to it is refused
  with `ErrorKind::Sealed`.
- **Checkpoints sit only where the run already passes**: a blob read or
  dispatched, a result received from a pool, a stage or pass boundary, a
  block or buffer written through an `Output`'s writer, a scratch shard or
  bucket started, and the publication decision. Each is one relaxed load and a branch. No
  per-element work, and no new per-blob work on a path that did not already
  do per-blob work (a blob read, a channel receive) at that point. A phase
  with no such point - an in-memory parallel pass, a sort of a rank table, a
  single large `copy_file_range`, the geocode builder's own data-file writes
  into its output directory - is not interrupted inside; the next
  boundary observes the token. That is the trade: an untripped token must
  cost nothing measurable, so the worst-case latency is one such phase.
- **A checkpoint exits only through a path that reports its cause, wakes or
  closes every participant, and keeps sequence accounting.** In an ordered
  pool the consumer checks (a worker that skipped a sequence number would
  stall the reorder buffer); workers check only where there is no sequence
  (unordered shard, bucket and accumulate pools). The first error recorded
  wins and is recorded before peers are woken, so a run that failed and was
  then cancelled reports the failure, and one that was cancelled reports
  `Cancelled`, not a disconnected channel.
- **Outputs are bound to their invocation explicitly, before work.** An
  output is constructed unbound (it can be built before a run's per-call
  token exists) and `Output::bind(self, &Invocation)` (likewise `OutputDir`
  and `OutputSet`, whose members inherit the set's binding) consumes it into
  a bound owner whose invocation cannot change. Binding enlists the output as
  pending in that invocation's publication log: the invocation has accepted
  responsibility for deciding it. The writing session opens only on the bound
  value, under its invocation, and `BoundOutput::publish(self)` decides under
  the same one, so the decision and the run cannot consult different tokens.
  Command entry points take an unbound output, which they bind to the host's
  invocation at entry, or a bound one, which they refuse (invalid argument)
  unless it is bound to the host's invocation; a command that fails before it
  opens its output has still decided it.
- **The completion decision.** Every bound output is decided exactly once,
  under its own invocation's log lock: by its publication, by `cancel`, or by
  its owner being dropped undecided (`refused_cancelled` once the token is
  tripped, `refused_error` otherwise; a writer or a set member is not an
  owner). Publication takes the invocation's execution lock first and holds
  it until its effects are recorded, so the publications of one invocation
  are admitted one at a time through completion and their ordinals are their
  execution order. Under the log lock it then decides: a tripped token
  refuses (`Cancelled`), an output not ready to publish (its session
  unfinished, its staging replaced) refuses with that error, anything else is
  authorized; a unit `cancel` already decided stays decided. After an
  authorized decision the publication completes and its own outcome is
  authoritative (ADR-0017). A callback of a publication in progress (a
  `before_publish` hook, the instrument sink) publishing under the same
  invocation is refused as re-entry, never waited on. The decisions are
  readable as rows through `Invocation::for_each_publication`. A read-only
  command decides at its last checkpoint instead: a token tripped by the time
  its work is done returns `Cancelled`, one tripped after leaves the result
  standing (what the CLI then prints of it is not interrupted). Writers built
  from a bound output observe the token on every call that writes:
  `BoundOutput::pbf_writer`'s `PbfWriter` on each write method (its
  end-of-stream `flush` and the drain of already-queued blobs do not check;
  publication does), `BoundOutput::writer` on each buffer flush and in
  `OutputWriter::finish`, so a run whose output is its bottleneck stops at its
  next write.
- **The CLI's policy.** SIGINT and SIGTERM are blocked before any thread
  starts and taken by one `sigwait` thread, so no signal handler ever runs.
  The first signal trips the token; the run ends with exit status
  `128 + signo` (130, 143) and one line saying it was interrupted and
  published nothing. A second signal `_exit`s with `128 + signo` of that
  signal, skipping the cleanup still under way; what that leaves is reported
  by the next run's leftover scan (ADR-0018). A signal that arrives after the
  run already completed is noted and does not change the outcome. The waiter
  never writes to stderr itself, since a full pipe could keep it from taking
  the second signal.

## Alternatives considered

- **A process-wide flag set from a signal handler inside the library.**
  Rejected by ADR-0016: an embedding caller owns its signals, and two
  concurrent invocations must be stoppable independently.
- **Checking per element, or per blob in every worker.** Rejected: the hot
  loops are the product, and a check in an ordered pool's worker would skip a
  sequence number and stall the reorder buffer unless every pool learned to
  account for it.
- **Polling the token from a watchdog thread that closes channels or kills
  workers.** Rejected: a closed channel looks like a peer's failure, and the
  run would report that instead of the cancellation.
- **Mapping every error that occurs after the token tripped to `Cancelled`.**
  Rejected: a real failure (a full disk, corrupt input) racing a signal must
  still be reported; only an actual checkpoint produces `Cancelled`.
- **Interrupting publication.** Rejected: a half-installed group or a skipped
  directory sync would leave a worse state than finishing a decided
  publication, which is bounded by the output's own `fsync`. Rolling back an
  irreversible replacement is rejected for the same reason (ADR-0017): a
  failure after a lone file was renamed over its predecessor keeps the new
  output, reported as installed.
- **The token as an argument to `publish`, with the writing session
  recording the invocation it opened under.** The earlier shape. Rejected: an
  output bound at its first writer open is unbound when a command fails or is
  cancelled before opening it, so nothing records its refusal; two members of
  a set could open under different invocations with nothing to arbitrate
  which one decides; and the decision could not be made by `cancel` at all.
- **A compare-and-swap decision word per invocation.** Rejected for one
  mutex of plain data per invocation, the log: a decision has to move a unit
  from pending to an ordinal-numbered row with its destinations, which one
  word cannot carry, and the log is touched once per publication, never on a
  hot path.
- **Letting the default disposition kill the process.** That was the state
  before this decision: every SIGINT left staging and scratch behind,
  possibly hundreds of GB of it, for the next run to report.
