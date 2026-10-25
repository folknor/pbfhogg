# pbfhogg CLI Reference

Version 0.5.0 (plus unreleased 0.6.0 changes). Generated from
`pbfhogg --help` output.

Every enumerated flag value (`--compression`, `--type`, `--index-type`,
`--clean`, `inspect tags --sort`, the `bench-*` modes) is parsed once at
the argument boundary, so `--help` lists the accepted values and shell
completion offers them.

`--type` is one comma-list grammar (`node,way,relation`, any subset) with a
single parser on every command that takes it. A command that acts on one
kind (`inspect tags`) refuses a list of more than one, and `export` refuses
`relation` (it accepts `node`, `way` or `node,way`); each refusal names what
the command accepts.

## Global flags

All commands support `-h, --help` and `-V, --version`, and
[`-q, --quiet`](#quiet), which is accepted before or after the subcommand
(`pbfhogg -q diff a b` and `pbfhogg diff a b -q` are the same).

## Quiet

`-q, --quiet` means one thing on every command: the exit code is the answer.

- **Dropped:** every `Note:`, every progress line, the command's closing
  summary (single- or multi-line, `apply-changes`' block, each region's line
  of `extract --config` and its opening `Multi-extract:` line, `inspect tags`'
  `N distinct tag values`), and the developer lines some backends print
  (`[altw external] ...`, the geocode builder's hotpath counters).
- **Kept:** every `Warning:` and the final `Error:` line, which explain the
  exit code, and clap's usage errors.
- **Never touched:** what a command writes to `-o` (a quiet run writes the
  same bytes), and a report a command prints on stdout (`inspect`,
  `inspect tags`, `digest`, `export` without `-o`), with three exceptions:
  `diff`, `check` and `inspect --indexed`, whose answer is their exit code,
  print nothing at all. A quiet text `diff` discards its diff text, so `-o`,
  `-c`, `-v` and `-s`, which only shape that text or its summary, are refused
  with `-q`; a quiet `diff --format osc` writes its `-o` like any other
  command. A quiet `check` refuses `--json`, `--show-ids` and `--max-errors`,
  and a quiet `inspect --indexed` refuses `--json`, for the same reason.
  `getid -v` is refused with `-q`: the ID lists are all it adds.
- **Not affected:** the `bench-read`, `bench-write` and `bench-merge`
  key=value blocks, which benchmark harnesses parse, print as they always
  have.

`diff -q` and `check --quiet` used to be flags of those two commands alone;
they are this flag now, with the same effect there (and `diff --format osc`
now accepts it).

## Exit codes

Every command uses the same contract. `1` is reserved for a command that
ran to completion and whose answer is negative; a command that could not
run at all exits `2`, so a script can branch on the answer without
swallowing failures. `3` is the one failure split out of `2`: a
requested platform I/O feature is unavailable, so the same invocation
without that flag can succeed. `130` and `143` are interruptions, following
the shell's `128 + signal` convention.

| Code | Meaning |
|------|---------|
| `0` | Success. |
| `1` | The command ran and its answer is negative. Only four commands can produce it: `diff` (differences found), `inspect --indexed` (a data blob lacks indexdata; a header-only file is indexed), `inspect --show` (element not in the file), and `check` (integrity violations). Nothing is printed to stderr for it beyond the command's normal output. |
| `2` | The command failed: missing or unreadable input, an invalid argument combination, a malformed PBF (a zero-byte file, or one whose first blob is not the OSMHeader, is `PBF file does not start with an OsmHeader blob` naming the file, from every command including `inspect --indexed`), an I/O error. `Error: ...` goes to stderr. Argument-parsing errors from clap also land here. |
| `3` | The command failed because the host refused a requested I/O feature at setup: `--direct-io` on a filesystem that rejects `O_DIRECT` at open (tmpfs, some overlay and network filesystems) or on a file whose reported direct I/O alignment (`statx` `STATX_DIOALIGN`) is not a power of two or exceeds the direct I/O transfer buffer, or `--io-uring` where the kernel lacks io_uring or `WriteFixed`, a sandbox denies `io_uring_setup`, or `RLIMIT_MEMLOCK` is below the writer's registered buffers. On kernels before 5.12, which also charge the ring itself to `RLIMIT_MEMLOCK`, a low limit can instead fail ring creation with a generic `ENOMEM` that pbfhogg cannot tell apart from kernel resource exhaustion; that case exits `2`. stderr carries `Error: platform feature <feature> unavailable: ...` with `<feature>` `direct-io` or `io-uring` (an `--io-uring` output also opens with `O_DIRECT`, so it can report either). A refused `--direct-io` input reads `Error: platform feature direct-io unavailable: failed to open <path>: Invalid argument (os error 22)`. Failures after setup, including an `EINVAL` or `ENOSPC` mid-run, stay `2`. The code follows the typed error chain, not the message text: it survives added context, `io::Error` carriers, and the thread joins of multi-stage commands, so it holds when the refusal is raised by a worker stage, such as the output writer of the external `add-locations-to-ways` backend (which previously exited `2` for the same refusal). |
| `130` | Interrupted by SIGINT (Ctrl-C). See [Interruption](#interruption). |
| `143` | Interrupted by SIGTERM. See [Interruption](#interruption). |

### Interruption

Every command handles SIGINT and SIGTERM the same way. The first one prints
`Note: SIGINT received; stopping and removing this run's staging and scratch
files. ...` (not under `-q`, which drops notes), and the run stops at its
next checkpoint: its workers wind down,
its output staging and scratch directory are removed, and nothing is
published - an existing output stays byte-identical. It then prints `Error:
interrupted by SIGINT: the run stopped before completing, removed its staging
and scratch files and published nothing` and exits `130` (`143` for
SIGTERM). A run that was already publishing when the signal arrived finishes
publishing, and a run that had already completed keeps its normal outcome
and exit code with a `Note:` that the signal arrived too late to matter. A
report printed to stdout once a command has decided its result is not cut
short by a signal; output streamed while the command runs (`diff` text to
stdout) stops where the run stopped.

The run stops at the next block it reads, dispatches or writes, the next
result a worker pool hands back, or the next stage boundary - normally well
under a second. A few phases have no such point inside them and finish first:
the geocode builder's in-memory admin assembly and interpolation passes and
its writes of the index's own data files,
`merge-changes --simplify`'s in-memory dedupe, `renumber`'s rank-table
builds, one external-join bucket or `add-locations-to-ways` node blob in
flight per worker, a single `copy_file_range` call of a raw blob copy, and
the removal of a large scratch directory during cleanup.

A **second** SIGINT or SIGTERM while that cleanup is under way exits at once
with `128 +` that signal's number, leaving whatever staging and scratch the
first had not yet removed; the next run in that directory reports it (see
[Scratch files](#scratch-files)). SIGKILL is never handled.

Before 0.6.0 every one of these outcomes exited `1`, so "the files
differ" and "the file does not exist" were indistinguishable. Callers
that branch on `1` for the semantic answer are unaffected; callers that
treat any non-zero as failure are unaffected. Only a caller that read
`1` as "the command failed" needs to widen to `!= 0`.

## Common flags

These flags appear on most commands that produce PBF output:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--compression <COMPRESSION>` | Blob compression: `none`, `zlib` (default; `renumber` defaults to `zlib:1`, see its section), `zstd`, or with level (`zlib:9`, `zstd:19`) |
| `--direct-io` | Use O_DIRECT to bypass page cache (requires `linux-direct-io` feature) |
| `--force` | On a command that would refuse unindexed input: proceed with the slower fallback (see [`--force`](#--force)) |
| `--generator <GENERATOR>` | Override the writing program name in the output header |
| `--output-header <KEY=VALUE>` | Set output header fields (repeatable). Keys: `osmosis_replication_timestamp` (UNIX seconds or `YYYY-MM-DDTHH:MM:SSZ`), `osmosis_replication_sequence_number`, `osmosis_replication_base_url`. An empty value (`KEY=`) omits the field |

## Short flags

Every short flag has one meaning across all subcommands. A command offers a
short only where it has the long flag the short stands for.

| Short | Long | Commands |
|-------|------|----------|
| `-b` | `--bbox` | extract, export |
| `-c` | `--suppress-common` | diff |
| `-d` | `--directory` | extract |
| `-e` | `--expressions` | tags-filter, export, inspect tags |
| `-g` | `--get` | inspect |
| `-h` | `--help` | every command |
| `-i` | `--id-file` | getid, getparents |
| `-I` | `--id-osm-file` | getid, getparents |
| `-j` | `--jobs` | tags-filter, inspect, inspect tags, diff, apply-changes, merge-changes |
| `-M` | `--max-count` | inspect tags |
| `-o` | `--output`, `--output-dir` | every command that writes an output (`--output-dir` on build-geocode-index) |
| `-O` | `--overwrite` | every command that writes an output |
| `-p` | `--polygon` | extract |
| `-q` | `--quiet` | every command (global) |
| `-r` | `--add-referenced` | getid |
| `-R` | `--omit-referenced` | tags-filter |
| `-s` | `--osmium-summary` | diff |
| `-t` | `--type` | cat, diff, check, export, inspect tags |
| `-v` | `--verbose` | diff, getid |
| `-V` | `--version` | `pbfhogg` |

Where a letter used to mean something else on a command, it now refuses on
that command, naming the long flag to spell instead, so an old command line
never silently means something new:

| Command | Short | Spell instead |
|---------|-------|---------------|
| cat | `-c` | `--clean` |
| extract | `-c` | `--config` |
| extract | `-s` | `--simple` |
| renumber | `-s` | `--start-id` |
| getparents | `-s` | `--add-self` |
| inspect | `-e` | `--extended` |
| inspect tags | `-s` | `--sort` |
| tags-filter | `-t` | `--remove-tags` |
| tags-filter | `-i` | `--invert-match` |
| getid | `-t` | `--remove-tags` |

## Flag rules

- **A flag the command line makes ineffective is refused**, as a usage error
  (exit `2`, in clap's format), never warned about or ignored: `-O` without
  `-o`, `--force` where nothing would refuse, `--tmp-dir` on a path that
  writes no scratch, `-j` on a path with no worker pool, a PBF-only flag on
  OSC input, and so on. The refusal names the flags involved.
- **A fallback the host or the input selects at run time is not grounds for
  refusal**: an unindexed input or one with negative IDs taking diff's
  sequential path, `-j 0` resolving to one worker on a one-core host. The same
  command line must not be valid on one machine or file and invalid on
  another. Such a fallback prints a `Note:`.
- **Every flag is parsed and checked before any input is opened**: malformed
  values (`--bbox`, `--start-id`, the time-filter timestamp, `--drop-ids`,
  `--output-header`, `--compression`, `-j`), flag combinations, and
  `--direct-io` / `--io-uring` in a build without the feature. Positional IDs
  (which depend on `--default-type`) and expressions are checked right after,
  still before the output is reserved; an `-e` expressions file, an
  `extract --polygon` file and an `extract --config` file are read at that
  point too. Two checks ride a read that happens anyway: tags-filter's input
  kind, when not given by `--input-kind`, comes from a four-byte sniff of the
  input, so its OSC-mode refusals follow that sniff; and an ID file
  (`-i`, `-I`) is read after the output is checked, so an existing output is
  refused before a full pass over an `-I` PBF. Refusals that depend on the
  input's contents (unsorted, unindexed, lacking `LocationsOnWays`) come from
  the first read of the input that can tell.

## `-j/--jobs`

One rule on every command that has it: `0` (the default) sizes the worker
pools automatically, `1` runs sequentially, `N` caps the worker pools at `N`
threads. "Automatically" is each command's own sizing (a reserve of cores for
its scanner, drain or writer threads, a minimum, a fallback when the host
cannot say how many cores it has); `-j 0` and an omitted `-j` are the same.
An explicit `-j` (any value, `0` included) is refused where it can have no
effect: `tags-filter -R` and OSC-mode tags-filter, `inspect` outside
`--nodes`, `diff -v`. Two exceptions to the rule:

- **diff**: `N` is also the number of ID-range shards the parallel path
  partitions the inputs into. The parallel path needs both inputs indexed and
  free of negative IDs; otherwise the run takes the sequential path with a
  `Note:`.
- **apply-changes**: `-j 1` is refused. Single-worker operation through the
  node-to-way barrier and its failure shutdown has not been validated.

The other commands that run worker pools (sort, cat, repack, degrade,
renumber, extract, add-locations-to-ways, getid, getparents, time-filter,
check, export, digest, build-geocode-index) size them themselves and have no
`-j`.

## `--force`

**A command that would refuse unindexed input proceeds with the slower
fallback.** Only commands that refuse unindexed input have the flag, and each
refuses through the same gate: a typed missing-indexdata error naming the
input, which the CLI follows with `Generate an indexed PBF first ...` (the
library's text names no command or flag). The CLI then adds `Or pass --force
to proceed anyway.` only where
`--force` is accepted: modes that need indexdata whatever `--force` says get
no such line, and `build-geocode-index` names `--allow-unindexed` instead.

| Command | Refuses unindexed input on | `--force` refused with |
|---------|----------------------------|------------------------|
| cat | `--type`, `--clean`, `--dedupe` | plain `cat`, which adds the indexdata |
| sort, repack, getid (with or without `--invert`), inspect tags | every path | - |
| degrade | the decode path (`--unsort`, `--unsort-intra`, `--strip-locations`, `--drop-ids`) | only header-only flags |
| tags-filter | PBF input | `--invert-match`, `--distrust-metadata` (neither refuses), OSC input |
| extract | complete-ways and `--smart` | `--simple` |
| inspect | `--nodes` | anything but `--nodes` |
| add-locations-to-ways | sparse, and `--index-type auto` routing to it | `--index-type external` and `--inject-prepass`, which need indexdata whatever `--force` says |
| apply-changes | every path | `--locations-on-ways`, which needs an indexed base whatever `--force` says |

`renumber` has no `--force`: its schedule needs indexdata on every blob and
has no fallback, so an unindexed blob is a hard error. `getparents`,
`time-filter`, `diff`, `digest`, `check` and `export` never refuse unindexed
input, so they have no `--force` either (`diff` prints a `Note:` when it falls
back to its sequential path).

`build-geocode-index` is the exception: its indexdata switch is
`--allow-unindexed`, and its `--force` is a legacy shorthand for both
`--overwrite` and `--allow-unindexed`. It keeps working, and warns every time
that it also grants overwrite, naming the two flags to spell instead.

This section is about absent indexdata. What commands believe when it is
present, and what a partially indexed file meets, is in
[indexdata-trust.md](indexdata-trust.md).

## Output header

Every PBF-writing command derives its `OSMHeader` from one rule
(`src/commands/output_header.rs`): a header never claims more than the output
contains. Each command states facts about its output - its order, what it did
to way payloads and way blob headers, its bbox and replication state - and the
header follows from them.

- **`Sort.Type_then_ID`**: declared when the command emits type-then-ID order
  by construction (sort, renumber, apply-changes, `cat --dedupe`), or when it
  keeps the input's order and its one input declares it. A concatenation of
  several inputs, and `degrade --unsort*`, are never declared sorted.
- **`LocationsOnWays`, `pbfhogg.WayMembers-v1`, `pbfhogg.SharedNodePins-v1`**:
  declared only when every way blob written carries them, with a meaning that
  still holds for the output. A command that copies every way blob whole
  (payload and `BlobHeader`) keeps each one its input declares; one that
  copies payloads but rebuilds some blob headers (to add indexdata) keeps
  `LocationsOnWays` and `SharedNodePins-v1` and drops `WayMembers-v1` (field
  5 lives in the `BlobHeader`); one that re-encodes ways with their
  coordinates keeps `LocationsOnWays` only; one that re-encodes ways
  otherwise keeps none. The two prepass features are facts about the whole
  file (relation membership, nodes shared between way positions), so they are
  kept only from a single input whose ways and relations reach the output
  with their content unchanged, all of them or a subset; a subset only
  over-approximates them, which their contract allows. With several inputs
  `LocationsOnWays` still means each way carries the coordinates its own
  input recorded, not that they agree with another input's nodes. The
  prepass fields are authoritative only in a file that declares them. When
  the input declares a feature the output does not, the command warns
  (`header.way_metadata_loss`); the warning means the declaration is gone,
  not necessarily the bytes.
- **bbox**: the input's, or the union of every input's when all declare one
  (an input without a box makes the extent unknown, and the box is omitted),
  widened onto the decimicrodegree grid so a re-encoded node on its edge
  stays inside. Omitted by apply-changes, whose OSC can place a node outside
  the base's box, and by `degrade --strip-bbox`. `extract --set-bounds`
  declares the region's envelope instead, which complete_ways and smart
  exceed by design with the out-of-region nodes of selected ways (osmium
  does the same).
- **Replication state**: the base URL names the feed, and the timestamp and
  sequence number the position in it, that the output's content was drawn
  from: every object is in that feed's ID space, and a snapshot's objects are
  at their version as of that position. They do not promise that applying the
  feed's later diffs maintains a selection (an extract or a filter grows under
  them), and they are only as true as the input headers they come from. The
  base URL is kept when every input names the same one; the timestamp and
  sequence number only when every input carries the same timestamp, sequence
  number and base URL. time-filter writes `min(input timestamp, cutoff)` and
  keeps the sequence number only when the input's timestamp is known and not
  after the cutoff. apply-changes keeps the base URL only: its output is past
  the base's position, at one it cannot name (an OSC has no sequence number,
  nor proof that it follows the base without a gap); a caller that knows the
  new position sets it with `--output-header`. renumber keeps none of the
  three: its new IDs are not the feed's, so the feed's diffs would address
  other objects.
- **`source`** is kept when every input agrees; **`HistoricalInformation`**
  when any input declares it.
- **Writing program**: `pbfhogg/<version>`, the library's package version.
- Optional features pbfhogg does not know are not carried: nothing can vouch
  for them after a rewrite.
- **`--generator` / `--output-header`** apply last, and each replaces only
  the field it names; a later `--output-header` for the same key replaces an
  earlier one. An empty `--output-header` value omits the field, whatever the
  policy would have written (`--output-header osmosis_replication_base_url=`
  writes no base URL), as in osmium. `--generator ""` writes a present, empty
  writing program (osmium writes the writing program unconditionally too).
  An empty `source` or base URL carried from an input header stays a
  present, empty field.

The exception is degrade's header-only passthrough (`--strip-indexdata`,
`--strip-tagdata`, `--strip-bbox`, `--corrupt-*`), which copies every blob
payload and every other blob-header field, and so forwards the input header
field for field - writing program, custom features and unknown fields
included - with the bbox removed under `--strip-bbox` and each override
replacing only its own field.

A command that copies blobs raw decides its header from the first data blob's
indexdata (`cat`, `cat --type`, `getid --invert`); `sort` and `cat --dedupe`
decide from their complete blob index. If a later way blob without indexdata
would have to go through a branch that drops a declared enrichment, the run
fails with an error rather than write a header that lies.

| Command | Order | Way enrichment kept |
|---|---|---|
| `cat` (one input) | input's | all |
| `cat` (several inputs) | not sorted | `LocationsOnWays` when every input declares it |
| `cat --type` | input's | all |
| `cat --clean` | input's | none |
| `cat --dedupe` | sorted | as copied, none when a way blob is in an overlap run |
| `sort` | sorted | all, none when a way blob is rewritten |
| `repack` | input's | `LocationsOnWays` |
| `degrade` (header-only flags) | input header, verbatim | input header, verbatim |
| `degrade` (re-encoding flags) | input's, not sorted under `--unsort*` | none |
| `renumber`, `time-filter` | sorted (both require sorted input) | none |
| `tags-filter`, `getid`, `getparents`, `extract` | input's | none |
| `getid --invert` | input's | all when no way ID is removed, else none |
| `add-locations-to-ways` | input's | `LocationsOnWays`, plus the prepass features with `--inject-prepass` |
| `apply-changes` | sorted | `LocationsOnWays` with `-l`, else none |

## Output files

Every output a command writes - PBF, OSC, diff text, GeoJSON, every file of
`extract --config`, the `build-geocode-index` directory - follows one contract:

- **An existing output is refused before any work is done**, unless
  `-O, --overwrite` is given. `Error: output <path> already exists and
  replacing it was not requested; pass --overwrite (-O) to replace it`, exit
  `2`. For `extract --config` every output the config names is checked up
  front, and `--overwrite` on the command line covers all of them.
- **An output may not be any file the command reads**, refused before any
  work with `output <path> aliases input <path>`, exit `2`, even with
  `--overwrite`. That covers every input, not only the positional ones:
  `getid` and `getparents` `-i` and `-I`, the `-e` expressions file of
  `tags-filter` and `export`, `extract --polygon`, the `extract --config`
  file and the `polygon_file` entries it names. A directory output may not
  hold one either: `build-geocode-index` refuses an `--output-dir` that
  contains its input, or the entry naming it (a symlink or hard link
  inside), with `output <dir> contains input <path>`, because replacing
  the index directory deletes everything in it. A path that merely passes
  through the directory (a symlinked directory inside it) is not refused;
  only that link would be lost.
- **A symlink is never an output**, even with `--overwrite`: replacing it
  would turn the link into a regular file. The error names the link and its
  target. An existing directory (or device, FIFO) where a file output is
  named, or a non-directory where a directory output is named, is refused
  the same way.
- **An OSC output is framed by its name, the same way for every command**
  (`merge-changes`, `tags-filter --input-kind osc`, `diff --format osc`). A
  name ending `.osc` is written as plain XML, a name ending `.osc.gz` is
  gzipped, and any other name is refused before any work with a usage error
  naming both suffixes, exit `2`. The match is case-sensitive (`.OSC` is
  refused). Every OSC reader here sniffs the gzip magic, so either form is
  read back.
- **Nothing appears under the output name until the run has succeeded.**
  The command writes a hidden staging file beside the output
  (`.pbfhogg-tmp-<pid>-<n>.<name>`), syncs it and renames it into place after
  its final flush; a run that fails before that removes it, so it leaves no
  file at the output path and leaves an existing output untouched. So does a
  run interrupted by SIGINT or SIGTERM (see [Interruption](#interruption)). A
  run killed outright (SIGKILL, the OOM killer, power loss, a second
  interrupt) before publication can leave the hidden staging file behind,
  never a file under the output name. The
  rename itself is the last step, and the few failures possible after it are
  reported as such: a directory sync that fails after the output was put in
  place is an error that says the output *was* published (naming any backup
  of a replaced output, which is then kept), and a group whose rollback
  cannot complete names what is left where. The output's directory must
  exist and be writable.
- Without `--overwrite` publication is exclusive: an output that appears
  while the command runs is not replaced either. On a filesystem with
  neither an exclusive rename nor hard links, such a run fails at the end
  and says so; `--overwrite` publishes there.
- Several outputs (`extract --config`) are published together: if one cannot
  be put in place, the ones already published are rolled back. If a rollback
  step itself fails, the previous output is kept in a `.pbfhogg-backup-*`
  directory beside it and the error names it.

`-o /dev/null` is not an output: it is refused as not a regular file.

## Scratch files

`add-locations-to-ways`, `diff` (parallel `-j`, and `--format osc`) and `build-geocode-index` write
scratch: disposable files that can be very large (hundreds of GB for `add-locations-to-ways
--index-type external` at planet scale). Each run makes **one** hidden directory,
`.pbfhogg-run-<pid>-<random>`, holds everything it writes in there and removes it when it ends,
whether it succeeded, failed or was interrupted. If the directory cannot be removed the run
says where the leftover is. The directory is made at startup, before any input is opened, by
every run that may need scratch. A run whose command line guarantees it writes none (text
`diff -j 1`, text `diff -v`, and a quiet text `diff`, which only counts and shards without
writing any text) makes no directory and needs no writable location.

- `--tmp-dir <DIR>` chooses the directory the run directory is made in. It must exist and be
  writable, and it is checked before any input is opened, so a bad value fails at once. Without
  it the scratch goes in the output's directory (the current directory for `diff` text to
  stdout, and for an output name with no directory part). `build-geocode-index` refuses a `--tmp-dir` inside the index
  directory it replaces.
- An output's own staging file (`.pbfhogg-tmp-*`) and a replaced output's backup
  (`.pbfhogg-backup-*`) stay beside the output whatever `--tmp-dir` says: publication is a
  rename, which needs the same filesystem.

**Leftovers of runs that never finished** (SIGKILL, power loss, the OOM killer, a second
SIGINT or SIGTERM) are reported,
never removed. Before it starts, a command that writes an output scans the output's directory
(and `--tmp-dir`, when given) and prints one `Warning:` per entry it finds: an unfinished run
directory, scratch left by older pbfhogg versions (`.pbfhogg-external-join-*`,
`.pbfhogg-sparse-index-*`, `diff-par-*`, `derive-*` files), and an output's hidden staging
file. Each line gives the size and why the entry is believed stale. That is a belief, not a
proof: a run directory is judged by whether its `flock` is still held, other entries by whether the
pid in the name exists, and a pid can be reused or belong to another container sharing the
volume. Check before you delete. A run directory whose lock cannot be probed at all (another
user's, an unexpected file type, a lock the filesystem refuses) is not reported as stale but as
a `Note:` saying whether it is in use is unknown: do not delete it. On an NFS mount that keeps
`flock` local to each client (`local_lock=flock` or `local_lock=all`, or NFSv3 with `nolock`)
the lock cannot see a live run on another host, so such a run's directory may be reported while
it is in use, as it may after a client lost its lock to an expired NFSv4 lease; keep `--tmp-dir`
on a local filesystem when hosts share a directory.

Each command also looks where older versions of the same command left their scratch:
`extract --config` scans the directory of every output the config names (each directory
once), `diff` also scans the old input's directory (`diff-par-*`; `--format osc` fragments of
older versions sat beside the output, which is scanned anyway), and `build-geocode-index` also
scans everything inside the existing index directory it is about to replace, at any depth,
including `.buckets-level*` trees (and the parent for `.geocode-build-*` staging of earlier
versions). A `.buckets-level*` tree inside an index is deleted along with the old index when
`--overwrite` replaces it; the report says so. That is the only reported entry that
publication removes: an index that holds anything else pbfhogg leaves behind (a backup, a
staging entry, a run directory, live or not, an older version's scratch) is not replaced at
all. `--overwrite` then fails before any work, naming the entries; move or remove them first.
An output named like one of these names, or inside one, is refused the same way.

A `.pbfhogg-backup-*` entry is reported with its own wording. It can be the only copy of a
previous output, so pbfhogg never deletes one and you should look at it before removing it.
If a backup cannot be removed after a successful replacement, the run still succeeds and
prints a `Warning:` naming it. If the replacement's directory cannot be synced, the backup is
kept rather than removed, and the error names it. Library callers get the same scan as typed values from
`pbfhogg::scratch::find_stale` and `pbfhogg::scratch::find_stale_in_index`.

### Names pbfhogg leaves behind

The spellings below are the contract for an external tool (a benchmark harness that kills
pbfhogg children, a cleanup job). `<pid>` is the pid of the pbfhogg process, `<seq>` a counter
within it, `<dest name>` the last component of the output path (the last 200 bytes of it, when
longer).

| Name | What it is | An external tool may delete it |
|------|-----------|--------------------------------|
| `.pbfhogg-tmp-<pid>-<seq>.<dest name>` | Staging for an output, beside the destination: a file for file outputs, a directory (holding `new/`) for the geocode index. Never a file under the output name. | When no pbfhogg process is live that could own it (check `<pid>`; it is a hint only) |
| `.pbfhogg-backup-<pid>-<seq>.<dest name>` | A directory holding the previous output, kept while a replacement is published, and kept for good when a rollback could not restore it, when syncing the replacement's directory failed (the error names it), when removing it after a successful replacement failed, or when something pbfhogg never deletes turned up inside it (a warning names it in the last two cases). | **Never without a human.** It can hold the only copy of the previous output |
| `.pbfhogg-run-<pid>-<random>` | One run's scratch directory, in the output's directory or `--tmp-dir`. | When its lock is not held (below), and nothing is using it |

A run directory holds a `LOCK` file, on which the owning process keeps an exclusive `flock(2)`
for its whole life, and a `READY` file created after the lock was taken. The kernel drops the
lock when the process dies. To test liveness, open `LOCK` read-write without creating it
(`O_RDWR`, no `O_CREAT`; on NFS an exclusive `flock` is a POSIX write lock and fails with
`EBADF` on a read-only descriptor) and try `flock(LOCK_EX | LOCK_NB)`: failure with
`EWOULDBLOCK` means a live run; success means the owner is gone (release the lock at once).
**Any other error**, from the open or the lock, means liveness is unknown: do not delete. A
directory without `READY`, or without `LOCK`, is a run that is starting or one that died while
starting or finishing; there is no lock to take, so treat it as live while any pbfhogg process
could still be using that location. Never create `LOCK`. The lock is advisory: on an NFS mount
that keeps `flock` local to each client (`local_lock=flock` or `local_lock=all`, NFSv3
`nolock`) it says nothing about a run on another host, and a client whose NFSv4 lease expired
has lost it.

Names older versions used, which the scan still recognises and which are disposable once no
pbfhogg process runs there: `.pbfhogg-external-join-<pid>*`, `.pbfhogg-sparse-index-<pid>*`
(directories and files), `diff-par-<pid>*.tmp`, `derive-creates-<pid>*.tmp`,
`derive-modifies-<pid>*.tmp`, `derive-deletes-<pid>*.tmp`, `derive-par-creates-<pid>*.tmp`,
`derive-par-modifies-<pid>*.tmp`, `derive-par-deletes-<pid>*.tmp`, and, inside an index
directory, `.buckets-level<N>`. The geocode staging of the previous version,
`.geocode-build-<n>` (`<n>` a counter, not a pid), is disposable only when it has no `old/`
inside: `old/` was the previous index during a swap and may be the only copy, so the scan
reports it as a backup.

---

## Commands

### inspect

Inspect PBF file: metadata, block breakdown, ordering analysis.

`inspect` describes a file: what it is and what it claims. By default it
reports counts, ID ranges and per-block stats from the blobs' own indexdata
(`--distrust-metadata` decodes instead). What it finds is not a verdict: a
readable file exits 0 (`--indexed` alone answers yes or no in its exit code).
Whether the file's contents match its claims, or meet the format's
invariants, is [`check`](#check)'s question.

On indexed PBFs, uses an index-only fast path that reads blob headers without decompression (~36ms on 473 MB vs ~4s for full decode).

```
pbfhogg inspect [OPTIONS] <FILE>
pbfhogg inspect tags [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `--indexed` | Check if PBF has blob-level indexdata (exit code 0 = yes, 1 = no). Reads the first data blob only; later blobs are not checked (to verify every blob's indexdata against its payload, use `check --index`). A header-only file (no data blobs) is indexed; a file without a leading OSMHeader is an error (exit 2), not a "no" |
| `--nodes` | Analyze node coordinate statistics for FOR compression sizing |
| `-j, --jobs <N>` | Worker threads for `--nodes` (see [`-j/--jobs`](#-j--jobs)). **Requires `--nodes`**: no other inspect mode has a worker pool, so passing it elsewhere is an error rather than a no-op. |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show blocks outside inspect's per-type median thresholds, plus mixed blocks |
| `--extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering. (`-e` used to be its short; `-e` now refuses here, naming `--extended`.) |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`). Data and metadata paths enable extended stats automatically. Not combinable with `--json`, `--blocks` or `--anomalies` (the last two only shape the full report). An unknown key is refused before the input is opened. |
| `--json` | Machine-readable JSON output. Not combinable with `--nodes` (node stats have no JSON form) or `--get`. |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123` or `node/123`). Also accepts `w456` / `way/456` and `r789` / `relation/789`, including signed IDs. Uses indexdata to skip non-matching blobs, early exit on sorted PBFs. **A mode of its own**: mutually exclusive with every other `inspect` flag (`--indexed`, `--nodes`, `-j`, `--blocks`, `--id-ranges`, `--locations`, `--anomalies`, `--extended`, `--get`, `--json`, `--force`), which it used to accept and discard. |
| `--distrust-metadata` | Decode every blob instead of reporting counts, ID ranges and per-block stats out of the blobs' own `indexdata`. Nothing validates indexdata against the payload it describes, so on a file you are inspecting *because* you suspect it, the default fast path reports the file's claims rather than its contents. Applies to the scanning report only: rejected alongside `--indexed`, `--nodes` and a `file.*` or `header.*` `--get` key, none of which consult indexdata contents. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback. **Requires `--nodes`**: only the node-stats scan refuses unindexed input, so passing it to another mode is an error rather than a no-op. |

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `--sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc. `count` and `name` are accepted as aliases for `count-desc` and `name-asc`. (`-s` used to be its short; `-s` now refuses here, naming `--sort`.) |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Count one element type: exactly one of node, way, or relation (a list is refused) |
| `-j, --jobs <N>` | Worker threads (see [`-j/--jobs`](#-j--jobs)) |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback (see [`--force`](#--force)) |

`inspect tags` expressions use the same syntax as `tags-filter`, including
element-type prefixes. For example, `w/highway=primary` counts that match only
on ways; `--type` adds an overall element-type restriction.

Expressions select individual tags for counting. If a way has both
`highway=primary` and `name=Main Street`, the expression `w/highway` counts
only its highway tag. In `tags-filter` the same expression selects the whole
way; export also selects whole elements and accepts an omitted expression
filter as unrestricted.

Counts are exact. Memory grows with the number of distinct selected key/value
pairs and their string bytes, with no fixed byte cap or spill to disk. One
global map stores one key string per distinct key and one value string and
counter per distinct pair, plus hash-table capacity overhead. The blob
schedule, decoded blobs and in-flight per-blob count maps add transient
storage; input blobs are not limited to the writer's default element count.
The sorted result reserves one entry per distinct pair before applying
`--min-count` or `--max-count`, and retained entries each own a copy of the key.
During conversion that allocation overlaps the remaining map storage.
Consequently count thresholds do not bound memory, even when few rows are
printed. Restrict element types and tag expressions to reduce the selected
cardinality; neither a higher minimum count nor fewer workers bounds the
global map. No approximate counts or silent truncation are used.

### check

Validate PBF file integrity (IDs + referential integrity).

`check` verifies a file: its contents against its claims and against the
format's invariants, exiting 1 on a violation. Describing a file - what it
claims, cheaply - is [`inspect`](#inspect)'s job.

Validate PBF integrity along three independent axes, selected by flag. With no
flag, the ID and referential integrity checks both run.

`--index` is a selector like the other two but is deliberately **not** in the
default set: it decompresses every blob in the file, and folding that into a
bare `check` would multiply the cost of the command everyone already runs.
Passing it suppresses the default pair; combine it with `--ids` / `--refs` to
run more than one.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--index` | Verify blob-level metadata against the blobs. Decompresses every blob |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Retain at most N violation details per check (0 = unlimited); scanning and total counts continue [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |
| `--json` | Machine-readable JSON output |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With the global [`-q, --quiet`](#quiet) `check` prints no report: the exit
code is the answer. `--json`, `--show-ids` and `--max-errors`, which only
shape the report, are refused with it.

`--full` and `-t/--type` apply to the ID check and are rejected under
`--refs` alone; `--check-relations` and `--show-ids` apply to the ref
check and are rejected under `--ids` alone. Violations exit `1`, not
`2` - see [Exit codes](#exit-codes).

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

The ID check judges monotonicity by canonical OSM order - `0`, then the
negative IDs by ascending absolute value, then the positives - which is the
order `sort` produces. On a positive-only file (every production PBF) that is
plain `i64` order; on a file carrying negative IDs the two differ, and the
canonical one is the definition.

#### `--index`: the metadata trust boundary

`BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox) and
`BlobHeader.tagdata` (the blob's tag key set) are claims every fast path in this
crate believes without opening the blob; who believes which, and what a false
one costs, is in [indexdata-trust.md](indexdata-trust.md). Nothing in the
format signs any of it, and `degrade --corrupt-indexdata` /
`--corrupt-indexdata-kind` / `--corrupt-tagdata` exist precisely to manufacture
files whose hints are well-formed and false.

`check --index` does not verify order: a file whose header claims
`Sort.Type_then_ID` with a blob internally out of order passes it. Combine it
with `--ids`, which checks ID and type order.

`check --index` is the other side of that boundary. It decompresses every
OSMData blob, re-derives the metadata from the payload with `scan_block_ids`
and `scan_block_tags`, and reports each disagreement with the blob's ordinal and
byte offset.

Those are the same two scanners the writer's passthrough path uses, so on a
re-framed blob the check cannot catch a bug *in the scanner itself* - it can
only catch metadata that has since diverged from its payload. Against the
ordinary write path it is genuinely independent: `BlockBuilder` derives its
index incrementally as elements are added and never rescans the bytes it
emitted, so a disagreement between the two is a real finding either way.

**There is no `--distrust-metadata` flag on it, by design.** A verifier that
consulted the thing it is verifying in order to decide what to look at would be
worthless, so the command installs no blob filter, never calls
`require_indexdata`, and decompresses every blob unconditionally - including
blobs whose stored index claims a kind nobody asked about. Distrust is the only
mode it has.

Absence is not a claim. A blob with no indexdata, or no tagdata, is not
reported; that is the ordinary state of any third-party PBF, and of every v1
(26-byte) index, which has no bbox field at all.

**Unreadable is not the same as absent.** Indexdata whose length, version byte
or element-kind byte the reader does not recognise is reported as a violation
even though every *reader* collapses it to "no index" and decodes the blob.
The safe reading is right for a reader and wrong for a verifier: a file that
claims something it cannot back up is not a file that claims nothing.
Malformed tagdata is reported the same way, and so is a well-formed tagdata
attached to a blob this crate refuses to index at all - a group mixing element
kinds, where any key set is necessarily incomplete because the scanner sees
only the first element field, and `tags-filter` would skip the blob on the
strength of it.

Two checks are containment rather than equality:

| Field | Test | Why |
|-------|------|-----|
| kind, count, min_id, max_id | exact equality | every consumer treats them as exact |
| bbox | stored must **contain** the blob's coordinates | the scan rounds outward through `granularity`/`lat_offset`/`lon_offset`, so a stored box narrower than the scan is the only shape that can make a spatial filter skip a blob it should have read. A wider box is sound; an absent box is no claim |
| tag key set | stored must contain every key the blob carries | an omitted key makes `tags-filter` skip a blob holding matches. A key listed but not present is sound - it only costs a decode - and is reported as a separate, distinct violation |

Exit `1` means mismatches were found, exit `2` that the check could not run.

The ref check reports element IDs it could not track - negative IDs; a
positive ID above the range it sized from the file's own indexdata is still
tracked, inserted after the parallel phase - as a separate
`Untrackable element IDs` line and as `untrackable_node_ids` /
`untrackable_way_ids` / `untrackable_relation_ids` under `--json`. References
to such an element are counted as missing, so a non-zero count means the
missing-reference numbers are an upper bound. `--ids --full` reports the same
situation as an `id_out_of_range` violation.

---

### cat

Concatenate PBF files with optional type filtering. Embeds blob-level indexdata and tagdata automatically.

With `--dedupe`, merges multiple sorted PBF files with blob-level passthrough and exact-duplicate deduplication.

A blob that already carries indexdata is passed through with it, unchecked,
so plain `cat` cannot repair false indexdata. `--type` copies a blob raw on
its indexdata kind and `--dedupe` passes blobs through on their kind and
range, both believed without a decode. See
[indexdata-trust.md](indexdata-trust.md).

```
pbfhogg cat [OPTIONS] --output <OUTPUT> <FILES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation). Not valid with `--dedupe`. |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user). Not valid with `--dedupe`. (`-c` used to be its short; `-c` now refuses here, naming `--clean`.) |
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted). Rejects `--type` and `--clean`: the dedupe path is blob-level passthrough and implements neither. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O. Only valid with `--dedupe` - the plain `cat` path has no io_uring writer, and passing it there is an error. |
| `--force` | Proceed on unindexed input with the slower fallback. Requires `--type`, `--clean` or `--dedupe`: plain `cat` adds the indexdata and never refuses, so `--force` alone is an error. |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

**Blocks no single index describes.** Plain `cat` copies an indexed blob
whole and an unindexed blob's payload unchanged, rebuilding only its blob
header to add indexdata (which drops any field 5 the header carried). The
exception is an unindexed blob whose block no single index can describe (no
elements, a group carrying more than one element field, kinds changing
across groups); an indexed blob is never scanned, so it passes through
whatever its block holds. Such an unindexed block is rebuilt into
single-kind blocks by the same rules `--type` and `--clean` re-encode with:
nodes written dense, coordinates truncated toward zero to decimicrodegrees,
timestamps truncated to whole seconds, metadata with an absent or sentinel
version discarded, missing fields of retained metadata written as defaults,
unknown fields discarded. One that yields no elements is omitted, with any
unknown fields it carried. A `Note:` line says how many blobs were rebuilt
and how many omitted; the summary counts both as decoded. A way in such a
block is refused under a declared `WayMembers-v1`, and a way carrying pins
under a declared `SharedNodePins-v1`, since the rebuild cannot write either.

**Output header.** Follows [Output header](#output-header). Plain `cat` copies way
payloads whole and `--type` copies indexed way blobs whole, so the way
enrichments the input declares are kept (a rebuilt block keeps inline
locations and refuses the rest, as above; `--clean` re-encodes, and drops
them); `--dedupe` re-encodes the way
blobs in its overlap runs. With several inputs the header is reconciled across
all of them, the output is not declared sorted (`--dedupe` excepted), and the
prepass features are never declared. osmium writes a wholly blank header for
multiple inputs - the same honesty rule, applied bluntly - so keeping the
agreed fields is a deliberate deviation (DEVIATIONS.md).

### sort

Sort PBF into standard order (nodes, ways, relations, each by ascending ID). For already-sorted inputs with indexdata, blobs pass through as raw bytes.

```
pbfhogg sort [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on unindexed input with the slower fallback (see [`--force`](#--force)) |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### repack

Re-encode a PBF with a configurable per-blob element cap. Element semantics, tags, refs, members, metadata, and DenseNodes encoding all round-trip; output is type-sorted and propagates `Sort.Type_then_ID` from the input header.

Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement (Geofabrik's ~8 k/blob convention vs `planet.openstreetmap.org`'s ~228 k/blob), so commands with implicit blob-count scaling (`HeaderWalker`-based paths in particular) can be measured at controlled densities.

```
pbfhogg repack [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--elements-per-blob <N>` | Per-blob element cap [default: 8000]. `8000` matches the osmium / Geofabrik convention; pass a larger value to approximate `planet.openstreetmap.org`-style packing. Must be > 0. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on unindexed input with the slower fallback (see [`--force`](#--force)) |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

Growing (e.g. europe 8 k -> 64 k) coalesces elements across input-blob boundaries, so caps larger than the input blob size fire correctly. On a coalescing shrink the output blob count is not the general `ceil(elements / cap)`: each input blob whose element count is not a multiple of the cap emits its tail as its own possibly-under-cap block (the deliberate trade that keeps output ID-monotonic across coalesced boundaries). When the input header declares `LocationsOnWays`, the output re-advertises it and every inline way-node coordinate round-trips exactly; the two `pbfhogg.*` prepass features (`WayMembers-v1`, `SharedNodePins-v1`) are dropped with a warning.

### degrade

Produce a valid-but-adversarial PBF by stripping properties or perturbing structure. Each flag composes; at least one is required. Used to produce inputs for benchmarking non-optimal code paths (`sort` overlap-rewrite, `add-locations-to-ways`, `--force` fallbacks).

```
pbfhogg degrade [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--unsort` | Clear `Sort.Type_then_ID`; perturb the element stream so at least one adjacent same-kind blob pair has overlapping IDs (one per kind that has more than `block-cap + 1` elements). Triggers `sort`'s overlap-rewrite path. |
| `--unsort-intra` | Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with an internal ID-order inversion but non-overlapping blob ranges - the intra-blob shape `sort`'s overlap detector cannot see. Mutually exclusive with `--unsort`. |
| `--strip-locations` | Drop the `LocationsOnWays` header feature. Inline way-node coordinates are not preserved; downstream `add-locations-to-ways` runs see a redundancy-free starting point. |
| `--strip-indexdata` | Clear `BlobHeader.indexdata` on every OsmData blob. Forces commands into their `--force` / non-indexed fallback paths (`sort`, `getid`, `tags-filter`). Blob payloads are not decompressed. |
| `--strip-tagdata` | Clear `BlobHeader.tagdata` (the per-blob tag key index) on every OsmData blob, forcing `tags-filter`'s no-hint fallback path. Leaves `indexdata` intact - a tagdata-stripped file is still indexed. |
| `--corrupt-tagdata` | Replace `BlobHeader.tagdata` on every OsmData blob with a well-formed tag key index naming one key (`pbfhogg:degrade:no-such-key`) the blob does not carry. The result parses cleanly and lies, so `tags-filter` skips blobs whose elements match and emits a short answer. This is the adversarial case `--strip-tagdata` cannot produce: a *missing* index is decoded rather than trusted, so stripping only exercises the slow path. Mutually exclusive with `--strip-tagdata`, and with every blob-re-encoding flag (which would rebuild the index correctly). `tags-filter --distrust-metadata` is what reads such a file correctly. |
| `--corrupt-indexdata` | Replace `BlobHeader.indexdata` on every OsmData blob with a well-formed index that overstates the blob: same element kind, `count` and `max_id` inflated by 1,000,000. The kind is preserved deliberately, so per-kind classify schedules still route correctly and the lie is confined to the counts. Produces the file `inspect` reports from without decoding anything - see `inspect --distrust-metadata`. Mutually exclusive with `--strip-indexdata` and with every blob-re-encoding flag. |
| `--corrupt-indexdata-kind` | Rotate the element *kind* declared by `BlobHeader.indexdata` on every OsmData blob - node to way to relation to node - leaving `min_id`, `max_id`, `count` and the bbox honest. The kind is what per-kind blob schedules (`build_classify_schedule`) select on, so unlike `--corrupt-indexdata` this is the corruption that makes a consumer skip a blob *outright* rather than misreport it, which is why the two are separate flags. It is what makes `tags-filter --distrust-metadata` testable through relation closure and way-dependency collection rather than only through the direct-match pass. Mutually exclusive with `--strip-indexdata`, combinable with `--corrupt-indexdata`, and rejected alongside every blob-re-encoding flag. |
| `--strip-bbox` | Clear `HeaderBlock.bbox` (field 1). Header-only change; no OsmData blob is touched. Exercises `inspect`'s bbox handling (`extract_header_metadata`, the `inspect --get header.bbox` fast path) and downstream/external-consumer tolerance of a file with no declared extent. Does not affect `extract --bbox`, which derives its region from the CLI argument and prunes via per-blob `indexdata` bboxes rather than reading the header. |
| `--drop-ids <N:SEED>` | Deterministically remove exactly N elements selected globally by kind, ID, and seed. Surviving references to removed elements intentionally dangle. |
| `--force` | Proceed on unindexed input on the decode path (`--unsort`, `--unsort-intra`, `--strip-locations`, `--drop-ids`), which falls back to scanning every blob for every kind; slower. The header-only flags never refuse, so `--force` without a decode-path flag is an error. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

**Implementation paths:** `--strip-indexdata`, `--strip-tagdata`, and/or `--strip-bbox` (with no `--unsort`/`--strip-locations`/`--drop-ids`) run as a header-and-blob-level passthrough: the input `HeaderBlock` payload is forwarded field-for-field via a surgical wire-level field stripper, with only the bbox (field 1) removed under `--strip-bbox` and each `--generator`/`--output-header` override replacing only the field it names, and raw OsmData frames are reframed by copying the original `BlobHeader` through byte-for-byte with only `indexdata` (field 2) and/or `tagdata` (field 4) cleared as targeted. This preserves every other header and blob-header field byte-for-byte - `source`, custom optional features, a non-default `writingprogram`, replication metadata, `WayMembers-v1`, and unknown/extension fields all survive, with or without an override. Any combination involving `--unsort`, `--strip-locations`, or `--drop-ids` decodes elements, re-encodes via `BlockBuilder`, and frames with `indexdata=None` when `--strip-indexdata` composes; `--strip-bbox` on this path clears the bbox from the rebuilt output header. `--drop-ids` requires `N:SEED`, rejects zero N, and is reproducible for a given input and seed.

`degrade --block-cap` and `getparents --full-scan-min-blobs` are hidden
(`hide = true`) test instruments: they override an internal threshold so
the suite can fire a code path without building a full-scale fixture.
They are not part of the supported surface and may change or disappear
without a deprecation cycle. `--full-scan-min-blobs` is nevertheless
range-checked at the CLI boundary (1 to 1,000,000,000): the library entry
point behind it takes the value raw, and 0 is not a threshold - it pins the
FullScan arm unconditionally.

`--recompress` from the design doc remains deferred. The `--unsort` perturbation is the minimum-viable swap that triggers `sort`'s overlap detector; configurable chaos modes (rotate / shuffle / reverse) are deferred.

### renumber

Renumber all element IDs sequentially, remapping cross-references (way node refs, relation member refs).

The output header carries no replication state: the new IDs are not the
feed's, so its diffs would address other objects (see
[Output header](#output-header)).

```
pbfhogg renumber [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--start-id <ID>` | Starting ID(s): single value or comma-separated node,way,relation [default: 1]. (`-s` used to be its short; `-s` now refuses here, naming `--start-id`.) |
| `--compression` | Blob compression [default for renumber: zlib:1; other commands: zlib]. Level 1 is the measured external-renumber write path (see `reference/performance.md`). An explicit `--compression zlib` or `zlib:6` also yields level 1 today, because the value-based API cannot tell omission from an explicit default; any other zlib level, zstd and none are honoured as given. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### extract

Extract elements within a geographic region (bounding box or polygon).

Three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), `--smart` (three passes, completes multipolygon/boundary relations).

Node blobs whose indexdata bbox misses the region are skipped unread, and the
sorted-input schedules split blobs on their indexdata kind; both claims are
believed. A node blob copied raw is first decoded to prove every node lies in
the region, but keeps its input `BlobHeader`. See
[indexdata-trust.md](indexdata-trust.md).

Supports multi-extract via `--config` with a JSON config file specifying multiple regions.
Relative `directory` and `polygon_file` values resolve from the config file's
directory. Relative `output` paths resolve from `directory`, or from the config
file's directory when it is omitted. `-d` rebases configured outputs under its
directory, preserving paths below the configured output directory; an absolute
output outside that root keeps only its filename.
These rules intentionally differ from `osmium extract`: relative `directory`
values use the config file's location here, and `-d` rebases absolute outputs
under the override instead of concatenating the two path strings.

```
pbfhogg extract [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file (required for single extract, omit with --config) |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `-b, --bbox <BBOX>` | Bounding box: minlon,minlat,maxlon,maxlat |
| `-p, --polygon <FILE>` | Polygon GeoJSON file |
| `--config <FILE>` | Multi-extract JSON config file. (`-c` used to be its short; `-c` now refuses here, naming `--config`.) |
| `-d, --directory <DIR>` | Rebase config outputs under this directory (only with --config) |
| `--simple` | Simple strategy (single pass). (`-s` used to be its short; `-s` now refuses here, naming `--simple`.) |
| `--smart` | Smart strategy (three passes, complete relations) |
| `--set-bounds` | Write the extract region bounding box to the output header instead of the input's. complete_ways and smart exceed it by design with the out-of-region nodes of selected ways |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback (complete-ways and `--smart`; not with `--simple`, which never refuses) |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### tags-filter

Filter elements by tag expressions. Default mode resolves relation members transitively (matched relations pull in member ways, nodes, and nested relations). With `-R`, only directly matched elements are emitted.

With `--input-kind osc` (or autodetected from `.osc`/`.osc.gz` extension), filters an OSC change file instead, always preserving deletes. Every flag that only means something for a PBF is refused in OSC mode: `-R`, `--invert-match`, `--remove-tags`, `--distrust-metadata`, an explicit `-j`, `--generator`, `--output-header`, an explicit `--compression` (OSC output is not a PBF; its framing comes from the output name, see "Output files"), `--direct-io` and `--force`. With `--input-kind osc` the refusal comes before the input is opened; an autodetected kind comes from the four-byte sniff that decides it.

At least one expression is required, positional or from `-e`; an `-e` file holding none is refused, and so is a malformed expression, before the input is opened.

Expressions use osmium syntax: `highway=primary`, `amenity`, `w/building=yes`, etc.

#### Expression syntax

An expression is `[nwr/]KEY[!]=VALUE` or `[nwr/]KEY`. The optional `nwr/`
prefix restricts the expression to element types. Key and value are each
matched by the same rules, which follow
`osmium-tool/src/util.cpp::get_string_matcher`:

| Form | Meaning |
|------|---------|
| `foo` | exact match |
| `foo,bar` | any one of the comma-separated alternatives (**both halves**, keys included) |
| `foo*` | prefix match |
| `*foo`, `*foo*` | substring match (osmium has no separate suffix matcher) |
| `*` | matches anything |

Spaces around each half, and around each comma-list element, are stripped, so
`highway = primary` from an `--expressions` file names the key `highway`. The
expression is split at the **first** `=`; a `!` inverts the value match, which
still requires the key to match (`highway!=primary` selects elements that have
a `highway` tag whose value is not `primary`, not elements without the tag).

The `!` is tested on the raw key, *before* the space stripping above, exactly
as `get_tag_matcher` does. It therefore counts as an inversion marker only when
it is the last character before the `=`. `highway !=primary` inverts;
`highway ! = primary` and `highway! = primary` do not, and name the literal
keys `highway !` and `highway!` instead. This is a corner worth stating rather
than discovering, because both spellings parse without error and select a
different set of elements than the reader intended.

```
pbfhogg tags-filter [OPTIONS] --output <OUTPUT> <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--input-kind <KIND>` | Input kind override: `pbf` or `osc` (autodetect from extension by default) |
| `-R, --omit-referenced` | Omit referenced objects (faster, single pass, direct matches only; PBF only). Not with `--remove-tags` (no referenced objects to strip) or `-j` (no worker pool) |
| `--invert-match` | Invert match: exclude matching objects, keep non-matching (PBF only). (`-i` used to be its short; `-i` now refuses here, naming `--invert-match`.) |
| `--remove-tags` | Remove tags from referenced objects not directly matched (not with -R; PBF only). (`-t` used to be its short; `-t` now refuses here, naming `--remove-tags`.) |
| `-e, --expressions <FILE>` | Read filter expressions from file (one per line, # comments) |
| `-j, --jobs <N>` | Worker threads for the two-pass mode's classify passes and its decode-and-filter pool (see [`-j/--jobs`](#-j--jobs)). Refused with `-R`, whose single pass reads through the pipelined reader and has no worker pool |
| `--distrust-metadata` | Do not skip blobs on their `indexdata` or tag index; decode every blob. Blob-level tag-index skipping is a trust boundary with nothing on the other side of it - nothing validates a blob's tag index against its payload - so a file whose index omits a key its elements carry silently loses those elements from the output. This flag trades the skip speedup for an answer that depends only on element data, and waives the indexdata precondition (`--force` is not needed alongside it). Not valid in OSC mode, which has no blobs. `degrade --corrupt-tagdata` produces the file this flag exists for. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback. Not with `--invert-match` or `--distrust-metadata`, which never refuse |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### export

Stream a PBF to GeoJSON. Tagged nodes become Points. Tagged ways become
LineStrings, or Polygons when they are closed and satisfy the built-in area
rules. Untagged nodes and ways are skipped. Relation features are not emitted.
Way export requires the input header to declare `LocationsOnWays`; `--type
node` works without it.

The default `geojsonseq` format writes one Feature object per newline with no
RFC 8142 record-separator byte. `geojson` writes one FeatureCollection.

```
pbfhogg export [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Write to a file instead of stdout |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way. Requires `-o`: stdout is never replaced |
| `--format <FORMAT>` | `geojsonseq` (default) or `geojson` |
| `-t, --type <TYPE>` | Export only these kinds: `node`, `way` or `node,way`. `relation` is refused |
| `-e, --expressions <FILE>` | Read tag expressions from a file, one per line |
| `--properties <KEYS>` | Comma-separated whitelist of tag property keys |
| `-b, --bbox <BBOX>` | `min_lon,min_lat,max_lon,max_lat`; ways match by vertex containment only, so crossing or enclosing geometry without an inside vertex is omitted |
| `--metadata` | Add available `@version`, `@timestamp`, `@changeset`, `@uid`, `@user`, and `@visible` properties |

Every feature includes `@id` and `@type`. Metadata timestamps are RFC 3339 UTC
strings. Tags that collide with emitted reserved property names are omitted.
Polygon exterior rings are closed and counterclockwise. Ways with invalid
geometry are skipped and reported in the stderr summary.

### diff

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering - deterministic regardless of metadata completeness (see [DEVIATIONS](../DEVIATIONS.md#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`/`--osmium-summary`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`. Either format exits `0` when the inputs are identical and `1` when they differ; an OSC diff publishes its `-o` in both cases.

With the global [`-q, --quiet`](#quiet) a text diff prints nothing, neither the diff text nor its summary: the exit code is the answer (`0` identical, `1` different). It compares exactly as a text diff without `-c` does (same paths, sharding and refusals) but formats nothing, so it writes no scratch at any `-j`. `-o`, `-c`, `-v` and `-s`, which only shape that text or its summary, are refused with it, and so is `--tmp-dir`. A quiet `--format osc` diff writes its `-o` as usual and drops only the summary.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only). Sequential-only, so not with `-j` or `--tmp-dir` |
| `-s, --osmium-summary` | Print osmium-style summary (`Summary: left=N right=N same=N different=N`) on stderr instead of the default pbfhogg-format summary. Both formats fire on stderr unless `-q` (text only; refused with `-q`) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`; refused with `-q` on a text diff) |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way. Requires `-o`: stdout is never replaced |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `-j, --jobs <N>` | Worker threads, and the number of ID-range shards (see [`-j/--jobs`](#-j--jobs)). `0` (default) uses one shard per core; `1` takes the sequential path, which for text writes no scratch. Applies to both text and `--format osc`; parallel sharding requires both inputs to be indexed. **Refused with `-v/--verbose`**, which is sequential-only. The parallel path writes its shard files to the run's scratch directory (planet scale: ~30 GB text, ~45 GB osc XML), removed on completion; a quiet text diff, which only counts, writes none. |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read. Default: the output's directory, or the current directory without `-o`. Refused with text `-j 1` and with `-v`, whose sequential path writes no scratch, and with `-q` on a text diff, which only counts (`--format osc` writes scratch on every path) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, roundtrips content - applying the derived OSC to the old PBF gives a file `diff` finds identical to the new PBF; metadata, which `diff` ignores, is outside that check (see [DEVIATIONS](../DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).

Inputs holding any negative ID always take the sequential block-pair path, whatever `-j` says: the parallel planner's shard boundaries are raw numeric comparisons, which do not order negative IDs the way the merge does.

**Input checks.** Both inputs must claim `Sort.Type_then_ID`. With both inputs indexed, every path - sequential or parallel, at any `-j` and `-t` - refuses the same structurally malformed files: a truncated or malformed frame anywhere in either file, a data blob without indexdata, and blobs out of canonical order as far as their indexdata ID ranges prove it (order inside a blob, and inside a range spanning negative and positive IDs, is trusted). The body of a blob of a kind `-t` excludes is never interpreted. Blob bodies of the selected kinds are decoded where the comparison needs them, and there the paths differ: the sequential path does not decode a byte-identical blob pair when it may skip equal blobs (`-c`, `--format osc`), and the parallel path trusts each blob's claimed last ID when it decides whether a block has elements left, where the sequential path checks the claim against the block. Without indexdata the comparison is sequential at every `-j` and decodes every block of both files to learn its kind: a block of an earlier kind after a later one, and elements going backwards within a selected kind, are refused as unsorted, while blocks of excluded kinds are classified and dropped without converting their elements, so faults only a conversion sees (a tag whose string index is out of range) are not refused there.

### digest

Print a digest of a PBF's element content. Two files the command accepts get the same digest exactly when `diff --suppress-common` reports them identical (barring a 128-bit hash collision), whatever their encoding: compression, blob boundaries, dense or plain nodes, string-table order, header fields and indexdata presence do not change it, and neither does metadata, which diff ignores. Element content is defined once, in `src/content.rs`, for both commands.

```
pbfhogg digest [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--direct-io` | Use O_DIRECT to bypass page cache |

Output, on stdout:

```
xxh128 v1 <32 hex digits>
node <count> <32 hex digits>
way <count> <32 hex digits>
relation <count> <32 hex digits>
```

The first line names the algorithm and the canonical-form version; the version changes whenever the content definition or its serialization does, so a stored digest is only comparable with one of the same version. The per-kind lines digest each kind alone, so a mismatch shows which kind differs.

The command refuses, exiting non-zero and printing no digest line, any input on which diff's answer is not a function of element content:

- no `OsmHeader` as the first blob, a second `OsmHeader`, or a blob of unknown type;
- a header that does not claim `Sort.Type_then_ID`;
- a data block that is mixed-kind (or classified as one kind while holding another), kinds out of node, way, relation order, or IDs that do not strictly ascend within a kind (duplicates included);
- indexdata on some data blobs and not others, or indexdata whose kind, minimum ID, maximum ID or count disagrees with its block, or on an empty block;
- a tag, role or metadata user name whose string-table index does not resolve.

A refusal is the safe answer for a cache keyed on the digest: fall back to the real comparison. The promise is "refuses input failing these checks", not "refuses every corrupt file": a file cut off exactly at a blob boundary is a valid shorter file.

### getid

Extract or remove elements by ID. By default, keeps only the listed IDs. With `--invert`, removes the listed IDs and keeps everything else.

Typed IDs accept `n123` or `node/123` (node), `w456` or `way/456` (way), and
`r789` or `relation/789` (relation). Bare numeric IDs are accepted only when
`--default-type` is supplied. Without it, a bare ID is an error. This also
applies to IDs in `--id-file` text input.

At least one ID source is required (positional IDs, `-i` or `-I`).
Positional IDs are parsed before the output is checked; the ID files are read
after it, so an existing output is refused before a full pass over an `-I`
PBF. Sources that turn out to hold no ID are refused once read.

```
pbfhogg getid [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--invert` | Invert selection: remove listed IDs instead of keeping them |
| `-r, --add-referenced` | Include referenced nodes of matching ways (two-pass; not with `--invert`) |
| `--remove-tags` | Remove tags from referenced objects. Requires `-r`, which is what emits them (so not with `--invert`). (`-t` used to be its short; `-t` now refuses here, naming `--remove-tags`.) |
| `-v, --verbose` | Print requested IDs, then report which were not found (not with `--invert`, and not with `-q`, which would drop both lists). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output. |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback (see [`--force`](#--force)) |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

`--verbose` is an exhaustive diagnostic with no output cap (`--verbose-ids`,
its former name, now refuses, naming `-v, --verbose`). Requested
IDs are space-separated on one line per kind; missing IDs are printed one
per line. With `-I`, every ID from that input participates, so large inputs
can produce very large stderr output. The missing-ID report also collects
three vectors, one per kind, requiring O(M) additional ID storage for M
missing objects. Omit `--verbose` when that output and storage are unwanted;
the normal command summary remains available.

### getparents

Find ways/relations referencing given IDs (reverse lookup).

ID syntax and the optional `--default-type` behavior are the same as for
`getid`: bare numeric IDs require an explicit default type.

```
pbfhogg getparents [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--add-self` | Also include the queried objects themselves in the output. (`-s` used to be its short; `-s` now refuses here, naming `--add-self`.) |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### add-locations-to-ways

Embed node coordinates in ways. Two index strategies:

- **sparse** (default) - Rank-indexed flat mmap array. Pre-allocates `referenced.total_count() * 8` bytes; workers store `(lat, lon)` at byte offset `IdSet::rank_if_set(node_id) << 3` via atomic stores. Working-set behavior depends on the available page cache; see [performance.md](performance.md) for measurements.
- **external** - Double radix permutation via 4-stage pipeline with bounded memory and high scratch-disk demand. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. See [performance.md](performance.md) for measured resource use.

`--index-type dense` was removed; see [performance.md](performance.md) for measured comparisons. `dense` survives as a hidden value (absent from `--help` and from completion) purely so passing it errors with a pointer to `sparse` rather than a bare "invalid value".

By default, untagged nodes not referenced by a relation are dropped from output.

Node blobs are found by their indexdata kind. `--index-type external` also
believes each node blob's elements are in order under the sorted header (a
blob out of order loses present locations), and drops a node blob whole on
its tagdata and ID range when removing untagged nodes. See
[indexdata-trust.md](indexdata-trust.md).

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (runtime routing; see [pipeline.md](pipeline.md#add-locations-to-ways)) |
| `--keep-untagged-nodes` | Keep all untagged nodes in output |
| `--inject-prepass` | Emit the opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata into the output header, readable back via `Blob::way_members` / `Way::shared_node_pins` |
| `--ignore-missing-nodes` | Accept ways whose node references are absent from the input, writing those vertices as `(0, 0)`. Without it the run fails and no output is written: `(0, 0)` is Null Island, a real coordinate, so a placeholder vertex cannot be told apart from genuine data by any reader. Matches osmium in name and in default. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower (sparse, decode-all) fallback. Not with `--index-type external` or `--inject-prepass`, which need indexdata whatever `--force` says; both are refused before any input is opened |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read, after the output is checked. Default: the output's directory |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### time-filter

Filter a history PBF to a snapshot at a given timestamp.

```
pbfhogg time-filter [OPTIONS] --output <OUTPUT> <FILE> <TIMESTAMP>
```

The timestamp can be UNIX seconds or RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).

The output's replication timestamp is the earlier of the input's and the
cutoff; the input's sequence number is kept only when the input's timestamp
is known and not after the cutoff, since it names the input's replication
state, which the snapshot no longer is (see [Output header](#output-header)).

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

---

### apply-changes

Apply an OSC diff to a sorted PBF file. Uses blob passthrough -- unmodified blobs are copied as raw bytes without decompression.

Which blobs are unmodified is decided from each blob's indexdata ID range,
and a passed-through blob's element count in the statistics is its indexdata
claim; neither is checked, so a base whose indexdata understates a blob's
range gets that blob copied past a change aimed at it. See
[indexdata-trust.md](indexdata-trust.md).

The output header declares no bbox (the OSC can place a node outside the
base's). Of the base's replication state it keeps the base URL and drops the
timestamp and sequence number, since the output is past them; set the new
state with `--output-header` (see [Output header](#output-header)).

```
pbfhogg apply-changes [OPTIONS] --output <OUTPUT> <BASE> <CHANGES>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--locations-on-ways` | Preserve way-node locations through the merge (requires base PBF with LocationsOnWays). Ways the OSC creates or modifies get locations from the merged nodes; base ways the OSC does not touch keep their existing locations, even when the OSC moves or deletes a node they reference. Rerun `add-locations-to-ways` on the output for a full refresh. See DEVIATIONS.md. |
| `--ignore-missing-nodes` | With `--locations-on-ways`, permit unresolved references to retain the legacy `(0,0)` fallback instead of failing |
| `-j, --jobs <N>` | Worker threads for the descriptor-first pipeline (see [`-j/--jobs`](#-j--jobs)). `0` (default) uses the `nproc - 2` heuristic (leaves two cores for scanner + drain, minimum 2). An explicit `-j 1` is refused before any input is opened (single-worker operation is not a validated configuration). |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on an unindexed base with the slower fallback. Not with `--locations-on-ways`, which needs an indexed base whatever `--force` says; the combination is refused before any input is opened |
| `--allow-version-regression` | Accept a change whose `version` is lower than the version already held for that object. Off by default: precedence is by version, not by position, so feeding diffs out of sequence order is an error naming both versions rather than a silent downgrade. An equal version is always accepted and the later change wins by position (overlapping diffs, regional-extract deletes at the held version). Deletes count -- a delete leaves a versioned tombstone, so a later stale create or modify is refused too. Elements claiming no `version` are unaffected. See DEVIATIONS.md. |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### merge-changes

Merge multiple OSC files into one OSC file.

```
pbfhogg merge-changes [OPTIONS] --output <OUTPUT> <CHANGES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--simplify` | Keep only the last change per object (type + id) |
| `--allow-version-regression` | Requires `--simplify`, which is the mode that resolves conflicts; passing it alone is a usage error. Accepts a later change whose `version` is lower than one seen earlier in input order. Off by default, so listing the inputs out of sequence order is an error rather than a silent downgrade. An equal version is always accepted and the later change wins. Default (non-simplify) mode preserves the full stream in input order and resolves nothing, so there is nothing for the flag to allow. See DEVIATIONS.md. |
| `-j, --jobs <N>` | Worker threads (see [`-j/--jobs`](#-j--jobs)). Sizes both the per-input parse fan-out (capped by input count) and the simplify path's chunk fan-out. |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`) unless `--allow-unindexed` is set. An existing output directory is refused before any work unless `--overwrite` is set, and even then it must hold a geocode index or be empty; any other existing path is refused because publication replaces the whole directory. The output path itself cannot be a symlink. On Linux, an existing mount-point output directory is refused before the build.

The node phase schedules blobs on their indexdata kind, so a node in a blob
labelled way or relation is never indexed; a node above the largest claimed
node `max_id` is refused. See [indexdata-trust.md](indexdata-trust.md).

```
pbfhogg build-geocode-index [OPTIONS] --output-dir <DIR> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output-dir <DIR>` | Output directory for index files |
| `--street-level <N>` | S2 cell level for streets/addresses; must exceed `--coarse-level`, within the builder's segment-cover limit. Default comes from the builder configuration |
| `--coarse-level <N>` | Fallback cell level for rural areas; must be below `--street-level`, within the builder's segment-cover limit. Default comes from the builder configuration |
| `--admin-level <N>` | S2 cell level for admin boundaries, within the builder's work limit. Default comes from the builder configuration |
| `--max-admin-vertices <N>` | Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value. Default comes from the builder configuration |
| `--search-radius <M>` | Fine-level max search distance in meters. Default comes from the builder configuration |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters. Default comes from the builder configuration |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `-O, --overwrite` | Replace an existing index (or empty directory) at `--output-dir` after the new build succeeds. Without it an existing `--output-dir` is refused before any work. Does not bypass the indexdata requirement |
| `--allow-unindexed` | Accept input without blob-level indexdata (slower fallback path). Does not replace an existing index |
| `--force` | Legacy shorthand equivalent to `--overwrite --allow-unindexed`. Every use warns that it also grants overwrite, naming the two flags to spell instead |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read. Default: the directory containing `--output-dir` |

A node whose coordinates fall outside the geographic domain fails the whole
build. Incomplete extracts remain usable: missing way nodes and missing admin
member ways contribute no geometry; admin boundary candidates with a missing,
unparseable, or unsupported level or no name are skipped; relation members
whose roles do not describe outer or inner rings are ignored (an empty role
counts as outer). The PBF replication sequence is stored as a u32 in the
geocode header. A missing value is stored as zero, and a present value outside
the u32 range fails the build rather than being wrapped to a different source
version.

### S2 level bounds

All three cell levels are validated before any input is read, and a violation
exits 2 with an explicit message:

- Street and coarse levels must remain within the builder's segment-cover
  budget. Cell covering samples each way segment at a fraction of the cell
  width, so the sample count doubles at each level. The builder's rejection
  message reports its current upper bound and explains the budget.
- Admin level must remain within the builder's polygon flood-fill budget.
  The number of cells considered grows by a factor of four at each level. The
  builder's rejection message reports its current upper bound and explains the
  budget.
- `--coarse-level` must be strictly less than `--street-level`. The coarse
  index is the fallback for rural queries and must be the coarser of the two.

The *reader* is deliberately looser and accepts levels up to the S2 limit of
30, since reading does no cell covering: an index written at a higher level by
another producer stays readable.

### Overwriting an existing index

Every build writes a complete generation in a sibling staging directory and
publishes it only after the build and smoke test succeed; `--overwrite` (or
legacy `--force`) lets that publication replace an existing index. Failed
builds leave the existing index intact. Publication moves the old directory aside, moves
the new one into place, then removes the old files. Already-open readers keep
their mapped inodes and remain usable. The entire output directory is replaced;
use a dedicated directory for the index. Allow disk space for both generations.

Publication uses two renames, so it is not crash-atomic and there is a short
pathname gap. Serialize publication with new reader opens and other builds;
queries on readers already open need no coordination. A publication killed
partway (SIGINT and SIGTERM do not stop one that has started) may leave the old
index in a `.pbfhogg-backup-*` directory beside the output directory; a
publication that fails and cannot put the old index back says so and names that
directory. SIGKILL, power loss or a second SIGINT/SIGTERM during a build can also leave an
incomplete `.pbfhogg-tmp-*` staging directory beside it because process cleanup
does not run. The next command that writes in that directory reports both
(see Scratch files) but never removes them: a directory may belong to a
concurrent build, and a backup may hold the only recoverable prior index. After
confirming no builder is active, inspect abandoned staging directories and
preserve or restore any backed-up index before removing scratch. A mount table change after the Linux preflight can
still make the final rename fail; on other platforms mount points are not
preflighted and the final rename may fail after the build.
External writers must also replace files rather than modify mapped inodes:
reader open-time string validation cannot protect against truncation, which
can fault before a string check can return an error.

A build that finds nothing queryable at all - no address points, no streets, no
interpolation ways and no admin polygons - fails rather than publishing an empty
index. Indexes containing only some of those kinds, such as an admin-boundaries
extract, are legitimate and still build.

Outputs a set of binary files. See [performance.md](performance.md) for measured build times, index size, and peak memory.

---

## I/O capability matrix

Which commands support `--direct-io` (O_DIRECT bypass of page cache) and `--io-uring`
(io_uring async writes), and which paths are affected.

| Command | Reads PBF | Writes PBF | `--direct-io` | `--io-uring` | Notes |
|---------|:---------:|:----------:|:-------------:|:------------:|-------|
| inspect | Yes | - | Yes | - | Read-only |
| inspect tags | Yes | - | Yes | - | Read-only |
| inspect --nodes | Yes | - | Yes | - | Read-only |
| check --ids | Yes | - | Yes | - | Read-only |
| check --refs | Yes | - | Yes | - | Read-only |
| check --index | Yes | - | Yes | - | Read-only; decompresses every blob |
| cat | Yes | Yes | Yes (R+W) | - | Passthrough + filtered paths |
| cat --dedupe | Yes | Yes | Yes (R+W) | Yes | Via merge-pbf path |
| sort | Yes | Yes | Yes (R+W) | Yes | |
| repack | Yes | Yes | Yes (R+W) | Yes | Re-encode at configurable elements-per-blob cap |
| degrade | Yes | Yes | Yes (R+W) | Yes | Adversarial PBF generator (--unsort / --strip-locations / --strip-indexdata / --drop-ids) |
| renumber | Yes | Yes | Yes (R+W) | - | |
| extract | Yes | Yes | Yes (R+W) | - | All strategies |
| tags-filter | Yes | Yes | Yes (R+W) | - | Both single-pass and two-pass |
| export | Yes | No | No | - | GeoJSON output to stdout or a regular file |
| getid | Yes | Yes | Yes (R+W) | - | Including --add-referenced |
| getid --invert | Yes | Yes | Yes (R+W) | - | |
| getparents | Yes | Yes | Yes (R+W) | - | |
| time-filter | Yes | Yes | Yes (R+W) | - | |
| add-locations-to-ways | Yes | Yes | Yes (R+W) | - | All index types |
| apply-changes | Yes | Yes | Yes (R+W) | Yes | Production merge path |
| diff | Yes | - | Yes | - | Read-only (text output) |
| diff --format osc | Yes | - | Yes | - | Read-only (OSC XML output) |
| derive-changes (via `diff --format osc`) | Yes | - | Yes | - | Read-only (OSC XML output) |
| build-geocode-index | Yes | - | Yes | - | Binary index output, not PBF |
| merge-changes | - | - | - | - | OSC-only, no PBF I/O |

**R+W** = `--direct-io` affects both PBF reads (via `BlobReader`) and PBF writes (via `PbfWriter`).
**`--io-uring`** requires the `linux-io-uring` compile-time feature and sufficient `RLIMIT_MEMLOCK`.
