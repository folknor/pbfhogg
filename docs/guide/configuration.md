# Configuration

pbfhogg has no configuration file. All behavior is controlled via CLI flags.

## Compression

Control output blob compression with `--compression`:

```sh
pbfhogg sort input.osm.pbf -o sorted.osm.pbf --compression zstd
pbfhogg cat input.osm.pbf -o output.osm.pbf --compression zlib:9
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o updated.osm.pbf --compression none
```

The default is `zlib` (level 6), which produces standard PBF files compatible with all tools. Use `none` for intermediate files or when the filesystem handles compression (erofs). Use `zstd` for better compression ratio and faster decompression.

## Index type for add-locations-to-ways

Select the node coordinate index strategy with `--index-type`:

```sh
pbfhogg add-locations-to-ways input.osm.pbf -o output.osm.pbf --index-type auto
```

Options: `dense` (default), `sparse`, `external`, `auto`. See [Advanced Topics](./advanced#add-locations-to-ways-index-types) for details on each strategy and when to use them.

## Output header metadata

### Generator string

Override the writing program name in the output header:

```sh
pbfhogg sort input.osm.pbf -o sorted.osm.pbf --generator "my-pipeline/1.0"
```

By default, the output header identifies `pbfhogg` as the writing program.

### Replication metadata

Set replication metadata fields in the output header with `--output-header`:

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o updated.osm.pbf \
  --output-header osmosis_replication_timestamp=2026-04-09T00:00:00Z \
  --output-header osmosis_replication_sequence_number=4706 \
  --output-header osmosis_replication_base_url=https://download.geofabrik.de/europe/denmark-updates
```

Supported keys: `osmosis_replication_timestamp`, `osmosis_replication_sequence_number`, `osmosis_replication_base_url`.

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

io_uring with registered buffers needs to pin memory pages. The default 64-buffer pool requires about 16 MB of locked memory. If the limit is too low, pbfhogg will print an error suggesting the fix:

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
