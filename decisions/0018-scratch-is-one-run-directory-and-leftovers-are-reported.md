# ADR-0018: Scratch is one per-run directory, and leftovers are reported, never reaped

Date: 2026-10-23
Status: Accepted (supersedes the scratch-directory and entry-time-sweep rules of ADR-0003)

## Decision

Disposable files a command writes - the external join's id shards and slot
buckets, the sparse node store, diff and derive shard files, the geocode
builder's cell buckets - live in exactly one directory per run, a
`pbfhogg::scratch::RunScratch`. Outputs are not scratch (ADR-0017); their
staging goes beside the destination, or in a caller-chosen staging directory
on the same mount, because publication is a rename, and never in the run
directory.

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
  `std::process::id` and `std::env::temp_dir` across the library (outside
  `crate::scratch` and `crate::output`, which own them) and the CLI.
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
  are gone. A cancelled run (ADR-0019) returns through the same error path,
  so its guard removes the directory like a failed run's; only a process
  that ends without unwinding (SIGKILL, the OOM killer, power loss, the
  CLI's second interrupt) leaves one for the leftover scan.
- **Liveness.** The guard holds an exclusive `flock` on `LOCK` for its whole
  life and creates `READY` after taking it. A scanner locks only when `READY`
  exists, never creates anything, opens `LOCK` read-write (on NFS `flock` is
  carried out as a whole-file POSIX lock, and an exclusive one on a
  read-only descriptor fails with `EBADF`, which would make every live run
  look stale), checks that what it opened is a regular file, and releases
  the lock before measuring, so scanning can neither fail nor stall a
  starting run. A held lock (`EWOULDBLOCK`) means live and the entry is not
  reported. Only a lock that could be taken, or a missing `READY` or `LOCK`
  (a handshake not completed or already undone), is an observation; every
  other probe outcome (no permission to open the lock, an unexpected file
  type, a lock the filesystem refuses) leaves liveness unknown, and the
  directory is recorded as a scan issue that says not to delete it, never as
  stale. The pid in a name is a hint only.
- **Leftovers are reported, never reaped.** `scratch::find_stale(dir)` lists,
  directly inside one directory, run directories, the scratch names of older
  versions, output staging entries and output backups, as typed values (kind,
  observed bytes, whether the walk was complete, and an `Observation` such as
  `LockAvailable`, `LockMissing`, `PidAbsent(pid)`, `PidPresent(pid)`), with
  per-entry inspection failures kept apart from the findings. The observations
  are what was seen, not proofs: network filesystems may lock locally (below), a forked
  child can keep a lock alive, and a pid can be reused or belong to another
  container sharing the volume; the rendered text says "probably". The CLI
  scans each output directory, `--tmp-dir` and `--staging-dir` (ADR-0017)
  once, before it reserves any output of its own, and `report_stale` emits
  one warning per entry through the
  diagnostics sink (`scratch.stale`, and `scratch.backup` for backups).
  A `.pbfhogg-backup-*` entry can hold the only copy of a user's previous
  output: it is its own kind with its own wording, and no code path removes it.
  Coverage follows where each command's older versions put their scratch:
  `extract --config` scans the directory of every output the config names
  (known only after parsing it, so before the outputs are reserved, each
  directory once); `diff` also scans the old input's directory (`diff-par-*`
  shards) and, with no output file, the current directory its own run
  directory goes in (older versions of `diff --format osc` put their
  `derive-*` fragments beside the output, which the output-directory scan
  covers); `build-geocode-index` also scans, by `scratch::find_stale_in_index`,
  everything inside the existing index directory at any depth - every kind
  `find_stale` knows, plus `.buckets-level*` (kind `IndexScratch`) - and
  recognises the previous version's `.geocode-build-<n>` staging siblings (a
  backup when it holds `old/`, which may be the only copy of the previous
  index, else legacy scratch). Names are matched on bytes, since a staging or
  backup name keeps a destination tail that need not be UTF-8. A classified
  entry is one entry whatever is inside it. The exact spellings are defined
  in prose in `docs/cli/commands.md`, "Names pbfhogg leaves behind".
  Replacing an index with `--overwrite` deletes the old index directory,
  `.buckets-level*` trees in it included. That is accepted: replacing an
  index wholesale is what was asked for, only an index or an empty directory
  can be replaced, scratch is by definition not index content, and the
  `IndexScratch` report says publication will delete it. It is the one reported
  kind that anything removes. Every other kind inside a directory output
  blocks its replacement outright (`OutputError::HoldsLeftovers`, ADR-0017),
  live or not: the scan reports, the output layer refuses.
  Network filesystems limit what the lock can say, accepted because the
  report is worded as a belief and nothing is deleted: an NFS mount with
  `local_lock=flock` or `local_lock=all` (or an NFSv3 mount with `nolock`)
  keeps `flock` local to each client, so a live run on another host is
  invisible and its directory is reported as probably stale; a client that
  lost its lock (an NFSv4 lease that expired while it was cut off from the
  server) no longer protects its directory either.
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
