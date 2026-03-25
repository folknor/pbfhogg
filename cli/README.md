# pbfhogg CLI

Fast command-line toolkit for OpenStreetMap PBF files. Built on the [pbfhogg](https://crates.io/crates/pbfhogg) library.

## Install

```
cargo install pbfhogg-cli
```

Requires Rust 1.85+. The binary is called `pbfhogg`.

## Commands

| Command | Description |
|---------|-------------|
| `inspect` | File inspection: metadata, block breakdown, ordering, tag frequencies |
| `check` | Validate IDs and referential integrity |
| `cat` | Concatenate PBFs with optional type filtering; `--dedupe` for sorted merge |
| `sort` | Sort into standard order (nodes, ways, relations by ID) |
| `extract` | Extract by bounding box or GeoJSON polygon (simple/complete/smart) |
| `tags-filter` | Filter elements by tag expressions (also supports OSC input) |
| `diff` | Compare two PBFs; `--format osc` generates an OSC diff |
| `getid` | Extract or remove (`--invert`) elements by ID |
| `getparents` | Find ways/relations referencing given IDs |
| `apply-changes` | Apply OSC diff to a sorted PBF with blob passthrough |
| `add-locations-to-ways` | Embed node coordinates in ways |
| `renumber` | Renumber all element IDs sequentially |
| `time-filter` | Filter history PBF to a point-in-time snapshot |
| `merge-changes` | Merge multiple OSC files into one |
| `build-geocode-index` | Build a reverse geocoding index (S2 cells, mmap-ready) |

## Common flags

Most write commands accept:

- `-o, --output <FILE>` — output file
- `--compression <SPEC>` — `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`)
- `--direct-io` — bypass page cache (Linux, requires `linux-direct-io` feature)
- `--force` — proceed without indexdata (slower)
- `--generator <NAME>` — override writing program in output header
- `--output-header <K=V>` — set replication metadata fields

## Indexdata

pbfhogg embeds blob-level index metadata (element type, ID range, spatial bbox, tag keys) in BlobHeader fields. Commands like `apply-changes`, `sort`, `tags-filter`, and `extract` use this to skip decompression of irrelevant blobs. Generate an indexed PBF with:

```
pbfhogg cat input.osm.pbf --type node,way,relation -o indexed.osm.pbf
```

Commands that benefit from indexdata will error if it's missing. Pass `--force` to proceed anyway.

## Performance

Benchmarked on Denmark (487 MB, 59M elements):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexed) | 0.036s | — | 109x vs full decode |
| sort (sorted, indexed) | 0.14s | 11.6s | 83x |
| apply-changes (indexed) | 2.7s | 7.2s | 2.7x |
| tags-filter | 0.24s | 0.56s | 2.3x |
| add-locations-to-ways | 6.5s | 12.1s | 1.9x |

At North America scale (18.8 GB, 645K-change daily diff), `apply-changes` runs in 17.3s (buffered+zlib) or 11.9s (io_uring+none), under 600 MB RSS.

## License

Apache-2.0
