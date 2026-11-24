# GeoJSON export design

`pbfhogg export` streams a PBF to GeoJSON or newline-delimited GeoJSONSeq -
the bridge to the GIS ecosystem (otherwise ogr2ogr / osmium export /
osm2pgsql + PostGIS).

**v1 shipped 2026-07-13.** This doc is now the forward-looking record:
what shipped, in brief, and the deferred work. Durable homes for the
shipped surface are the code (`src/commands/export/`), the CLI reference
(`docs/cli/commands.md`), the format / area / property decisions
([ADR-0010](../decisions/0010-geojson-export-format-and-area-heuristic.md)),
and the osmium deviation (`DEVIATIONS.md`). The implementation spec that
drove the landing was retired on completion; its deferred items are all
folded into "Deferred / future work" below.

## Shipped in v1

- Point features from tagged nodes; LineString / Polygon from tagged ways
  (closed + area-tagged -> Polygon, CCW exterior ring per RFC 7946),
  geometry read from inline `LocationsOnWays` coordinates only.
- Two formats: newline-delimited GeoJSONSeq (default, one Feature per
  `\n`, no `0x1e` record separator - NOT RFC 8142) and a wrapped
  `FeatureCollection`.
- Filters, all composable: `--type` (node/way), tag `--expressions`
  (positional + `-e file`), `--properties` key whitelist, `--bbox`
  (vertex-overlap), `--metadata`. Every feature carries `@id` and `@type`.
- Hard-errors when ways are requested from input lacking `LocationsOnWays`
  (`--type node` works without it).
- Sequential streaming, O(1) memory, `--output` published through the
  library's staged output (`src/output.rs`),
  stats to stderr so stdout stays pipeable.

## Deferred / future work

Each is a separate TODO; none is scheduled. Every open design question from
v1 (id property, coordinate precision, area heuristic, multipolygon,
enriched-input requirement) was resolved in ADR-0010 and the shipped code.

### In-workspace (no brokkr dependency)

- **Relations -> MultiPolygon.** v1 emits zero relation features (not even
  a single closed way promoted from a relation). v2 needs ring assembly
  from member ways - joined end-to-end, outer/inner rings by winding or
  member `role`, holes. `src/geo.rs` already carries ring assembly and
  hole detection, and extract's smart strategy already assembles relation
  members. Significant effort.
- **Raw-PBF fallback via an in-memory node index.** v1 hard-errors when
  ways are requested from a non-`LocationsOnWays` input rather than
  building a coordinate index. The index path (same shape as altw's
  coordinate scatter) would lift that requirement at a memory cost.
- **GeoPackage / binary output.** Needs an SQLite dependency; the
  desktop-GIS standard but out of scope entirely for the text formats.
- **Configurable coordinate precision.** Fixed at 7 dp (decimicrodegree),
  trailing zeros trimmed.
- **Configurable area rules.** v1 uses the fixed `AREA_KEYS` list plus
  `area=yes`/`area=no`; osmium uses a config file. A config surface is
  deferred.
- **True bbox overlap for ways.** v1's way bbox filter is vertex-only and
  lossy: a way whose edge crosses the box with no vertex inside, or a
  polygon that encloses the whole box, is dropped. Real overlap / clipping
  is future work.

### Orchestrator-owned (require brokkr changes)

These cannot be built from inside the pbfhogg workspace; they need the
`brokkr` dev tool extended.

- **osmium cross-check / `brokkr verify export`.** Must be *strict*, not a
  permissive intersection comparator: exact feature identity, non-area
  geometry within 1e-7, the *expected* property set (an intersection-of-
  keys comparator cannot detect pbfhogg dropping a tag), and an
  independent area fixture matrix with expected classifications and
  geometry. The osmium invocation must be pinned to one that emits
  `@id`/`@type` (e.g. `osmium export -f geojsonseq` with `--add-unique-id`
  or an attributes config naming `id`/`type`). Additive confidence only -
  the in-tree golden-file tests already assert area / winding / collision
  strictly.

## Cross-references

- Code: `src/commands/export/` (`mod` / `geometry` / `properties` /
  `writer`), `src/commands/spatial.rs` (`BboxInt`), `src/coord_fmt.rs`.
- [ADR-0010](../decisions/0010-geojson-export-format-and-area-heuristic.md)
  - format framing, `@id`/`@type` identity model, `AREA_KEYS` + closed-ring
  area rule, reserved-key collision precedence, polygon validity/winding.
- `DEVIATIONS.md` "export" - why output is not osmium byte-parity.
- `docs/cli/commands.md` "export" - the user-facing flag surface.
