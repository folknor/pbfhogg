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

## Consequences

- New crate-private module `src/path_guard.rs` with three unit
  tests (drop-removes, commit-preserves, recursive-dir).
- `renumber/relations.rs` - worker-side
  `rels_written.fetch_add(blob_count, Relaxed)` removed; consumer
  bumps both `rels_written` and `r2d_orphans` after the successful
  `write_primitive_block_owned`. Same total atomic count; one
  moved from worker to consumer.
- `renumber/mod.rs` - output path wrapped in `PathGuard::file`
  immediately after `writer_from_header`; `output_guard.commit()`
  at the tail of the success path (after `writer.flush()?`). Any
  mid-stream error removes the partial file via Drop.
- `geocode_index/builder/pass3.rs` - `fine_bucket_dir` and
  `coarse_bucket_dir` wrapped in `PathGuard::dir`; each guard
  `drop`ped explicitly right after its tree's `run_stage_b`
  succeeds (disk pressure). The redundant `remove_dir_all` at the
  tail of `run_stage_b` removed. Entry-time stale-dir sweep
  retained for SIGKILL recovery.
- `geocode_index/builder/pass3.rs` - `parse_bucket_file` now
  returns `io::Result`; errors on `data.len() %
  BUCKET_RECORD_SIZE != 0` with a message citing the incomplete
  file length.
- `CHANGELOG.md` - three new Bug-fix entries (renumber partial
  output on error; geocode bucket-dir leak; geocode silent
  truncation).
- **Performance:** no hot path touched. Counter bumps: same total
  atomic count, different owner thread. PathGuard Drop on success:
  one `Option::take` plus a best-effort `remove_dir_all` on an
  already-empty directory. No new syscalls on the happy path.
- **Compliance map (commands that already follow the pattern):**
  renumber (this ADR), geocode builder Pass 3 (this ADR).
  Existing `altw/external/radix.rs::ScratchDir` is a pre-existing
  always-remove variant that pre-dates `PathGuard`; it's
  compatible with the rule and does not need migration unless it
  grows a "survive on success" use case.

## Cross-references

- `src/path_guard.rs` - the primitive.
- Commit `a1e16d9` (2026-04-24) - the PathGuard primitive and
  counter-ordering landing.
