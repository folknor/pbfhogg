# pbfhogg CLI

Fast command-line toolkit for OpenStreetMap PBF files. Built on the [pbfhogg](https://crates.io/crates/pbfhogg) library.

## Install

```
cargo install pbfhogg-cli
```

Requires Rust 1.96+. The binary is called `pbfhogg`.

## Commands

| Command | Description |
|---------|-------------|
| `inspect` | File inspection: metadata, block breakdown, ordering, tag frequencies |
| `check` | Validate IDs and referential integrity |
| `cat` | Concatenate PBFs with optional type filtering; `--dedupe` for sorted merge |
| `sort` | Sort into standard order (nodes, ways, relations by ID) |
| `repack` | Re-encode a PBF at a configurable `--elements-per-blob N` cap |
| `degrade` | Produce a valid-but-adversarial PBF: unsort, strip hints/locations/bbox, drop elements |
| `extract` | Extract by bounding box or GeoJSON polygon (simple/complete/smart) |
| `export` | Stream tagged nodes and ways to GeoJSON / GeoJSONSeq |
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

- `-o, --output <FILE>` - output file
- `-O, --overwrite` - replace the output if it already exists (an existing output is refused before any work without it; a symlink is refused either way)
- `--compression <SPEC>` - `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`)
- `--direct-io` - bypass page cache (Linux, requires `linux-direct-io` feature)
- `--force` - on a command that would refuse unindexed input, proceed with the slower fallback; only those commands have it. For `build-geocode-index` it is a legacy shorthand for `--overwrite --allow-unindexed` that warns every time
- `--generator <NAME>` - override writing program in output header
- `--output-header <K=V>` - set replication metadata fields
- `-j, --jobs <N>` (on the commands with worker pools that take it) - `0` (default) sizes them automatically, `1` runs sequentially, `N` caps them at `N` threads

Every short flag means the same thing on every subcommand (`-t` is always `--type`, `-c` always `--suppress-common`, `-s` always `--osmium-summary`, and so on). Shorts that used to mean something else on a command (`cat -c`, `extract -c` / `-s`, `renumber -s`, `getparents -s`, `inspect -e`, `inspect tags -s`, `tags-filter -t` / `-i`, `getid -t`) now refuse, naming the long flag to use. A flag that would have no effect on a command line is refused rather than ignored, and every flag is checked before any input is opened. The full rules are in `reference/cli-reference.md`.

## Indexdata

pbfhogg embeds blob-level index metadata (element type, ID range, spatial bbox, tag keys) in BlobHeader fields. Commands like `apply-changes`, `sort`, `tags-filter`, and `extract` use this to skip decompression of irrelevant blobs. Generate an indexed PBF with:

```
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

Commands that benefit from indexdata will error if it's missing. Pass `--force` to proceed anyway (where a mode needs indexdata whatever happens, such as `add-locations-to-ways --index-type external`, `--force` is refused instead). For `build-geocode-index`, use `--allow-unindexed` to opt into the slower fallback while keeping overwrite permission separate; its `--force` flag remains a shorthand for both `--overwrite` and `--allow-unindexed`.

## Performance

Benchmarked on Denmark (487 MB, 59M elements):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexed) | 0.036s | - | 109x vs full decode |
| sort (sorted, indexed) | 0.14s | 11.6s | 83x |
| apply-changes (indexed) | 2.7s | 7.2s | 2.7x |
| tags-filter | 0.24s | 0.56s | 2.3x |
| add-locations-to-ways | 6.5s | 12.1s | 1.9x |

At North America scale (18.8 GB, 645K-change daily diff), `apply-changes` runs in 17.3s (buffered+zlib) or 11.9s (io_uring+none), under 600 MB RSS.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or [MIT License](../LICENSE-MIT) at your option.
