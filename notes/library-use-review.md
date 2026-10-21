# Library-use design: review and open decisions

Companion to `notes/library-use.md` (the Phase 1 audit and design for bz
pbfhogg#1). That document is the design; this one records what came out of
reviewing it: the decisions the user must make before any code moves, a
recommendation for each, and the side findings the audit turned up that are not
part of the design.

Status: Phase 1 done. The user signed off on 2026-10-21 and ruled on all five
questions below as recommended: C1 accepted, C2 declined, public readers keep
the page cache by default, germany interleaved pairs added on top of trace
identity, and strace and ltrace are now installed on bygg. No code has changed.

## Direction the design was written against

- Library users set process-global behaviour explicitly. The library makes no
  process-global decision on a caller's behalf.
- The CLI keeps every value exactly as it is today unless a change is defended
  to the extreme. The default answer is no change, and the CLI's behaviour must
  come out identical: same allocator settings on the same paths, same rlimits,
  same thread counts, same budgets, same stdout and stderr bytes, same exit
  codes.
- Theoretical exercise first: no long A/B benchmarks.

## What the audit established

- The arena cap is irreversible, verified against glibc 2.42 source rather than
  memory: `mallopt(M_ARENA_MAX, v)` ignores `v <= 0`, `arena_get2` computes the
  arena limit once into a function-static and never recomputes it, and arenas
  are never destroyed. The cap is also a silent no-op if the process already
  had more than 8 arenas (glibc's 64-bit default threshold) before it ran, and
  `mallopt` overrides a user's `GLIBC_TUNABLES` arena setting.
- The path-specific choices cannot be rebuilt from CLI flags. `time_filter`
  picks snapshot or history from the input file's header, and
  `add-locations-to-ways --index-type auto` routes to external (and so to the
  `RLIMIT_NOFILE` raise) from `/proc/meminfo` after walking the input. The
  original framing of "move the calls into the CLI" does not work; the path
  knowledge has to stay in the library.
- Scratch names keyed on the process ID collide when two runs share a process:
  two concurrent library `external_join` runs delete each other's scratch, and
  `sparse` and `derive`/`diff` collide the same way. A real bug for library
  callers.
- Other process-ownership assumptions: `PbfWriter` starts and uses rayon's
  global pool even when the caller runs inside its own pool; public readers
  evict the input from the shared page cache by default; 95 `eprintln!` sites
  in library code; about a dozen thread-count sites sized to the whole machine,
  each with its own formula and fallback; a one-time warning in
  `src/read/blob_wire.rs` spent by the first run in the process; the library
  reading `BROKKR_MARKER_FIFO` itself and caching it forever; thread-local
  scratch left on the caller's rayon threads.
- The panic-hook swap in `src/read/reader.rs` is test-only.

## Design in one paragraph

Wherever the library calls `mallopt` or `setrlimit` today, it instead calls a
caller-supplied `ProcessActions` at that same spot; the CLI's implementation
calls the new public `pbfhogg::process::cap_malloc_arenas` and
`raise_nofile_to_hard_cap`, so the CLI's call sequence is identical by
construction. Thread and memory figures come from a `Resources` provider asked
at the original spots with each site's formula intact; `MachineProbe`
reproduces today's probes. Calls take an explicit `Executor`, which makes
`PbfWriter::to_path*` a breaking change. Diagnostics go through a per-run sink
whose CLI implementation is `eprintln!`, byte for byte. Scratch names gain a
per-operation sequence number, and number 0 reproduces today's names. The
BUG-066 deadlock is fixed by cooperative waiting, used only when the writer
runs on a worker of its own pool, which no CLI path does. Verification is an
`LD_PRELOAD` shim logging `mallopt`, rlimit, `posix_fadvise` and thread
creation, plus golden-trace and golden-stderr CLI tests captured against
`fa94c6c7` before the first code change.

## Decisions for the user

Each carries a recommendation from the review; none is decided.

### Q1 - C1: a closed stderr no longer kills the run

Today `pbfhogg sort ... 2>&1 | head` exits 101 when `head` closes the pipe,
because `eprintln!` panics on the write error (BUG-067). C1 suppresses only
broken-pipe errors on stderr, keeps the command's own exit status, and covers
every CLI stderr path including `main`'s `Error:` line.

Recommendation: accept. It is the stderr half of the stdout broken-pipe fix
already landed, with the same narrow rule.

### Q2 - C2: cancellation guard for framing panics

A guard that turns a panic in a rayon framing task into an orderly
cancellation instead of rayon's default abort.

Recommendation: decline. A framing panic is a bug, and aborting reports it
honestly. Sparring showed the guard's send can block and its panic-path stderr
can differ from today's, which runs against the CLI-unchanged rule for little
gain.

### Q3 - page-cache eviction default for public readers

Public file readers evict the input's pages from the shared page cache by
default. Options: flip the library default to not evicting, with the CLI
requesting eviction explicitly; or keep evicting as a documented exception.

Recommendation: flip. A library should not drop its host's page cache unless
asked. The CLI's behaviour is unchanged either way, since it would request
eviction explicitly.

### Q4 - is trace identity enough to show performance is unchanged?

The design proves the CLI's process-level behaviour unchanged by trace
identity. That shows the same `mallopt`, rlimit, fadvise and thread-creation
sequence; it does not show the new indirection is free (provider calls, the
executor handle, the diagnostics sink).

Recommendation: add short interleaved pairs on germany as well. Its input
stays in page cache, each cell takes seconds, and the 2026-10-21 A/B on bygg
(`reference/performance.md`) showed it resolves small CPU and synchronization
differences that europe-scale cells bury in drive drift. This is not a long
A/B.

### Q5 - install strace and ltrace on bygg

None of strace, ltrace, gdb, bpftrace or uftrace is installed, which is why the
design builds its own `LD_PRELOAD` shim.

Recommendation: install them as an independent cross-check of the shim. The
shim is built regardless. Installing needs the user (sudo).

## Side findings, not part of the design

To file in `notes/bugs.md` or `notes/hygiene.md` when the design lands, or
sooner:

- The `WRITE_AHEAD` doc comment in `src/write/pipeline.rs` says no send can
  ever block. Sends do block briefly while the writer is writing. No deadlock,
  but the stated reasoning is wrong.
- `diff/parallel.rs` puts its scratch next to the old input file, which may be
  on a read-only filesystem.
- `derive.rs` sizes its gzip workers from the machine and ignores the resolved
  `-j`.
- `emit_mallinfo2` calls `mallinfo2()` and allocates even when no profiler is
  attached.
- The eviction comment in `blob.rs` says eviction keeps the file out of process
  RSS; page cache filled by `read()` is not process RSS. Medium confidence.
- Raising `RLIMIT_NOFILE` is inherited by any child process the caller starts,
  and breaks `select()` for descriptors above 1024.
- The panic-hook test in `src/read/reader.rs` silences other tests' panic
  output while it runs.
- `reference/testing.md` and `reference/performance.md` will go stale when the
  design lands; the spec includes updating them.
