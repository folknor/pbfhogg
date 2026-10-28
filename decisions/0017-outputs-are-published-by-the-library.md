# ADR-0017: Outputs are checked, staged and published by the library

Date: 2026-10-23
Status: Accepted (supersedes the user-named-output rules of ADR-0003)

## Decision

Every file or directory a command produces is a `pbfhogg::output` value -
`Output` (one file), `OutputDir` (one directory; the geocode builder takes
it wrapped as `IndexOutput`, which only `index_output` builds, so its
index-or-empty rule cannot be skipped, on top of the leftover rule every
directory output has) or
`OutputSet` (a group of files, `extract --config`) - and no command writes an
output any other way.

- **Construction is the policy check, before any work.** The caller states
  its overwrite intent (`Overwrite::Refuse` or `Overwrite::Replace`; there is
  no default) and hands over the `Diagnostics` that cleanup trouble goes to.
  An existing destination is refused under `Refuse`
  (`OutputError::Exists`); a symlink is refused always, naming the link and
  its target; a destination of the wrong kind (a directory or special file
  for a file output, a non-directory or mount point for a directory output)
  is refused even under `Replace`. A destination named like, or inside, an
  entry pbfhogg leaves behind (staging, backup, run directory, an older
  version's scratch; the path as given and with its parent resolved) is
  refused (`OutputError::ReservedName`): publication must never write over
  or into one. An existing directory to be replaced is walked, and refused
  (`OutputError::HoldsLeftovers`, naming the entries) when anything of that
  kind is anywhere inside it, live run directories included, because
  replacing it deletes the old tree; only an index's `.buckets-level*`
  scratch trees may go with it (ADR-0018). Construction then reserves a hidden
  staging entry, `.pbfhogg-tmp-<pid>-<n>.<name tail>` (the marker is a
  prefix, so suffix-sniffing writers see the destination's suffix), created
  exclusively. By default it is a sibling of the destination, and creating
  it also proves the destination's directory exists and is writable.
  `OutputSet::files` checks every member, and refuses members resolving to
  one file, before any of them is staged.
- **A caller may choose where staging goes.** The `with_staging`
  constructors (`Output`, `OutputDir`, `OutputSet::files_with_staging`,
  `index_output_with_staging`) reserve the staging entry in a caller-chosen
  directory instead, so a caller that gives each run a private directory can
  remove it whole after a run killed outright, without matching any name.
  Nothing else moves there: a backup always stays beside its destination,
  because it can be the only copy of a previous output and the caller removes
  its staging directory wholesale. The names stay unique per process and
  sequence: a name derived from the destination alone would put every run to
  one destination in one namespace, and taking over a dead run's entry there
  needs an ownership protocol spanning reservation through rollback that no
  lock can provide where locks are unreliable (client-local NFS locks), while
  writers reopen the staging path by name. Construction refuses the
  directory (`OutputError::StagingDir`) unless it is a directory on the same
  mount as every destination's directory (device, and `statx` mount id where
  reported; a bind mount shares the device), is not and does not contain any
  destination's directory (removing it must never delete an output or a
  backup beside one), and does not lie inside a directory output; a set is
  checked against every member before any is reserved. Because reserving
  there no longer proves the destination's directory is writable, that is
  checked separately (`faccessat` with `W_OK | X_OK` and `AT_EACCESS`).
- **Library entry points take the output by value and are its only
  publisher.** `sort(input, output: Output, ...)` writes through a borrowing
  engine (`sort_to(input, &output, ...)`) and calls `output.publish(&host.invocation)` as its
  last step, after every post-write gate (the altw missing-locations check
  runs before publication). Engines that dispatch to other engines
  (`extract_multi` to `extract_to`, altw to the external join, derive to its
  parallel driver) pass the borrow; publication authority cannot be borrowed
  because `publish` takes `self`. Every PBF backend - buffered, `O_DIRECT`,
  `io_uring`, parallel - writes the staging file through
  `Output::pbf_writer` (public, so a library caller gets the same
  session-bearing writer the commands use); non-PBF writers (OSC, diff text,
  export) get an `OutputWriter` from `Output::writer` and end it with
  `OutputWriter::finish`, the one checked flush (dropping a `BufWriter`
  swallows its last write error); a directory output is filled through
  `OutputDir::writer`'s `DirWriter`.
