# Advanced Topics

## Performance Tuning

### Thread Count

By default, Nidhogg uses all available CPU cores. On shared machines, you may want to limit this:

```sh
nidhogg --threads 8 input.osm.pbf output.pmtiles
```

The processing pipeline has three parallelizable stages:

1. **PBF decoding** — decompresses and parses OSM data blocks
2. **Feature extraction** — matches tags against Shortbread layer rules
3. **Tile encoding** — clips, simplifies, and encodes MVT tiles

Each stage scales near-linearly up to about 16 threads, after which contention on the sort buffer starts to dominate.

### Memory Management

The `--memory-limit` flag controls how much RAM the external merge sort can use for in-memory buffers. Larger buffers mean fewer disk passes:

```sh
# Use up to 32 GB for sort buffers
nidhogg --memory-limit 32GB input.osm.pbf output.pmtiles
```

| Memory Limit | Planet Build Time | Sort Passes |
|---|---|---|
| 2 GB | ~6 hours | 12 |
| 4 GB | ~4 hours | 6 |
| 8 GB | ~3 hours | 3 |
| 16 GB | ~2.5 hours | 2 |
| 32 GB | ~2 hours | 1 |

::: tip
If you have enough RAM to keep the entire sort in memory (typically 24-32 GB for a planet build), the sort completes in a single pass with no temporary files written to disk.
:::

### Temporary Storage

The external merge sort writes intermediate files to `--temp-dir`. For best performance:

- Point this at your fastest disk (NVMe > SATA SSD > HDD)
- Ensure at least 2x the input file size in free space
- Avoid network-mounted filesystems

```sh
nidhogg --temp-dir /mnt/nvme/scratch input.osm.pbf output.pmtiles
```

## Custom Layer Selection

### Include Only Specific Layers

```sh
# Only generate transportation and water layers
nidhogg --layers transportation,water,waterways \
        input.osm.pbf transport-water.pmtiles
```

### Exclude Layers

```sh
# Everything except POIs and address labels
nidhogg --exclude pois,addresses \
        input.osm.pbf no-pois.pmtiles
```

### Available Shortbread Layers

The full Shortbread spec defines 26 layers:

| Layer | Description | Min Zoom |
|---|---|---|
| `ocean` | Ocean fill polygons | 0 |
| `water` | Inland water bodies | 4 |
| `waterways` | Rivers, streams, canals | 9 |
| `landuse` | Parks, forests, residential areas | 4 |
| `landcover` | Natural land cover (grass, sand, etc.) | 7 |
| `transportation` | Roads, railways, paths | 4 |
| `transportation_labels` | Road names and route numbers | 10 |
| `buildings` | Building footprints | 13 |
| `pois` | Points of interest | 14 |
| `places` | City, town, village labels | 2 |
| `boundaries` | Country and state borders | 0 |
| `boundary_labels` | Border crossing names | 10 |
| `addresses` | House numbers | 14 |
| `sites` | Industrial/commercial/retail areas | 14 |
| `aerialways` | Ski lifts, cable cars | 12 |
| `ferries` | Ferry routes | 8 |

<small>See the [Shortbread schema specification](https://shortbread-tiles.org/) for the complete list and tag mapping rules.</small>

## Simplification

Nidhogg applies Douglas-Peucker simplification to geometry at each zoom level. You can tune the aggressiveness:

```toml
[simplification]
tolerance = 1.0        # units: tile coordinate space (0-4096)
area_threshold = 4.0   # drop polygons smaller than this
```

Lower tolerance values preserve more detail but produce larger tiles. The default of `1.0` is a good balance for most use cases.

### Per-Layer Overrides

```toml
[simplification.overrides.buildings]
tolerance = 0.5        # keep building shapes crisp
area_threshold = 1.0

[simplification.overrides.landcover]
tolerance = 2.0        # landcover can be more aggressive
area_threshold = 16.0
```

## Coordinate Reference System

Nidhogg outputs tiles in Web Mercator (EPSG:3857), which is the standard for vector tile renderers. Input data is expected in WGS 84 (EPSG:4326) — the native CRS of OpenStreetMap.

The ocean shapefile must also be in EPSG:4326. The recommended source is the [osmdata.openstreetmap.de water polygons](https://osmdata.openstreetmap.de/data/water-polygons.html), specifically the "WGS84, split" variant.

## Extending with Plugins

::: warning Experimental
The plugin system is under active development. The API may change between minor versions.
:::

Nidhogg supports Lua plugins for custom tag-to-layer mapping:

```lua
-- custom_layers.lua
function process_node(tags, layer_builder)
  if tags["amenity"] == "charging_station" then
    layer_builder:add("ev_chargers", {
      operator = tags["operator"] or "unknown",
      capacity = tonumber(tags["capacity"]) or 1,
    })
  end
end

function process_way(tags, layer_builder)
  if tags["highway"] == "cycleway" or tags["cycleway"] then
    layer_builder:add("cycling", {
      surface = tags["surface"] or "unknown",
      lit = tags["lit"] == "yes",
    })
  end
end
```

Load a plugin with:

```sh
nidhogg --plugin custom_layers.lua input.osm.pbf output.pmtiles
```

The custom layers appear alongside the standard Shortbread layers in the output.
