# Usage

## Basic Usage

The simplest invocation takes an OpenStreetMap PBF extract and produces a PMTiles archive:

```sh
nidhogg input.osm.pbf output.pmtiles
```

For proper ocean and coastline rendering, pass an ocean shapefile:

```sh
nidhogg --ocean water-polygons-split-4326/water_polygons.shp \
        input.osm.pbf output.pmtiles
```

## Common Workflows

### Regional Extract

Process a single country or region. Downloads are available from [Geofabrik](https://download.geofabrik.de/):

```sh
# Download the extract
wget https://download.geofabrik.de/europe/germany-latest.osm.pbf

# Generate tiles for zoom 0-14
nidhogg --min-zoom 0 --max-zoom 14 \
        --ocean /data/water_polygons.shp \
        germany-latest.osm.pbf germany.pmtiles
```

### Planet Build

Processing the full planet extract (~75 GB) requires adequate disk space for temporary sort files. Expect 2-4 hours on a modern machine with 16+ cores:

```sh
nidhogg --threads 16 \
        --memory-limit 32GB \
        --temp-dir /fast-ssd/tmp \
        --ocean /data/water_polygons.shp \
        planet-latest.osm.pbf planet.pmtiles
```

::: tip
Use `--temp-dir` to point at your fastest storage. The external merge sort is I/O-bound, so an NVMe drive makes a significant difference.
:::

### Custom Zoom Range

Generate tiles for a specific zoom range only:

```sh
# Only street-level tiles
nidhogg --min-zoom 12 --max-zoom 14 input.osm.pbf streets.pmtiles

# Only overview tiles
nidhogg --min-zoom 0 --max-zoom 8 input.osm.pbf overview.pmtiles
```

### Output to MBTiles

If your tile server requires MBTiles format:

```sh
nidhogg --format mbtiles \
        --compression gzip \
        input.osm.pbf output.mbtiles
```

### Debug: Single Tile

Extract a single tile for inspection:

```sh
nidhogg --tile 14/8691/5677 input.osm.pbf tile.mvt

# Inspect with tippecanoe's tile-join or vt-cli
vt info tile.mvt
```

## CLI Reference

```
nidhogg [OPTIONS] <INPUT> <OUTPUT>

Arguments:
  <INPUT>   Path to .osm.pbf input file
  <OUTPUT>  Path to output file (.pmtiles, .mbtiles, or directory)

Options:
      --ocean <PATH>        Ocean polygon shapefile for coastline fill
      --natural-earth <PATH> Natural Earth vectors for low-zoom features
      --config <PATH>       Path to nidhogg.toml config file
      --format <FORMAT>     Output format: pmtiles, mbtiles, directory
                            [default: pmtiles]
      --compression <ALG>   Compression: zstd, gzip, brotli, none
                            [default: zstd]
      --min-zoom <N>        Minimum zoom level [default: 0]
      --max-zoom <N>        Maximum zoom level [default: 14]
      --threads <N>         Worker threads (0 = auto) [default: 0]
      --memory-limit <SIZE> Max memory for sort buffers [default: 4GB]
      --temp-dir <PATH>     Directory for temporary sort files
      --tile <Z/X/Y>        Generate a single tile (debug mode)
      --layers <LIST>       Comma-separated list of layers to include
      --exclude <LIST>      Comma-separated list of layers to exclude
  -v, --verbose             Increase log verbosity (-vv for debug)
  -q, --quiet               Suppress all output except errors
  -h, --help                Print help
  -V, --version             Print version
```

## Logging

Nidhogg uses the `RUST_LOG` environment variable for fine-grained log control:

```sh
# Show progress bars and summary (default)
nidhogg input.osm.pbf output.pmtiles

# Verbose: show per-layer statistics
nidhogg -v input.osm.pbf output.pmtiles

# Debug: show individual tile processing
RUST_LOG=nidhogg=debug nidhogg input.osm.pbf output.pmtiles

# Trace: show everything including sort internals
RUST_LOG=nidhogg=trace nidhogg input.osm.pbf output.pmtiles
```

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | General error (invalid input, I/O failure) |
| `2` | Invalid command-line arguments |
| `3` | Input file not found or unreadable |
| `4` | Output write failure (disk full, permissions) |
| `5` | Out of memory |
