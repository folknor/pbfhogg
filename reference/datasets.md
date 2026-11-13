# Development datasets

The separate `pbfhogg-datasets` binary acquires development inputs and
registers them in the composed brokkr host config. It links devkit, never
pbfhogg. Run it under brokkr's machine lock, with the built CLI handed in:

```sh
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark
brokkr run --with pbfhogg pbfhogg-datasets -- --config brokkr.toml --host HOST download europe/france
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark --osc-seq 5000
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark --as-snapshot 20261113
```

Global `--config PATH` defaults to `brokkr.toml`; `--host HOST` defaults
to the kernel hostname. `--pbfhogg PATH` can explicitly supply the CLI
instead of `BROKKR_WITH_EXE`. The tool never invokes brokkr itself.
Downloading is acquisition, not a benchmark. For completed cat, repack or
degrade output use [snapshot promotion](dataset-promotion.md).

## Sources and registered state

Short region aliases and full Geofabrik paths select a dataset key and
source independently of the registry. Planet uses planet.openstreetmap.org.
The dataset's `origin` must equal the requested exact source:
`download.geofabrik.de/PATH` or `planet.openstreetmap.org`. Legacy
`Geofabrik` means the alias path for an alias key only; other keys refuse.
Missing or mismatched origins refuse.

Primary acquisition is a no-op for configured, valid entries. Every missing
target entry is registered, including recoverable unregistered raw files and
OSCs. Registered files must exist and pass their configured XXH3-128 pins;
pins and configured replication sequences are never rewritten.
Unregistered indexed files are always regenerated from the selected raw
with the handed CLI's `pbfhogg cat`, never adopted.

Transfers use curl with its config disabled, bounded stalls and retries.
Children start with a cleared environment; curl receives only the boundary's
proxy and CA settings. No in-process HTTP client or library cat is used.
The separate binary keeps child processes out of the shipped pbfhogg CLI,
which promises to start none. Using the handed CLI also ensures indexed
files match shipped CLI output without constructing a production library
Host. Curl is resolved from PATH to an absolute executable at the boundary;
`-q` is its first argument, and transfer bounds live in
`crates/datasets/src/tunables.rs`.

## OSC continuity and snapshots

`--osc-seq N` fills every missing sequence from the selected raw's base
sequence plus one through N, including holes. A configured raw `seq` wins.
Otherwise header-only CLI inspection may learn it, but only when the
replication URL matches the selected source's OSC stream (a trailing slash
is accepted). Inspection uses `--get header.replication.sequence` and
`--get header.replication.url`; plain `--json` would scan the entire file.
A learned seq is
registered on raw and indexed entries lacking one. Planet downloads day
diffs while its PBF header describes minutely replication, so planet requires
an explicitly configured day sequence.

`--as-snapshot KEY` requires an existing dataset and a new key matching
`[A-Za-z0-9_-]+`, excluding reserved `base`. It acquires independent
raw/indexed files without changing the primary. A valid Gregorian
`YYYYMMDD` key supplies the date; other keys use today in UTC.
Snapshot continuity comes from that snapshot alone.

To extend an existing snapshot:

```sh
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark --snapshot 20261113 --osc-seq 5000
```

`--snapshot` requires `--osc-seq` and conflicts with `--as-snapshot`
and `--refresh`. It transfers no PBF and changes no dates. For a new
planet snapshot, acquire it without OSCs, configure its raw day seq, then
extend it this way.

Missing OSC entries may share registered diffs of the same sequence from
this dataset's primary or other snapshots, including custom filenames.
Every donor must exist, pass its pin, and agree in byte digest with every
other registered copy. Conflicting bytes refuse. Sharing never crosses
datasets, rewrites donor registrations or changes donor pins. All extension
registrations and learned seq values commit atomically.

## Refresh

```sh
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark --refresh
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark --refresh --force
```

Refresh archives the entire old primary PBF/OSC generation under its date
and installs a new primary in one registry transaction. The archive date
comes from valid `download_date`, otherwise raw mtime in UTC. Existing
archive keys and same-day replacement refuse. All retained files and pins
are validated before rotation.

Only the final HEAD response's Last-Modified establishes upstream newness.
Missing, invalid or non-newer dates require `--force`; HEAD errors still
fail even with force. Force requires refresh; refresh conflicts with
`--as-snapshot`. Canonical destination collisions with any registered
primary or snapshot on the selected host refuse, including symlink aliases.
With `--osc-seq`, continuity comes from the new raw only, never the
archived sequence. Old seq values and OSCs archive together.

## Locks, publication and registry ownership

The permanent `DATA/.KEY.download.lock` uses a nonblocking exclusive flock
held through registry commit. It is never unlinked, renamed or explicitly
unlocked. Each acquisition child inherits the lock as stdin, so an orphaned
writer prevents retry until it exits. This assumes curl and pbfhogg keep
stdin open until exit.

The lock's content records the source identity of potentially unregistered
acquisition files. Raw and OSC adoption requires an already matching
identity; a torn record matches nothing. Identity is rewritten only when
nothing is adoptable. Stale staged files are removed under the lock before
their writer starts. Registered shared diffs do not require a recovery
identity. Retry preserves registered pins and can recover completed,
unregistered publication safely.

Publication writes a staged sibling, fsyncs it, renames it to the final
name, fsyncs the directory, then hashes the file. One atomic registry write
follows, using its own staged write, fsync, rename and directory fsync.
A final acquisition filename therefore denotes complete bytes; failure
before registration can leave recoverable unregistered files.

Devkit edits the one project/include file owning the dataset table.
Dataset ownership split across files refuses. A new dataset goes into the
one file holding that host's datasets, otherwise the project config.
The writer preserves legacy `[HOST]` or explicit `[host.HOST]` spelling;
both spellings together refuse. It writes brokkr host-schema keys only.
Paths resolve through host `data` relative to the root project directory,
not an include's parent. Provenance chooses the file to edit, not path
resolution. See [library dataset resolution](library-protocol.md#dataset-resolution)
for composition and input digest checking.
