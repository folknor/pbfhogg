# GeoParquet export - research note

> **STATUS: parked research, no scheduled work.** Initial spelunking
> only (2026-07-16). The GeoParquet 2.0 spec repo is vendored at
> `research/geoparquet/` and a `geoparquet` standards archetype exists
> in `.review.toml`. When this is picked up, it goes through the
> orchestrate spec loop - this note is the raw material for that spec,
> not the spec.

A `pbfhogg export --format geoparquet` target: stream tagged nodes and
enriched ways into a conformant GeoParquet 2.0 file. Candidate for the
post-0.5.0 1.0 surface - Parquet is what the analytics ecosystem
(DuckDB, GeoPandas, Overture tooling, QGIS) actually ingests, and it
would supersede the parked GeoPackage idea in TODO Milestone 3.
osmium-tool has no comparable command; the closest producers are GDAL
and DuckDB-spatial.

## Why the spec fits pbfhogg unusually well

Checked against the vendored spec (`format-specs/geoparquet.md`,
v2.0.0):

- **Footer-last metadata suits streaming.** Parquet writes all
  metadata - the `geo` JSON, column statistics, per-column bbox - in
  the footer, after the data. The exporter can stream bounded row
  groups and accumulate `bbox` + `geometry_types` as it goes; the
  planet-safe shape falls out naturally. This is the inverse of the
  GeoJSON FeatureCollection problem, and no deferred-header machinery
  is needed.
- **The hard MUSTs are things export already does.** Polygon exterior
  rings closed and counterclockwise (export enforces this today, so
  `orientation: "counterclockwise"` can be asserted); OSM coordinates
  are WGS84 lon/lat, which is exactly the spec default (`OGC:CRS84`
  when `crs` is absent) - zero CRS machinery required, or emit the
  spec's canonical CRS84 PROJJSON blob verbatim; `edges` defaults to
  `planar`, correct for us.
- **`geometry_types` strictness is free.** The spec demands the list
  be exactly correct (`["Point", "LineString", "Polygon"]`, not a
  superset); the exporter knows precisely what it emitted by the time
  the footer is written.
- **WKB is trivial.** Byte order + geometry type + coordinate doubles;
  squarely in this project's hand-rolled wire-format comfort zone.
  Geometry columns must be root-level `BYTE_ARRAY`, never
  nested/repeated - our schema is flat anyway.
- **File extension `.parquet`** (spec: `.geoparquet` SHOULD NOT be
  used).

## The two conformance rails

- **GeoParquet 2.0 (target):** native Parquet `GEOMETRY` logical type
  (Parquet format >= 2.11, March 2025) plus the `geo` key-value
  metadata with inline-PROJJSON-or-null CRS. Note the trap the
  ecosystem has already named: native types alone are NOT GeoParquet -
  the `geo` metadata is what makes it conformant, and once present its
  `crs` field must be inline PROJJSON (or null with `srid:0` on the
  logical type).
- **GeoParquet 1.1 (fallback):** plain `BYTE_ARRAY` + the same `geo`
  metadata with `version: "1.1.0"`. Every reader in the wild accepts
  this today. `format-specs/compatible-parquet.md` additionally
  defines the metadata-less compatibility profile (column named
  `geometry`, WKB, lon/lat) - we should *read* that profile someday if
  an import ever exists, but must never *write* it.

## Dependency decision (the one real tension)

The Parquet container (Thrift metadata, page encodings, compression
framing) is not hand-rolling territory - it is a different league from
protobuf. The `parquet` crate is pure Rust (zero-C policy survives)
and usable without the heavy Arrow layer via the low-level
`ColumnWriter` API; gate it behind a `geoparquet` cargo feature like
the existing optional deps.

Spelunked 2026-07-16: parquet-rs writes native `GEOMETRY`/`GEOGRAPHY`
logical types behind a `geospatial` feature flag as of the Arrow ~57
era (the arrow-rs geospatial epic is
<https://github.com/apache/arrow-rs/issues/8373>; announcement context
<https://parquet.apache.org/blog/2026/02/13/native-geospatial-types-in-apache-parquet/>).
One known wart: geo *statistics* are written automatically for
Geometry but not for non-point Geography - irrelevant to us (we write
Geometry, planar). To verify at spec time: that the `geospatial`
feature composes with the no-arrow low-level API, and the crate's MSRV
against ours (1.96).

## The schema fork (the one real design decision)

OSM tags are sparse key-value; Parquet wants a schema. Options:

1. Whitelist-only columns via the existing `--properties` flag -
   clean, lossy by default.
2. A single `tags` map column - lossless; spec-legal (only *geometry*
   columns must be root-level scalars); what GDAL does.
3. Both: `@id` (i64), `@type` (dictionary), `geometry` (WKB),
   optional `--metadata` columns, whitelisted tag columns, plus a map
   column for the rest. Recommended starting point.

## Verification story

- `research/geoparquet/test_data/` ships per-geometry-type WKB parquet
  files paired with WKT CSV oracles - ready-made conformance fixtures
  for round-trip tests.
- `research/geoparquet/format-specs/schema.json` validates the `geo`
  metadata JSON - a unit test can validate our emitted metadata
  against it directly.
- The natural `brokkr verify` oracle is DuckDB-spatial (read our
  output back, compare feature count/geometry WKT against the GeoJSON
  export of the same input). osmium cannot serve here.

## Inherited gaps

- Relations -> MultiPolygon is still export v2 work
  (notes/geojson-export-design.md); until it lands, geoparquet export
  emits the same node/way surface as GeoJSON and `geometry_types`
  stays `["Point", "LineString", "Polygon"]`.
- Way export still requires `LocationsOnWays` input, same as GeoJSON.

## Open questions for the spec phase

- Row-group size (bounded-memory vs scan efficiency; Overture uses
  large groups, DuckDB likes ~122k rows).
- Compression default: snappy is the Parquet lingua franca; zstd is
  pure-Rust-available and smaller. Interop argues snappy.
- Dictionary encoding for `@type` and tag-value columns.
- Whether to also emit per-row-group covering bbox columns (the 1.1
  `covering` extension) for spatial predicate pushdown - readers
  increasingly use it, but native geo statistics may make it redundant
  under 2.0.
- Feature identifiers: spec recommends custom key-value metadata
  naming the id column (`@id`).

## Disposition

Parked. Not scheduled for 0.5.x. Re-entry: when export v2 work is
scheduled or a consumer asks for Parquet output, run the orchestrate
spec loop over this note - `review bare --profile deep` for the spec
critique, with the `geoparquet` archetype available for
spec-conformance questions.
