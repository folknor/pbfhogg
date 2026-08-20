# Bug hunt: write path (`src/write/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/write/` in full. Findings
only, unverified. Ordered roughly by severity as the hunter ranked them.

Rounds 1 and 2 are landed; their resolved findings are removed rather than
archived - git history is the record. What remains below is what is still
open: one coverage gap, and the structural notes the next round owns.

## Smaller / structural notes

- `frame_blob_into` clones both scratch buffers per blob (`framing.rs`) while the
  doc block directly above claims "after warmup, only the returned `out` Vec is
  allocated per call". Two allocations plus two memcpys per blob, and the comment
  is wrong about it. A capacity-preserving swap (keep a small pool of Vecs in
  `FrameScratch`, swap in an empty one with reserved capacity) gets the
  documented behaviour.
- `encode_blob_body` uses `encode_bytes_field` (skip-if-empty) for the raw /
  compressed payload. A zero-length payload yields a `Blob` message with no
  fields at all. Should be `_always` for correctness of the wire contract; today
  unreachable but it is a latent malformed-output path.
- `WRITER_METRICS` is a process-global static, and `PbfWriter::flush()` calls
  `emit()`. `multi-extract` constructs one `PbfWriter` per region, so counters
  are cross-contaminated across regions and re-emitted (cumulatively) once per
  region flush. Per-writer attribution is impossible and sidecar counters for
  multi-writer commands are misleading.
- `parallel_writer.rs` module doc is stale and misleading: "Status: scaffold
  (2026-04-21) ... `to_path_parallel` / integration lands in a follow-up commit"
  - it is integrated and is the default backend for `apply-changes` and ALTW pass
  2. The whole module also sits under `#![allow(dead_code)]`, which suppresses
  genuine dead-code detection for a now-live backend. `POOL_SIZE` is 16 while its
  doc justifies 8.
- `raw_passthrough.rs` is 106 lines of `#[allow(dead_code)]` scaffolding whose own
  doc says two measurements disproved the idea at planet scale (0/50,364 and
  0/32,835 qualifying blobs). The analysis is valuable; the code is not. Move the
  prose into `notes/` and delete the module.
- `FileWriter`, `DirectWriter`, `UringState`, and `parallel_writer`'s pool each
  re-implement the same "accumulate, page-align, write, set_len" logic with
  different flush/durability semantics and different bug sets (the DirectWriter,
  uring pool, and fsync findings above are all instances). Four backends behind
  one `OutputSink` trait, three of which own their own file lifecycle and one of
  which does not, is the structural problem underneath several findings above. If
  willing to rewrite: collapse to one buffering/alignment core parameterized by
  submit strategy (write / pwrite-pool / uring), with a single explicit
  `finish() -> io::Result<()>` that is the only place `set_len` plus `sync_all`
  happen, and make `Drop` without `finish` a loud error rather than a silent
  best-effort.

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

The corrected diagnosis of the teardown hazard itself - real asymmetry, but
not memory corruption while `WriteFixed` is the only opcode submitted - is
recorded durably on `impl Drop for UringState` in the source, which is where
it has to live to survive this note.
