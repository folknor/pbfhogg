# Bug hunt: write path (`src/write/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/write/` in full. Findings
only, unverified. Ordered roughly by severity as the hunter ranked them.

Rounds 1, 2 and 3 are landed; their resolved findings are removed rather than
archived - git history is the record. The structural notes round 3 owned are
closed: five as code changes, the sixth as
[ADR-0014](../decisions/0014-output-backends-stay-separate.md), which records
why the four output backends stay four implementations and corrects the
premise that all four duplicated the alignment logic (only two ever did).

What remains below is the one item deliberately left open: a coverage gap
that cannot be closed from inside a work session.

## OPEN: the io_uring writer changes are unexercised on real data here

Not a bug - a coverage gap that the release gate has to see.

Round 2 changed three things inside `src/write/uring_writer.rs`: the `Drop`
teardown drain (`drain_best_effort`), the `NUM_BUFS <= 64` compile-time
assertion against the six-bit mask in `ud_buf_idx`, and
`OutputChunk::CopyRange` carrying an `Arc<File>` rather than a `RawFd`. All
three have compile-time and unit coverage. None has been run against a real
PBF, because this host's `RLIMIT_MEMLOCK` soft limit is 8 MB and the writer
registers 64 x 256 KB = 16 MB, so the io_uring preflight refuses before any
SQE is submitted. `brokkr check` (both profiles) and `brokkr verify all
--dataset denmark` are green, and neither runs an `--io-uring` variant.

Close it by running one `--io-uring` command on a host whose limit is raised
(the same `limits.conf` change that blocks the sort `--io-uring` re-pin and
the io_uring output-rail characterisation - see TODO.md, "Deferred with
explicit triggers"). Do not raise the limit from inside a work session; it is
a host-configuration change, not a code change.

This item is also written up in TODO.md twice, on purpose, because a release
gate owner will not read `notes/`: once under "Deferred with explicit
triggers" alongside the other work the same limit blocks, and once as a
"Release gate" subsection at the top of "Planet-scale validation coverage",
which is the section a sign-off actually goes through. `notes/` keeps this
copy because it is where the write-path hunt's record lives; the TODO.md
copies are the ones that have to be found.

The corrected diagnosis of the teardown hazard itself - real asymmetry, but
not memory corruption while `WriteFixed` is the only opcode submitted - is
recorded durably on `impl Drop for UringState` in the source, which is where
it has to live to survive this note.
