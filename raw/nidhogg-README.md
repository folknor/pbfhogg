<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="nidhogg-logo-text-dark.svg">
    <img src="nidhogg-logo-text.svg" width="300" alt="Nidhogg">
  </picture>
  <br>
  <em>A self-hosted OpenStreetMap stack in Rust</em>
</p>

<p align="center">
  <a href="https://github.com/folknor/nidhogg/actions"><img src="https://img.shields.io/github/actions/workflow/status/folknor/nidhogg/ci.yml?label=CI&logo=github" alt="CI"></a>
  <img src="https://img.shields.io/badge/platform-linux-informational?logo=linux&logoColor=white" alt="Linux">
  <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
</p>

---

Replaces Overpass API, Nominatim, and external tile servers with a single binary that reads `.osm.pbf` files directly.

**Linux-only** — Nidhogg uses mmap for all data access, erofs for read-only compressed data images, and is deployed as a systemd service. These are Linux kernel features with no portable equivalents. This is a deliberate choice: the target is a dedicated Linux server, and leaning into the platform lets us use the kernel as our caching layer, compression layer, and process manager instead of reimplementing them in userspace.

The project contains a vendored and heavily modified copy of [osmpbf](https://github.com/b-r-u/osmpbf), because we needed zero-copy protobuf deserialization, a three-stage pipelined reader with a dedicated decompression thread pool (isolated from the global rayon pool to avoid contention during tilegen), zlib-ng for 2-3x faster decompression, and a fix for decompression buffer pre-allocation that was wasting ~2.4 GB in allocations on Denmark alone.

## Requirements

- Rust (stable)
- A C compiler and cmake — needed by zlib-ng, which is used for fast gzip decompression in both PBF parsing and tile generation

## Quick start

```bash
# Build
cargo build --release

# Download a PBF extract (e.g. Denmark, ~460 MB)
mkdir -p data
wget -O data/denmark-latest.osm.pbf https://download.geofabrik.de/europe/denmark-latest.osm.pbf

# Ingest into disk format
./target/release/nidhogg ingest data/denmark-latest.osm.pbf data/denmark

# Start the server (queries + geocoding)
./target/release/nidhogg serve data/denmark
# → listening on http://localhost:3033

# Generate vector tiles (optional, for map rendering)
scripts/download_ocean.sh
./target/release/nidhogg tilegen data/denmark-latest.osm.pbf data/denmark.pmtiles \
  --ocean data/water-polygons-split-3857/water_polygons.shp

# Serve with tiles enabled
./target/release/nidhogg serve data/denmark --tiles data/denmark.pmtiles
```

## Features

**Spatial queries** — Bbox + tag filter queries returning ways and relations with inline geometry as JSON. Drop-in replacement for Overpass API `[out:json]` queries.

**Geocoding** — Full-text place name search powered by Tantivy. Returns coordinates and address details. Replaces Nominatim.

**Vector tile server** — Serves Shortbread-schema MVT tiles from a PMTiles v3 archive. Hilbert-ordered, gzip-compressed, ready for OpenLayers/MapLibre.

**Tile generation** — Native Rust tile pipeline. Reads a PBF, processes ocean shapefiles, and produces a PMTiles archive with 24 layers at z0-14. Parallelized with rayon. Planetiler on the dev computer processes denmark-latest.osm.pbf with ocean data in 2 minutes and 1 second, while nidhogg processes the same data with the same end result (though Planetiler does support an extremely large set of both input and output formats, and we do not - on purpose) in roughly 47 seconds.

**Disk-backed storage** — Tile-sorted, mmap'd binary format. Queries run directly against memory-mapped files with no server-side caching layer needed.

## CLI

### `nidhogg ingest <pbf> <output-dir>`

Parse a PBF file and write the disk-backed storage format (flat indices + tile-sorted data).

### `nidhogg serve <data-dir> [flags]`

Start the HTTP server on the given data directory.

| Flag | Description |
|------|-------------|
| `--tiles <path.pmtiles>` | Enable vector tile serving from a PMTiles archive |

Port defaults to `3033`, override with `PORT` env var.

The geocode index is auto-detected at `<data-dir>/geocode_index/`.

### `nidhogg tilegen <pbf> <out.pmtiles> [flags]`

Generate a PMTiles v3 vector tile archive from a PBF file.

| Flag | Description |
|------|-------------|
| `--tmp-dir <path>` | Temp directory for sort chunks and indices (default: `.tilegen_tmp`) |
| `--ocean <path.shp>` | Ocean shapefile for water polygon layers |
| `--skip-to <phase>` | Resume from a checkpoint: `ocean` or `sort` |
| `--in-memory` | Keep tile blob in RAM instead of streaming to a temp file |

Pipeline phases: PBF read, ocean shapefile, external merge sort, tile assembly + PMTiles write.

## REST API

### `POST /api/query`

Spatial query for ways and relations within a bounding box, with optional tag filters.

**Request body:**

```json
{
  "bbox": [55.67, 12.55, 55.69, 12.59],
  "query": [
    { "highway": ["primary", "secondary"] }
  ]
}
```

- `bbox` — `[south, west, north, east]` in WGS84 degrees
- `query` — array of tag filter objects (OR between objects, AND between keys within an object). Values can be an array of accepted values, or `true` to match any value.

**Response:**

```json
{
  "elements": [
    {
      "type": "way",
      "id": 4578041,
      "tags": { "highway": "primary", "name": "Vesterbrogade" },
      "geometry": [
        { "lat": 55.6731, "lon": 12.5563 },
        { "lat": 55.6728, "lon": 12.5571 }
      ]
    },
    {
      "type": "relation",
      "id": 123456,
      "tags": { "type": "multipolygon", "building": "yes" },
      "members": [
        {
          "type": "way",
          "ref": 789,
          "role": "outer",
          "geometry": [{ "lat": 55.68, "lon": 12.57 }]
        }
      ]
    }
  ]
}
```

### `GET /api/geocode?q=<query>`

Full-text place name search.

| Param | Description |
|-------|-------------|
| `q` | Search string (e.g. `"Copenhagen"`) |

**Response:**

```json
[
  {
    "display_name": "Copenhagen",
    "lat": 55.6761,
    "lon": 12.5683,
    "city": "Copenhagen",
    "country": "Denmark"
  }
]
```

### `GET /api/tiles/{z}/{x}/{y}`

Fetch a single vector tile. Returns gzip-compressed MVT (Mapbox Vector Tile) data.

Available when the server is started with `--tiles`. Tiles follow the [Shortbread](https://shortbread-tiles.org/) schema with 24 layers.

All endpoints support CORS (permissive) and gzip response compression.

## Acknowledgements

This project stands on the shoulders of excellent open-source work:

- [OpenStreetMap](https://www.openstreetmap.org/) — the map data that makes all of this possible, and the community that maintains it.
- [Geofabrik](https://www.geofabrik.de/) — PBF extracts, the [Shortbread](https://shortbread-tiles.org/) tile schema, and ocean polygon shapefiles.
- [Planetiler](https://github.com/onthegomap/planetiler) — the Java tile generation tool that served as both our original tile pipeline and the benchmark to beat. Our Shortbread layer definitions and area-based zoom thresholds are adapted from Planetiler's profiles.
- [osmpbf](https://github.com/b-r-u/osmpbf) — the Rust PBF parser we vendored and built on. The core protobuf decoding and element abstraction come from this crate.
- [Overpass API](https://overpass-api.de/) — the spatial query engine we set out to replace. Its bbox + tag query model directly shaped our `/api/query` endpoint.
- [Nominatim](https://nominatim.openstreetmap.org/) — the geocoding service we set out to replace. Our geocode endpoint mirrors its place search functionality.
- [PMTiles](https://github.com/protomaps/PMTiles) — the single-file tile archive format by Protomaps. Our writer and reader implement the v3 spec with Hilbert-ordered tile IDs.
- [Tantivy](https://github.com/quickwit-oss/tantivy) — the Rust full-text search engine powering our geocoding index.
- [libosmium](https://github.com/osmcode/libosmium) — the C++ OSM library whose flat node store design inspired our mmap'd `node_index` and `way_index`.

## License

Licensed under [Apache-2.0](LICENSE).

The vendored [osmpbf](https://github.com/b-r-u/osmpbf) crate is dual-licensed under MIT and Apache-2.0; we use it under Apache-2.0.
