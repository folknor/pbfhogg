# GeoJSON export v1 - implementation specification

Written against `reference/technical-implementation-spec.md`. Source
design this spec is spawned from: `notes/geojson-export-design.md`. Test
placement and tiering follow `reference/testing.md`. This spec adds a new
CLI command and a new library module; it does not touch any measured read
or write path, so no baseline in `reference/performance.md` moves (see
Performance below).

## 1. Goal and scope

Add `pbfhogg export`: a streaming PBF -> GeoJSON / GeoJSONSeq converter.
v1 emits Point features from tagged nodes and LineString / Polygon
features from tagged ways, reading way geometry exclusively from inline
`LocationsOnWays` coordinates (the `altw` variant). Output defaults to
newline-delimited GeoJSON on stdout; a wrapped `FeatureCollection` mode
is available.

**Format note (R2 finding 1):** the default is *newline-delimited*
GeoJSON - one JSON Feature object per line terminated by a single `\n`,
no leading record separator. This is NOT RFC 8142 "GeoJSON Text
Sequences", which mandates an ASCII record separator (`0x1e`) before each
JSON text. The RFC 8142 claim is dropped throughout this spec: the
headline consumer is `jq` / `head` / line-oriented Unix tooling, which
expects plain newline framing, and osmium's own `geojsonseq` writer is
also newline-only. The format is referred to as "GeoJSONSeq
(newline-delimited)" below; no `0x1e` byte is ever emitted, and the
line-parsing tests assert exactly one JSON object per `\n`-terminated
line with no control-character prefix.

Filters (all optional, all composable): `--type`, tag `--expressions`,
`--properties` key selection, `--bbox`, `--metadata`.

### In scope (v1)
- Nodes with >= 1 tag -> `Point`.
- Ways with >= 1 tag -> `LineString`, or `Polygon` when closed and area-tagged.
- GeoJSONSeq (newline-delimited; NOT RFC 8142, no `0x1e`) and wrapped
  GeoJSON.
- Tag/type/bbox filtering; property key selection; `@id`/`@type`
  identity properties; opt-in OSM metadata properties.

