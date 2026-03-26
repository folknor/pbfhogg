---
layout: home

hero:
  name: "Pbfhogg"
  text: "Fast OpenStreetMap PBF toolkit for Rust"
  tagline: "Read, write, and transform .osm.pbf files at planet scale. Pipelined parallel decoding, blob passthrough, and 25+ CLI commands."
  image:
    src: /pbfhogg-logo.svg
    alt: Pbfhogg logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: CLI Reference
      link: /cli/
    - theme: alt
      text: GitHub
      link: https://github.com/folknor/pbfhogg

features:
  - icon:
      src: /icons/gauge.svg
    title: Planet-Scale Performance
    details: "Read 59M elements in 0.31s (parallel) or 1.3s (pipelined). Apply a daily diff to 18.8 GB North America in 12 seconds at under 600 MB RSS."
  - icon:
      src: /icons/wrench.svg
    title: 25+ CLI Commands
    details: "inspect, sort, extract, apply-changes, add-locations-to-ways, tags-filter, diff, getid, and more. Cross-validated against osmium."
  - icon:
      src: /icons/globe.svg
    title: Zero External Dependencies
    details: "All protobuf encoding and decoding is hand-rolled wire format. Pure Rust zlib via zlib-rs. No C compiler required."
---

## Highlights

### Blob Passthrough

Unmodified blobs are copied as raw bytes — no decompression, no re-encoding. At North America scale (18.8 GB), 92% of blobs pass through during a daily diff merge. This is what makes pbfhogg fast for incremental workflows.

### Blob Indexdata

Every blob gets metadata embedded in its header: element type, ID range, spatial bounding box, and tag key index. Commands like `apply-changes`, `sort`, `extract`, and `tags-filter` use this to skip decompression of irrelevant blobs — often reducing work by 80-95%.

### Three Read Modes

| Method | Order | Best for |
|--------|-------|----------|
| `for_each` | File order | Sequential processing |
| `for_each_pipelined` | File order | Production hot path (parallel decompression) |
| `par_map_reduce` | Arbitrary | Aggregation (counts, statistics) |

### Library + CLI

Use pbfhogg as a Rust library for custom PBF processing, or use the CLI for common operations. The CLI covers the same feature surface as osmium-tool, with comparable or better performance on most commands.
