---
layout: home

hero:
  name: "Elivagar"
  text: "Shortbread vector tile generator"
  tagline: "Reads OSM PBF files and produces PMTiles v3 archives with 26 layers. Fast, single-binary, no runtime dependencies."
  image:
    light: /elivagar-logo.svg
    dark: /elivagar-logo-dark.svg
    alt: Elivagar logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: API Docs
      link: /api/
    - theme: alt
      text: GitHub
      link: https://github.com/user/elivagar

features:
  - icon:
      src: /icons/globe.svg
    title: Full Shortbread Profile
    details: All 26 layers — roads, buildings, land use, water, POIs, and more. Faithful to the Shortbread spec with 65+ tested tag-matching rules.
  - icon:
      src: /icons/gauge.svg
    title: Planet-Scale Performance
    details: External merge sort, parallel processing with rayon, streaming PMTiles output. Handles 75GB planet extracts.
  - icon:
      src: /icons/wrench.svg
    title: Single Binary
    details: Just pass a PBF and an ocean shapefile. No database, no Java, no Docker. One binary, one command.
---

<div class="demo-frame">
  <div style="background: var(--vp-c-bg-soft); padding: 3rem; text-align: center; color: var(--vp-c-text-3); font-family: var(--vp-font-family-mono); font-size: 0.85rem;">
    screenshot or terminal recording goes here
  </div>
</div>

<!-- To use with a real image:
<div class="demo-frame">
  <img src="/screenshot.png" alt="Screenshot" />
</div>
-->
