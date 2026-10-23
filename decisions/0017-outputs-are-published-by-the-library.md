# ADR-0017: Outputs are checked, staged and published by the library

Date: 2026-10-23
Status: Accepted (supersedes the user-named-output rules of ADR-0003)

## Decision

Every file or directory a command produces is a `pbfhogg::output` value -
`Output` (one file), `OutputDir` (one directory; the geocode builder takes
it wrapped as `IndexOutput`, which only `index_output` builds, so its
index-or-empty rule cannot be skipped) or
`OutputSet` (a group of files, `extract --config`) - and no command writes an
output any other way.

- **Construction is the policy check, before any work.** The caller states
  its overwrite intent (`Overwrite::Refuse` or `Overwrite::Replace`; there is
  no default). An existing destination is refused under `Refuse`
  (`OutputError::Exists`); a symlink is refused always, naming the link and
  its target; a destination of the wrong kind (a directory or special file
  for a file output, a non-directory or mount point for a directory output)
  is refused even under `Replace`. Construction then reserves a hidden
  staging sibling, `.pbfhogg-tmp-<pid>-<n>.<name tail>` (the marker is a
  prefix, so suffix-sniffing writers see the destination's suffix), created
  exclusively, which also proves the directory exists and is writable.
  `OutputSet::files` checks every member, and refuses members resolving to
  one file, before any of them is staged.
- **Library entry points take the output by value and are its only
  publisher.** `sort(input, output: Output, ...)` writes through a borrowing
  engine (`sort_to(input, &output, ...)`) and calls `output.publish()` as its
  last step, after every post-write gate (the altw missing-locations check
  runs before publication). Engines that dispatch to other engines
  (`extract_multi` to `extract_to`, altw to the external join, derive to its
  parallel driver) pass the borrow; publication authority cannot be borrowed
  because `publish` takes `self`. The command layer's writer factory takes
  `&Output`, so every PBF backend - buffered, `O_DIRECT`, `io_uring`,
  parallel - writes the staging file; non-PBF writers (OSC, diff text,
  export) open it with `Output::create` and finish with a checked flush and
  sync before publishing.
- **Publication.** A lone file under `Replace` is one `rename(2)`. Under
  `Refuse` publication is exclusive: `renameat2(RENAME_NOREPLACE)` (the raw
  syscall, so musl builds have it), falling back for files to `link(2)` and
  never to a check-then-rename; where neither exists the publish is refused
  (`OutputError::ExclusivePublishUnsupported`). A directory, or any member of
  a group, moves an existing destination into an exclusively created
  `.pbfhogg-backup-*` sibling, installs, and on a later failure rolls the
  installed members back in reverse; a rollback step that cannot complete
  keeps the backup (never deleted) and the error names it
  (`OutputError::RollbackIncomplete`). Every destination is re-inspected at
  publication, before anything is installed, and a directory's new contents
  are synced before they replace anything. The parent directory is synced after
  the renames; a sync failure returns `OutputError::Unsynced`, whose text
  says the output was published. Dropping an unpublished output removes only
  its staging entry.
- **The front end maps flags, nothing more.** The CLI turns `--overwrite`
  (`-O`) into `Overwrite::Replace` and attaches the advice that names the
  flag; the library's error text names no flag. `build-geocode-index`'s
  legacy `--force` keeps granting both overwrite and unindexed input.
- **Enforcement.** clippy.toml disallows the path-writing `std::fs`
  methods; the lint is switched on for `src/commands/` and the CLI, where
  outputs go through `crate::output` and scratch through `crate::scratch`.

Under `Replace`, another process mutating the destination entry between the
final inspection and the rename is outside the contract (the rename never
follows a link). Nothing makes a group visible to readers atomically. A
process killed outright can leave `.pbfhogg-tmp-*` staging behind, and a
failed rollback leaves `.pbfhogg-backup-*` on purpose; neither is ever under
the name the caller chose. Removing a backup after a successful publication
is best effort and unreported (the library has no diagnostics channel in
`Output`): a previous output whose contents cannot be unlinked stays in its
`.pbfhogg-backup-*` sibling.

## Alternatives considered

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
