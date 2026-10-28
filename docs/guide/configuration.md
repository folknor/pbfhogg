# Configuration

pbfhogg has no configuration file. All behavior is controlled via CLI flags.

## Compression

Control output blob compression with `--compression`:

```sh
pbfhogg sort input.osm.pbf -o sorted.osm.pbf --compression zstd
pbfhogg repack input.osm.pbf -o output.osm.pbf --compression zlib:9
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o updated.osm.pbf --compression none
```

The default, `zlib`, produces standard PBF files every tool reads. Use `none` for intermediate files or when the filesystem handles compression (erofs). Use `zstd` for a better compression ratio and faster decompression, where the tools that read the file support it. The accepted values and levels, each command's default, and which blobs the flag reaches are in the CLI reference's [Common flags](../cli/commands.md#common-flags).

## Index type for add-locations-to-ways

Select the node coordinate index strategy with `--index-type`:

```sh
pbfhogg add-locations-to-ways input.osm.pbf -o output.osm.pbf --index-type auto
```

Options: `sparse` (default), `external`, `auto`. See [Advanced Topics](./advanced.md#add-locations-to-ways-index-types) for when to use each.

## Output header metadata

### Generator string

Override the writing program name in the output header:

```sh
pbfhogg sort input.osm.pbf -o sorted.osm.pbf --generator "my-pipeline/1.0"
```

By default, the output header names `pbfhogg/<version>` as the writing program, so a file can be traced to the release that wrote it. An empty `--generator ""` is written as an empty writing program; it does not remove the field.

### Replication metadata

Set replication metadata fields in the output header with `--output-header`:

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o updated.osm.pbf \
  --output-header osmosis_replication_timestamp=2026-04-09T00:00:00Z \
  --output-header osmosis_replication_sequence_number=4706 \
  --output-header osmosis_replication_base_url=https://download.geofabrik.de/europe/denmark-updates
```

The keys, and how each command derives the header it writes, are in the CLI reference's [Output header](../cli/commands.md#output-header).

`apply-changes` drops the base's replication timestamp and sequence number (the output is past them, and an OSC does not say where to) and keeps its base URL, so an updater should stamp the state of the last diff it applied, as above.

## I/O mode flags

### O_DIRECT

Bypass the page cache for reads and writes. Useful at planet scale to prevent cache pollution:

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
```

Requires the `linux-direct-io` feature at compile time and a real filesystem (not tmpfs).

### io_uring

Use io_uring for output writes with pre-registered buffers:

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

Requires the `linux-io-uring` feature at compile time, Linux 5.1+, and sufficient `RLIMIT_MEMLOCK`.

## RLIMIT_MEMLOCK for io_uring

io_uring with registered buffers needs to pin memory pages. The default 64-buffer pool requires about 16 MB of locked memory. If the limit is too low, the run is refused at setup with exit code 3 and an `Error: platform feature io-uring unavailable: ...` line (kernels before 5.12 can report it as a generic exit 2 instead; see [Exit codes](../cli/commands.md#exit-codes)). Raise the limit:

```sh
ulimit -l unlimited
```

For a permanent change, add to `/etc/security/limits.conf`:

```
your-user  soft  memlock  unlimited
your-user  hard  memlock  unlimited
```

Or set a specific value (in KB):

```
your-user  soft  memlock  65536
your-user  hard  memlock  65536
```
