# ADR-0014: The four output backends stay separate implementations

Date: 2026-08-20
Status: Accepted

## Decision

`FileWriter` (buffered), `DirectWriter` (`O_DIRECT`), `UringState`
(`io_uring`), and `parallel_writer`'s `pwrite` pool remain four separate
implementations behind the `OutputSink` boundary. We do not collapse them
into one buffering-and-alignment core parameterized by submit strategy.
What they must share, they share explicitly and by name: the
`SeqTotal`/`check_stream_complete` end-of-stream handshake, the
`PermitGuard` write-ahead discipline, `WriterMetrics::sync_recorded` (the
one recorded fsync), `record_recv_wait`, `time_handoff` and
`record_accepted` (so each writer counter means one thing on every
backend) - since 2026-10-26 composed in one ordered-delivery loop, see the
amendment below - and the `flush`-versus-`sync` split. Adding a fifth
backend, or changing any of those shared contracts, is the trigger to
revisit this.

The premise usually offered for unification does not survive checking. The
four do **not** each re-implement "accumulate, page-align, write,
`set_len`": only `DirectWriter` and `UringState` do. `FileWriter::Buffered`
is a `BufWriter<File>` - it accumulates via std and never page-aligns or
truncates. `parallel_writer`'s pool does none of the four: workers `pwrite`
whole owned buffers at pre-computed absolute offsets, never accumulate,
never align, and deliberately never `set_len` on the success path, because
the kernel extends EOF to the highest offset written. The genuine
duplication is one pair, not a set of four.

## Amendment (2026-10-26): one ordered-delivery loop

The three pipelined writer threads (buffered and `O_DIRECT` through
`FileOutputSink`, `io_uring`, the `pwrite` pool's dispatcher) used to carry
three copies of one receive loop: timed `recv`, reorder buffer push and
high-water, drain of the ready run with each chunk timed as one handoff,
the `OutputChunk` match with its per-operation accepted-byte count, and
the end-of-stream completeness check. That loop is now one function,
`pipeline::deliver_in_order`, and each backend is an `OutputSink`
supplying only its byte path (`write_owned`, `copy_range`) and its
between-items maintenance (`after_drain`: io_uring's non-blocking CQE
reap, the pool's stopped check). Finalization - last flush, ring drain,
pool join, fsync - stays with each backend, after the loop returns.

This does not reverse the decision above. The loop is the composition of
contracts this ADR already requires every backend to share
(`check_stream_complete`, `record_recv_wait`, `time_handoff`,
`record_accepted`); it flattens chunks into owned byte buffers but manages
no backend buffer, alignment, submission or completion. What the decision rejects is a shared buffering and
alignment core, and the four byte paths remain four implementations with
their own guards. Sharing the loop by code rather than by convention is
what keeps each writer counter meaning one thing on every backend: a
fourth copy of the loop could drift where a shared function cannot. A
`brokkr.toml` textlint keeps the loop's primitives out of every other
file under `src/write/`.

## Alternatives considered

- **One core parameterized by submit strategy (write / pwrite-pool /
  uring).** Rejected: the only logic actually common to `DirectWriter` and
  `UringState` is the page-rounding arithmetic and the zero-padded tail -
  roughly twenty lines. Everything else that looks alike is not: one holds
  a single reusable buffer and writes synchronously, the other holds a pool
  of sixty-four registered buffers whose lifetime is driven by completion
  events. Abstracting "obtain a buffer / hand it to the submit strategy /
  wait for it back" *is* the asynchronous structure, so the shared core
  would be as large as the two implementations it replaced. The pool has no
  buffer lifecycle at all and would sit outside the abstraction regardless.
- **Unify anyway, for the sake of one bug-fix surface.** Rejected on
  validation grounds, which are decisive here. The `io_uring` backend
  cannot be exercised against real data on the current bench host: the
  writer registers 64 x 256 KB = 16 MB and the host's `RLIMIT_MEMLOCK` soft
  limit is 8 MB, so the preflight refuses before a single SQE is submitted.
  A rewrite of a backend that no test run can execute end-to-end is how a
  silent regression ships. Separately, each of the four currently carries a
  recently-added guard at its own layer - `DirectWriter`'s write-after-flush
  refusal, `UringState`'s teardown drain and `NUM_BUFS` compile-time
  assertion, `FileWriter`'s `flush`/`sync` split, the pool's permit-held
  send sites - and a unification that quietly dropped any one of them would
  cost more than the duplication it removed.
- **Make `Drop` without an explicit `finish()` a loud error across all
  four.** Not adopted as part of this decision. It is a real improvement in
  principle, but two of the four backends would have to change their
  teardown behaviour to get it and one of those two is the unexercisable
  one; the `Drop` paths are best-effort precisely because they are the paths
  taken when something has already gone wrong. Callers that want errors call
  `flush()` and `sync()` by name, and both are documented as the way to get
  them.
