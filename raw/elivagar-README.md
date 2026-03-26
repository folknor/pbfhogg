<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="elivagar-logo-text-dark.svg">
    <img src="elivagar-logo-text.svg" width="300" alt="Elivagar">
  </picture>
  <br>
  <em>Shortbread vector tile generator</em>
</p>

<p align="center">
  <a href="https://github.com/folknor/elivagar/actions"><img src="https://img.shields.io/github/actions/workflow/status/folknor/elivagar/ci.yml?label=CI&logo=github" alt="CI"></a>
  <img src="https://img.shields.io/badge/rust-nightly-orange?logo=rust" alt="Rust nightly">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
</p>

---

Reads OSM PBF files and produces [PMTiles v3](https://github.com/protomaps/PMTiles) archives with the [Shortbread](https://shortbread-tiles.org/) schema (26 layers).

## Usage

```
elivagar <input.osm.pbf> <output.pmtiles> [options]
```

### Options

| Flag | Description |
|------|-------------|
| `--ocean path.shp` | Ocean shapefile (`water-polygons-split-3857`) |
| `--ocean-simplified path.shp` | Simplified ocean shapefile for z0-7 (fewer vertices) |
| `--tmp-dir path` | Directory for temporary sort files (default: `.tilegen_tmp`) |
| `--skip-to ocean\|sort` | Resume from a previous run's checkpoint |
| `--in-memory` | Keep tile blob in RAM instead of streaming to disk |

### Example

```
elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean water-polygons-split-3857/water_polygons.shp \
  --ocean-simplified simplified-water-polygons-split-3857/simplified_water_polygons.shp
```

## Pipeline

1. **PBF read** -- single-pass read building node/way indices and emitting sort records
2. **Ocean** -- ocean shapefile processing (optional, requires `--ocean`)
3. **Sort** -- external merge sort by Hilbert tile ID
4. **Assembly** -- MVT encode + gzip + PMTiles write

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).

## Output size

Denmark extract (483 MB PBF), gzip level 6, z0-14:

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| With ocean | **380 MB** | 406 MB | 308 MB |
| Without ocean | **317 MB** | 406 MB | 308 MB |

Full analysis: [`docs/tile-comparison-2026-02-24.md`](docs/tile-comparison-2026-02-24.md)

## Performance

Denmark extract (483 MB PBF) → Shortbread PMTiles, best of 3 runs:

<!-- BENCH:START -->
| Tool | Total | PBF+Features | Ocean | Sort | Assembly |
|------|-------|-------------|-------|------|----------|
| **elivagar** | **29s** | 20s | 2.8s | 0.7s | 3.4s |
| Tilemaker | 29s | — | — | — | — |
| Planetiler 0.10 | 41s | — | — | — | — |
<!-- BENCH:END -->

System: Linux 6.18, Ryzen 9 7950X.

Measured with `scripts/bench.sh`. Results are logged to `benchmarks.tsv` for tracking over time.

### PMTiles writer

Elivagar's hand-rolled PMTiles v3 writer vs [pmtiles-rs](https://github.com/stadiamaps/pmtiles-rs) 0.20,
synthetic tiles (unique gzipped payloads, Hilbert-ordered), best of 5 runs:

| Tiles | elivagar | pmtiles-rs | Speedup |
|------:|---------:|-----------:|--------:|
| 100K | 28 ms | 68 ms | 2.4x |
| 500K | 110 ms | 309 ms | 2.8x |
| 1M | 223 ms | 646 ms | 2.9x |

Run with `scripts/bench-pmtiles.sh [tiles] [runs]`.

## Building

Requires Rust nightly (edition 2024) and [pbfhogg](https://github.com/folknor/pbfhogg) as a sibling directory.

```
cargo build --release
```

## License

Apache-2.0