### Out of scope (named, not deferred)
These are separate TODOs, excluded here by explicit decision:
- **Relations / multipolygon assembly.** Ring assembly from member ways
  is a distinct feature (`notes/geojson-export-design.md` "Relations ->
  MultiPolygon"). v1 emits zero relation features. `geo.rs` already
  carries the ring machinery; wiring it to relation members with role
  handling is v2.
- **Raw-PBF fallback via an in-memory node index.** v1 hard-errors when
  ways are requested from input lacking `LocationsOnWays` rather than
  building a coordinate index. The index path is a separate TODO.
- **GeoPackage / any binary output.** Needs an SQLite dependency; out of
  scope entirely.

## 2. Survey of the ground

### Standing decisions checked
- `decisions/0002-negative-ids-rejected-project-wide.md`: export is
  read-only and emits whatever ids the input carries as `@id`. It does
  not construct or reject ids, so the negative-id policy does not bind
  here. No debug-assert invariant from `decisions/0005` is on this path.
- `decisions/0003-error-path-hygiene-via-pathguard.md` **binds this
  landing** (R2 finding 7): export writes a final output file when
  `--output` is given, so it inherits the ADR-0003 checklist. The file
  output path MUST be wrapped in `PathGuard::file` at writer-construction
  time and `commit()`ed only after the writer's final `flush()`; a
  mid-stream decode/write error therefore removes the partial GeoJSON
  rather than leaving a truncated document downstream tools can't repair.
  User-facing `ExportStats` counters bump *after* the successful feature
  write, never before. The CLI additionally rejects `--output` pointing
  at the input PBF (same canonical path or a hard link into it): the
  guard opens/creates the output before `export()` opens the input, so
  without the check a self-target would truncate the source before it is
  read. See CLI surface (3.4) and data flow (3.1) for the exact ordering.
- `CORRECTNESS.md` / `DEVIATIONS.md`: no existing entry governs GeoJSON
  output. This spec *adds* a `DEVIATIONS.md` entry (final-stage docs)
  because the
  area-detection heuristic and property model deliberately differ from
  `osmium export`; export is therefore NOT held to byte-parity with
  osmium, and no `reference/osmium-parity.md` exception is claimed.
- This landing establishes a new user-facing output format and an area
  heuristic policy. Per `decisions/README.md` that is ADR-worthy: the
  docs stage adds
  `decisions/0010-geojson-export-format-and-area-heuristic.md` capturing
  the format choice, the identity-property model, the area-key list, the
  reserved-key collision precedence (section 4), and the
  polygon-validity/winding rule, while it is fresh. (ADR numbers 0001 and
  0008 are unused gaps in the existing 0002-0007/0009 sequence; 0010 is
  the next free number and is intentional - R1 minor.)

### Existing code reused (exact locations)
- `src/read/reader.rs`: `ElementReader::new`/`from_path` (line ~53/934),
  `header()` (line ~116) -> `HeaderBlock`, `for_each` (line ~156)
  sequential loop delivering `Element`. v1 uses the sequential
  `for_each` path (design: "No parallel decode for v1").
  **`for_each`'s closure is `FnMut(Element) -> ()`, not `-> Result`
  (R1 finding 2 / R2 finding 4).** It cannot signal "stop" from inside
  the closure: the loop always decodes the whole PBF. Consequences the
  spec must honor: (a) a write error (including a broken stdout pipe when
  the consumer is `head`/`jq -e` and exits early) is captured into an
  outer `Option<io::Error>` cell and returned *after* `for_each` finishes
  - export keeps decoding to EOF rather than aborting on first write
  failure. This is an accepted v1 limitation, documented, not a defect.
  (b) `for_each` itself returns `Result<()>` for *decode* errors; that is
  a different channel from the closure's captured *write/serialize* error.
  The two must be merged at the end: decode error OR captured write error
  -> the function's `Err`. The earlier wording "errors propagate by
  capturing the first io::Error into the Result the loop returns" is
  imprecise and is corrected here; the cited "idiom tags_filter uses" is
  also imprecise - the tags_filter *write* path runs through
  `for_each_fused_block`, not plain `for_each`.
- `src/read/block.rs`: `HeaderBlock::has_locations_on_ways()` (line ~240)
  gates the way-export requirement; `LOCATIONS_ON_WAYS` const (line ~227).
- `src/read/elements.rs`:
  - `Element` enum (line ~69) with `Node`, `DenseNode`, `Way`,
    `Relation`. Both `Node` and `DenseNode` must be handled.
  - `Node::lat()/lon()` (f64 degrees, via `impl_coordinate_conversions!`
    macro at line ~34), `id()` (~108), `tags()` -> `TagIter` (~113),
    `info()` -> `Info` (~122, non-optional `Info` whose *fields* are
    optional).
  - `Way::id()` (~180), `tags()` (~185), `refs()` -> `WayRefIter` (~205),
    `node_locations()` -> `WayNodeLocationsIter` (~215), `info()` (~194).
  - `WayNodeLocation::lat()/lon()` (f64 degrees, ~397).
  - `Info::version/timestamp/changeset/uid/user/visible` (~638-684) for
    `--metadata`. `Info::user()` returns `Option<Result<&str>>` (~670),
    so metadata emission is fallible and must propagate that error.
  - `TagIter` yields `(key, value)` string pairs.
  - **Dense nodes have a different accessor shape (R2 finding 3),
    `src/read/dense.rs`:** `DenseNode::tags()` -> `DenseTagIter` (~55),
    NOT `TagIter`; `DenseNode::info()` -> `Option<&DenseNodeInfo>` (~36),
    NOT a non-optional `Info`; `DenseNodeInfo::user()` -> `Result<&str>`
    (~285). The property emitter therefore cannot be typed against
    `TagIter` + `Option<Info>` (as the original `write_properties`
    signature was); it needs a generic tag-iterator bound and an explicit
    metadata abstraction that both `Node` and `DenseNode` satisfy - see
    the revised `properties.rs` in 3.1.
- `src/tag_expr.rs`: `Expression` (line 32, `type_filter` + `matcher`),
  `parse_expressions` (112), `read_expressions_file` (122, `pub`),
  `tag_matches` (148), `TagMatcher` (17). The exact matching idiom
  already used by `tags_filter/mod.rs:70-90` is copied: an element
  matches if, for some expression whose `type_filter` admits the
  element's type, some `(key,value)` satisfies `tag_matches`.
  **Visibility (R2 finding 2):** `Expression` is `pub(crate)` (tag_expr.rs:32)
  and `parse_expressions` is `pub(crate)` (tag_expr.rs:112); only
  `read_expressions_file` is `pub`. The separate `pbfhogg-cli` crate
  therefore cannot name `Expression` or call `parse_expressions`. So the
  public `export()` option type must NOT expose `Expression` - it accepts
  raw expression *strings* and parses them inside the library (where
  `parse_expressions` is reachable). See revised `ExportOptions` in 3.1.
- `src/owned.rs`: `TypeFilter { nodes, ways, relations }` (used by
  `Expression::type_filter` and reused for `--type`). **`TypeFilter` is
  `pub(crate)` (owned.rs:19) - R2 finding 2**: it likewise cannot appear
  in the public `export()` signature; the option type carries a small
  public type-selection enum instead (3.1).
- `src/commands/extract/common.rs`: `BboxInt` (line 26, integer
  decimicrodegree box) + `contains(lat,lon)` (46). These are
  `pub(super)` today; the shared-bbox step promotes them to a shared
  `pub(crate)` location (3.3) rather than duplicating the integer-bbox
  logic. Because `BboxInt` will be `pub(crate)`, it too cannot appear in
  the public `export()` signature (R2 finding 2): `ExportOptions` carries
  an owned `Option<BboxInt>` only as a library-internal field built from
  a parsed bbox string; the public constructor takes the bbox as a
  `&str`. `extract::parse_bbox` is `pub` (extract/mod.rs:38) and the CLI
  calls `pbfhogg::extract::parse_bbox` (main.rs:2262) - it MUST stay
  `pub`; the earlier proposal to demote it to `pub(crate)` is rejected
  (see 3.3).
- `src/osc/write.rs`: `format_coord(&mut String, f64)` (line 161) -
  7-decimal formatting with trailing-zero trim. **The consumer survey
  was wrong (R1 finding 1 / R2 finding 12):** besides `osc::write`
  itself, `format_coord` is imported (co-imported with `from_decimicro`)
  by `src/commands/diff/mod.rs:38` and
  `src/commands/tags_filter/osc.rs:15`. Moving it out of `osc/write.rs`
  breaks both unless they are repointed or a re-export is left behind.
  The shared-formatter step (3.2) repoints all importers and moves
  `from_decimicro` alongside `format_coord` (they are a natural pair used
  together at every call site).
- CLI arg plumbing: `cli/src/main.rs` `CompressionArg`/`OutputArg`
  pattern (line 41-66), `Command` subcommand enum (line 106), the
  `Command::TagsFilter` arm (958) and `run_tags_filter` (1874) are the
  template for the new `Command::Export` arm and `run_export`.

### Failure history
No prior export attempt exists; `notes/*.md` "Don't re-attempt" ledgers
carry nothing on this path. Not an optimization spec.

## 3. Target artifacts

### 3.1 New library module `src/commands/export/`
Registered as `pub mod export;` in `src/commands/mod.rs`, re-exported
from `src/lib.rs` alongside the other `commands::{...}` exports. Gated by
the `commands` feature (it lives under `commands/`, same as `extract`).

```
src/commands/export/
  mod.rs         // entry point, options, reader loop, dispatch
  geometry.rs    // Point / LineString / Polygon emission + area detection
  properties.rs  // property object emission (@id/@type, tags, metadata)
  writer.rs      // FeatureWriter: seq vs collection framing + buffering
```

#### `mod.rs` public surface

The option type is built from primitives / strings only, so the separate
`pbfhogg-cli` crate can construct it without naming any `pub(crate)` type
(R2 finding 2). All crate-private parsing (`parse_expressions`,
`TypeFilter`, `BboxInt::from_bbox`) happens inside the library
constructor, which is fallible.

```rust
pub enum ExportFormat { GeoJsonSeq, GeoJson }   // seq is default

/// Which element types to emit. Public 2-variant-plus-all selector
/// (NOT the pub(crate) TypeFilter, and NOT the 3-variant DefaultTypeArg:
/// relation is not representable, so it cannot be requested - R1 minor,
/// R2 finding 8).
pub enum ExportTypes { All, NodesOnly, WaysOnly }

/// Built by `ExportOptions::new(...)`, which parses strings internally.
pub struct ExportOptions { /* private fields */ }

impl ExportOptions {
    /// `expressions`: raw expression strings (CLI positional + file
    /// lines already combined). Parsed via crate-internal
    /// `parse_expressions`. `bbox`: raw "min_lon,min_lat,max_lon,max_lat"
    /// or None; parsed via `extract::parse_bbox` + `BboxInt::from_bbox`.
    pub fn new(
        format: ExportFormat,
        types: ExportTypes,
        expressions: &[String],
        properties: Option<Vec<String>>,   // Some = tag-key whitelist
        bbox: Option<&str>,
        metadata: bool,
    ) -> crate::Result<Self>;
}

pub struct ExportStats {
    pub nodes: u64,                    // node Point features emitted
    pub ways: u64,                     // way features emitted
    pub features: u64,                 // nodes + ways emitted after filtering
    pub skipped_untagged_nodes: u64,
    pub skipped_untagged_ways: u64,
    pub skipped_invalid_ways: u64,     // ways dropped by geometry validation (R2 finding 9)
}

/// `input` is the source PBF. `out` is any `io::Write`
/// (stdout lock or a `BufWriter<File>`). Never opens the output path
/// itself, so the CLI owns stdout-vs-file selection and PathGuard.
pub fn export<W: std::io::Write>(
    input: &std::path::Path,
    out: W,
    opts: &ExportOptions,
) -> crate::Result<ExportStats>;
```

Data flow inside `export`:
1. `ElementReader::from_path(input)`.
2. Read `header()`. If ways may be emitted (`ExportTypes::All` or
   `WaysOnly`) and `!header.has_locations_on_ways()`, return
   `ErrorKind::MissingFeature("LocationsOnWays")` (new variant) with a
   message naming `--type node` and the `altw` variant as the two ways
   out. If only nodes are requested, no requirement.
3. Construct the `FeatureWriter`. **`FeatureWriter::new` is fallible**
   (`io::Result<Self>`, R2 finding 4): for `GeoJson` it writes the
   collection prefix immediately, which can fail. `export` propagates
   that error before the loop.
4. `reader.for_each(|element| { ... })` with an outer
   `first_err: Option<crate::Error>` cell the closure writes on the
   first failure (write, serialize, or metadata-lookup error) and every
   subsequent element short-circuits to a no-op. Per-arm:
   - `Node`/`DenseNode`: **skip unless `types` admits nodes** (`All` or
     `NodesOnly` - R2 finding 8: nodes were previously never type-gated,
     so `--type way` could still emit Points); skip if untagged
     (`tags().next().is_none()`); skip if `bbox` set and
     `!bbox.contains(decimicro_lat, decimicro_lon)`; skip if expressions
     non-empty and no match; else write a Point feature. Dense and
     sparse nodes go through the generic property/metadata abstraction
     (3.1 properties.rs).
   - `Way`: skip unless `types` admits ways (`All` or `WaysOnly`); skip
     if untagged; skip if expressions non-empty and no match; build geometry by walking
     `node_locations()` into a coordinate vector, then validate
     (cardinality/closure/winding, see geometry.rs); if the geometry is
     invalid, bump `skipped_invalid_ways` and emit nothing; if bbox set,
     admit the way when any vertex is inside (vertex-containment
     overlap); write feature.
   - `Relation`: ignored in v1.
5. After the loop: if `first_err` is set OR `for_each` returned a decode
   `Err`, return that error (decode error takes precedence if both).
   Otherwise `writer.finish()` (writes collection suffix for `GeoJson`,
   flushes) and return `ExportStats`.

Error-propagation note (R1 finding 2 / R2 finding 4): because the
`for_each` closure returns `()`, a write error does not stop decoding -
the first error is captured and surfaced after the whole PBF is decoded.
A broken stdout pipe (consumer exited) therefore does not abort early in
v1; this is an accepted, documented limitation, not a stub. The CLI's
`PathGuard` (3.4) ensures a mid-stream error still leaves no partial
output file.

#### `geometry.rs`
```rust
/// Writes `{"type":"Point","coordinates":[lon,lat]}` into buf.
fn write_point(buf: &mut String, lon: f64, lat: f64);

/// Collect the way's vertices from node_locations() into a Vec<(lon,lat)>.
/// Single walk of the coordinate stream; geometry is built from these
/// actual coordinates, NOT from refs() (R2 finding 9).
fn collect_coords(way: &Way) -> Vec<(f64, f64)>;

/// Given collected coords and the area decision, emit LineString or
/// Polygon, or report the geometry invalid. Validation (R2 finding 9,
/// RFC 7946):
///   - LineString: >= 2 positions, else Invalid.
///   - Polygon: >= 4 positions AND first == last coordinate (close it if
///     the coordinate stream did not, but only when >= 3 distinct
///     positions exist; fewer -> Invalid). Exterior ring is oriented
///     counterclockwise (compute signed area; reverse if clockwise).
/// Returns whether a geometry was written; the caller bumps
/// skipped_invalid_ways on Invalid.
enum WayGeom { Written, Invalid }
fn write_way_geometry(buf: &mut String, coords: &[(f64, f64)], is_area: bool) -> WayGeom;

/// Area heuristic (see decision ADR-0010). A way is an area when it is
/// closed (first ref == last ref, >= 4 refs) AND not tag `area=no`
/// AND (`area=yes` OR it carries a key in AREA_KEYS).
fn is_area_way(way: &Way) -> bool;

const AREA_KEYS: &[&str] = &[
    "building", "landuse", "natural", "leisure", "amenity",
    "boundary", "waterway",  // waterway=riverbank etc.; excludes linear waterways via closed-ring test
];
```
The area *decision* is tested on `refs()` (first == last id, >= 4 refs),
so it is robust to coincident distinct nodes. But the emitted *geometry*
is built and validated from the actual `node_locations()` coordinate
stream, because `WayNodeLocationsIter` silently ends when either the ref
or the coordinate stream runs out (elements.rs:~412): a way the ref-based
heuristic calls "closed area" can yield fewer than four coordinates or a
non-coincident first/last coordinate. Validation therefore runs on the
collected coords, not on the ref count. An invalid way is a **counted
skip** (`skipped_invalid_ways`), never a hard error and never a malformed
Polygon (R2 finding 9). Polygon coordinates repeat the first point per
RFC 7946; the exterior ring is emitted counterclockwise. Coordinate order
is `[lon, lat]`. All coordinate tokens go through the shared formatter
(3.2).

Double-walk note (R1 minor): `is_area_way` walks `refs()`/`tags()` and
`collect_coords` walks `node_locations()` - two passes over the same way
plus `tags()` for expression matching. Accepted for sequential v1; noted
so a future parallel/fused rework knows the walks exist.

#### `properties.rs`

The emitter is generic over the tag iterator and takes an explicit
metadata abstraction so it serves sparse `Node`, `DenseNode`, and `Way`
alike (R2 finding 3):
```rust
/// Uniform metadata view over Node's non-optional Info (optional fields),
/// DenseNode's Option<&DenseNodeInfo>, and Way's Info. `None` = element
/// carried no info message at all (dense) -> no @-metadata emitted.
struct MetaView { version: Option<i32>, timestamp: Option<i64>,
                  changeset: Option<i64>, uid: Option<i32>,
                  user: Option<Result<&str>>, visible: Option<bool> }

/// Writes the `"properties":{...}` object value (the object only).
/// Generic over any Iterator<Item=(&str,&str)> tag source so DenseTagIter
/// and TagIter both fit. Fallible: propagates serde_json errors and the
/// user() decode error (Info::user / DenseNodeInfo::user are fallible).
fn write_properties<'a, T: Iterator<Item = (&'a str, &'a str)>>(
    buf: &mut String, id: i64, otype: &str, tags: T,
    meta: Option<&MetaView>, opts: &ExportOptions,
) -> crate::Result<()>;
```
Emission rules:
- Always emits `"@id"` (JSON number) and `"@type"` (`"node"`/`"way"`)
  first.
- With `--metadata` and a present `MetaView`: emits `"@version"` (number),
  `"@changeset"` (number), `"@uid"` (number), `"@user"` (string),
  `"@visible"` (boolean), and `"@timestamp"` as an **RFC 3339 UTC string**
  derived from the PBF's seconds-since-epoch timestamp (pin the unit
  explicitly - R2 finding 10; the raw field is seconds, emitted as an ISO
  string to match `osmium export`, not a bare integer). A field absent
  from the info message is simply omitted. `@visible` is emitted only
  when the underlying info actually carries visibility; ordinary `Info`
  with no visibility field and dense nodes with no info message emit no
  `@visible`.
