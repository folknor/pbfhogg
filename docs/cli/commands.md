# pbfhogg CLI Reference

Version 0.5.0. Generated from `pbfhogg --help` output.

## Global flags

All commands support `-h, --help` and `-V, --version`, and `-q, --quiet`,
accepted before or after the subcommand.

`-q, --quiet` means the exit code is the answer. It drops every `Note:`,
every progress line and the command's closing summary, and keeps every
`Warning:` and the final `Error:` line. It never changes what a command
writes to `-o`, nor a report it prints on stdout (`inspect`, `inspect tags`,
`digest`, `export` without `-o`), except for `diff`, `check` and
`inspect --indexed`: a quiet run of those prints nothing at all. The flags
that only shape the output a quiet run drops are refused with it: `-o`, `-c`,
`-v` and `-s` on a text `diff` (a quiet `diff --format osc` writes its `-o`
as usual), `--json`, `--show-ids` and `--max-errors` on `check`, `--json` on
`inspect --indexed`, and `getid -v`. The `bench-*`
commands' key=value blocks print as they always have. The full contract is
in the
[CLI reference](https://github.com/folknor/pbfhogg/blob/main/reference/cli-reference.md#quiet).

## Exit codes

The full exit status contract, including stderr behavior, lives in the
[CLI reference](https://github.com/folknor/pbfhogg/blob/main/reference/cli-reference.md#exit-codes).

## Interruption

SIGINT (Ctrl-C) or SIGTERM stops any command at its next checkpoint: the run
removes its output staging and scratch files, publishes nothing (an existing
output is left untouched), prints `Error: interrupted by SIGINT: ...` and
exits `130` (`143` for SIGTERM). A second signal exits at once with the same
convention, leaving the cleanup unfinished for the next run to report. The
full contract, including the few phases that finish before the run stops,
is in the
[CLI reference](https://github.com/folknor/pbfhogg/blob/main/reference/cli-reference.md#interruption).

## Indexdata is believed

Commands that act on blob-level indexdata or tagdata, or on the header's
`Sort.Type_then_ID` flag, believe them; a few checks happen in passing where
a command decodes anyway, but none is a guarantee. A file whose claims are
false can get a wrong answer that reports success. `check --index --ids`
verifies a file of unknown provenance, except for element-kind order on a
file without indexdata and the header's feature flags. Which command believes which claim,
and what a false one costs, is in the
[indexdata trust model](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

## Common flags

These flags appear on most commands that produce PBF output:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--compression <COMPRESSION>` | Blob compression: `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`) |
| `--direct-io` | Use O_DIRECT to bypass page cache (requires `linux-direct-io` feature) |
| `--force` | On a command that would refuse unindexed input: proceed with the slower fallback. Only those commands have it |
| `--generator <GENERATOR>` | Override the writing program name in the output header |
| `--output-header <KEY=VALUE>` | Set output header fields (repeatable). Keys: `osmosis_replication_timestamp` (UNIX seconds or `YYYY-MM-DDTHH:MM:SSZ`), `osmosis_replication_sequence_number`, `osmosis_replication_base_url`. Each key replaces only its own field; an empty value (`KEY=`) omits it |

## Flag rules

- Every short flag has one meaning across all subcommands: `-b` bbox, `-c`
  suppress-common, `-d` directory, `-e` expressions, `-g` get, `-i` id-file,
  `-I` id-osm-file, `-j` jobs, `-M` max-count, `-o` output (output-dir on
  build-geocode-index), `-O` overwrite, `-p` polygon, `-q` quiet, `-r`
  add-referenced, `-R` omit-referenced, `-s` osmium-summary, `-t` type, `-v`
  verbose. A short that used to mean something else on a command now refuses
  there, naming the long flag to spell: `cat -c` (`--clean`), `extract -c`
  (`--config`) and `-s` (`--simple`), `renumber -s` (`--start-id`),
  `getparents -s` (`--add-self`), `inspect -e` (`--extended`), `inspect tags
  -s` (`--sort`), `tags-filter -t` (`--remove-tags`) and `-i`
  (`--invert-match`), `getid -t` (`--remove-tags`).
- A flag the command line makes ineffective is refused as a usage error
  (exit `2`), not ignored or warned about: `-O` without `-o`, `--force` where
  nothing would refuse, `--tmp-dir` on a path that writes no scratch, `-j` on
  a path with no worker pool, a PBF-only flag on OSC input. A fallback the
  host or the input picks at run time (an unindexed input taking a
  sequential path) is never grounds for refusal.
- Every flag, malformed values included, is checked before any input is
  opened.
- `-j/--jobs`, on every command that has it: `0` (default) sizes the worker
  pools automatically, `1` runs sequentially, `N` caps them at `N` threads.
  For `diff`, `N` is also the shard count.
  Commands without `-j` size their pools themselves.
- `--force`: a command that would refuse unindexed input proceeds with the
  slower fallback. `build-geocode-index --force` is the exception: a legacy
  shorthand for `--overwrite --allow-unindexed` that warns every time.

The full rules are in the
[CLI reference](https://github.com/folknor/pbfhogg/blob/main/reference/cli-reference.md#flag-rules).

## Output header

A command's output header never claims more than its output contains:

- `Sort.Type_then_ID` only when the output is in that order: by construction
  (`sort`, `renumber`, `apply-changes`, `cat --dedupe`), or because the
  command keeps the order of its one sorted input. An extract of an unsorted
  file is not declared sorted.
- `LocationsOnWays`, `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1`
  only when every way blob written carries them. Commands that copy way blobs
  whole (`cat`, `cat --type`, `sort` of sorted input, `getid --invert` of
  node or relation IDs, degrade's header-only flags) keep what the input
  declares; `repack` and `apply-changes --locations-on-ways` keep
  `LocationsOnWays`; `add-locations-to-ways` adds it; everything that
  re-encodes ways drops them, with a warning. The prepass features survive
  only from a single input.
- The input's bbox (the union when every input declares one), except
  `apply-changes`, which declares none, and `extract --set-bounds`, which
  declares the region.
- The input's replication state, naming the feed and the position the
  content was drawn from; `time-filter` lowers the timestamp to its cutoff
  when the cutoff is earlier, and then drops the sequence number.
  `apply-changes` keeps only the base URL (its output is past the base's
  position, at one it cannot name; set it with `--output-header`), and
  `renumber` keeps none (its new IDs are not the feed's).
- Writing program `pbfhogg/<version>` (the release that wrote the file),
  unless `--generator` says otherwise; `--generator ""` writes a present,
  empty writing program.
- An empty `--output-header` value (`osmosis_replication_base_url=`) omits
  that field, as in osmium.

`degrade`'s header-only flags forward the input header field for field
instead. The full rule is in `reference/cli-reference.md` "Output header".

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
  (`.pbfhogg-tmp-<pid>-<n>.<name>`), syncs it and renames it into place after its
  final flush; a run that fails before that removes it, so it leaves no file
  at the output path and leaves an existing output untouched. So does a run
  interrupted by SIGINT or SIGTERM. A run killed outright (SIGKILL, the OOM
  killer, power loss, a second interrupt) before publication can leave the
  hidden staging file behind, never a file under the output name. The rename itself
  is the last step, and the few failures possible after it are reported as
  such: a directory sync that fails after the output was put in place is an
  error that says the output *was* published (naming any backup of a
  replaced output, which is then kept), and a group whose rollback cannot
  complete names what is left where. The output's directory must exist and
  be writable.
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

`add-locations-to-ways`, `diff` (parallel `-j` text that is not `--quiet`, and `--format osc`) and
`build-geocode-index` write scratch: disposable files that can be very large (hundreds of GB for
`add-locations-to-ways --index-type external` at planet scale). Each run makes **one** hidden directory,
`.pbfhogg-run-<pid>-<random>`, holds everything it writes in there and removes it when it ends,
whether it succeeded, failed or was interrupted. If the directory cannot be removed the run
says where the leftover is.

- `--tmp-dir <DIR>` chooses the directory the run directory is made in. It must exist and be
  writable, and it is checked before any input is opened, so a bad value fails at once. Without
  it the scratch goes in the output's directory (`diff` text to stdout: the current
  directory). `build-geocode-index` refuses a `--tmp-dir` inside the index
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
`--overwrite` replaces it; the report says so. An index that holds anything else pbfhogg leaves
behind (a backup, a staging entry, a run directory, live or not, an older version's scratch) is
not replaced at all: `--overwrite` fails before any work, naming the entries. An output named
like one of these names, or inside one, is refused the same way.

A `.pbfhogg-backup-*` entry is reported with its own wording. It can be the only copy of a
previous output, so pbfhogg never deletes one and you should look at it before removing it.
If a backup cannot be removed after a successful replacement, the run still succeeds and
prints a `Warning:` naming it. If the replacement's directory cannot be synced, the backup is
kept rather than removed, and the error names it. Library callers get the same scan as typed values from
`pbfhogg::scratch::find_stale` and `pbfhogg::scratch::find_stale_in_index`.

---

## Commands

### inspect

Inspect PBF file: metadata, block breakdown, ordering analysis.

`inspect` describes a file: what it is and what it claims. By default it
reports counts, ID ranges and per-block stats from the blobs' own indexdata
(`--distrust-metadata` decodes instead). What it finds is not a verdict: a
readable file exits 0 (`--indexed` alone answers yes or no in its exit code).
Whether the contents match the claims is [`check`](#check)'s question.

On indexed PBFs, uses an index-only fast path that reads blob headers without decompression (~36ms on 473 MB vs ~4s for full decode).

```
pbfhogg inspect [OPTIONS] <FILE>
pbfhogg inspect tags [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `--indexed` | Check if PBF has blob-level indexdata (exit code 0 = yes, 1 = no). Reads the first data blob only; later blobs are not checked (to verify every blob's indexdata against its payload, use `check --index`). A header-only file (no data blobs) is indexed; a file without a leading OSMHeader is an error (exit 2), not a "no" |
| `--nodes` | Analyze node coordinate statistics for FOR compression sizing |
| `-j, --jobs <N>` | Worker threads for `--nodes`. **Requires `--nodes`**: no other inspect mode has a worker pool, so passing it elsewhere is an error rather than a no-op |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show blocks outside inspect's per-type median thresholds, plus mixed blocks |
| `--extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`). Data and metadata paths enable extended stats automatically. Not with `--blocks` or `--anomalies`; an unknown key is refused before the input is opened |
| `--json` | Machine-readable JSON output. Not with `--nodes` or `--get` |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123` or `node/123`). Also accepts `w456` / `way/456` and `r789` / `relation/789`, including signed IDs. Uses indexdata to skip non-matching blobs, early exit on sorted PBFs. **A mode of its own**: mutually exclusive with every other `inspect` flag, which it used to accept and discard |
| `--distrust-metadata` | Decode every blob instead of reporting counts, ID ranges and per-block stats out of the blobs' own indexdata. Nothing validates indexdata against the payload it describes, so on a file you are inspecting because you suspect it, the fast path reports the file's claims rather than its contents. Scanning report only: rejected with `--indexed`, `--nodes`, and a `file.*` or `header.*` `--get` key |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback. **Requires `--nodes`**, the only mode that refuses unindexed input |

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `--sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Count one element type: exactly one of node, way, or relation (a list is refused) |
| `-j, --jobs <N>` | Worker threads |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback |

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

With no flags, runs the ID and referential integrity checks. Use `--ids`,
`--refs`, `--index` or `--locations` to select. `--index` and `--locations`
are not part of the default pair: each decompresses the whole file (the way
blobs once per pass, for `--locations`), so they are opt-in.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--index` | Verify blob-level metadata (`BlobHeader` indexdata and tagdata) against the blobs themselves. Decompresses every blob |
| `--locations` | Verify a LocationsOnWays file's inline way-node coordinates against its node records (see below) |
| `--agreement` | Also require every inline coordinate of a node the file does not contain to agree across the ways referencing it (applies to the locations check) |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Retain at most N violation details per check (0 = unlimited); scanning and total counts continue [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |
| `--json` | Machine-readable JSON output |
| `--direct-io` | Use O_DIRECT to bypass page cache where the check supports it: the ID check's first read of the file, and `--index`. No effect on `--refs` and `--locations` (their parallel readers read unaligned blob ranges), which say so when given |

With the global `-q, --quiet`, `check` prints no report and the exit code is
the answer; `--json`, `--show-ids` and `--max-errors` are refused with it.

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

The ID check judges monotonicity by canonical OSM order (`0`, then negative IDs
by ascending absolute value, then positives) - the order `sort` produces. On a
positive-only file that is plain `i64` order.

#### `--index`: verifying blob-level metadata

`BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox) and
`BlobHeader.tagdata` (the blob's tag key set) are hints a reader believes without
opening the blob: `cat --type` passes a blob through raw on the strength of the
kind, `tags-filter` skips whole blobs on the strength of the tag index, and the
spatial filter skips node blobs on the strength of the bbox. Nothing in the
format signs those claims. `check --index` decompresses every blob, re-derives
the metadata from the payload, and reports each disagreement with its blob
ordinal and byte offset. It does not check order: combine it with `--ids` to
check that IDs ascend within and across blobs and that kinds come in order.
Which commands believe which claim, and what a false one costs, is in the
[indexdata trust model](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md).

It never routes on the metadata it is checking - no blob filter, no
`require_indexdata`, every OSMData blob decompressed - so there is no
`--distrust-metadata` flag to pass: distrust is the only mode it has.

A blob that carries no indexdata or no tagdata makes no claim and is not
reported; that is the ordinary state of a third-party PBF. Metadata that is
present but *unreadable* is a different matter and is reported: an indexdata of
an unrecognised length or version, a malformed tag index, or a tag index
attached to a blob whose element kinds are mixed (where no key set can be
complete). Ordinary reads treat all of those as "no metadata, decode the blob",
which is safe for a reader and useless in a verifier.

Two of the checks are containment rather than equality:

- A stored **bbox** must contain every coordinate in the blob. A wider box is
  sound (and is what a non-default `granularity` legitimately produces), an
  absent box is no claim at all, and only a box that is present and too small
  can make a spatial filter skip a blob it should have read. A node coordinate
  outside the decimicrodegree range (about 214.7 degrees either way) lies
  outside every box, so any stored box over it is reported.
- A stored **tag key set** that omits a key the blob carries makes `tags-filter`
  skip a blob holding matches; one that lists a key the blob does not carry only
  costs a needless decode. Both are reported, as distinct violations.

Kind, count and ID range are checked for exact equality, because every consumer
treats them as exact.

Exit 1 means mismatches were found; exit 2 means the check could not run.

#### `--locations`: verifying inline way locations

A file whose header declares `LocationsOnWays` (what `add-locations-to-ways`
writes, and what `apply-changes --locations-on-ways` maintains) claims that
every way carries, for each node it references, that node's location.
`check --locations` compares every inline coordinate with the record of the
same node in the same file and reports each disagreement as
`w<way> ref <position> (n<node>): inline <lat,lon>, node record <lat,lon>`.

What fails the check (exit 1):

- an inline coordinate that differs from its node's record;
- a way with node references but no coordinate arrays;
- an inline coordinate outside the geographic domain, including the
  undefined location (`2147483647` on both axes) osmium writes for a node it
  could not resolve;
- two records of one node at different coordinates, or a node record outside
  the domain (references to such a node are counted, not compared);
- with `--agreement`, a node the file does not contain whose references carry
  different coordinates.

**References to nodes the file does not contain are counted, not failed.**
`add-locations-to-ways` drops untagged nodes that no relation references
unless given `--keep-untagged-nodes`, so on its default output most references
have no node record, and nothing in `LocationsOnWays` requires one. Such a
reference cannot be verified; the report counts them (`Refs without a node
record`), and among them those at exactly `0,0`, the placeholder
`--ignore-missing-nodes` writes. A pass on a file that verified little says
so in those counts. `--agreement` adds what the file can still show about
those nodes - that every way carrying one agrees on where it is - which is
consistency, not freshness: ways that all carry the same stale position pass.

Every reference is counted in exactly one of: verified, mismatched, without
a valid inline coordinate (a way with no coordinates, or one outside the
domain, referencing a node that has a record), to a node with conflicting or
out-of-domain records, and without a node record.

Coordinates are compared in decimicrodegrees, after each block's
`granularity`, `lat_offset` and `lon_offset` are applied exactly; two values
equal to the decimicrodegree are equal. Refused (exit 2) rather than checked:
a file without the `LocationsOnWays` header claim, a history file, a
partially indexed file, a way referencing a negative node ID, a blob whose
indexdata claims ways while it holds other elements, and any blob that fails
the validation the reader makes when it builds a block (malformed framing,
tag, `Info` or `DenseInfo` arrays, way or relation arrays, a string table
that is not UTF-8). Faults the reader only finds later, such as a tag's
string-table index past the end of the table, are not looked for.

The node IDs that get a slot are held in a bit set (about 1.7 GB across the
planet's ID range), and their coordinates in a store of about 8.25 bytes per
slot. The store is sized to 60 % of the memory available when the check
starts (with other selectors, `--locations` runs first), less a 2 GiB
headroom and the bit set; when it does not fit, the node
ID space is split into parts and the way blobs are read once per part. A run
that plans more than 16 parts says so on stderr. The first pass records
what every blob holds, and later passes read only the blobs that can matter
to them, so on a sorted file each node blob is read about twice in all; on an
unsorted one a node blob whose IDs are spread out can be read once per part.
No scratch disk is used. A file that keeps every node gets a slot per node, so a
planet of about 10 billion nodes needs about 85 GB of store, which a host
with 28 GB free covers in several passes; a default `add-locations-to-ways`
output keeps a few percent of the nodes and needs a small fraction of that.
`--agreement` gives every referenced node a slot, present or not, so on a
default output it costs as much as on a complete one.

The budget bounds the store, not the whole run: the bit set grows with the
number of distinct node IDs, each worker holds its decompressed blob and a
copy for validation, and the violation details are held until the report is
printed - at most `--max-errors` of them, but every one with
`--max-errors 0`, which on a badly broken file can be one per reference. The
`partitions` and `blob_decodes` fields of the JSON report follow the memory
available, not the file. `--direct-io` has no effect on this check (its
parallel readers read unaligned blob ranges) and says so when given.

The ref check refuses (exit 2) a file with a negative ID it would compare: a
node, a way when relations are checked, a relation, or any reference it checks.
With relations checked it also refuses a relation member of an unknown type,
which it has nothing to resolve against. A positive ID above the range sized
from the file's own indexdata is still tracked. Missing references are counted
once per distinct ID in each category, so a file in which most references
dangle (a ways-only file) costs about one bit set over the missing IDs' range,
not memory per reference. Two things still cost memory per occurrence: the
details `--show-ids` retains, every one with `--max-errors 0`, and
relation-to-relation members, which are held until every relation has been
read.

---

### cat

Concatenate PBF files with optional type filtering. Embeds blob-level indexdata and tagdata automatically.

With `--dedupe`, merges multiple sorted PBF files with blob-level passthrough and exact-duplicate deduplication.

```
pbfhogg cat [OPTIONS] --output <OUTPUT> <FILES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation) |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user). Decodes and re-encodes every selected blob once, keeping input order |
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted). Rejects `--type` and `--clean`: the dedupe path is blob-level passthrough and implements neither |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O (only with `--dedupe`) |
| `--force` | Proceed on unindexed input with the slower fallback. Requires `--type` or `--dedupe`: plain `cat` and `--clean` without `--type` never refuse |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

Plain `cat` and `--type` copy way blobs whole, so they keep the
`LocationsOnWays` and prepass declarations the input carries; `--clean`
re-encodes and drops them. With several inputs the output header is reconciled
across all of them rather than copied from the first: the bbox is the union,
and is omitted unless every input declares one; the base URL is kept when every
input names the same feed, the timestamp and sequence number only when every
input agrees on all three; `HistoricalInformation` is the OR; the output is
not declared sorted and the prepass features are dropped. osmium writes a
blank header for multiple inputs, so keeping the agreed fields is a deliberate
deviation. See [Output header](#output-header).

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
| `--force` | Proceed on unindexed input with the slower fallback |
| `--distrust-metadata` | Believe no claim the input makes about itself (the header's sort claim, indexdata, tagdata): decode every blob, rewrite one found out of order, and reframe one whose indexdata or tag keys its payload contradicts. Needs no indexdata. The repair a command's `Unsorted` refusal names: by default a sorted-claiming file's blobs are copied as they are |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### repack

> **0.4.0**

Re-encode a PBF with a configurable per-blob element cap. Element semantics, tags, refs, members, metadata, and DenseNodes encoding all round-trip; output is type-sorted and propagates `Sort.Type_then_ID` from the input header.

Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement (Geofabrik's ~8 k/blob convention vs `planet.openstreetmap.org`'s ~228 k/blob), so commands with implicit blob-count scaling can be measured at controlled densities.

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
| `--force` | Proceed on unindexed input with the slower fallback |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

Growing (e.g. europe 8 k -> 64 k) coalesces elements across input-blob boundaries, so caps larger than the input blob size fire correctly. On a coalescing shrink the output blob count is not the general `ceil(elements / cap)`: each input blob whose element count is not a multiple of the cap emits its tail as its own possibly-under-cap block (the deliberate trade that keeps output ID-monotonic across coalesced boundaries). When the input header declares `LocationsOnWays`, the output re-advertises it and every inline way-node coordinate round-trips exactly; the two `pbfhogg.*` prepass features (`WayMembers-v1`, `SharedNodePins-v1`) are dropped with a warning.

### degrade

> **0.4.0**

Produce a valid-but-adversarial PBF by stripping properties or perturbing structure. Each flag composes; at least one is required. Used to produce inputs for benchmarking non-optimal code paths (`sort` overlap-rewrite, `add-locations-to-ways`, `--force` fallbacks).

```
pbfhogg degrade [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--unsort` | Clear `Sort.Type_then_ID`; perturb the element stream so at least one adjacent same-kind blob pair has overlapping IDs. Triggers `sort`'s overlap-rewrite path. |
| `--unsort-intra` | Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with an internal ID-order inversion but non-overlapping blob ranges, the intra-blob shape `sort`'s overlap detector cannot see. Mutually exclusive with `--unsort`. |
| `--strip-locations` | Drop the `LocationsOnWays` header feature. Inline way-node coordinates are not preserved. |
| `--strip-indexdata` | Clear `BlobHeader.indexdata` on every OsmData blob. Forces commands into their `--force` / non-indexed fallback paths (`sort`, `getid`, `tags-filter`). Blob payloads are not decompressed. |
| `--strip-tagdata` | Clear `BlobHeader.tagdata` (the per-blob tag key index) on every OsmData blob, forcing `tags-filter`'s no-hint fallback path. Leaves `indexdata` intact. |
| `--corrupt-tagdata` | Replace `BlobHeader.tagdata` on every OsmData blob with a well-formed tag key index naming a key the blob does not carry. Parses cleanly and lies, so `tags-filter` skips blobs whose elements match - the adversarial case `--strip-tagdata` cannot produce, since a missing index is decoded rather than trusted. Mutually exclusive with `--strip-tagdata` and with any blob-re-encoding flag. |
| `--corrupt-indexdata` | Replace `BlobHeader.indexdata` on every OsmData blob with a well-formed index that overstates the blob (same kind, inflated count and max ID). Produces the file `inspect --distrust-metadata` exists for. Mutually exclusive with `--strip-indexdata` and with any blob-re-encoding flag. |
| `--corrupt-indexdata-kind` | Rotate the element kind declared by `BlobHeader.indexdata` on every OsmData blob (node to way to relation to node), leaving count and ID range honest. The kind is what per-kind blob schedules select on, so this makes a consumer skip a blob outright rather than misreport it. Mutually exclusive with `--strip-indexdata`; combinable with `--corrupt-indexdata`. |
| `--strip-bbox` | Clear `HeaderBlock.bbox` so the output declares no file-level extent. Header-only change; no OsmData blob is touched. Does not affect `extract --bbox`, which derives its region from the CLI argument and per-blob indexdata bboxes rather than the header. |
| `--drop-ids <N:SEED>` | Deterministically remove exactly N elements selected globally by kind, ID, and seed. Surviving references to removed elements intentionally dangle. |
| `--force` | Proceed on unindexed input on the decode path (`--unsort`, `--unsort-intra`, `--strip-locations`, `--drop-ids`), which scans every blob for every kind; slower. Refused without a decode-path flag, since the header-only flags never refuse |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

`--strip-indexdata`, `--strip-tagdata`, and/or `--strip-bbox` (with no `--unsort`/`--strip-locations`/`--drop-ids`) run as a header-and-blob-level passthrough: only the targeted field is cleared and every other header and blob-header field survives byte-for-byte, including `source`, custom optional features, replication metadata, and unknown/extension fields; `--generator` and `--output-header` replace only the field they name. Any combination involving `--unsort`, `--strip-locations`, or `--drop-ids` decodes elements and re-encodes via `BlockBuilder`. `--drop-ids` requires `N:SEED`, rejects zero N, and is reproducible for a given input and seed.

`--recompress` from the design doc remains deferred; so do configurable `--unsort` chaos modes (rotate / shuffle / reverse).

### renumber

Renumber all element IDs sequentially, remapping cross-references (way node refs, relation member refs).

The output header carries no replication state: the new IDs are not the
feed's, so its diffs would address other objects.

```
pbfhogg renumber [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--start-id <ID>` | Starting ID(s): single value or comma-separated node,way,relation [default: 1]. (`-s` used to be its short; `-s` now refuses here, naming `--start-id`.) |
| `--compression` | Blob compression [default for renumber: zlib:1; other commands: zlib]. Level 1 is the measured fast path for renumber output. An explicit `--compression zlib` or `zlib:6` also yields level 1 today, because the value-based API cannot tell omission from an explicit default; any other zlib level, zstd and none are honoured as given. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### extract

Extract elements within a geographic region (bounding box or polygon).

Three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), `--smart` (three passes, completes multipolygon/boundary relations).

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
| `--config <FILE>` | Multi-extract JSON config file |
| `-d, --directory <DIR>` | Rebase config outputs under this directory (only with --config) |
| `--simple` | Simple strategy (single pass). Never refuses unindexed input, so not with `--force` |
| `--smart` | Smart strategy (three passes, complete relations) |
| `--set-bounds` | Write the extract region bounding box to the output header instead of the input's. complete_ways and smart exceed it by design with the out-of-region nodes of selected ways |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### tags-filter

Filter elements by tag expressions. Default mode resolves relation members transitively (matched relations pull in member ways, nodes, and nested relations). With `-R`, only directly matched elements are emitted.

With `--input-kind osc` (or autodetected from `.osc`/`.osc.gz` extension), filters an OSC change file instead, always preserving deletes. Every flag that only means something for a PBF is refused in OSC mode: `-R`, `--invert-match`, `--remove-tags`, `--distrust-metadata`, an explicit `-j`, `--generator`, `--output-header`, an explicit `--compression`, `--direct-io` and `--force`.

Expressions use osmium syntax: `highway=primary`, `amenity`, `w/building=yes`, etc.

An expression is `[nwr/]KEY[!]=VALUE` or `[nwr/]KEY`. Key and value are matched
by the same rules, following osmium's `get_string_matcher`:

| Form | Meaning |
|------|---------|
| `foo` | exact match |
| `foo,bar` | any one of the alternatives (both halves, keys included) |
| `foo*` | prefix match |
| `*foo`, `*foo*` | substring match (there is no separate suffix matcher) |
| `*` | matches anything |

Spaces around each half, and around each comma-list element, are stripped. The
expression splits at the first `=`; a `!` inverts the value match, which still
requires the key to match.

The `!` is tested on the raw key, before that space stripping, as osmium does.
So it marks an inversion only when it is the last character before the `=`:
`highway !=primary` inverts, while `highway ! = primary` and
`highway! = primary` name the literal keys `highway !` and `highway!`.

```
pbfhogg tags-filter [OPTIONS] --output <OUTPUT> <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--input-kind <KIND>` | Input kind override: `pbf` or `osc` (autodetect from extension by default) |
| `--distrust-metadata` | Do not skip blobs on their indexdata or tag index; decode every blob (PBF only). Nothing validates a blob's tag index against its payload, so a file whose index omits a key its elements carry silently loses those elements. Trades the skip speedup for an answer that depends only on element data, and waives the indexdata precondition |
| `-R, --omit-referenced` | Omit referenced objects (faster, single pass, direct matches only; PBF only). Not with `--remove-tags` or `-j` |
| `--invert-match` | Invert match: exclude matching objects, keep non-matching (PBF only) |
| `--remove-tags` | Remove tags from referenced objects not directly matched (not with -R; PBF only) |
| `-e, --expressions <FILE>` | Read filter expressions from file (one per line, # comments) |
| `-j, --jobs <N>` | Worker threads for the two-pass mode (PBF only). Refused with `-R`, whose single pass has no worker pool |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback. Not with `--invert-match` or `--distrust-metadata`, which never refuse |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### export

> **0.5.0**

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
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way. Requires `-o` |
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

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering - deterministic regardless of metadata completeness (see [DEVIATIONS](/cli/deviations#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`. Either format exits `0` when the inputs are identical and `1` when they differ; an OSC diff publishes its `-o` in both cases.

With the global `-q, --quiet` a text diff prints nothing, neither the diff text nor its summary, and the exit code is the answer (`0` identical, `1` different). It compares exactly as a text diff without `-c` does, sharding included, but formats nothing, so it writes no scratch. `-o`, `-c`, `-v` and `-s`, which only shape that output, are refused with it, and so is `--tmp-dir`. A quiet `--format osc` diff writes its `-o` as usual and drops only the summary.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only), at any `-j` |
| `-s, --osmium-summary` | Print osmium-style summary (left/right/same/different counts) to stderr instead of the default pbfhogg-format summary (text only). The pbfhogg-format summary always fires on stderr unless `-q` - this flag only swaps the format (refused with `-q`) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`; refused with `-q` on a text diff) |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way. Requires `-o` |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `-j, --jobs <N>` | Worker threads and ID-range shard count. `0` (default) uses one shard per core; `1` runs one shard on the main thread (for text, scratch-free). The answer is the same at any `-j`. Applies to both `--format text` and `osc`; sharding requires both inputs indexed. A multi-shard run writes shard files to scratch, removed on completion (a quiet text diff, which only counts, writes none); scratch demand varies by input. See `reference/performance.md` for measurements |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read. Default: the output's directory, or the current directory without `-o`. Refused with text `-j 1` and with `-q` on a text diff, which write no scratch |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, roundtrips content - applying the derived OSC to the old PBF gives a file `diff` finds identical to the new PBF; metadata, which `diff` ignores, is outside that check (see [DEVIATIONS](/cli/deviations#derive-changes-lossless-delete-roundtrip)).

Inputs holding any negative ID always run as one shard, whatever `-j` says: shard boundaries are raw numeric comparisons, which do not order negative IDs the way the merge does.

**Input checks.** Both inputs must claim `Sort.Type_then_ID`. With both inputs indexed, the comparison is one engine at every `-j` and `-t`: a walk of every blob header of both files first refuses a truncated or malformed frame anywhere in either file, a data blob without indexdata, and blobs out of canonical order as far as their indexdata ID ranges prove it (order inside a blob, and inside a range spanning negative and positive IDs, is trusted). The body of a blob of a kind `-t` excludes is never read. Blob bodies of the selected kinds are decoded where the comparison needs them: a byte-identical blob pair (same claimed range and count, same compression and compressed bytes) is not decoded when the comparison may skip equal blobs (`-c`, `--format osc`), so a corrupt body inside such a pair passes there and is refused by a diff that prints common elements. Where a block holds elements past its claimed last ID, the merge refuses the claim when it meets them (a shard boundary hides elements past it from the shard below, which a later shard reads). Without indexdata the comparison is sequential at every `-j` and decodes every block of both files to learn its kind: a block of an earlier kind after a later one, and elements going backwards within a selected kind, are refused as unsorted, while blocks of excluded kinds are classified and dropped without converting their elements, so faults only a conversion sees (a tag whose string index is out of range) are not refused there.

### digest

Print a digest of a PBF's element content. Two files the command accepts get the same digest exactly when `diff --suppress-common` reports them identical (barring a 128-bit hash collision), whatever their encoding: compression, blob boundaries, dense or plain nodes, string-table order, header fields and indexdata presence do not change it, and neither does metadata, which diff ignores.

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

```
pbfhogg getid [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--invert` | Invert selection: remove listed IDs instead of keeping them |
| `-r, --add-referenced` | Include referenced nodes of matching ways (two-pass; not with `--invert`) |
| `--remove-tags` | Remove tags from referenced objects. Requires `-r` (so not with `--invert`) |
| `-v, --verbose` | Print requested IDs, then report which were not found (not with `--invert`, and not with `-q`, which would drop both lists). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback |
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
| `--add-self` | Also include the queried objects themselves in the output |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### add-locations-to-ways

Embed node coordinates in ways. Two index strategies:

- **sparse** (default) - Rank-indexed flat mmap array. Pre-allocates `referenced.total_count() * 8` bytes; workers store `(lat, lon)` at byte offset `IdSet::rank_if_set(node_id) << 3` via atomic stores. Working-set behavior depends on the available page cache; see `reference/performance.md` for measurements.
- **external** - Double radix permutation via 4-stage pipeline. Bounded memory with high scratch-disk demand. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. See `reference/performance.md` for measured resource use.

`--index-type dense` was removed; see `reference/performance.md` for measured comparisons. `dense` survives as a hidden value (absent from `--help` and from completion) purely so passing it errors with a pointer to `sparse` rather than a bare "invalid value".

By default, untagged nodes not referenced by a relation are dropped from output.

Every blob either index type decodes must match its indexdata: a blob holding
another kind than it claims, or whose claimed ID range or count is wrong, fails
the run and nothing is written. The external index also refuses an input whose
blobs it finds out of order under the sorted header.

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (runtime routing; see `reference/pipeline.md`) |
| `--keep-untagged-nodes` | Keep all untagged nodes in output |
| `--inject-prepass` | Emit the opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata for downstream reuse |
| `--ignore-missing-nodes` | Accept ways whose node references are absent from the input, writing those vertices as `(0, 0)`. Without it the run fails and no output is written: `(0, 0)` is Null Island, a real coordinate, so a placeholder vertex cannot be told apart from genuine data by any reader. Matches osmium in name and in default. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed on unindexed input with the slower fallback. Not with `--index-type external` or `--inject-prepass`, which need indexdata whatever `--force` says |
| `--tmp-dir <DIR>` | Directory for this run's scratch files (see [Scratch files](#scratch-files)). Must exist and be writable; checked before any input is read. Default: the output's directory |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### time-filter

Filter a history PBF to a snapshot at a given timestamp.

```
pbfhogg time-filter [OPTIONS] --output <OUTPUT> <FILE> <TIMESTAMP>
```

The timestamp can be UNIX seconds or RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).

The output's replication timestamp is the earlier of the input's and the
cutoff; the input's sequence number is kept only when the input's timestamp is
known and not after the cutoff.

The input must be sorted, and is checked as it is read: an element out of
order fails the run (sort it with `sort --distrust-metadata`), as do a history
object whose versions do not strictly ascend and an object appearing twice in
a non-history input.

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

Apply an OSC diff to a sorted PBF file. Uses blob passthrough -- unmodified blobs are copied as raw bytes without decompression (under `--locations-on-ways` some are decompressed first and then copied; see below).

The output header declares no bbox (the OSC can place a node outside the
base's). Of the base's replication state it keeps the base URL and drops the
timestamp and sequence number, since the output is past them; set the new
state with `--output-header`, for example
`--output-header osmosis_replication_sequence_number=4707 --output-header osmosis_replication_timestamp=2026-04-10T00:00:00Z`.

```
pbfhogg apply-changes [OPTIONS] --output <OUTPUT> <BASE> <CHANGES>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-O, --overwrite` | Replace the output if it already exists. Without it an existing output is refused before any work is done; a symlink is refused either way |
| `--locations-on-ways` | Keep inline way-node locations current through the merge (see below) |
| `--ignore-missing-nodes` | With `--locations-on-ways`, permit unresolved references to retain the legacy `(0,0)` fallback instead of failing |
| `-j, --jobs <N>` | Worker threads for the descriptor-first pipeline. `0` (default) uses the `nproc - 2` heuristic (leaves two cores for the scanner + drain threads, minimum 2). An explicit `-j N` runs exactly `N` workers, `1` included |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed on an unindexed base with the slower fallback. Not with `--locations-on-ways`, which needs an indexed base whatever `--force` says |
| `--allow-version-regression` | Accept a change whose `version` is lower than the version already held for that object. Off by default: feeding diffs out of sequence order is refused rather than silently downgrading objects to an older state. A delete leaves a versioned tombstone, so a stale change after a delete is refused too. An equal version is always accepted and the later change wins, so overlapping diffs and regional-extract deletes at the held version apply. Elements that claim no `version` are unaffected |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

#### `--locations-on-ways`: maintaining a LocationsOnWays file

The base must be sorted, indexed and declare `LocationsOnWays`; the output
declares it too.

- A surviving base way (one the OSC neither modifies nor deletes) takes the
  OSC's coordinate for each node the OSC creates or modifies, whether its blob
  is rewritten or would have been copied, and keeps its other inline
  coordinates as they are, including one for a node the OSC deletes, which is
  counted.
- A way the OSC creates or modifies is written from the OSC's nodes and the
  base's node records; its old inline coordinates are not used. The run fails,
  publishing nothing, if a node is in neither the OSC nor the base or is
  deleted by the OSC (`--ignore-missing-nodes` writes `(0, 0)` there instead).
  It fails as soon as the base's nodes have been read, before any way is
  written, and the error counts the distinct nodes it could not resolve.

**Prepare the base with `add-locations-to-ways --keep-untagged-nodes`.** A
default `add-locations-to-ways` output drops untagged nodes, so it can fail on
an ordinary daily diff: a changed way can reference unchanged untagged nodes
whose records are in neither the base nor the OSC.

When the OSC creates or modifies any node, every way blob is read and
decompressed to find ways holding a stale coordinate (node blobs always are);
one the merge would otherwise copy is still copied unchanged unless it holds
one. That read fails the run on a
blob it cannot decompress, or whose block framing, way messages, node
references or coordinate arrays are malformed; it does not decode tag
values, metadata contents or the string table, so a blob found clean whose
errors are confined to those is still copied. Rewritten blobs are written in
the default coordinate and date framing, so values from a source block with
another `granularity`, offset or `date_granularity` are truncated to
decimicrodegrees and whole seconds there.

The summary adds a line counting the coordinates and ways refreshed, the way
blobs searched, those rewritten only for a refresh, and the references to
deleted nodes kept (counted only in the way blobs the run searched or
rewrote).

The refresh does not repair a coordinate that was already stale for a node
this OSC does not create or modify, such as one left by an older pbfhogg
version, which refreshed only the ways the OSC touched. `check --locations` finds stale
coordinates where the file holds the node's record; to rebuild a file, run
`add-locations-to-ways` on an input holding the current record of every node.

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
| `--allow-version-regression` | Requires `--simplify`, the mode that resolves conflicts; passing it alone is a usage error. Accepts a later change whose `version` is lower than one seen earlier in input order. Off by default, so listing the input files out of sequence order is an error rather than a silent downgrade. An equal version is always accepted and the later change wins |
| `-j, --jobs <N>` | Worker threads. Sizes both the per-input parse fan-out and the simplify path's chunk fan-out |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`) unless `--allow-unindexed` is set. An existing output directory is refused before any work unless `--overwrite` is set, and even then it must hold a geocode index or be empty; any other existing path is refused because publication replaces the whole directory. The output path itself cannot be a symlink. On Linux, an existing mount-point output directory is refused before the build.

```
pbfhogg build-geocode-index [OPTIONS] --output-dir <DIR> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output-dir <DIR>` | Output directory for index files |
| `--street-level <N>` | S2 cell level for streets/addresses; must exceed `--coarse-level` and fit the builder limit. Default comes from the builder configuration |
| `--coarse-level <N>` | Fallback cell level for rural areas; must be below `--street-level` and fit the builder limit. Default comes from the builder configuration |
| `--admin-level <N>` | S2 cell level for admin boundaries, within the builder's work limit. Default comes from the builder configuration |
| `--max-admin-vertices <N>` | Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value. Default comes from the builder configuration |
| `--search-radius <M>` | Fine-level max search distance in meters. Default comes from the builder configuration |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters. Default comes from the builder configuration |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `-O, --overwrite` | Replace an existing index (or empty directory) at `--output-dir` after the new build succeeds. Without it an existing `--output-dir` is refused before any work. Does not bypass the indexdata requirement |
| `--allow-unindexed` | Accept input without blob-level indexdata (slower fallback path). Does not replace an existing index |
| `--force` | Legacy shorthand equivalent to `--overwrite --allow-unindexed`; every use warns that it also grants overwrite |
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

The three cell levels are validated before any input is read; a violation exits
2 with an explicit message containing the builder's current upper bound.
Street and coarse levels are limited by the per-segment cell-covering budget,
whose sample count doubles at each level. Admin level is limited by the
polygon flood-fill budget, whose cell count grows by a factor of four at each
level. `--coarse-level` must be strictly below `--street-level`. Readers still
accept indexes written at levels up to the S2 format limit of 30.

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

A build that finds nothing queryable at all fails rather than publishing an
empty index. Indexes holding only some record kinds, such as an
admin-boundaries extract, still build.

Outputs a set of binary files. See `reference/performance.md` for measured build times, index size, and peak memory.
