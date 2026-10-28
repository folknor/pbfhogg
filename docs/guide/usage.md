# Basic Usage

## Inspecting a PBF file

Get an overview of any PBF file - block breakdown, element counts, ordering analysis:

```sh
pbfhogg inspect denmark.osm.pbf
```

Check if a PBF has blob-level indexdata (exit code 0 = indexed, 1 = not):

```sh
pbfhogg inspect --indexed denmark.osm.pbf
```

Count tag key=value frequencies:

```sh
pbfhogg inspect tags denmark.osm.pbf
```

For extended analysis (timestamp range, data bbox, metadata coverage):

```sh
pbfhogg inspect --extended denmark.osm.pbf
```

## Generating indexdata

Many commands are faster with blob-level indexdata. Generate an indexed PBF with `cat`:

```sh
pbfhogg cat denmark-raw.osm.pbf -o denmark.osm.pbf
```

This passthrough path adds indexdata without re-compressing blobs, with minimal memory, so it suits planet-scale files; see [Performance](./performance) for measurements. A blob that already carries indexdata is copied as it is; `cat --reindex` rebuilds indexdata from the payload when it is suspect.

## Extracting a region

Extract by bounding box:

```sh
pbfhogg extract denmark.osm.pbf -o copenhagen.osm.pbf -b 12.4,55.6,12.7,55.8
```

The default strategy is complete-ways (two passes, all way nodes included). For faster extraction with possible dangling refs:

```sh
pbfhogg extract denmark.osm.pbf -o copenhagen.osm.pbf -b 12.4,55.6,12.7,55.8 --simple
```

For complete multipolygon and boundary relations:

```sh
pbfhogg extract denmark.osm.pbf -o copenhagen.osm.pbf -b 12.4,55.6,12.7,55.8 --smart
```

Extract by GeoJSON polygon:

```sh
pbfhogg extract denmark.osm.pbf -o region.osm.pbf -p boundary.geojson
```

## Applying a daily diff

Apply an OSC change file to a sorted PBF:

```sh
pbfhogg apply-changes denmark.osm.pbf changes.osc.gz -o updated.osm.pbf
```

To keep a file with inline way-node coordinates (`LocationsOnWays`) current
without re-running `add-locations-to-ways` after each update, build it once
from a sorted, indexed PBF with untagged nodes kept (`add-locations-to-ways`
keeps its order and indexdata), then apply each diff with
`--locations-on-ways`:

```sh
pbfhogg add-locations-to-ways denmark.osm.pbf -o denmark-low.osm.pbf --keep-untagged-nodes
pbfhogg apply-changes denmark-low.osm.pbf changes.osc.gz -o updated.osm.pbf --locations-on-ways
```

Each way's inline coordinates get the new position of every node the diff
moves, whether the diff touches the way or not. The kept untagged nodes are
what makes this work: a way the diff changes takes the coordinates of its
unchanged nodes from the base's node records, and a default
`add-locations-to-ways` output, which drops untagged nodes, lacks many of
them, so the update can fail on an ordinary daily diff.

## Filtering by tags

Filter elements by tag expressions:

```sh
pbfhogg tags-filter denmark.osm.pbf -o highways.osm.pbf "highway=primary" "highway=secondary"
```

By default, matched relations pull in their member ways, nodes, and nested relations transitively. For direct matches only (faster, single pass):

```sh
pbfhogg tags-filter denmark.osm.pbf -o restaurants.osm.pbf -R "amenity=restaurant"
```

## Sorting

Sort a PBF into standard order (nodes, ways, relations by ID):

```sh
pbfhogg sort unsorted.osm.pbf -o sorted.osm.pbf
```

## Extracting elements by ID

```sh
pbfhogg getid denmark.osm.pbf -o subset.osm.pbf n123 w456 r789
```

Remove specific elements (keep everything else):

```sh
pbfhogg getid denmark.osm.pbf -o filtered.osm.pbf --invert n123 w456
```

## The --force flag

Commands that benefit from indexdata (`apply-changes`, `sort`, `extract`, `tags-filter`, `getid`, `add-locations-to-ways`, and others) will error if the input PBF lacks it. Pass `--force` to proceed without indexdata - the command will work but use slower fallback paths:

```sh
pbfhogg sort raw-input.osm.pbf -o sorted.osm.pbf --force
```

`renumber` has no such escape and `build-geocode-index` uses `--allow-unindexed`; see [Indexdata](./indexdata#which-commands-require-indexdata) for the per-command rules.

The recommended workflow is to generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations.

## Common flags

Most write commands accept these flags:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `-O, --overwrite` | Replace the output if it already exists |
| `--compression <SPEC>` | Compression: `none`, `zlib` (default; `renumber` defaults to `zlib:1`), `zstd`, or with level (`zlib:9`, `zstd:19`) |
| `--direct-io` | Bypass page cache via O_DIRECT (Linux, requires `linux-direct-io` feature) |
| `--io-uring` | Use io_uring for output writes (Linux, requires `linux-io-uring` feature) |
| `--force` | Proceed without indexdata (slower fallback path) |
| `--generator <NAME>` | Override the writing program name in the output header |
| `--output-header <K=V>` | Set replication metadata fields in the output header |

An existing output is refused before any work is done unless `-O,
--overwrite` is given, and a symlink is never accepted as an output. Every
output is written to a hidden staging file (beside it by default;
`--staging-dir` moves it) and renamed into place
only when the command succeeds, so a failed run never leaves a partial file
under the output name. The [CLI reference](../cli/commands#output-files) has
the details.
