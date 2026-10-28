# CLI Overview

pbfhogg includes a command-line toolkit for common OSM PBF operations. Install it with:

```sh
cargo install pbfhogg-cli
```

The binary is called `pbfhogg`. Requires the Rust version named by `rust-version` in the workspace `Cargo.toml`.

## Commands at a Glance

| Command | Description |
|---------|-------------|
| `inspect` | File inspection: metadata, block breakdown, ordering, tag frequencies |
| `check` | Validate IDs and referential integrity |
| `cat` | Concatenate PBFs with optional type filtering |
| `sort` | Sort into standard order (nodes, ways, relations by ID) |
| `repack` | Re-encode a PBF with a configurable per-blob element cap |
| `degrade` | Produce a valid-but-adversarial PBF for benchmarking non-optimal paths |
| `extract` | Extract by bounding box or GeoJSON polygon (simple/complete/smart) |
| `tags-filter` | Filter elements by tag expressions (PBF or OSC input) |
| `export` | Stream tagged nodes/ways to GeoJSON or GeoJSONSeq |
| `diff` | Compare two PBFs; `--format osc` generates an OSC diff |
| `digest` | Print a digest of a PBF's element content, equal for files `diff` finds identical |
| `getid` | Extract or remove elements by ID |
| `getparents` | Find ways/relations referencing given IDs |
| `apply-changes` | Apply OSC diff to a sorted PBF with blob passthrough |
| `add-locations-to-ways` | Embed node coordinates in ways |
| `renumber` | Renumber all element IDs sequentially |
| `time-filter` | Filter history PBF to a point-in-time snapshot |
| `merge-changes` | Merge multiple OSC files into one |
| `build-geocode-index` | Build reverse geocoding index (S2 cells, mmap-ready) |

The benchmark instruments `bench-read`, `bench-write` and `bench-merge` are
commands too.

## Flags

`pbfhogg <command> --help` lists a command's flags. [Commands](./commands.md)
is the full reference: every flag, the [flags most write commands
share](./commands.md#common-flags), the exit codes and the output contract.
