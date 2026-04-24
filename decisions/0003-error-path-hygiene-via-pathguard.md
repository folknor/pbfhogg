# ADR-0003: Error path hygiene via PathGuard and counters-after-write

Date: 2026-04-24
Status: Accepted

## Decision

On a mid-stream error, pbfhogg's recorded state (summary counters,
output artifacts, scratch directories) must not diverge from work
actually emitted. Enforce via a shared RAII primitive plus a
counter-ordering rule. No hot-path cost; every future command with
file output or scratch inherits the pattern by following the
checklist below.

**Primitive:** `src/path_guard.rs::PathGuard`. Wraps a `PathBuf` and
removes the pointed-at file or directory on Drop, unless `commit()`
was called first. Separate constructors for files
(`PathGuard::file` -> `remove_file` on drop) and directories
(`PathGuard::dir` -> `remove_dir_all` on drop). Happy-path cost is
one `Option::take`; filesystem work only runs on the error path.

**Rule:** counters that track emitted work bump *after* the
consumer's successful `writer.write_*` call, not before the
worker's `tx.send`. Intermediate debug/throughput counters (work
attempted, blobs processed, etc.) can bump wherever, but
user-facing summary counters must match output.

**Checklist for new commands:**

- Every final output path is wrapped in `PathGuard::file` at
  writer-construction time; `commit()` is called on the success
  leaf, after the writer's final `flush()`.
- Every scratch directory is wrapped in `PathGuard::dir` at
  `create_dir_all` time; no `commit()` call (scratch is always
  cleaned up). Drop explicitly to release disk ASAP when
  downstream stages are done with it.
- User-facing counters bump in the consumer, after the successful
  write.
- If a pre-existing crash could have left stale scratch dirs,
  retain an entry-time sweep (PathGuard's Drop doesn't run if the
  prior process was SIGKILLed).

## Alternatives considered

- **(b) Per-site bespoke shape.** Smaller per-site diffs, no new
  primitive. Guarantees the posture is inconsistent across
  commands: the next command to need the pattern would invent it
  from scratch, and any audit of "which commands are safe against
  mid-stream errors" becomes four separate code reads instead of
  one.
- **(c) Corruption-class only.** Fixes items 3 and 4, closes items
  1 and 2 as "accepted." Saves a handful of lines. But the counter
  drift surfaces to users directly in the command's summary output,
  and a half-written PBF is a real hazard for downstream tooling
  (osmium can't `--fix` a file that ends mid-blob). Both are worth
  fixing even if neither is a hard corruption.
- **(d) Documented-accepted, close cluster.** Minimal commitment,
  highest future-regret. Users already see an error on the abort
  path; the fix here is about what the error leaves behind, not
  whether the error surfaces.
