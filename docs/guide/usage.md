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

This passthrough path adds indexdata without re-compressing blobs - minimal memory, suitable for planet-scale files (87 GB planet in ~8 minutes). No `--type` flag means passthrough; adding `-t` does full decode and re-encode.

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

To preserve and update inline way-node coordinates through the merge (avoids re-running `add-locations-to-ways` after each update):

```sh
pbfhogg apply-changes denmark.osm.pbf changes.osc.gz -o updated.osm.pbf --locations-on-ways
```

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

The recommended workflow is to generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations.

## Common flags

Most write commands accept these flags:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `--compression <SPEC>` | Compression: `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`) |
| `--direct-io` | Bypass page cache via O_DIRECT (Linux, requires `linux-direct-io` feature) |
| `--io-uring` | Use io_uring for output writes (Linux, requires `linux-io-uring` feature) |
| `--force` | Proceed without indexdata (slower fallback path) |
| `--generator <NAME>` | Override the writing program name in the output header |
| `--output-header <K=V>` | Set replication metadata fields in the output header |