- Tags: each selected tag as `"key":"value"`.

Reserved-key collision precedence (R2 finding 10, pinned in ADR-0010): a
source tag whose key is one of the emitted reserved keys (`@id`, `@type`,
or, under `--metadata`, `@version`/`@timestamp`/`@changeset`/`@uid`/
`@user`/`@visible`) is **suppressed** - the identity/metadata value wins
and the colliding tag is dropped, matching `osmium export`'s
attribute-name suppression. This guarantees no duplicate JSON object
names from reserved keys. For two OSM tags sharing the same key (rare but
possible on the wire), **last one wins** is NOT relied upon; instead the
emitter tracks emitted keys within the feature and drops a later
duplicate key, so output never contains duplicate names. Key/value JSON
escaping: each string token written via `serde_json` (`to_writer` into a
scratch `Vec<u8>` or `to_string`) so quotes/backslashes/control chars are
RFC 8259 correct. Geometry never goes through serde_json.

`--properties` whitelist: when `Some(keys)`, only tags whose key is in the
set are emitted (identity `@id`/`@type` and, when `--metadata`, the
`@`-metadata keys are always emitted regardless of the whitelist).

#### `writer.rs`
```rust
pub struct FeatureWriter<W> { out: W, format: ExportFormat,
                              wrote_any: bool, buf: String }
impl<W: Write> FeatureWriter<W> {
    // GeoJson: writes the collection prefix immediately, which can fail,
    // so new is fallible (R2 finding 4).
    fn new(out: W, format: ExportFormat) -> io::Result<Self>;
    fn write_feature_geometry_props(&mut self, geom: &str, props: &str)
        -> io::Result<()>;  // assembles {"type":"Feature","geometry":..,"properties":..}
    fn finish(mut self) -> io::Result<()>;          // GeoJson: write "]}" then flush
}
```
- **GeoJSONSeq (newline-delimited)**: each feature = one JSON object +
  `\n`, no wrapper, **no `0x1e` record separator** (R2 finding 1).
