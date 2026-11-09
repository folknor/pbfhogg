---
layout: home

hero:
  name: "pbfhogg"
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
    details: "Read Denmark's 59M elements in 0.31s (parallel) or 1.3s (pipelined). Apply a daily diff to the full planet in under 100 seconds at under 2 GiB peak memory."
  - icon:
      src: /icons/wrench.svg
    title: 25+ CLI Commands
    details: "inspect, sort, extract, apply-changes, add-locations-to-ways, tags-filter, diff, getid, and more. Cross-validated against osmium."
  - icon:
      src: /icons/globe.svg
    title: Zero External Dependencies
    details: "All protobuf encoding and decoding is hand-rolled wire format. Pure Rust zlib via zlib-rs. No C compiler required."
---