- **One writing session per output, and publication needs it finished.**
  The session is `Reserved -> Writing -> Finished`, with `Failed` terminal and
  never overwritten by a later success. `Output::pbf_writer`,
  `Output::writer` and `OutputDir::writer` each open it under the run's
  invocation (a second session is refused, so two writers cannot truncate
  each other), and `publish` refuses a session opened under an invocation
  other than its own (ADR-0019). A PBF writer holds a
  lease that its first fully successful end-of-stream `flush` finishes; any
  error any of its write methods returns, from wherever in the method (a
  scan, a permit, framing, the write, a send that recovered the writer
  thread's error), a failed flush, or taking the inner writer out fails the
  session for good, so a caller that carries on past a lost blob cannot
  certify the output; a finished writer accepts no further writes. An
  `OutputWriter` fails the session on any failed `write` or `flush` (an
  interruption, which wrote nothing, aside). `publish` checks every member
  first and refuses anything but a `Finished` session with
  `OutputError::Unfinished`, before any hook runs and before anything is
  synced or installed: nothing in the crate publishes an output it did not
  write, so an output nobody opened a writer on is refused too: writing to
  `Output::path` without a session is never published. The check runs
  before the `before_publish` hooks and again after them, since a hook is
  caller code that can fail a session.
  It also refuses a staging entry that is no longer the file, or the
  directory, reserved, a symlink put in its place included
  (`OutputError::StagingReplaced`). A `DirWriter`'s `finish` certifies the
  caller's declaration, not the files: whatever was written under its path
  must be flushed and closed first, and nothing may write there afterwards.
  The same holds for `Output::path` after a session finished. Those are
  caller contracts, not enforcement; a directory has no single descriptor
  whose end of stream could be observed.
- **Publication.** A lone file under `Replace` is one `rename(2)`. Under
  `Refuse` publication is exclusive: `renameat2(RENAME_NOREPLACE)` (the raw
  syscall, so musl builds have it), falling back for files to `link(2)` and
  never to a check-then-rename; where neither exists the publish is refused
  (`OutputError::ExclusivePublishUnsupported`). A directory, or any member of
  a group, moves an existing destination into an exclusively created
  `.pbfhogg-backup-*` sibling, installs, and on a later failure rolls the
  installed members back in reverse; a rollback step that cannot complete
  keeps the backup (never deleted) and the error names it
  (`OutputError::RollbackIncomplete`). The backup is the previous output's
  only name from the move until the new output is in place, so the move is
  made durable first (the backup container, then the parent) and a failure
  there is an install failure that restores. After a rollback the touched
  parents and any kept backup are synced best effort: a successful rollback
  is a statement about what is visible, not about crash durability. Every
  destination is re-inspected at publication, before anything is installed.
- **Publication owns durability, and starts after every timer has stopped.**
  Writers finish their files; they do not sync (the pipelined backends take
  `Finish::Deferred` from the output factory, which skips only the `fsync`,
  never the tail padding, truncation, worker join or completeness check; the
  public `PbfWriter::to_path*` constructors stay durable for callers with no
  publication). `publish` syncs every staged file through the descriptor the
  output opened at reservation, before anything wrote it. Linux tracks
  writeback errors per file and reports one to each descriptor's next
  `fsync` once; a descriptor opened later still sees an error nobody has
  reported yet, but misses one that an `fsync` on another descriptor (the
  writer's own, another process's) already consumed. Holding a descriptor
  from before the first write means publication sees every error since then,
  whoever else synced. A directory output's tree is synced file by file,
  reopening them (a known limit: an error another descriptor already
  consumed is not seen), and the parent directory after the renames. Every
  one of those syncs goes through `WriterMetrics::sync_recorded`, so the
  sidecar's `writer_sync_all_*` counters see them; clippy.toml bans
  `File::sync_all` and `sync_data` across the library and the CLI so a
  hand-rolled sync cannot return. `O_DIRECT` and `io_uring` outputs are
  synced too: bypassing the page cache is not stable storage.
  After the renames publication syncs every directory they changed: each
  destination's directory and the directory each new generation was renamed
  out of (a staging directory, or a directory output's staging container).
  Syncing an ancestor does not make a child directory's change durable.
  What changed at the end of a run, for anyone reading the counters or the
  descriptor budget: `writer_sync_all_calls` / `writer_sync_all_ns` now also
  count the directory syncs (each changed directory once, plus a backup's
  container and parent before a replacing swap), not just file syncs; for the
  command-layer writers `writer_flush_ns` no longer includes the `fsync`,
  which moved into the publication span (the public durable
  `PbfWriter::to_path*` writers, `bench-write`'s among them, still sync
  inside their timed `flush`);
  publication emits its own share as `publish_sync_calls` /
  `publish_sync_ns` just before `PUBLISH_END`, a tally of exactly the syncs
  that publication attempted (failures and rollback syncs included, one
  tally per group), so a run where both totals agree had every sync in
  publication; and every file
  output holds one extra descriptor (the reservation's) from construction
  until publication or drop, which matters only to an `extract --config`
  with very many regions under a tight descriptor limit.
  The wrappers that own run scratch and publish (`add_locations_to_ways`,
  `external_join`, `derive_changes`, `build_geocode_index`) drop it, removing possibly
  hundreds of GB, before `publish`, so that removal precedes the output's
  `fsync` and rename. That order is deliberate: the scratch is the run's to
  clean up as soon as the work is done, publication then syncs only the
  output, and a crash between the two leaves an unpublished staging entry and
  no scratch, which is the cheaper leftover.
  The publication span is `PUBLISH_START` .. `PUBLISH_END` (the end marker on
  every return, best effort like every marker: the sidecar drops one when its
  pipe is full). Publications of one invocation are serialized: each holds
  the invocation's publication lock from before `PUBLISH_START` until after
  `PUBLISH_END`, so on the invocation's stream every span's
  `publish_sync_calls` / `publish_sync_ns` lie inside that span alone, even
  when a caller runs several commands at once under one token. The lock is
  taken after the cancellation decision and the `before_publish` hooks, so a
  token tripped while a publication waits does not stop it.
  Everything before `PUBLISH_START` is the run's work. An
  in-process timer that must not include publication stops in
  `Output::before_publish`, a hook that runs at the start of publication before
  anything is synced or installed and not at all when publication is refused up
  front: `bench-merge`'s `elapsed_ms` is captured there, and the geocode
  builder's profiler span covers `build_geocode_index_to`, not the publishing
  wrapper. A caller's process-wide profiler (the CLI's `pbfhogg::main` hotpath
  guard) spans the whole process by definition and includes publication.
  Brokkr's external wall clock around the child process necessarily includes
  the sync and the rename; that is intended, and the sidecar's spans are how to
  attribute it. The changed directories are synced after the renames, every
  one is attempted, and only when every one succeeded are the replaced
  outputs' backups removed. A sync failure returns
  `OutputError::Unsynced`, whose text says the output was published; every
  backup is then kept, its container synced best effort, and the error names
  each one (and says so if that sync failed too): until the replacement is
  known to be durable, a backup may be the only durable copy. A lone file
  replaced under `Replace` has no backup to keep: its single rename is the
  replacement the caller asked for. Dropping an unpublished output removes
  only its staging entry.
- **The front end maps flags, nothing more.** The CLI turns `--overwrite`
  (`-O`) into `Overwrite::Replace` and attaches the advice that names the
  flag; the library's error text names no flag. `build-geocode-index`'s
  legacy `--force` keeps granting both overwrite and unindexed input.
- **Enforcement.** clippy.toml disallows the path-writing `std::fs`
  methods; the lint is switched on for the whole library and the CLI, where
  outputs go through `crate::output` and scratch through `crate::scratch`.
  Those two modules own the calls and allow it; the few other owners (the
  write backends' creates and syncs, the path guard, procfs reads) allow it
  on the single statement or one-call helper, with the reason beside it.

Under `Replace`, another process mutating the destination entry between the
final inspection and the rename is outside the contract (the rename never
follows a link), and so is another process creating entries inside a
directory destination while it is published: the walk before every
recursive removal narrows that window and does not close it. Nothing makes a
group visible to readers atomically. A cancelled run (ADR-0019) is a failed
run here: `publish(&Invocation)` takes the token as an argument and refuses a
tripped one before anything is checked, synced or installed, and once that
decision is made publication completes; `Output::writer(&Invocation)` (on each
buffer flush and in `finish`) and the PBF writer (on each write method, not
in its end-of-stream `flush`) observe the token, so a run cancelled mid-write
fails its session. A process killed outright (SIGKILL, the OOM killer, power
loss, the CLI's second interrupt) can leave `.pbfhogg-tmp-*` staging behind (in the staging directory, when the caller chose one), and a failed rollback or an unsynced
publication leaves `.pbfhogg-backup-*` on purpose; neither is ever under the
name the caller chose. Every recursive removal publication does - a replaced
output's backup after success, an unpublished directory output's staging on
drop - first walks what it is about to remove; anything pbfhogg never
deletes found there (or a walk that cannot complete) keeps the whole entry.
Cleanup after a successful publication is best effort and cannot fail the
run, but nothing is dropped silently: the `Diagnostics` every output is
constructed with (required, so a library caller cannot lose it by
omission; `Diagnostics::discard()` is an explicit choice) receives a
`Warning` with key `output.cleanup` for every removal that failed and every
entry kept back, naming what is left. They are emitted when the output is
dropped, after every sync publication attempted, so a sink cannot interrupt
durability; while a panic is unwinding nothing is emitted, because a sink
that panicked then would abort the process, and the next scan reports the
leftover instead. A previous output whose contents cannot be unlinked stays
in its `.pbfhogg-backup-*` sibling, possibly partly removed.

## Alternatives considered

- **Staging named by the destination alone (`.pbfhogg-tmp.<name>`), a dead
  run's entry taken over by the next run.** Rejected (2026-10-28): a renamed
  file frees the name while group rollback can still unlink the destination,
  so a second run reserving it can lose its published output; writers reopen
  the staging path by name, so on client-local NFS locks a displaced run
  writes into the new owner's file and an identity check only detects it;
  and a staging directory without its lock file cannot be told apart from
  one being set up. A caller-chosen staging directory gives a caller the same
  cleanup without any shared namespace.
- **Deterministic backup names, refusing while one exists.** Rejected: a
  backup is never a caller's to clean up, so a computable name buys no
  cleanup, while refusing turns one interrupted publication into a failure
  of every later replacing run, and it would not serialize concurrent
  publishers anyway.

- **Keep the guard at the CLI front end (ADR-0003's rule).** A library
  caller then got truncate-then-fail, and two library entry points had
  grown their own guards (altw's `DestinationGuard`, merge-changes'
  exists-check) - four mechanisms for one policy. ADR-0003's reason, that
  only the caller knows whether a path is user-named or run-owned, is
  answered by the caller constructing the `Output`: run-owned scratch never
  goes through it.
- **Publication inside `PbfWriter` (rename on `flush`).** Gates that run
  after the write (altw's missing-locations check) would publish a file the
  run then refuses, and the non-PBF writers would need a second mechanism.
- **Entry points take `&Path` plus an overwrite flag and check on entry.**
  Nothing would stop an entry from forgetting the check or writing to the
  final path; the type makes both impossible.
- **Check-then-rename under `Refuse`.** A file created while a planet-scale
  run is in flight would be clobbered; the policy is "never replace without
  permission", so the race is not documented away.
- **Accept an existing empty directory as a geocode output without
  `--overwrite`.** Rejected for one rule across every output: an existing
  path is refused unless replacing it was asked for.