- **GeoJson**: prefix `{"type":"FeatureCollection","features":[`, then
  features comma-separated (track `wrote_any` to place the comma before
  every feature after the first), then `]}\n`. The empty-input case
  yields `{"type":"FeatureCollection","features":[]}\n`.
The single reusable `buf: String` is cleared per feature to keep memory
O(1) (design "Memory - streaming ... O(1)").

### 3.2 Shared coordinate formatter
Move `format_coord` **and its partner `from_decimicro`** out of
`src/osc/write.rs` into a new `src/coord_fmt.rs` as
`pub(crate) fn format_coord(&mut String, f64)` /
`pub(crate) fn from_decimicro(i64) -> f64`; add `mod coord_fmt;` to
`lib.rs`. They are co-imported and used together at every call site, so
moving one without the other splits a natural pair (R1 finding 1).
**Repoint every importer (R1 finding 1 / R2 finding 12):** `osc::write`
(defining module), `src/commands/diff/mod.rs:38`, and
`src/commands/tags_filter/osc.rs:15` all currently import from
`crate::osc::write`; each must import from `crate::coord_fmt` instead.
`commands::export` also calls `crate::coord_fmt::format_coord`. (If a
re-export is preferred over touching diff/tags_filter, leave
`pub(crate) use crate::coord_fmt::{format_coord, from_decimicro};` in
`osc::write`; either way the survey's "two consumers" claim is corrected
to four call-site modules.) Behavior is byte-identical (same 7-dp +
trailing-zero-trim). This step is bundled into the single export commit
(section 5), not a standalone landing.

