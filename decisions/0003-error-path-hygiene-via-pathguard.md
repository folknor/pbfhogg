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
clearing a bool; filesystem work only runs on the error path.

**Second primitive:** `src/path_guard.rs::OutputFile`, for a path the
*user* named. `PathGuard` is destructive by design, which is correct only
for a path the run created and therefore owns. A user-named `--output`
may already hold unrelated data, so `OutputFile` never opens it: the
writer targets `OutputFile::path()`, a scratch sibling in the same
directory, and `commit()` renames it over the destination once the run
has succeeded. Drop removes only the scratch file. This closes the
truncate-on-open window as well as the delete-on-failure one.

**Rule:** counters that track emitted work bump *after* the
consumer's successful `writer.write_*` call, not before the
worker's `tx.send`. Intermediate debug/throughput counters (work
attempted, blobs processed, etc.) can bump wherever, but
user-facing summary counters must match output.

**Checklist for new commands:**

- Every final output path the *user named* is wrapped in
  `path_guard::OutputFile`, and that wrapping happens **at the front
  end, not in the library**. The library entry points (`sort::sort`,
  `cat::cat`, `extract::extract`, ...) write where they are told; only
  the caller knows whether the path it passed is a name a user chose or
  a scratch file the run created, and guarding a caller-supplied path
  unconditionally would be wrong for the second case. `pbfhogg-cli`
  therefore routes every `--output` through one helper,
  `guarded_output` in `cli/src/main.rs`: it derives the scratch sibling,
  hands the command `OutputFile::path()`, and commits on success. Adding
  a command means adding its dispatch arm to that helper, not threading
  a guard through the library.

  `PathGuard::file` must not be used for a user-named path: it deletes
  whatever is there, so a mid-run failure would destroy a pre-existing
  file that merely shares the output name, and opening the destination
  directly would already have truncated it before the first byte of new
  output existed.

  Two consequences the helper carries deliberately. First,
  `ExitWithCode` is committed, not discarded: a command that ran to
  completion and answered "no" (`diff -o` finding differences) has a
  complete output, and treating its non-zero exit as a failure would
  delete the very result the user asked for. Second, the scratch name is
  `.pbfhogg-tmp-<pid>-<seq>.<name>` - marker in *front* - so the scratch
  file keeps the destination's suffix. `merge-changes` picks gzip from a
  `.gz` tail; a suffixed scratch name would have made it write plain XML
  into a `.gz` file.

  Because the scratch file is created in the destination's directory, a
  destination whose directory is not writable now fails where writing
  in place over an existing, writable file used to succeed. That is
  accepted: it is the price of never opening the destination. So is the
  other visible consequence: a run killed with SIGKILL skips Drop and
  leaves a `.pbfhogg-tmp-*` sibling behind, which nothing removes
  automatically.

  `bench-merge`, a synthetic write benchmark, goes through the helper
  too, but its `elapsed_ms` is taken inside the closure, so the scratch
  file's creation and the commit `rename` both fall outside the timed
  region. Keep it that way: a guard on a benchmark must commit outside
  what it measures.
- **A command with several user-named outputs gets a guard each, and a
  preflight before the first commit.** `extract --config` is the only
  such command today. Extraction is all-or-nothing - a failure drops
  every guard and no destination was ever opened - but *publication* is
  not a transaction: it is a sequence of renames, and POSIX has no N-way
  atomic rename. What the front end does instead is check every
  destination (`preflight_publishable` in `cli/src/main.rs`) immediately
  before the first rename, so the ordinary causes of a mid-publication
  failure - a destination that is a directory, a parent directory that
  vanished - become a refusal that publishes nothing. Concurrent external
  interference between that check and a later rename can still leave a
  partial set. Do not describe this as transactional anywhere; state the
  narrowing, not a guarantee.
- `PathGuard::file` remains correct for a path the run itself created
  and owns (internal scratch files).
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
