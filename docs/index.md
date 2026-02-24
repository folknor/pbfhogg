---
layout: home

hero:
  name: "Nidhogg"
  text: "Shortbread vector tiles from OpenStreetMap"
  tagline: "A fast, single-binary tile generator that turns OSM extracts into production-ready PMTiles. No database, no Java, no Docker."
  image:
    light: /nidhogg-logo.svg
    dark: /nidhogg-logo-dark.svg
    alt: Nidhogg logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: API Docs
      link: /api/
    - theme: alt
      text: GitHub
      link: https://github.com/user/nidhogg

features:
  - icon:
      src: /icons/globe.svg
    title: Full Shortbread Profile
    details: All 26 layers — roads, buildings, land use, water, POIs, and more. Faithful to the Shortbread spec with 65+ tested tag-matching rules.
  - icon:
      src: /icons/gauge.svg
    title: Planet-Scale Performance
    details: External merge sort, parallel processing with rayon, streaming PMTiles output. Handles 75GB planet extracts in under 3 hours.
  - icon:
      src: /icons/wrench.svg
    title: Single Binary
    details: Just pass a PBF and an ocean shapefile. No database, no Java, no Docker. One binary, one command.
---

<div class="demo-frame">
  <div id="asciinema-container"></div>
</div>

<script setup>
import { onMounted } from 'vue'

onMounted(() => {
  const s = document.createElement('script')
  s.src = 'https://asciinema.org/a/569727.js'
  s.id = 'asciicast-569727'
  s.async = true
  document.getElementById('asciinema-container').appendChild(s)
})
</script>

## Why Nidhogg?

Generating vector tiles from OpenStreetMap has traditionally required a complex stack: a PostgreSQL database with PostGIS, imposm or osm2pgsql for import, Tegola or T-Rex for serving, and Tippecanoe for packaging. A planet build can take days and requires careful tuning of dozens of moving parts.

Nidhogg replaces all of that with a single Rust binary. It reads `.osm.pbf` files directly, applies the [Shortbread](https://shortbread-tiles.org/) tag-matching rules, and streams tiles into a PMTiles archive that can be served straight from S3 or any static file host. The entire planet builds in 2-3 hours on a 16-core machine.

### Benchmarks

Tested on a 16-core AMD EPYC with 64 GB RAM and NVMe storage:

| Input | Size | Time | Output |
|---|---|---|---|
| Germany | 4.1 GB | 4 min | 1.2 GB |
| Europe | 27 GB | 38 min | 9.8 GB |
| Planet | 75 GB | 2h 12min | 31 GB |

All runs used `--max-zoom 14` with Zstandard compression.

### How It Works

1. **Parse** — PBF blocks are decoded in parallel across all available cores
2. **Match** — Each OSM element is tested against 65+ Shortbread layer rules
3. **Sort** — Features are sorted by tile coordinate using an external merge sort
4. **Encode** — Tiles are built, simplified per zoom level, and written as MVT protobufs
5. **Package** — The tile stream is written directly to PMTiles with a clustered B-tree index