### 3.3 Shared integer bbox
Promote `BboxInt` from `src/commands/extract/common.rs` (currently
`pub(super)`) into a shared `pub(crate)` location reachable by both
`extract` and `export`. **Placement (R1 finding 5):** put it in a small
`src/commands/spatial.rs` (`pub(crate) struct BboxInt` + `from_bbox` +
`contains`), NOT in `src/commands/mod.rs` - parking a domain struct in
the module-tree file while extract's `spatial_blob_filter` reaches into
its fields is the cross-module smell R1 flags; a named `spatial` module
reads cleaner and still adds only one file. extract's `common.rs`
re-imports `BboxInt` from `commands::spatial`; its `spatial_blob_filter`
(which needs `BlobFilter`) stays in extract.

**bbox parsing stays public (R2 finding 2):** `extract::parse_bbox` is
`pub` today and the CLI calls `pbfhogg::extract::parse_bbox`
(main.rs:2262). The earlier proposal to demote it to `pub(crate)` is
**rejected** - it would break the CLI's existing call. `parse_bbox`
remains `pub` in `extract`; `export`'s library-internal
`ExportOptions::new` calls it plus `BboxInt::from_bbox` when a bbox
string is supplied.

**bbox way-filter is vertex-only and lossy (R1 finding 4):** admitting a
way when any vertex is inside the box drops a way whose edge crosses the
box with no vertex inside, and drops a polygon that *encloses* the whole
bbox. This is a spatial-correctness limitation (not merely "no
clipping"); documented here and in the CLI help, accepted for v1.

### 3.4 CLI surface (`cli/src/main.rs`)
New `Command::Export` variant:
```rust
/// Export a PBF to GeoJSON / GeoJSONSeq (streaming).
Export {
    /// Input PBF (LocationsOnWays required for way export)
    file: PathBuf,
    /// Output file; omit to write GeoJSONSeq to stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Output format: geojsonseq (default) or geojson
    #[arg(long = "format", default_value = "geojsonseq")]
    format: ExportFormatArg,
    /// Only export this element type: node or way
    #[arg(long = "type")]
    type_filter: Option<ExportTypeArg>,
    /// Read filter expressions from file (one per line, # comments)
    #[arg(short = 'e', long = "expressions")]
    expressions_file: Option<PathBuf>,
    /// Tag filter expressions (e.g. "highway", "w/building=yes")
    expressions: Vec<String>,
    /// Comma-separated whitelist of tag keys to keep as properties
    #[arg(long = "properties", value_delimiter = ',')]
    properties: Option<Vec<String>>,
    /// Spatial filter min_lon,min_lat,max_lon,max_lat (vertex-overlap; lossy)
    #[arg(long = "bbox")]
    bbox: Option<String>,
    /// Include OSM metadata (@version, @timestamp, @changeset, @uid, @user, @visible)
    #[arg(long = "metadata")]
    metadata: bool,
},
```
`ExportFormatArg` is a `ValueEnum { Geojsonseq, Geojson }`. **`--type`
uses a dedicated 2-variant `ExportTypeArg` `ValueEnum { Node, Way }`
(R1 minor / R2 finding 8)**, NOT the 3-variant `DefaultTypeArg`: relation
is not a legal value, so clap rejects `--type relation` at parse time
with a self-documenting error instead of a runtime string check.
`ExportTypeArg` maps to `ExportTypes` (`None` -> `All`, `Node` ->
`NodesOnly`, `Way` -> `WaysOnly`).

`run_export`:
- Combines expressions (positional + `--expressions` file via the
  existing `combine_expressions`, which uses `pub read_expressions_file`)
  into a `Vec<String>` and passes the raw strings to
  `ExportOptions::new` (the library parses them; `parse_expressions` is
  crate-private and unreachable from the CLI - R2 finding 2).
- Same-file guard (R2 finding 7 / ADR-0003): if `output` is `Some(p)`,
  reject when `p` canonicalizes to the input path or is a hard link into
  it, *before* creating the file, so the input PBF is never truncated by
  a self-target.
- Output selection that type-checks (R2 finding 4): the two arms produce
  different writer types, so do NOT return them from one `match`
  expression. Either branch on `output` and call a generic
  `run_export_to<W: Write>(...)` once per arm, or erase to
  `Box<dyn Write>`. For the file arm, wrap the path in `PathGuard::file`
  at creation and `commit()` only after `export()` returns `Ok` and the
  writer has flushed (ADR-0003); on error the guard removes the partial
  file.
- Calls `pbfhogg::commands::export::export`, prints `ExportStats` to
  stderr (never stdout - stdout is pure GeoJSON so it stays pipeable into
  `jq`).

**Dispatch gating (single-commit landing - orchestrator constraint):**
the `Command::Export` arm is added to the CLI enum and dispatch as the
final step of the one export commit. Nothing registers `export` in the
dispatch match until the complete v1 behavior (nodes + ways + all
filters + both formats + metadata + validation + collision handling)
compiles and its in-tree tests pass. There is no interim released binary
that exposes a partial `export`.

### 3.5 New error variant
`ErrorKind::MissingFeature(&'static str)` in `src/error.rs` (boxed
ErrorKind pattern, matching `MissingHeader`). `Display`: "input PBF is
missing required feature: {feature}".

## 4. Resolved design open questions

1. **ID property.** Always emit `@id` and `@type` (matches
   `osmium export`, enables round-tripping). `@version` and the rest are
   opt-in via `--metadata`. Fixed in ADR-0010.
2. **Coordinate precision.** 7 decimal places (decimicrodegree), trailing
   zeros trimmed, via the shared `format_coord`. Not configurable in v1.
3. **Area heuristic.** Hard-coded `AREA_KEYS` list (section 3.1) plus
   `area=yes`/`area=no` overrides and a closed-ring requirement. Not a
   config file in v1. Recorded in ADR-0010 and DEVIATIONS.md (osmium's
   list differs, so output is not osmium-identical by design).
4. **Multipolygon assembly.** Deferred to v2 as a named separate TODO
   (section 1). v1 emits no relation features - not even single closed
   ways promoted from relations.
5. **Enriched-input requirement.** Hard error when ways are requested and
   the header lacks `LocationsOnWays`; no in-memory index fallback in v1.
   `--type node` skips the requirement.
6. **Default format framing (R2 finding 1).** Newline-delimited GeoJSON
   (one Feature per `\n`, no `0x1e`). The RFC 8142 claim is dropped;
   docs/help/tests call it "GeoJSONSeq (newline-delimited)".
7. **Reserved-key collisions (R2 finding 10).** Source tags colliding
   with an emitted reserved key (`@id`/`@type`/metadata keys) are
   suppressed; identity/metadata wins. Duplicate source tag keys are
   de-duplicated (first-seen key wins) so no duplicate JSON names are
   ever emitted. Fixed in ADR-0010.
8. **Metadata JSON types/units (R2 finding 10).** `@version`/`@changeset`/
   `@uid` are JSON numbers, `@user` a string, `@visible` a boolean,
   `@timestamp` an RFC 3339 UTC string derived from the seconds-epoch PBF
   field. Absent fields omitted; `@visible` emitted only when the info
   message carries visibility.
9. **Invalid way geometry (R2 finding 9).** A way whose collected
   coordinates fail cardinality/closure validation is a counted skip
   (`skipped_invalid_ways`), never a hard error and never a malformed
   Polygon. Exterior polygon rings are emitted counterclockwise per
   RFC 7946.
10. **Early-abort on broken pipe (R1 finding 2 / R2 finding 4).** Not
    supported in v1: `for_each`'s `()`-returning closure cannot stop the
    decode. The first write/serialize error is captured and returned
    after the full decode; the file output path is `PathGuard`-protected
    so no partial file survives. Adding a fallible reader callback is a
    named post-v1 follow-up, deliberately left out to keep the stopping
    rule bounded.

## 5. Landing as one commit (single coherent unit)

**This item lands as ONE commit and stays unexposed until its complete v1
behavior compiles and passes its in-tree gate** (orchestrator constraint;
resolves R1 finding 3 and R2 findings 5, 6, 11). The earlier six-brick,
`1 -> 6` sequence with a per-brick `brokkr check` gate and a real-data
`brokkr verify export` gate is retired: it required an interim released
binary that silently dropped ways (R1 finding 3 / R2 finding 5 bullet 1),
wired `--type` in a later brick than the brick whose tests depend on it
(R2 finding 5 bullet 2), gated brick 3 on an instrument delivered in
brick 6 (bullet 3), and benchmarked before the command was registered
(bullet 4). None of those can be independent, fully-gated landings.

The implementer works ONLY inside this pbfhogg workspace and CANNOT
modify the separate `brokkr` dev tool. Therefore no correctness gate may
depend on a brokkr change (`brokkr verify export`, a Denmark ALTW dataset
variant, or a brokkr GeoJSON scratch/benchmark output kind). The entire
v1 correctness gate is **in-tree**: inline unit tests plus one
golden-file CLI test (`tests/cli_export.rs`) driving the compiled binary
over synthetic fixtures the test constructs itself.

### Internal build order (all inside the one commit)
These are implementation stages, not separately-landed bricks. The commit
is not made - and `Command::Export` is not added to the CLI dispatch (3.4)
- until every stage below compiles and the in-tree gate is green.

1. **Shared coordinate formatter (3.2).** Move `format_coord` +
   `from_decimicro` to `src/coord_fmt.rs`, add `mod coord_fmt;` to
   `lib.rs`, repoint `osc::write`, `commands::diff`, and
   `commands::tags_filter::osc` (or leave a `pub(crate) use` re-export).
   Byte-identical; the existing inline `format_coord` unit tests move
   with it.
2. **Shared integer bbox (3.3).** Promote `BboxInt` into
   `src/commands/spatial.rs`; re-import from extract's `common.rs`; keep
   `extract::parse_bbox` `pub`.
3. **Error variant (3.5).** `ErrorKind::MissingFeature`.
4. **`export/` module (3.1).** `writer.rs`, `properties.rs` (generic tag
   iterator + `MetaView`, collision handling), `geometry.rs`
   (`collect_coords` + validation + winding + `is_area_way` +
   `AREA_KEYS`), `mod.rs` (`ExportOptions::new`, the reader loop with
   node/way arms, both formats, all filters, metadata, the
   `has_locations_on_ways` gate, error capture).
5. **CLI surface (3.4).** `ExportFormatArg`, 2-variant `ExportTypeArg`,
   `run_export` with the same-file guard, `PathGuard`-wrapped file
   output, generic-writer or `Box<dyn Write>` output selection, and -
   last - registration of `Command::Export` in the dispatch match.
6. **Docs / ADR (bundled in the same commit).**
   `decisions/0010-geojson-export-format-and-area-heuristic.md` (format
   choice, `@id`/`@type` identity model, `AREA_KEYS` + closed-ring rule,
   reserved-key collision precedence, polygon-validity/winding rule,
   "not osmium-parity" stance); a `DEVIATIONS.md` entry (export area
   detection + property model differ from `osmium export`, excluded from
   osmium byte-parity); `CHANGELOG.md` new-capability line; and the
   `export` command in `reference/cli-reference.md`.

### In-tree correctness gate (the only landing gate)
`brokkr check` (the `all` sweep builds `pbfhogg-cli` and runs
`tests/cli_export.rs`) plus the inline unit tests. The golden-file CLI
test builds every fixture itself with `BlockBuilder` + `PbfWriter` +
inline `LocationsOnWays` (stable allowlist only) and asserts **exact**
expected GeoJSON, covering at minimum:
- Node -> Point: `[lon,lat]` order, 7-dp trailing-zero-trimmed
  formatting, untagged node dropped, `@id`/`@type` present, JSON string
  escaping on a tag value containing a quote and a backslash.
- Way -> LineString for a `highway` way; Polygon for a `building` way
  with the ring closed (first coordinate repeated) and the exterior ring
  **counterclockwise** (R2 finding 9); an untagged way is skipped and
  counted in `skipped_untagged_ways`.
- Polygon validity (R2 finding 9): a synthetic enriched way whose
  coordinate stream is shorter than its ref count is a counted skip
  (`skipped_invalid_ways`), emits no feature, and never a malformed
  Polygon.
- Area heuristic truth table: `area=no` on a closed building -> LineString;
  `area=yes` on a closed untagged ring -> Polygon; a closed `highway` with
  no area key -> LineString.
- `LocationsOnWays` gate: a fixture WITHOUT `LocationsOnWays` -> `--type
  way` (and default) exits non-zero with the `MissingFeature` message;
  `--type node` on the same input still succeeds.
- Filters: `--type node` emits only Points AND `--type way` emits only
  way features (mixed fixture, both directions - R2 finding 8);
  `--expressions highway` emits only matching ways; `--properties
  name,highway` drops other tag keys but keeps `@id`/`@type`; `--bbox`
  drops out-of-box nodes and keeps a way with a vertex inside.
- Formats: default newline-delimited output has exactly one JSON object
  per `\n` line and **no `0x1e`** byte (R2 finding 1); `--format geojson`
  parses as a single `FeatureCollection` with the right feature count,
  including empty input -> `"features":[]`.
- `--metadata`: `@version`/`@timestamp`/... present with the pinned JSON
  types/units when the fixture carries `Info`, absent otherwise;
  `@timestamp` is the RFC 3339 string form.
- Reserved-key collision (R2 finding 10): a fixture node carrying a
  source tag literally named `@id` (and one named `@type`) emits the
  identity value, suppresses the colliding source tag, and produces no
  duplicate JSON object name.
- `tests/cli_extract.rs` stays green through the `BboxInt` move (it
  drives the CLI, so the promotion cannot break it by type change) -
  confirmed by the same `brokkr check`.

## 6. Deferred post-v1 follow-ups (orchestrator-owned, NOT bricks)

These require `brokkr` changes and so cannot be landing bricks the
in-workspace implementer builds. They are named here as explicit
follow-ups the orchestrator owns and schedules after the v1 commit lands.
None gates the v1 landing.

- **osmium cross-check + `brokkr verify export`** (R2 finding 11). When
  built, it must be *strict*, not the permissive comparator the prior
  spec described: compare feature identity exactly, non-area geometry
  exactly (coords within 1e-7), and the *expected* property set - not
  merely the intersection of keys both tools emit, which cannot detect
  pbfhogg dropping a tag. Area behavior needs an independent fixture
  matrix with expected classifications and geometry, not a comparator
  that accepts every area disagreement. The osmium invocation must be
  pinned to one that emits `@id`/`@type` (e.g. `osmium export -f
  geojsonseq` with `--add-unique-id` / an attributes config that names
  `id` and `type`); the exact command is part of building this
  instrument. The strict area/winding/collision assertions the reviews
  demand are already carried in-tree by the golden-file test (section 5),
  so this cross-check is additive confidence, not the primary gate.
- **Denmark ALTW artifact** (R2 finding 6). Denmark currently defines
  only `indexed` and `raw`; no `altw` variant exists (only Europe and
  planet do). A Denmark ALTW artifact must be produced and registered in
  `brokkr.toml` (or generated on demand by the verify instrument) before
  any `--variant altw` Denmark command resolves.
- **Throughput baseline** (was scheduled after brick 5). `brokkr export`
  is not a registered brokkr subcommand and brokkr has no GeoJSON scratch
  output kind, so `brokkr export --dataset denmark --variant altw
  --bench` cannot run until both the subcommand registration and an
  output kind are added in brokkr. Establish the v1 export throughput
  baseline (host + commit hash recorded against the row) once that
  plumbing and the Denmark ALTW artifact exist; record it in
  `reference/performance.md`. No `performance-history.md` arc is needed
  for a first-ever baseline.

## 7. Performance

Export is a brand-new command off every existing measured path: no
`reference/performance.md` baseline moves and no measured read/write path
changes shape. The coord-formatter move is byte-identical formatting on
the osc path; the `BboxInt` promotion is a visibility move with no
call-site logic change. Neutrality of the existing surface is confirmed by
the unchanged `brokkr check` result. The throughput baseline itself is a
deferred follow-up (section 6) because it needs brokkr plumbing this
workspace cannot add.

## 8. Test placement summary (per reference/testing.md)

- **Inline unit tests** (`src/commands/export/*.rs` `#[cfg(test)]`):
  `is_area_way` truth table, `write_point` / `write_way_geometry` string
  output (including winding correction and invalid-geometry skip),
  `write_properties` escaping, `--properties` whitelist, and reserved-key
  collision suppression, `FeatureWriter` seq-vs-collection framing and
  comma placement. These die with the module on rewrite, which is correct
  - they cover internals no CLI test reaches.
- **CLI integration** (`tests/cli_export.rs`, `CliInvoker`, stable
  allowlist only): the golden-file cases enumerated in section 5. This
  file is the refactor-immune surface and the primary v1 gate. Fixtures
  are built with `BlockBuilder` + `PbfWriter` + inline `LocationsOnWays`
  so internal rewrites cannot break them.
- **External cross-validation** (`brokkr verify export`): a deferred
  post-v1 follow-up (section 6), NOT a v1 gate; tier 5, release gate,
  never run in `brokkr check`.
- No fault-injection hooks: export adds no parallel pipeline, so the
  "three tests per parallel pipeline" policy proposal does not apply.

## 9. Stopping rule

The teardown is bounded to, all in one commit: one new
`src/commands/export/` module, one new `ErrorKind` variant, one
`format_coord`/`from_decimicro` module move, one `BboxInt` visibility
promotion into `commands::spatial`, one CLI subcommand, one ADR, and the
doc updates above. Nothing in the reader, writer, `BlockBuilder`,
`PbfWriter`, or any existing command's logic is restructured. Relations,
raw-PBF index fallback, GeoPackage, parallel export, coordinate-precision
configuration, configurable area rules, early-abort on broken pipe (a
fallible reader callback), and the brokkr-side deferred follow-ups
(section 6) are explicitly out of scope and are separate TODOs.

## 10. Review consolidation (R1 + R2)

Both review reports (`notes/geojson-review-r1-opus.md`,
`notes/geojson-review-r2-codex.md`) were validated against the source and
folded above. Duplicates were merged: R1 finding 1 and R2 finding 12 are
the same `format_coord` survey error; R1 finding 3 and R2 finding 5
(bullet 1) are the same brick-2 silent-way-drop, both resolved by the
single-commit landing.

**Folded (finding -> where):**
- R2-1 RFC 8142 -> newline-delimited framing: section 1, 3.4, 4.6,
  section 5 gate.
- R2-2 crate-private types in public API -> `ExportOptions::new` takes
  strings, `parse_bbox` stays `pub`: survey, 3.1, 3.3.
- R2-3 dense-node property API -> generic tag iterator + `MetaView`:
  survey, 3.1 properties.rs.
- R2-4 / R1-2 I/O + error flow -> fallible `FeatureWriter::new`, typed
  output selection, captured-error semantics, no early abort: survey,
  3.1, 3.4, 4.10.
- R2-5 / R1-3 landing order + silent way drop -> single-commit,
  unexposed-until-complete: section 5.
- R2-6 nonexistent Denmark ALTW -> deferred follow-up: section 6.
- R2-7 PathGuard + same-file -> ADR-0003 in survey, 3.1, 3.4.
- R2-8 node arm missing type filter -> type-gate every arm + both-way
  tests: 3.1, 3.4, section 5 gate.
- R2-9 polygon validity + winding -> `collect_coords` validation,
  counted skips, CCW exterior: 3.1 geometry.rs, 4.9, section 5 gate.
- R2-10 reserved-key collisions + metadata types -> suppression
  precedence + pinned JSON types/units: 3.1 properties.rs, 4.7, 4.8.
- R2-11 permissive verifier -> strict deferred cross-check + in-tree
  strict assertions: section 6, section 5 gate.
- R1-1 / R2-12 format_coord survey -> four call-site modules, move
  `from_decimicro` too: survey, 3.2.
- R1-4 vertex-only bbox lossy -> documented limitation: 3.3, 3.4 help.
- R1-5 `BboxInt` in mod.rs smell -> `commands::spatial.rs`: 3.3.
- R1 minor (double-walk) -> noted: 3.1 geometry.rs.
- R1 minor (2-variant `--type` enum) -> `ExportTypeArg`: 3.4.
- R1 minor (`write_properties` param redundancy) -> subsumed by the
  properties.rs redesign for dense nodes (R2-3): 3.1.

**Rejected:**
- R1 minor "ADR numbering gaps (0001/0008 unused)": informational only;
  0010 is the correct next free number. Noted in the survey, no spec
  change.
- R2-2's sub-claim that `parse_bbox` must be *made* crate-private: the
  spec's own earlier proposal to demote it is what R2 objected to;
  keeping `parse_bbox` `pub` (the current reality the CLI depends on) is
  the fix, so the demotion proposal is dropped rather than adopted.
