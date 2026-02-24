# Getting Started

## What is Nidhogg?

Nidhogg is a vector tile generator that reads OpenStreetMap data in PBF format and produces [Shortbread](https://shortbread-tiles.org/)-schema vector tiles packaged as PMTiles, MBTiles, or a directory of individual `.mvt` files.

It's designed for the common case: you have an OSM extract, you want production-ready vector tiles, and you don't want to set up a database or learn a new configuration language. One binary, one command.

## Quick Start

Install Nidhogg and generate your first tileset in under a minute:

```sh
# Install
cargo install nidhogg

# Download an extract (e.g., Monaco — tiny, great for testing)
wget https://download.geofabrik.de/europe/monaco-latest.osm.pbf

# Generate tiles
nidhogg monaco-latest.osm.pbf monaco.pmtiles
```

That's it. The output is a single `.pmtiles` file you can open in [PMTiles Viewer](https://protomaps.github.io/PMTiles/) or serve from any static file host.

## Adding Ocean Fill

Without an ocean shapefile, water areas will be transparent. For proper coastlines and ocean rendering:

```sh
# Download the ocean shapefile (~800 MB)
wget https://osmdata.openstreetmap.de/download/water-polygons-split-4326.zip
unzip water-polygons-split-4326.zip

# Generate tiles with ocean fill
nidhogg --ocean water-polygons-split-4326/water_polygons.shp \
        monaco-latest.osm.pbf monaco.pmtiles
```

## Serving Tiles

### From Cloud Storage

PMTiles can be served directly from S3, GCS, R2, or any HTTP server that supports range requests:

```sh
# Upload to S3
aws s3 cp monaco.pmtiles s3://my-bucket/tiles/monaco.pmtiles

# Or serve locally with any HTTP server
python -m http.server 8080
```

Then point your map renderer at `http://localhost:8080/monaco.pmtiles` using a PMTiles-aware client like [MapLibre GL JS](https://maplibre.org/) with the [pmtiles protocol](https://docs.protomaps.com/pmtiles/maplibre).

### With a Tile Server

If you prefer MBTiles, generate that format instead and serve with [Martin](https://martin.maplibre.org/) or TileServer GL:

```sh
nidhogg --format mbtiles monaco-latest.osm.pbf monaco.mbtiles
martin monaco.mbtiles
```

## Next Steps

- [Installation](./install) — building from source, pre-built binaries, Docker
- [Configuration](./configuration) — config file reference, environment variables
- [Usage](./usage) — CLI reference, common workflows, examples
- [Advanced](./advanced) — performance tuning, custom layers, plugins
- [API Reference](/api/) — Rust library API documentation
