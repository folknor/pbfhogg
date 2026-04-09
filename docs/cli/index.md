# CLI Overview

pbfhogg includes a command-line toolkit for common OSM PBF operations. Install it with:

```sh
cargo install pbfhogg-cli
```

The binary is called `pbfhogg`. Requires Rust 1.87+.

## Commands at a Glance

| Command | Description |
|---------|-------------|
| `inspect` | File inspection: metadata, block breakdown, ordering, tag frequencies |
| `check` | Validate IDs and referential integrity |
| `cat` | Concatenate PBFs with optional type filtering |
| `sort` | Sort into standard order (nodes, ways, relations by ID) |
| `extract` | Extract by bounding box or GeoJSON polygon (simple/complete/smart) |
| `tags-filter` | Filter elements by tag expressions (PBF or OSC input) |
| `diff` | Compare two PBFs; `--format osc` generates an OSC diff |
| `getid` | Extract or remove elements by ID |
| `getparents` | Find ways/relations referencing given IDs |
| `apply-changes` | Apply OSC diff to a sorted PBF with blob passthrough |
| `add-locations-to-ways` | Embed node coordinates in ways |
| `renumber` | Renumber all element IDs sequentially |
| `time-filter` | Filter history PBF to a point-in-time snapshot |
| `merge-changes` | Merge multiple OSC files into one |
| `build-geocode-index` | Build reverse geocoding index (S2 cells, mmap-ready) |

## Common Flags

Most write commands accept:

- `-o, --output <FILE>` — output file
- `--compression <SPEC>` — `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`)
- `--direct-io` — bypass page cache (Linux, requires `linux-direct-io` feature)
- `--force` — proceed without indexdata (slower)

See [Commands](./commands) for the full reference.
