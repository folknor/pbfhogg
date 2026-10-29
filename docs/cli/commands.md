# pbfhogg CLI Reference

Every command, flag and output contract of the `pbfhogg` binary, as it
stands on the main branch (unreleased changes included; `CHANGELOG.md` says
which release brought what). This page is written by hand, not generated:
`pbfhogg <command> --help` gives the short form of each flag, and a test
(`cli/src/lib.rs`, `doc_coverage_tests`) fails when a visible command,
flag or enumerated value of the CLI has no place here.

Every enumerated flag value (`--compression`, `--type`, `--index-type`,
`--clean`, `inspect tags --sort`, the `bench-*` modes) is parsed once at
the argument boundary, so a malformed one is a usage error before any input
is opened. `--help` lists the accepted values of the fixed sets (such as
`--index-type`, `--clean`, `--sort` and the `bench-*` modes);
the `--compression` and `--type` grammars are spelled out below.

`--type` is one comma-list grammar (`node,way,relation`, any subset) with a
single parser on every command that takes it. A command that acts on one
kind (`inspect tags`) refuses a list of more than one, and `export` refuses
`relation` (it accepts `node`, `way` or `node,way`); each refusal names what
the command accepts.

## Global flags

| Flag | Description |
|------|-------------|
| `-h, --help` | Print help, for `pbfhogg` or for the command it follows |
| `-V, --version` | Print the version (`pbfhogg` only) |
| `-q, --quiet` | The exit code is the answer (see [Quiet](#quiet)). Accepted before or after the subcommand: `pbfhogg -q diff a b` and `pbfhogg diff a b -q` are the same |

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

## Indexdata is believed

Commands that act on blob-level indexdata or tagdata, or on the header's
`Sort.Type_then_ID` flag, believe them; a few checks happen in passing where
a command decodes anyway, but none is a guarantee. A file whose claims are
false can get a wrong answer that reports success. `check --index --ids`
verifies a file of unknown provenance, except for element-kind order on a
file without indexdata and the header's feature flags. Which command
believes which claim, and what a false one costs, is in the
[indexdata trust model](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

## Common flags

These flags appear on most commands that produce PBF output:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--compression <COMPRESSION>` | Compression of the blobs the command encodes: `none`, `zlib` (level 6), `zstd` (level 3), or with a level, `zlib:0` to `zlib:9`, `zstd:-7` to `zstd:22`. The default is `zlib` everywhere except `renumber`, which defaults to `zlib:1` and also writes level 1 for an explicit `zlib` or `zlib:6` (see its section). Blobs a command passes through (plain `cat`, the blobs `apply-changes` leaves alone) keep the compression they had; `repack` re-encodes every blob |
| `--direct-io` | Use O_DIRECT to bypass page cache (requires `linux-direct-io` feature) |
| `--force` | On a command that would refuse unindexed input: proceed with the slower fallback (see [Proceeding without indexdata](#proceeding-without-indexdata)) |
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
  refusal**: an unindexed input taking diff's sequential path, or one with
  negative IDs its single-shard one, `-j 0` resolving to one worker on a
  one-core host. The same
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

## Worker threads

`-j, --jobs <N>` has one rule on every command that has it: `0` (the default) sizes the worker
pools automatically, `1` runs sequentially, `N` caps the worker pools at `N`
threads. "Automatically" is each command's own sizing (a reserve of cores for
its scanner, drain or writer threads, a minimum, a fallback when the host
cannot say how many cores it has); `-j 0` and an omitted `-j` are the same.
An explicit `-j` (any value, `0` included) is refused where it can have no
effect: `tags-filter -R` and OSC-mode tags-filter, `inspect` outside
`--nodes`. One exception to the rule:

- **diff**: `N` is also the number of ID-range shards the block-pair merge
  partitions the inputs into. Sharding needs both inputs indexed and free of
  negative IDs; otherwise the run is one shard (negative IDs) or the
  sequential element stream (no indexdata), with a `Note:`.

`apply-changes -j 1` runs one worker beside the scanner and drain threads;
its automatic sizing still starts at two.

The other commands that run worker pools (sort, cat, repack, degrade,
renumber, extract, add-locations-to-ways, getid, getparents, time-filter,
check, export, digest, build-geocode-index) size them themselves and have no
`-j`.

## Proceeding without indexdata

**With `--force`, a command that would refuse unindexed input proceeds with
the slower fallback.** Only commands that refuse unindexed input have the flag, and each
refuses through the same gate: a typed missing-indexdata error naming the
input, which the CLI follows with `Generate an indexed PBF first ...` (the
library's text names no command or flag). The CLI then adds `Or pass --force
to proceed anyway.` only where
`--force` is accepted: modes that need indexdata whatever `--force` says get
no such line, and `build-geocode-index` names `--allow-unindexed` instead.

| Command | Refuses unindexed input on | `--force` refused with |
|---------|----------------------------|------------------------|
| cat | `--type` (with or without `--clean`), `--dedupe` | plain `cat`, which adds the indexdata, `--reindex`, which rescans every blob whatever it carries, and `--clean` without `--type`, which decodes every blob exactly once either way. `--type node,way,relation --clean` is the clean pass and does not refuse; `--force` alongside it is accepted and does nothing |
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
input, so they have no `--force` either (`diff` prints a `Note:` when an
unindexed input sends it to its sequential path).

`build-geocode-index` is the exception: its indexdata switch is
`--allow-unindexed`, and its `--force` is a legacy shorthand for both
`--overwrite` and `--allow-unindexed`. It keeps working, and warns every time
that it also grants overwrite, naming the two flags to spell instead.

This section is about absent indexdata. What commands believe when it is
present, and what a partially indexed file meets, is in
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

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
| `apply-changes` | sorted | `LocationsOnWays` with `--locations-on-ways`, else none |

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
  (`.pbfhogg-tmp-<pid>-<n>.<name>`), or in `--staging-dir` when given (see
  [Staging directory](#staging-directory)), syncs it and renames it into place
  after its final flush; a run that fails before that removes it, so it leaves
  no file at the output path and leaves an existing output untouched. So does
  a run interrupted by SIGINT or SIGTERM (see [Interruption](#interruption)). A
  run killed outright (SIGKILL, the OOM killer, power loss, a second
  interrupt) can leave the hidden staging file behind, never a partial file
  under the output name. Killed during publication, it can also leave
  `.pbfhogg-backup-*` entries beside the outputs (see
  [Names pbfhogg leaves behind](#names-pbfhogg-leaves-behind)), and an output
  that was being replaced through a backup (a geocode index, an
  `extract --config` output) may be missing, its previous version in the
  backup. The
  rename itself is the last step, and the few failures possible after it are
  reported as such: a directory sync that fails after the output was put in
  place is an error that says the output *was* published (naming any backup
  of a replaced output, which is then kept), and a group whose rollback
  cannot complete names what is left where. The output's directory must
  exist and be writable, which is checked before any work.
- Without `--overwrite` publication is exclusive: an output that appears
  while the command runs is not replaced either. On a filesystem with
  neither an exclusive rename nor hard links, such a run fails at the end
  and says so; `--overwrite` publishes there.
- Several outputs (`extract --config`) are published together: if one cannot
  be put in place, the ones already published are rolled back. If a rollback
  step itself fails, the previous output is kept in a `.pbfhogg-backup-*`
  directory beside it and the error names it.

`-o /dev/null` is not an output: it is refused as not a regular file.

### Staging directory

`--staging-dir <DIR>`, on every command that writes an output, puts each
output's staging entry (the staging file, or the geocode index's staging
directory) in `DIR` instead of beside the output, under the same
`.pbfhogg-tmp-<pid>-<n>.<name>` name. Publication still renames it into
place, so `DIR` is checked before any work and refused unless:

- it exists and is a directory;
- it is on the same mount as every output's directory: the same device, and
  a mount the kernel does not refuse a rename across. A bind mount of the
  same device is a different mount, across which a rename fails, and is
  refused on every Linux kernel whose answer settles it (see below),
  including those that report no `statx` mount id (before 5.8);
- it is not, and does not contain, any output's directory (a directory below
  an output's directory is fine): a caller removes its staging directory
  whole, and a backup of a replaced output lands beside the output;
- for `build-geocode-index`, it does not lie inside `--output-dir`, which
  publication replaces.

For `extract --config` it is checked against every output the config names
before any output is reserved. A refusal names the staging directory and the
output, exit `2`. The mount check detects a refusal rather than promising the
rename. It asks the kernel with a rename that cannot move anything, and an
answer that settles nothing (a lookup error such as `EACCES`, or a sandbox
such as a seccomp filter refusing the call) lets the run go ahead rather than
refusing a directory that may be fine. A refusal it cannot foresee, or that
went unanswered (a filesystem that refuses the rename itself, a mount table
changed while the run worked), is met at the final rename, and is
reported as the same staging-directory refusal with the kernel's error
appended, after the run's outputs were rolled back as for any failed
publication. `--staging-dir` needs an output: with `export` or a text
`diff` writing to stdout it is refused as a usage error, exit `2`.
Publication syncs both directories a rename changes, the staging side as well
as the output's directory. Library callers get the same placement and checks
from `Output::with_staging`, `OutputDir::with_staging`,
`OutputSet::files_with_staging` and
`geocode_index::builder::index_output_with_staging` with `Staging::Dir`,
whose refusals are `OutputError::StagingDir` with a `StagingProblem`. A
library caller may also pass `Staging::DirWithBackups`, which the CLI never
uses: backups of replaced outputs then go in the staging directory too, so
nothing but the outputs appears beside them, and removing that directory
removes any previous output a publication kept there. The same checks
apply, and a move into such a backup refused across mounts is the same
staging-directory refusal.

## Scratch files

`add-locations-to-ways`, `diff` (text other than `-j 1` or `--quiet`, and `--format osc`) and `build-geocode-index` write
scratch: disposable files that can be very large (hundreds of GB for `add-locations-to-ways
--index-type external` at planet scale). Each run makes **one** hidden directory,
`.pbfhogg-run-<pid>-<random>`, holds everything it writes in there and removes it when it ends,
whether it succeeded, failed or was interrupted. If the directory cannot be removed the run
says where the leftover is. The directory is made at startup, before any input is opened, by
every run that may need scratch. A run whose command line guarantees it writes none (text
`diff -j 1`, and a quiet text `diff`, which only counts and shards without writing any text)
makes no directory and needs no writable location. `-v` does not suppress the directory: a
verbose text diff shards like any other. A text diff with automatic jobs makes its directory
even when the run turns out to need one shard and writes nothing there, since the shard count
depends on inputs it has not opened yet.

- `--tmp-dir <DIR>` chooses the directory the run directory is made in. It must exist and be
  writable, and it is checked before any input is opened, so a bad value fails at once. Without
  it the scratch goes in the output's directory (the current directory for `diff` text to
  stdout, and for an output name with no directory part). `build-geocode-index` refuses a `--tmp-dir` inside the index
  directory it replaces.
- `--tmp-dir` never moves an output's staging entry (`.pbfhogg-tmp-*`), which stays beside the
  output unless `--staging-dir` moves it, nor a replaced output's backup (`.pbfhogg-backup-*`),
  which always stays beside the output: publication is a rename, which needs the same mount.

**Leftovers of runs that never finished** (SIGKILL, power loss, the OOM killer, a second
SIGINT or SIGTERM) are reported,
never removed. Before it starts, a command that writes an output scans the output's directory
(and `--tmp-dir` and `--staging-dir`, when given) and prints one `Warning:` per entry it finds: an unfinished run
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
prints a `Warning:` naming it. If a directory the replacement's publication changed cannot be synced, the backup is
kept rather than removed, and the error names it. Library callers get the same scan as typed values from
`pbfhogg::scratch::find_stale` and `pbfhogg::scratch::find_stale_in_index`.

### Names pbfhogg leaves behind

**A caller that kills pbfhogg children should not match these names at all.** The supported way
to own cleanup is to give each run directories of its own. When both `--staging-dir` and
`--tmp-dir` are given, pbfhogg creates every staging entry under `--staging-dir` and all run
scratch under `--tmp-dir`. They may be one directory: staging entries and run directories have
distinct names, and a directory named by both is scanned once. `--tmp-dir` exists only on the
commands that make run scratch (`add-locations-to-ways`, `diff`, `build-geocode-index`); every
other command makes none, and `--staging-dir` alone holds everything disposable it leaves.
After every process using those directories has stopped, the caller
may remove them, provided they are private to the run, dedicated to its disposable state, and
hold no outputs or other data it means to keep (pbfhogg refuses a `--staging-dir` that is or
contains an output's directory; `--tmp-dir` is not checked that way). Publication may also leave
backup entries (`.pbfhogg-backup-*`) beside the outputs. They are excluded from automatic
cleanup: one can hold the only copy of a replaced output, though one left by an interrupted
publication may also be empty, or hold only part of the previous output when the run was killed
while removing it after the new one was in place. The run's diagnostics name them when they get to, and every later
scan of that directory reports them. Published outputs stay at the paths that were asked for,
including ones a killed group publication left installed: publishing several outputs is not
all-or-nothing across a process death. Stale staging therefore never reaches the next run's
directories, so no run has to decide whether to overwrite or refuse it.

"Every process using those directories" is the pbfhogg process alone. The `pbfhogg` CLI starts no
other process; its threads end when it exits, so once it has exited no process or thread it
started can still use its staging or scratch directories. It never closes or replaces a
descriptor it inherited, nor changes that descriptor's descriptor or file-status flags, so an
extra descriptor the caller passes it (for example one on which the caller holds a lock as a
lease for those directories) stays open until the pbfhogg process exits. This is about processes
and descriptors, not kernel I/O: the kernel may finish asynchronous cleanup after the process is
gone. The CLI test suite runs every pbfhogg child it starts under a seccomp filter that kills it
on any breach of this, for the whole run (`reference/testing.md` says what that filter does and
does not see).

The spellings below are the contract for an external tool that cannot do that (a cleanup job
sweeping directories it did not hand out). `<pid>` is the pid of the pbfhogg process, `<seq>` a
counter within it, `<dest name>` the last component of the output path (the last 200 bytes of
it, when longer).

| Name | What it is | An external tool may delete it |
|------|-----------|--------------------------------|
| `.pbfhogg-tmp-<pid>-<seq>.<dest name>` | Staging for an output, beside the destination or in `--staging-dir`: a file for file outputs, a directory (holding `new/`) for the geocode index. Never a file under the output name. | When no pbfhogg process is live that could own it (check `<pid>`; it is a hint only) |
| `.pbfhogg-backup-<pid>-<seq>.<dest name>` | A directory holding the previous output, kept while a replacement is published, and kept for good when a rollback could not restore it, when syncing a directory the replacement's publication changed failed (the error names it), when removing it after a successful replacement failed, or when something pbfhogg never deletes turned up inside it (a warning names it in the last two cases). | **Never without a human.** It can hold the only copy of the previous output |
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
| `-j, --jobs <N>` | Worker threads for `--nodes` (see [Worker threads](#worker-threads)). **Requires `--nodes`**: no other inspect mode has a worker pool, so passing it elsewhere is an error rather than a no-op. |
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

The ordering analysis splits the data blobs into segments, runs of
consecutive blobs of one kind (nodes, ways, relations, or mixed for a
decoded blob holding more than one kind). The order is standard when there
is at most one segment of each kind, in that order, and none mixed. The
report counts every segment but lists only the first 64 with their block
ranges (`ordering.segment_count` and `ordering.segments_head` in `--json`),
so a file whose kind changes every blob costs no per-blob memory; the
segment where a file first leaves standard order is always among those
listed.

Text taken from the file prints as a quoted literal, escaped exactly as
[`diff -v`](#diff) escapes keys, values and roles (a complete double-quoted
string that is also a valid JSON string), so a newline, tab or quote in the
text can never split a line or forge another one. In the report that is the
file name (`File:     "name" (size)`), the writing program
(`Program:  "..."`), each header feature (`Features: "a", "b"`) and the
replication URL on the `Repl:` line. `--show` prints tags as
`    "key"="value"`, the user as `  user: "name"`, and each member as
`    type/id "role"` (an unknown member type as `unknown(raw)/id`); a user or
role whose string index is invalid prints a bare `<invalid>`, which a real
string, always quoted, cannot be confused with. `--get` is the exception: it
prints the raw value followed by one newline, unescaped, so a value holding a
newline spans lines; use `--json` to read such a value losslessly. The file
name is the path's last component as UTF-8, with invalid bytes replaced, in
every form.

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

```
pbfhogg inspect tags [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `--sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc. `count` and `name` are accepted as aliases for `count-desc` and `name-asc`. (`-s` used to be its short; `-s` now refuses here, naming `--sort`.) |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Count one element type: exactly one of node, way, or relation (a list is refused) |
| `-j, --jobs <N>` | Worker threads (see [Worker threads](#worker-threads)) |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback (see [Proceeding without indexdata](#proceeding-without-indexdata)) |

Each output row on stdout is three tab-separated fields: the count, then the
key and the value, each a double-quoted literal escaped as in
[`diff -v`](#diff) (a valid JSON string), for example
`120`, TAB, `"highway"`, TAB, `"primary"`. A tab or newline inside a key or value prints
escaped, so splitting a row on tabs always yields three fields and each
quoted field JSON-decodes to the original text.

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

Validate PBF file integrity (IDs + referential integrity; opt-in blob metadata and inline way locations).

`check` verifies a file: its contents against its claims and against the
format's invariants, exiting 1 on a violation. Describing a file - what it
claims, cheaply - is [`inspect`](#inspect)'s job.

Validate PBF integrity along four independent axes, selected by flag. With no
flag, the ID and referential integrity checks both run.

`--index` and `--locations` are selectors like the other two but are
deliberately **not** in the default set: each decompresses every blob in the
file (`--locations` the way blobs once per pass), and folding that into a
bare `check` would multiply the cost of the command everyone already runs;
`--locations` also applies only to LocationsOnWays files. Passing either
suppresses the default pair; combine selectors to run more than one.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--index` | Verify blob-level metadata against the blobs. Decompresses every blob |
| `--locations` | Verify a LocationsOnWays file's inline way-node coordinates against its node records |
| `--agreement` | Also require every inline coordinate of a node the file does not contain to agree across the ways referencing it. Requires `--locations` |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Retain at most N violation details per check (0 = unlimited); scanning and total counts continue [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |
| `--json` | Machine-readable JSON output |
| `--direct-io` | Use O_DIRECT to bypass page cache where the check supports it: the ID check's front read and `--index`. It has no effect on `--refs` and `--locations`, whose parallel readers pread unaligned blob ranges, and each says so on stderr when it runs |

With the global [`-q, --quiet`](#quiet) `check` prints no report: the exit
code is the answer. `--json`, `--show-ids` and `--max-errors`, which only
shape the report, are refused with it.

`--full` and `-t/--type` apply to the ID check and are rejected under
`--refs` alone; `--check-relations` and `--show-ids` apply to the ref
check and are rejected under `--ids` alone; `--agreement` is rejected
without `--locations`. Violations exit `1`, not `2` - see
[Exit codes](#exit-codes).

**A false kind claim stops the check (exit 1).** `--refs`, `--ids` and
`--locations` read each blob in the pass of the kind its indexdata claims
(`--locations` only for blobs claiming ways), so a blob holding an element
of another kind is one the pass that needs it never reads. When a pass that
does decode such a blob meets the foreign element, that check stops: its
report is the one line `<Check> stopped: blob N at byte offset M claims X
but holds Y elements; its claimed-kind partition is invalid`, with no counts
and no other findings (they may already be false: a node hidden in a blob
claiming relations makes a way's reference to it look missing before the
relation pass finds the lie), its JSON section is `{"complete": false,
"passed": false, "kind_contradiction": {"claimed", "found", "blob",
"offset"}}`, and `check` exits `1`, the same verdict `--index` gives the
file. Other selected checks still run and print. A `Note:` names `check
--index` and [`cat --reindex`](#cat), the repair. The test is for a foreign
kind only, decided on the decoded elements (an empty dense-node message is
no node): an empty blob under any claim passes it. What no pass decodes is
not seen: without `--check-relations` the ref check never reads a blob
claiming relations, and `--ids -t` reads only the selected kinds' blobs;
`--index` verifies every claim. When a file has two lying blobs, which one
is named can vary from run to run.

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

The ID check judges monotonicity by canonical OSM order - `0`, then the
negative IDs by ascending absolute value, then the positives - which is the
order `sort` produces. On a positive-only file (every production PBF) that is
plain `i64` order; on a file carrying negative IDs the two differ, and the
canonical one is the definition.

#### Index verification

`check --index` is the metadata trust boundary. `BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox) and
`BlobHeader.tagdata` (the blob's tag key set) are claims every fast path in this
crate believes without opening the blob; who believes which, and what a false
one costs, is in [indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md). Nothing in the
format signs any of it, and `degrade --corrupt-indexdata` /
`--corrupt-indexdata-kind` / `--corrupt-tagdata` exist precisely to manufacture
files whose hints are well-formed and false.

`check --index` does not verify order: a file whose header claims
`Sort.Type_then_ID` with a blob internally out of order passes it. Combine it
with `--ids`, which checks ID and type order.

`check --index` is the other side of that boundary. It decompresses every
OSMData blob, re-derives the metadata from the payload with `try_scan_block_ids_checked`
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
| bbox | stored must **contain** the blob's coordinates | the scan rounds outward through `granularity`/`lat_offset`/`lon_offset`, so a stored box narrower than the scan is the only shape that can make a spatial filter skip a blob it should have read. A wider box is sound; an absent box is no claim. A node coordinate outside the `i32` decimicrodegree range lies outside every box, so a stored box over it is reported |
| tag key set | stored must contain every key the blob carries | an omitted key makes `tags-filter` skip a blob holding matches. A key listed but not present is sound - it only costs a decode - and is reported as a separate, distinct violation |

Exit `1` means mismatches were found, exit `2` that the check could not run.

#### Inline location verification

A `LocationsOnWays` header claims every way carries, per node reference, that
node's location. `check --locations` compares each inline coordinate with the
same file's record of that node. It exists because nothing else answers
whether such a file is right - `inspect --locations` only counts ways with and
without coordinates - and because it is an oracle independent of osmium for
any producer of such files, `add-locations-to-ways` and `apply-changes
--locations-on-ways` included.

**Violations (exit 1).** A reference whose inline coordinate differs from its
node's record; a way with references but no coordinate arrays; an inline
coordinate outside the geographic domain, judged on the exact nanodegree value
(osmium's undefined location, `2147483647` on both axes, is one); a node whose
valid records disagree with each other; a node record outside the domain
(each record counted once). References to a node with conflicting or
out-of-domain records are counted and not compared, since the node already
carries its own violation. With `--agreement`, an absent node whose references
carry two or more different coordinates.

**Counted, not failed.** A reference to a node the file has no record of.
`add-locations-to-ways` keeps a node only if it is tagged, a relation member,
or `--keep-untagged-nodes` is given, so on its default output most references
have no record. Nothing in `LocationsOnWays` requires the record, so its
absence is not a violation; the report counts such references, and those of
them at exactly `0,0`, the placeholder `--ignore-missing-nodes` writes. The
count is of references, not distinct nodes: a distinct count would need
another ID set that grows after the store is sized. `--agreement` checks the
one thing the file can still show about those nodes, that all their inline
copies agree, and reports the distinct number of absent nodes. That is
consistency, not freshness.

Every reference is counted once, in one of: verified, mismatched, not
compared (a node with one valid record, referenced by a way without
coordinates or with an out-of-domain one, each reported as such), to a node
with conflicting records, to a node with an out-of-domain record, and
without a node record. The six sum to the reference count.

**Refused (exit 2).** A header without `LocationsOnWays` (no claim to check);
a header declaring `HistoricalInformation` (several versions of a node
legitimately disagree); a way referencing a negative node ID (ADR-0002: the
store is keyed through an `IdSet`, and a skipped comparison cannot be
certified); a partially indexed file (a blob whose indexdata cannot be read,
a reversed range included, counts as unindexed, so among indexed blobs it
makes the file partially indexed); any data blob the reader refuses to build
a block from - every blob is validated once with the reader's own
construction, which checks group and element framing, tag, `Info` and
`DenseInfo` arrays, way geometry and relation member arrays and string table
UTF-8, but not what the reader only finds when it resolves strings (a tag's
string-table index past the end of the table passes). A blob whose
indexdata claims ways while it holds elements of another kind is not a
refusal: the check reads such a blob only in its way passes, so it stops
there and fails the file (exit 1), as described under
[`check`](#check) above.

**Comparison.** Each coordinate is `offset + granularity * raw` nanodegrees,
computed in 128-bit arithmetic (a product that overflows `i64` can be brought
back into range by the offset), judged against the domain exactly, then
truncated toward zero to decimicrodegrees as the reader's `decimicro_lat`
does. Inline and node values are compared there: values equal to the
decimicrodegree are equal, which only blocks with a granularity below 100 or
an offset that is not a multiple of 100 can distinguish further.

**Mechanism and scale.** An up-front pass decodes every blob that does not
claim ways (node, relation and unindexed blobs) once: it validates it, counts
node records and invalid ones, inserts their IDs into the key set (the node
IDs that get a slot; with `--agreement`, a further pass over the way blobs
adds every referenced ID), and records which element kinds and which node-ID
range the blob actually holds. Later passes route those blobs on what this
pass saw, never on their indexdata, so after it an unindexed file costs the
same passes as an indexed one; the up-front pass itself reads an unindexed
file's way blobs too, which an indexed file's it skips. The key set is built by the workers directly, sharded by
`IdSet` chunk, so no per-blob ID vector waits in a queue. It is then ranked
and its rank space cut into partitions whose store fits the budget;
partition `k` owns the node IDs of a contiguous ID interval, the intervals
cover all of `i64`, and each ID belongs to exactly one. Per partition, the
blobs holding nodes in the interval are decoded and their records joined into
the store (a join over `empty < coordinate < conflicting < out of domain`, so
the result is independent of worker order), then every blob holding ways is
decoded and each reference the partition owns is compared, streaming one
reference at a time and scaling a coordinate only for an owned reference.
The first partition also makes the partition-independent checks (missing
arrays, out-of-domain inline values, negative references) and validates the
blobs claiming ways, which the up-front pass did not read. Counts come from
a scan of the final slot states, never from which worker won a race, and the
retained details are the smallest by file position or node ID, so neither
depends on timing or on the number of partitions.

**Memory.** The store costs 8 bytes per slot and two bits for the record and
mismatch flags. Its budget is 60 % of the memory the host reported available
before the run read any blob, less 2 GiB of headroom and an upper bound on
the key set and its rank index (`IdSet::ranked_bytes_upper_bound`: 4 MiB
and its block prefixes per allocated chunk, plus the chunk directory and its
rank vectors); 4 GiB
when the host reports none; never below 64 MiB (`CHECK_LOCATIONS_*` in
`src/tunables.rs`). It is read before the passes because a host that charges
page cache to the process (a cgroup v2 limit) reports far less available
once the file has streamed through; for the same reason `check` runs
`--locations` before any other selected check (the report order is
unchanged). More than 16 planned partitions draw a
warning naming the count and the budget. The budget bounds the store only:
the key set (about 1.7 GB over a planet's node ID range) follows the number
of distinct IDs; each worker holds its decompressed blob, a copy for
validation and a fixed ID batch; and retained violation details are at most
`--max-errors` per blob in flight and in total, but unbounded with
`--max-errors 0`. No scratch disk is used. A planet that keeps every node has
about 10 billion slots, about 85 GB of store: on a host with 28 GB free that
is several partitions, so the way blobs are read several times and, the
planet being sorted, each node blob about twice (in an unsorted file a node
blob whose IDs are spread out can meet every partition). A default `add-locations-to-ways` planet keeps a few
percent of its nodes and fits one partition; with `--agreement` it needs as
many as a complete one. The `partitions` and `blob_decodes` fields of the
JSON report are these cost figures; they follow the host, not the file.

The ref check refuses (exit 2) a file holding a negative ID it would compare
(ADR-0002): a node, a way under `--check-relations` (the only mode that
tracks way IDs), a relation, or any checked reference. Under
`--check-relations` it also
refuses a relation member of an unknown type, which no set can resolve. A
positive ID above the range it sized from the file's own indexdata is still
tracked, inserted from each phase's merge. `--ids --full` reports a negative
ID as an `id_out_of_range` violation instead. Missing references are counted
into one ID set per category, so their memory follows the range of the
missing IDs rather than the number of references. Retained `--show-ids`
details (all of them under `--max-errors 0`) and the deferred
relation-to-relation members still cost memory per occurrence.

---

### cat

Concatenate PBF files with optional type filtering. Embeds blob-level indexdata and tagdata automatically.

With `--dedupe`, merges multiple sorted PBF files with blob-level passthrough and exact-duplicate deduplication.

A blob that already carries indexdata is passed through with it, unchecked,
so plain `cat` cannot repair false indexdata; `--reindex` (below) does.
`--type` copies a blob raw on its indexdata kind and `--dedupe` passes blobs
through on their kind and range, both believed without a decode. See
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

**`--reindex`: the repair for false indexdata.** Every data blob is
decompressed and scanned, and its `BlobHeader` is rewritten with every
`indexdata` (field 2) and `tagdata` (field 4) occurrence replaced by one
derived from the payload; the compressed payload and every other header
field (the `WayMembers-v1` field 5 and unknown fields included) are copied
byte for byte, the file header's `HeaderBlock` is carried through unchanged
(its frame is written afresh under `--compression`, so a field of the
header blob's own `BlobHeader` or `Blob` envelope is not kept), and any
other blob is copied as it is. The summary counts the blobs written
and how many carried indexdata that was missing or disagreed with the
payload. A block no single index can describe (no elements, a group carrying
more than one element field, or kinds mixed across groups) is refused (exit
2, nothing published) rather than left unindexed, which would make the
output partially indexed, or re-encoded, which is not lossless; such a file
is repaired only by a re-encoding mode. A rebuilt header that would reach
the 64 KiB `BlobHeader` limit is written without its tag index, as the
writer does, and refused if it still would. It takes exactly one input and
is refused with `--type`, `--clean`, `--dedupe`, `--force`, `--generator` and
`--output-header`. It accepts an unindexed and a partially indexed input.

```
pbfhogg cat [OPTIONS] --output <OUTPUT> <FILES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation). Not valid with `--dedupe`. |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user). Not valid with `--dedupe`. (`-c` used to be its short; `-c` now refuses here, naming `--clean`.) |
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted). Rejects `--type` and `--clean`: the dedupe path is blob-level passthrough and implements neither. |
| `--reindex` | Rewrite every data blob's indexdata and tagdata from its payload, losslessly; one input. See above |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O. Only valid with `--dedupe` - the plain `cat` path has no io_uring writer, and passing it there is an error. |
| `--force` | Proceed on unindexed input with the slower fallback. Requires `--type` or `--dedupe`: plain `cat` adds the indexdata and `--clean` without `--type` decodes every blob once either way, so neither refuses and `--force` with only those is an error. `--type node,way,relation --clean` is the clean pass, which does not refuse; `--force` there is accepted and does nothing. |
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

`--dedupe` (and `sort`, which shares its engine) treats an unindexed blob the
same way where it can: a blob one index describes passes through under
indexdata and tag keys scanned from its payload, and one holding no element
is omitted. A block no single index describes is not split in input order
but merged: each kind's elements join that kind's merge, are re-encoded as
the overlap runs are (the way blobs among them losing their enrichments, so
the output header declares none), and come out in type-then-ID order, with
exact duplicates dropped under `--dedupe`. A `Note:` line says how many blobs
were restructured and how many omitted, and the summary counts a
restructured blob once, as rewritten, and an omitted one as omitted.

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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on unindexed input with the slower fallback (see [Proceeding without indexdata](#proceeding-without-indexdata)) |
| `--distrust-metadata` | Believe no claim the input makes about itself: decode every blob in pass 1 as for a header without `Sort.Type_then_ID` (below), and also scan each blob's tag keys, so a blob whose indexdata or tagdata (compared as a key set) its payload contradicts is reframed under scanned ones. Needs no indexdata (`--force` is not needed alongside it) and accepts a partially indexed input. The repair every `Unsorted` refusal names: under a sorted header the default fast path copies an internally unsorted blob unchanged, because it believes the claim and the blob's indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

An input whose header does not claim `Sort.Type_then_ID` (or any input,
under `--distrust-metadata`) has every blob decoded in pass 1 to check its
order. An indexed blob whose indexdata that scan contradicts is copied under
indexdata and tag keys scanned from its payload rather than with the false
claim, and a `Note:` line counts them; the rebuilt `BlobHeader` keeps every
other field, the way-members field 5 included, as `cat --reindex` keeps it,
so the output keeps declaring `WayMembers-v1`. Unindexed blobs, and blocks
no single index describes, are handled as `cat --dedupe` handles them (see
[cat](#cat), "Blocks no single index describes").

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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--elements-per-blob <N>` | Per-blob element cap [default: 8000]. `8000` matches the osmium / Geofabrik convention; pass a larger value to approximate `planet.openstreetmap.org`-style packing. Must be > 0. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on unindexed input with the slower fallback (see [Proceeding without indexdata](#proceeding-without-indexdata)) |
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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
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
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

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
| `--staging-dir <DIR>` | Stage the output, or every output a `--config` names, in `DIR` instead of beside it; checked against every output's directory before any is reserved (see [Staging directory](#staging-directory)) |
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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--input-kind <KIND>` | Input kind override: `pbf` or `osc` (autodetect from extension by default) |
| `-R, --omit-referenced` | Omit referenced objects (faster, single pass, direct matches only; PBF only). Not with `--remove-tags` (no referenced objects to strip) or `-j` (no worker pool) |
| `--invert-match` | Invert match: exclude matching objects, keep non-matching (PBF only). (`-i` used to be its short; `-i` now refuses here, naming `--invert-match`.) |
| `--remove-tags` | Remove tags from referenced objects not directly matched (not with -R; PBF only). (`-t` used to be its short; `-t` now refuses here, naming `--remove-tags`.) |
| `-e, --expressions <FILE>` | Read filter expressions from file (one per line, # comments) |
| `-j, --jobs <N>` | Worker threads for the two-pass mode's classify passes and its decode-and-filter pool (see [Worker threads](#worker-threads)). Refused with `-R`, whose single pass reads through the pipelined reader and has no worker pool |
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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)). Requires `-o` |
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

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering - deterministic regardless of metadata completeness (see [DEVIATIONS](https://github.com/folknor/pbfhogg/blob/main/DEVIATIONS.md#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`/`--osmium-summary`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`. Either format exits `0` when the inputs are identical and `1` when they differ; an OSC diff publishes its `-o` in both cases.

With the global [`-q, --quiet`](#quiet) a text diff prints nothing, neither the diff text nor its summary: the exit code is the answer (`0` identical, `1` different). It compares exactly as a text diff without `-c` does (same paths, sharding and refusals) but formats nothing, so it writes no scratch at any `-j`. `-o`, `-c`, `-v` and `-s`, which only shape that text or its summary, are refused with it, and so is `--tmp-dir`. A quiet `--format osc` diff writes its `-o` as usual and drops only the summary.

With `-v`, each modified element is followed by indented detail lines: `coordinates: (lat, lon) -> (lat, lon)` for a node, `-ref @i n<id>` and `+ref @i n<id>` for a way's node refs, `-member @i type/id "role"` and `+member @i type/id "role"` for a relation (an unknown member type prints as `unknown(raw)/id`), and `-"k"="v"`, `+"k"="v"`, `~"k": "old" -> "new"` for tags. Way refs, member and tag order are content, so detail is never empty for a modified element. Refs and members are ordered edit scripts: all removals, then all insertions, each with its zero-based position in the old list (removals) or the new list (insertions), so a moved member prints as a `-member`/`+member` pair of the same member at its old and new positions, and a dropped duplicate as one `-member`. Removing the marked old items and placing the inserted ones at their new positions, with the kept items filling the rest in order, gives the new list. When several shortest scripts exist the one printed is fixed by the algorithm, not by which item moved. A script of more than 1000 edits, or one too costly to compute, prints one line instead: `refs: N -> M, old @a..b -> new @c..d replaced (details omitted)` (`members:` and `tags:` likewise), with N and M the list lengths and half-open ranges; it says the old range was replaced by the new one, not that every item in them differs. Tags are keyed, without positions: a reorder of keys present on both sides adds `tags: order changed`, and a tag list with a duplicate key prints an ordered `-tag @i "k"="v"`/`+tag @i "k"="v"` edit script instead. Every key, value and role prints as a complete double-quoted literal that is also a valid JSON string, never truncated: `"` and `\` print as `\"` and `\\`, newline, carriage return and tab as `\n`, `\r` and `\t`, and every other control character (C0, DEL, C1), U+2028, U+2029 and the Unicode bidi controls (U+061C, U+200E-U+200F, U+202A-U+202E, U+2066-U+2069) as `\u` with four lowercase hex digits; all other text prints as itself. So each detail record is exactly one line, whichever of LF, CRLF or Unicode line breaks a reader splits on, and `=` or ` -> ` inside a literal is never a separator.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only), at any `-j` |
| `-s, --osmium-summary` | Print osmium-style summary (`Summary: left=N right=N same=N different=N`) on stderr instead of the default pbfhogg-format summary. Both formats fire on stderr unless `-q` (text only; refused with `-q`) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`; refused with `-q` on a text diff) |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way. Requires `-o`: stdout is never replaced |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)). Requires `-o` |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `-j, --jobs <N>` | Worker threads, and the number of ID-range shards (see [Worker threads](#worker-threads)). `0` (default) uses one shard per core; `1` runs one shard on the main thread, which for text writes no scratch. The answer is the same at any `-j`. Applies to both text and `--format osc`; sharding requires both inputs to be indexed. A multi-shard run writes its shard files to the run's scratch directory (planet scale: ~30 GB text, ~45 GB osc XML), removed on completion; a quiet text diff, which only counts, writes none. |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read. Default: the output's directory, or the current directory without `-o`. Refused with text `-j 1`, whose one shard writes no scratch, and with `-q` on a text diff, which only counts (`--format osc` writes scratch on every path) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, roundtrips content - applying the derived OSC to the old PBF gives a file `diff` finds identical to the new PBF; metadata, which `diff` ignores, is outside that check (see [DEVIATIONS](https://github.com/folknor/pbfhogg/blob/main/DEVIATIONS.md#derive-changes-changes-are-decided-by-content-not-version)).

Inputs holding any negative ID always run as one shard, whatever `-j` says: shard boundaries are raw numeric comparisons, which do not order negative IDs the way the merge does.

**Input checks.** Both inputs must claim `Sort.Type_then_ID`. With both inputs indexed, the comparison is one engine at every `-j` and `-t`: a walk of every blob header of both files first refuses a truncated or malformed frame anywhere in either file, a data blob without indexdata, and blobs out of canonical order as far as their indexdata ID ranges prove it (order inside a blob, and inside a range spanning negative and positive IDs, is trusted). The body of a blob of a kind `-t` excludes is never read. Blob bodies of the selected kinds are decoded where the comparison needs them: a byte-identical blob pair (same claimed range and count, same compression and compressed bytes) is not decoded when the comparison may skip equal blobs (`-c`, `--format osc`), so a corrupt body inside such a pair passes there and is refused by a diff that prints common elements. Where a block holds elements past its claimed last ID, the merge refuses the claim when it meets them (a shard boundary hides elements past it from the shard below, which a later shard reads). Without indexdata the comparison is sequential at every `-j` and decodes every block of both files to learn its kind: a block of an earlier kind after a later one, and elements going backwards within a selected kind, are refused as unsorted, while blocks of excluded kinds are classified and dropped without converting their elements, so faults only a conversion sees (a tag whose string index is out of range) are not refused there.

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

The input is read front to back through one open, with every check made while it decodes, so `<FILE>` may be a stream (a FIFO, `/dev/stdin`) as well as a file. A refusal partway through a stream can wait for the stream's writer to write again or close it before the command exits.

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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--invert` | Invert selection: remove listed IDs instead of keeping them |
| `-r, --add-referenced` | Include referenced nodes of matching ways (two-pass; not with `--invert`) |
| `--remove-tags` | Remove tags from referenced objects. Requires `-r`, which is what emits them (so not with `--invert`). (`-t` used to be its short; `-t` now refuses here, naming `--remove-tags`.) |
| `-v, --verbose` | Print requested IDs, then report which were not found (not with `--invert`, and not with `-q`, which would drop both lists). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output. |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback (see [Proceeding without indexdata](#proceeding-without-indexdata)) |
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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
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

- **sparse** (default) - A flat memory-mapped array of 8 bytes per referenced node, in scratch storage; works on any PBF, sorted or not. How it is laid out is in [pipeline.md](https://github.com/folknor/pbfhogg/blob/main/reference/pipeline.md#add-locations-to-ways). Working-set behavior depends on the available page cache; see [performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md) for measurements.
- **external** - Double radix permutation via 4-stage pipeline with bounded memory and high scratch-disk demand. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. See [performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md) for measured resource use.

`--index-type dense` was removed; see [performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md) for measured comparisons. `dense` survives as a hidden value (absent from `--help`) purely so passing it errors with a pointer to `sparse` rather than a bare "invalid value".

By default, untagged nodes not referenced by a relation are dropped from output.

Blobs are found by their indexdata kind, and every blob a backend decodes is
held to its claim: a blob holding another kind than it claims, or whose
claimed ID range or count is not exact for what it holds (a way-claimed blob
with no way included), fails the run with exit 2 on either backend, pointing
at `check --index` and `cat --reindex`, and nothing is published. Sparse
decodes every node and way blob, and the relation blobs unless
`--keep-untagged-nodes` is given without `--inject-prepass`. `--index-type
external` also refuses, as unsorted, a decoded blob out of order under the
sorted header, inside it or behind the blob before; it decodes the node
blobs its ID buckets reach and the ones it re-encodes, and drops a node blob
whole, undecoded, on its tagdata and ID range when removing untagged nodes.
Sparse does not depend on order and joins such a file. See
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (runtime routing; see [pipeline.md](https://github.com/folknor/pbfhogg/blob/main/reference/pipeline.md#add-locations-to-ways)) |
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

The input must declare `Sort.Type_then_ID`, and time-filter holds it to that
as it reads: an element sorting below the one before it is refused as
unsorted (exit 2; the advice is `sort --distrust-metadata`). A history input
(`HistoricalInformation`) must also carry each object's versions strictly
ascending, and an input without that feature must hold each object once;
either broken is refused as invalid data, which `sort` cannot repair. No
indexdata is read, so a wholly unindexed input is filtered as an indexed one
is; a partially indexed input (indexdata on some data blobs and not others)
is refused once a read has met both kinds of data blob.

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

---

### apply-changes

Apply an OSC diff to a sorted PBF file. Uses blob passthrough -- unmodified blobs are copied as raw bytes without decompression (under `--locations-on-ways` some are decompressed first and then copied; see below).

Which blobs are unmodified is decided from each blob's indexdata ID range,
and a passed-through blob's element count in the statistics is its indexdata
claim. A blob whose claimed range misses the diff is copied without being
decoded, so a base whose indexdata understates that range gets the blob
copied past a change aimed at it. Every blob the run does decode (the ones
whose claimed range meets the diff, and under `--locations-on-ways` the way
blobs it searches) must hold exactly the kind, range and count its indexdata
claims and be in the order the header's sort claim promises; otherwise the
run fails (exit 2) and publishes nothing. See
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md), "Range, count and order claims
refused at decode".

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
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--locations-on-ways` | Keep inline way-node locations current through the merge (see below and DEVIATIONS.md). |
| `--ignore-missing-nodes` | With `--locations-on-ways`, permit unresolved references to retain the legacy `(0,0)` fallback instead of failing |
| `-j, --jobs <N>` | Worker threads for the descriptor-first pipeline (see [Worker threads](#worker-threads)). `0` (default) uses the `nproc - 2` heuristic (leaves two cores for scanner + drain, minimum 2). An explicit `-j N` runs exactly `N` workers, `1` included. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on an unindexed base with the slower fallback, which decodes every blob. The output is indexed on every blob, as plain `cat`'s is: a blob the diff leaves alone keeps its compressed payload under a header carrying indexdata derived from it, a blob no single index describes is re-encoded, and one holding no element is left out. A blob whose elements span more than one kind is refused (exit 2, nothing published), since it cannot be placed in type order; plain `cat` splits such blobs. Not with `--locations-on-ways`, which needs an indexed base whatever `--force` says; the combination is refused before any input is opened |
| `--allow-version-regression` | Accept a change whose `version` is lower than the version already held for that object. Off by default: precedence is by version, not by position, so feeding diffs out of sequence order is an error naming both versions rather than a silent downgrade. An equal version is always accepted and the later change wins by position (overlapping diffs, regional-extract deletes at the held version). Deletes count -- a delete leaves a versioned tombstone, so a later stale create or modify is refused too. Elements claiming no `version` are unaffected. See DEVIATIONS.md. |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

#### `--locations-on-ways`

The base must be sorted, indexed and declare `LocationsOnWays` (refused before
the OSC is parsed otherwise); the output declares it too.

**Which coordinates change.** A surviving base way (one the OSC neither
modifies nor deletes), whether its blob is rewritten or would have been
copied, takes the OSC's coordinate (the OSC's final state of the node) for
each existing inline coordinate whose node the OSC creates or modifies. It is
never resolved against base node records: it keeps every other inline
coordinate, one for a node the OSC deletes included (counted, see below), and
a way with no coordinate arrays is left without them. A way the OSC creates or
modifies is written from scratch: OSC coordinates for the nodes the OSC
carries, the base's node records for the rest, and none of the old version's
inline coordinates. A reference found in neither, or to a node the OSC
deletes, fails the run with nothing published (`--ignore-missing-nodes`
writes `(0, 0)` instead). The run fails as soon as the base's nodes have been
read, before any way is written, and the error counts the distinct nodes it
could not resolve. A base node record a written way takes its coordinate
from must lie in the geographic domain (latitude within +-90 degrees,
longitude within +-180, judged on the decimicrodegree value): one outside it
fails the run with nothing published, as add-locations-to-ways refuses it.
Base nodes no changed way references are not judged. Every base node's
coordinate must still fit an `i32` decimicrodegree, under any `int32`
block granularity. Only coordinates that
differ count as refreshed: a tag-only node edit refreshes nothing where the
inline coordinate already agrees, and costs a way blob no rewrite.

**Base preparation.** The node records a changed way needs must come from the
OSC or the base. `add-locations-to-ways` drops untagged nodes unless given
`--keep-untagged-nodes`, so its default output can fail on an ordinary daily
diff: a changed way can reference unchanged untagged nodes whose records are
in neither file. Make the base with `--keep-untagged-nodes`; osmium documents
the same requirement.

**What is read.** Node blobs are always decompressed, for their coordinates.
When the OSC creates or modifies at least one node, every way blob is
decompressed as well: one the merge would otherwise copy is searched at the
wire level (refs and coordinate arrays, without a full parse) and still
copied unchanged unless it holds a stale coordinate, in which case it is
parsed and rewritten. A diff whose node changes are all deletes leaves those
blobs unread. The search fails the run on a blob it cannot decompress, on
malformed wire framing of the block, its groups or a Way message (fields
it skips included), on malformed ref varints, and on coordinate arrays that
are not both absent or both complete and as long as the refs. For a way with
coordinate arrays and at least one reference to an OSC-upserted node it
also checks that every coordinate of the way converts to an in-range
decimicrodegree. It does not decode tag values, `Info` contents or the
string table, so a blob found clean whose errors are confined to those is
still copied unchanged; a blob rewritten for any reason is fully parsed.
Which blobs are way blobs is their indexdata kind claim (see
[indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md)). Relation blobs are never searched.

**Framing.** Rewritten blocks are written in the default coordinate and date
framing. A surviving way or relation from a source block with another
`granularity`, `lat_offset`, `lon_offset` or `date_granularity` is converted:
coordinates truncated toward zero to decimicrodegrees, timestamps to whole
seconds (timestamps with or without `--locations-on-ways`). Blobs copied
unchanged keep their own framing.

**Summary.** A progress line reports the refresh probe's memory, and the
summary adds `Inline locations refreshed: R refs in W ways, S way blobs
scanned, O rewritten only for this, D refs to deleted nodes kept`. `S` counts
way blobs searched that had no merge work of their own; `O` those rewritten
only for a refresh; `D` references in surviving ways to OSC-deleted nodes,
counted only in the way blobs the run searched or rewrote, so it is not a
whole-file count. A nonzero `D` means base and OSC disagree: the OSM API
refuses to delete a node a way still uses.

**Not a repair.** The refresh corrects coordinates for nodes this OSC creates
or modifies. A coordinate already stale for any other node stays stale, such
as one left by a pbfhogg version that refreshed only the ways the OSC
touched. `check --locations` reports stale coordinates where the file holds the node's
record. Rebuilding with `add-locations-to-ways` recomputes coordinates only
from node records, so it repairs a file only from an input holding the
current record of every referenced node, not from a default
`add-locations-to-ways` output.

### merge-changes

Merge multiple OSC files into one OSC file.

```
pbfhogg merge-changes [OPTIONS] --output <OUTPUT> <CHANGES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--simplify` | Keep only the last change per object (type + id) |
| `--allow-version-regression` | Requires `--simplify`, which is the mode that resolves conflicts; passing it alone is a usage error. Accepts a later change whose `version` is lower than one seen earlier in input order. Off by default, so listing the inputs out of sequence order is an error rather than a silent downgrade. An equal version is always accepted and the later change wins. Default (non-simplify) mode preserves the full stream in input order and resolves nothing, so there is nothing for the flag to allow. See DEVIATIONS.md. |
| `-j, --jobs <N>` | Worker threads (see [Worker threads](#worker-threads)). Sizes both the per-input parse fan-out (capped by input count) and the simplify path's chunk fan-out. |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`) unless `--allow-unindexed` is set. An existing output directory is refused before any work unless `--overwrite` is set, and even then it must hold a geocode index or be empty; any other existing path is refused because publication replaces the whole directory. The output path itself cannot be a symlink. On Linux, an existing mount-point output directory is refused before the build.

The node phase schedules blobs on their indexdata kind, so a node in a blob
labelled way or relation is never indexed; a node above the largest claimed
node `max_id` is refused. See [indexdata-trust.md](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

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
| `--staging-dir <DIR>` | Stage the new index in `DIR` instead of beside `--output-dir`; `DIR` must not lie inside `--output-dir` (see [Staging directory](#staging-directory)) |
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

#### S2 level bounds

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

#### Overwriting an existing index

Every build writes a complete generation in a staging directory (beside the
output directory, or in `--staging-dir`) and
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
incomplete `.pbfhogg-tmp-*` staging directory beside it (in `--staging-dir`,
when given) because process cleanup does not run. The next command that writes in that directory reports both
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

Outputs a set of binary files. See [performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md) for measured build times, index size, and peak memory.

### bench-read

Benchmark instrument: count the elements of a PBF through one of the
library's readers and print `elapsed_ms=`, `nodes=`, `ways=` and
`relations=` lines on stderr for a benchmark harness to parse (`-q` leaves
them alone, see [Quiet](#quiet)). It reads through the library's public
constructors, so, like a library user, it reads a partially indexed file the
commands would refuse.

```
pbfhogg bench-read --mode <MODE> <FILE>
```

| Flag | Description |
|------|-------------|
| `--mode <MODE>` | Reader to time: `sequential`, `parallel`, `pipelined` or `blobreader` |

### bench-write

Benchmark instrument: decode every element of a PBF and re-encode it through
`BlockBuilder` into a byte sink (nothing is published), printing
`elapsed_ms=` and the element counts on stderr.

```
pbfhogg bench-write [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--compression <COMPRESSION>` | Compression of the encoded blobs, as in [Common flags](#common-flags) [default: zlib] |
| `--writer <WRITER>` | Writer to time: `sync` (a buffered writer on the calling thread) or `pipelined` (the parallel-compression writer) [default: sync] |

### bench-merge

Benchmark instrument: run the `apply-changes` merge of `<BASE>` and
`<CHANGES>` into `-o` and print its timing and statistics on stderr. The
timer stops where publication begins. The merge runs as with `--force`, so
an unindexed base is accepted; there is no `-j`, `--locations-on-ways` or
header override.

```
pbfhogg bench-merge [OPTIONS] --output <OUTPUT> <BASE> <CHANGES>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--staging-dir <DIR>` | Stage the output in `DIR` instead of beside it (see [Staging directory](#staging-directory)) |
| `--compression <COMPRESSION>` | Blob compression, as in [Common flags](#common-flags) [default: zlib] |
| `--io-mode <IO_MODE>` | Output writer: `buffered` or `uring` (io_uring; needs the `linux-io-uring` feature) [default: buffered] |

---

## I/O capability matrix

Which commands support `--direct-io` (O_DIRECT bypass of page cache) and `--io-uring`
(io_uring async writes), and which paths are affected.

| Command | Reads PBF | Writes PBF | `--direct-io` | `--io-uring` | Notes |
|---------|:---------:|:----------:|:-------------:|:------------:|-------|
| inspect | Yes | - | Yes | - | Read-only |
| inspect tags | Yes | - | Yes | - | Read-only |
| inspect --nodes | Yes | - | Yes | - | Read-only |
| check --ids | Yes | - | Partly | - | Read-only; `--direct-io` covers only the front read (header and first blob header); the blob reads are parallel unaligned preads without it |
| check --refs | Yes | - | No | - | Read-only; `--direct-io` accepted and ignored, with a note (parallel unaligned preads) |
| check --index | Yes | - | Yes | - | Read-only; decompresses every blob |
| check --locations | Yes | - | No | - | Read-only; `--direct-io` accepted and ignored, with a note (parallel unaligned preads) |
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
