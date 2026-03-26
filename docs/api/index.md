# API Reference

Nidhogg is both a CLI tool and a Rust library. You can use the library directly to embed tile generation in your own applications or to build custom processing pipelines.

## Crate Overview

```sh
cargo add nidhogg
```

```rust
use nidhogg::{Config, Pipeline};

let config = Config::from_file("nidhogg.toml")?;
let stats = Pipeline::from_config(config)?.run()?;
println!("Generated {} tiles", stats.tiles_written);
```

## Modules

### [`nidhogg::config`](./config)

Configuration types and the builder API. Covers everything from output format and compression to zoom ranges and layer selection.

- `Config` — top-level configuration struct
- `ConfigBuilder` — fluent builder for programmatic config
- `OutputFormat` — PMTiles / MBTiles / Directory
- `Compression` — Zstd / Gzip / Brotli / None

### [`nidhogg::tile`](./tile)

Core tile data types. Everything you need to represent, manipulate, and encode vector tiles.

- `TileCoord` — zoom/x/y tile address
- `Tile` — a complete vector tile with layers
- `Layer` — a named collection of features
- `Feature` — geometry + properties + layer assignment
- `Geometry` — point, line, polygon variants
- `LayerId` — Shortbread layer enum

### [`nidhogg::pipeline`](./pipeline)

The processing pipeline. Read OSM data, transform features, and write tiles.

- `Pipeline` — orchestrates source -> processors -> sink
- `TileProcessor` — trait for custom processing stages
- `TileSource` — trait for tile input (PBF reader)
- `TileSink` — trait for tile output (PMTiles/MBTiles writer)
- `TileStats` — run statistics and metrics

## Feature Flags

The library respects the same feature flags as the CLI:

```toml
[dependencies]
nidhogg = { version = "0.4", features = ["lua"] }
```

| Feature | Description |
|---|---|
| `zstd` | Zstandard compression (default) |
| `brotli` | Brotli compression (default) |
| `lua` | Lua plugin support |
| `simd` | SIMD-accelerated geometry |

## Minimum Supported Rust Version

The MSRV is **1.75.0**. This is tested in CI and will only be bumped in minor version releases.

## Examples

The repository includes several examples:

```sh
# Basic usage — generate tiles from a PBF
cargo run --example basic -- input.osm.pbf output.pmtiles

# Custom pipeline — add a processing stage that filters features
cargo run --example custom_pipeline -- input.osm.pbf output.pmtiles

# Single tile — extract and inspect one tile
cargo run --example single_tile -- 14/8691/5677 input.osm.pbf
```

See the [`examples/`](https://github.com/user/nidhogg/tree/main/examples) directory for source code.
