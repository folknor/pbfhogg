# ADR-0019: Cancellation is a caller's token, checked where the run already passes

Date: 2026-10-23
Status: Accepted

## Decision

A run stops early only because its caller asked, through the
`pbfhogg::host::Invocation` token in `Host::invocation`, and it stops the way a
failure does: workers wind down, every guard drops (output staging and the
run's scratch directory are removed), nothing is published, and the call
returns `ErrorKind::Cancelled` (`Error::is_cancelled`).

- **The library never trips a token and never touches signals** (ADR-0016).
  `Invocation::cancel` is lock-free and callable from any thread; tripping is
  permanent, so a token belongs to one invocation, or to a group meant to
  stop together.
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
- **The completion decision.** Publication checks the token once, first
  (`Output::publish(&Invocation)`, and likewise `OutputDir` and `OutputSet`);
  after that the publication completes and its own outcome is authoritative
  (ADR-0017). A read-only command decides at its last checkpoint instead: a
  token tripped by the time its work is done returns `Cancelled`, one tripped
  after leaves the result standing (what the CLI then prints of it is not
  interrupted). Writers built from an `Output` observe the token on every
  call that writes: `Output::pbf_writer`'s `PbfWriter` on each write method
  (its end-of-stream `flush` and the drain of already-queued blobs do not
  check; publication does), `Output::writer(&Invocation)` on each buffer flush
  and in `OutputWriter::finish`, so a run whose output is its bottleneck
  stops at its next write. The token is an argument, not something an
  output captures, so the decision and the run cannot consult different
  tokens by accident.
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
  publication, which is bounded by the output's own `fsync`.
- **Letting the default disposition kill the process.** That was the state
  before this decision: every SIGINT left staging and scratch behind,
  possibly hundreds of GB of it, for the next run to report.
