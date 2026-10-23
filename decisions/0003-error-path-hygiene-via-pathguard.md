# ADR-0003: Error path hygiene via PathGuard and counters-after-write

Date: 2026-04-24
Status: Accepted; the rules for caller-named outputs are superseded by
[ADR-0017](0017-outputs-are-published-by-the-library.md), and the rules
for scratch directories and entry-time sweeps by
[ADR-0018](0018-scratch-is-one-run-directory-and-leftovers-are-reported.md)

## Decision

On a mid-stream error, pbfhogg's recorded state (summary counters,
output artifacts, scratch directories) must not diverge from work
actually emitted. Enforce via a shared RAII primitive plus a
counter-ordering rule. No hot-path cost; every future command with
scratch inherits the pattern by following the checklist below.

**Primitive:** `src/path_guard.rs::PathGuard`. Wraps a `PathBuf` and
removes the pointed-at file or directory on Drop, unless `commit()`
was called first. Separate constructors for files
(`PathGuard::file` -> `remove_file` on drop) and directories
(`PathGuard::dir` -> `remove_dir_all` on drop). Happy-path cost is
clearing a bool; filesystem work only runs on the error path.

**Outputs are not scratch.** A path the caller named is a
`pbfhogg::output` value - checked, staged, and published only on
success by the library entry point that owns it - and never a
`PathGuard`, which deletes whatever is at its path. ADR-0017 owns that
rule; this record originally put the output guard at the CLI front end,
and that part is superseded.

**Rule:** counters that track emitted work bump *after* the
consumer's successful `writer.write_*` call, not before the
worker's `tx.send`. Intermediate debug/throughput counters (work
attempted, blobs processed, etc.) can bump wherever, but
user-facing summary counters must match output.

**Checklist for new commands:**

- Every output goes through `pbfhogg::output` per ADR-0017: the entry
  point takes an `Output` (or `OutputDir`, `OutputSet`) and publishes it
  last.
- Scratch goes in the run's `RunScratch` directory per ADR-0018, never
  in a `PathGuard` of its own; `PathGuard` remains for a path outside
  that directory which the run itself created and owns, and never for a
  caller-named path.
- User-facing counters bump in the consumer, after the successful
  write.
- Leftovers of a run killed outright are reported by
  `scratch::find_stale` (ADR-0018), never swept.

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
