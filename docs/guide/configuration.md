# Configuration

Nidhogg can be configured via command-line flags, a TOML configuration file, or environment variables. When multiple sources are present, the precedence order is: **CLI flags > environment variables > config file > defaults**.

## Config File

By default, Nidhogg looks for `nidhogg.toml` in the current directory. You can specify a different path with `--config`:

```sh
nidhogg --config /etc/nidhogg/production.toml input.osm.pbf output.pmtiles
```

### Example Configuration

```toml
[input]
ocean_shapefile = "/data/water-polygons-split-4326/water_polygons.shp"
natural_earth   = "/data/natural_earth_vector.gpkg"

[output]
format       = "pmtiles"       # "pmtiles" | "mbtiles" | "directory"
min_zoom     = 0
max_zoom     = 14
compression  = "zstd"          # "gzip" | "zstd" | "brotli" | "none"
tile_size    = 4096

[processing]
threads      = 0               # 0 = auto-detect (number of CPUs)
memory_limit = "4GB"           # max memory for sort buffers
temp_dir     = "/tmp/nidhogg"  # scratch space for external sort
batch_size   = 50000           # features per batch

[layers]
# Selectively enable or disable Shortbread layers.
# All layers are enabled by default.
include = ["transportation", "buildings", "water", "landuse", "pois"]
# Or exclude specific layers:
# exclude = ["boundary_labels"]

[simplification]
tolerance = 1.0                # Douglas-Peucker tolerance in tile units
area_threshold = 4.0           # drop polygons smaller than this (tile units^2)
```

## Environment Variables

All configuration options can be set via environment variables with the `NIDHOGG_` prefix. Nested keys use double underscores:

| Variable | Config Equivalent | Example |
|---|---|---|
| `NIDHOGG_THREADS` | `processing.threads` | `8` |
| `NIDHOGG_MEMORY_LIMIT` | `processing.memory_limit` | `8GB` |
| `NIDHOGG_OUTPUT__FORMAT` | `output.format` | `mbtiles` |
| `NIDHOGG_OUTPUT__COMPRESSION` | `output.compression` | `zstd` |
| `NIDHOGG_OUTPUT__MIN_ZOOM` | `output.min_zoom` | `0` |
| `NIDHOGG_OUTPUT__MAX_ZOOM` | `output.max_zoom` | `14` |

## Output Formats

### PMTiles

The default and recommended format. Produces a single `.pmtiles` file that can be served directly from cloud storage (S3, GCS, R2) without a tile server.

```toml
[output]
format = "pmtiles"
compression = "zstd"
```

### MBTiles

SQLite-based format compatible with most tile servers (Martin, TileServer GL, etc.).

```toml
[output]
format = "mbtiles"
compression = "gzip"
```

### Directory

Writes individual `.mvt` files to a directory tree. Useful for debugging or serving from a static file server.

```toml
[output]
format = "directory"
compression = "none"
```

The directory structure follows the `{z}/{x}/{y}.mvt` convention.

## Zoom Levels

Control which zoom levels are generated:

```toml
[output]
min_zoom = 0    # coarsest level (whole world in one tile)
max_zoom = 14   # most detailed level
```

Higher max zoom values produce significantly more tiles. As a rough guide:

| Max Zoom | Approx. Tiles (planet) | Typical Use |
|---|---|---|
| 10 | ~1M | Country-level overview |
| 12 | ~17M | City-level detail |
| 14 | ~270M | Street-level detail |
| 16 | ~4.3B | Building-level detail |

## Tile Size

The `tile_size` option controls the extent of the vector tile coordinate space. The default is `4096`, which is the standard for Mapbox Vector Tiles.

::: warning
Changing the tile size from the default `4096` may cause rendering issues with some map renderers. Only change this if you know what you're doing.
:::
