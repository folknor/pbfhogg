# Dataset output promotion

`pbfhogg-datasets promote` registers a completed `pbfhogg cat`, `repack` or
`degrade` output as a named dataset snapshot. Run the producer and then
promote its output, both through `brokkr run`:

```sh
brokkr run pbfhogg -- cat INPUT.osm.pbf -o OUTPUT.osm.pbf
brokkr run --with pbfhogg pbfhogg-datasets -- promote denmark OUTPUT.osm.pbf --producer cat --as-snapshot copied
```

See [dataset acquisition and registry contracts](datasets.md).

Global `--config PATH` and `--host HOST` select the composed registry just
as for download. Promotion needs neither curl nor a handed pbfhogg binary.
The dataset must already be registered. Snapshot keys match
`[A-Za-z0-9_-]+`; `base` is reserved. Dataset keys used for promotion must
also match that alphabet because they enter output filenames.

`--producer cat` and `--producer repack` register `pbf.indexed`.
`--producer degrade --strip-indexdata` registers `pbf.raw`; degrade without
that flag registers `pbf.indexed`. The producer and strip flag describe the
completed file, they do not execute a command or inspect its contents.
The caller must supply the actual output of a successful producer and the
flags used to produce it. Promotion checks a nonempty regular file and its
byte digest, not PBF validity or element identity. In particular it does not
create the old typed repack's `expect.diff-snapshots` same-elements fact:
importing a file supplies no evidence of what source was repacked.

An existing snapshot refuses unless `--replace-snapshot` is given:

```sh
brokkr run pbfhogg -- degrade INPUT.osm.pbf --strip-indexdata -o stripped.osm.pbf
brokkr run --with pbfhogg pbfhogg-datasets -- promote denmark stripped.osm.pbf --producer degrade --strip-indexdata --as-snapshot copied --replace-snapshot
```

Replacement removes the entire old snapshot tree, including OSCs and
per-file expectations, in the same registry transaction that installs the
new PBF entry. The primary and other snapshots are preserved. A valid
`YYYYMMDD` key supplies the snapshot's `download_date`; other keys use today
in UTC. Replication sequences are not inferred or copied.

Promotion copies the input into the selected host's data directory. It
never consumes or modifies the input. Published names are
`DATASET-SNAPSHOT-VARIANT-XXH128.osm.pbf`, with the observed 32-digit
XXH3-128 digest. The per-dataset `.DATASET.download.lock` is shared with
acquisition and held through registry commit, without modifying its source
identity record. brokkr holds its machine lock around the whole invocation.

The copy is staged as a hidden sibling, checked against the input digest,
fsynced, renamed, and followed by directory fsync. Only then does devkit's
registry writer atomically replace the owning config file. The writer
preserves the owning include and legacy or explicit host namespace; split
ownership refuses. Existing final files are reused only when regular and
byte-equal. Symlinks and different bytes refuse. Publication never clears a
registered staging path or fills an absent registered destination.

Digest-qualified names keep replacement safe across process death: before
the registry commit the old registry still names unchanged old bytes. A
failed commit can leave an unregistered complete file; retry verifies its
digest and reuses it. Promotion retains displaced files, including shared
files, rather than deleting data after the registry changes. No automatic
garbage collection is provided. It also leaves the caller's output in place.
