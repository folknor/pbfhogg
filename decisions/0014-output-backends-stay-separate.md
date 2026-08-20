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
`PermitGuard` write-ahead discipline, `WriterMetrics::record_sync_all` and
`record_recv_wait`, and the `flush`-versus-`sync` split. Adding a fifth
backend, or changing any of those four shared contracts, is the trigger to
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
