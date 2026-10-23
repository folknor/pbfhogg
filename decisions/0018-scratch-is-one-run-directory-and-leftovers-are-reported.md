# ADR-0018: Scratch is one per-run directory, and leftovers are reported, never reaped

Date: 2026-10-23
Status: Accepted (supersedes the scratch-directory and entry-time-sweep rules of ADR-0003)

## Decision

Disposable files a command writes - the external join's id shards and slot
buckets, the sparse node store, diff and derive shard files, the geocode
builder's cell buckets - live in exactly one directory per run, a
`pbfhogg::scratch::RunScratch`. Outputs are not scratch (ADR-0017); they are
staged beside their destination because publication is a rename.

- **The directory.** `RunScratch::create(parent, &host)` requires `parent` to
  exist and be a directory, then makes
  `parent/.pbfhogg-run-<pid>-<128 random bits>` exclusively with mode 0700.
  Everything the run writes goes inside it under fixed simple names; the
  directory, not the names, makes them unique, so there are no pid, run-token
  or nanosecond tags and no per-site naming of shards (`altw::external::radix`
  spells the shard, bucket and payload names once). Scratch files are only
  created at a `ScratchPath`, which only a `RunScratch` hands out and which
  names a single component of its own directory; clippy.toml bans the
  directory- and file-removing and renaming `std::fs` functions,
  `std::process::id` and `std::env::temp_dir` in the command layer and the CLI.
- **Ownership.** The public entry points that use scratch
  (`add_locations_to_ways`, `external_join`, `diff`, `derive_changes`,
  `build_geocode_index`) take the `RunScratch` by value; the engines under
  them borrow it. One invocation owns one directory, so two invocations cannot
  share one, and the caller creates it before opening any input, which is how
  an unusable scratch location fails before any work. There is no library
  default location: the CLI's `--tmp-dir`, else the output's directory (the
  current directory when there is no output file).
- **Cleanup.** Dropping the guard removes everything but its lock files, then
  the lock files, then the directory, so a partial failure keeps the evidence
  a later scan needs. A failure is a `Warning` through `host.diagnostics`
  naming the leftover path. Mid-run reclamation (id shards after stage 2, slot
  buckets after stage 3, shard files after they are appended) goes through
  `RunScratch::remove`, which reports a failure and carries on; the final
  removal retries. The error-path sweeps and `PathGuard`s that preceded this
  are gone.
- **Liveness.** The guard holds an exclusive `flock` on `LOCK` for its whole
  life and creates `READY` after taking it. A scanner locks only when `READY`
  exists, never creates anything, and releases the lock before measuring, so
  scanning can neither fail nor stall a starting run. A held lock means live
  and the entry is not reported. The pid in a name is a hint only.
- **Leftovers are reported, never reaped.** `scratch::find_stale(dir)` lists,
  directly inside one directory, run directories, the scratch names of older
  versions, output staging entries and output backups, as typed values (kind,
  observed bytes, whether the walk was complete, and an `Observation` such as
  `LockAvailable`, `LockMissing`, `PidAbsent(pid)`, `PidPresent(pid)`), with
  per-entry inspection failures kept apart from the findings. The observations
  are what was seen, not proofs: network filesystems may lock locally, a forked
  child can keep a lock alive, and a pid can be reused or belong to another
  container sharing the volume; the rendered text says "probably". The CLI
  scans each output directory and `--tmp-dir` once, before it reserves any
  output of its own, and `report_stale` emits one warning per entry through the
  diagnostics sink (`scratch.stale`, and `scratch.backup` for backups).
  A `.pbfhogg-backup-*` entry can hold the only copy of a user's previous
  output: it is its own kind with its own wording, and no code path removes it.
- **Trust.** The scratch parent and the path to it are trusted and must stay
  stable for the run, as an output's parent is; operations are path based.
  A scratch location inside a directory that publication replaces is refused
  (the geocode index).

## Alternatives considered

- **Per-site names with pid and run tokens, plus an entry-time sweep (the
  ADR-0003 checklist).** Rejected: pids are reused and shared across
  containers, a sweep deletes what it guesses is stale, and every site grows
  its own name format and cleanup.
- **A `Host` field for the scratch location, or creating the directory lazily
  inside the library.** Rejected: a cloneable `Host` hides ownership (two
  invocations could share one directory and its fixed names), and laziness
  moves the validation to the middle of a run.
- **Reaping stale scratch automatically.** Rejected: liveness cannot be proven
  across containers and network filesystems, and a backup may be the only copy
  of an output.
- **Directory-handle (`openat`) operations to defend against a hostile
  parent.** Rejected: the code base is path based throughout and a same-user
  adversary in the user's own scratch directory is outside the contract.
- **Placing output staging in the scratch location.** Rejected: publication is
  a rename and needs the same filesystem as the destination.
