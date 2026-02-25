//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::Path;

use crate::block_builder::{build_header, BlockBuilder, MemberData, Metadata};
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element, MemberId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Bounding box
// ---------------------------------------------------------------------------

/// A geographic bounding box in WGS84 degrees.
pub struct Bbox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bbox {
    /// Returns `true` if the point (lat, lon) in degrees falls within this bbox.
    fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

/// Parse a bbox string in osmium convention: `minlon,minlat,maxlon,maxlat`.
pub fn parse_bbox(s: &str) -> Result<Bbox> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(format!("bbox must have 4 comma-separated values, got {}", parts.len()).into());
    }
    let min_lon: f64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| format!("invalid min_lon: {}", parts[0]))?;
    let min_lat: f64 = parts[1]
        .trim()
        .parse()
        .map_err(|_| format!("invalid min_lat: {}", parts[1]))?;
    let max_lon: f64 = parts[2]
        .trim()
        .parse()
        .map_err(|_| format!("invalid max_lon: {}", parts[2]))?;
    let max_lat: f64 = parts[3]
        .trim()
        .parse()
        .map_err(|_| format!("invalid max_lat: {}", parts[3]))?;

    if min_lon >= max_lon {
        return Err(format!("min_lon ({min_lon}) must be less than max_lon ({max_lon})").into());
    }
    if min_lat >= max_lat {
        return Err(format!("min_lat ({min_lat}) must be less than max_lat ({max_lat})").into());
    }

    Ok(Bbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

// ---------------------------------------------------------------------------
// Region
// ---------------------------------------------------------------------------

/// A geographic region filter for extraction.
pub enum Region {
    /// Rectangular bounding box.
    Bbox(Bbox),
    /// Polygon with optional holes (and precomputed bounding box for fast rejection).
    /// Coordinates are (lon, lat) pairs in degrees, following GeoJSON convention.
    Polygon {
        /// All polygons (exterior ring + holes each). For simple Polygon, this has one entry.
        /// For MultiPolygon, one entry per polygon.
        polygons: Vec<PolygonRings>,
        /// Precomputed bounding box of all exterior rings (for fast rejection).
        bbox: Bbox,
    },
}

/// A single polygon: exterior ring + optional holes.
pub struct PolygonRings {
    /// Exterior ring: Vec of (lon, lat) in degrees.
    pub exterior: Vec<(f64, f64)>,
    /// Interior rings (holes): Vec of rings, each a Vec of (lon, lat).
    pub holes: Vec<Vec<(f64, f64)>>,
}

impl Region {
    /// Returns true if the point (lat, lon) in degrees falls within this region.
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        match self {
            Region::Bbox(bbox) => bbox.contains(lat, lon),
            Region::Polygon { polygons, bbox } => {
                if !bbox.contains(lat, lon) {
                    return false;
                }
                polygon_contains(polygons, lon, lat)
            }
        }
    }

    /// Returns the bounding box of this region.
    pub fn bbox(&self) -> &Bbox {
        match self {
            Region::Bbox(bbox) => bbox,
            Region::Polygon { bbox, .. } => bbox,
        }
    }
}

/// Check if any polygon in the list contains the point (px=lon, py=lat).
fn polygon_contains(polygons: &[PolygonRings], px: f64, py: f64) -> bool {
    polygons.iter().any(|p| polygon_rings_contains(p, px, py))
}

/// Check if a single polygon (exterior + holes) contains the point.
fn polygon_rings_contains(poly: &PolygonRings, px: f64, py: f64) -> bool {
    if !point_in_ring(px, py, &poly.exterior) {
        return false;
    }
    !poly.holes.iter().any(|hole| point_in_ring(px, py, hole))
}

/// Ray-casting point-in-polygon test.
/// Point and ring vertices are (lon, lat) == (x, y).
fn point_in_ring(px: f64, py: f64, ring: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// GeoJSON parsing
// ---------------------------------------------------------------------------

/// Parse a GeoJSON file and extract polygon geometry as a `Region`.
///
/// Accepts:
/// - A bare Geometry with type "Polygon" or "MultiPolygon"
/// - A Feature with a Polygon/MultiPolygon geometry
/// - A FeatureCollection whose first feature has a Polygon/MultiPolygon geometry
pub fn parse_geojson(path: &Path) -> Result<Region> {
    let data = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&data)?;
    let geometry = extract_geometry(&value)?;
    let geo_type = geometry
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or("geometry missing 'type' field")?;
    let coords = geometry
        .get("coordinates")
        .ok_or("geometry missing 'coordinates' field")?;
    let polygons = parse_geometry_by_type(geo_type, coords)?;
    let bbox = bbox_from_polygons(&polygons)?;
    Ok(Region::Polygon { polygons, bbox })
}

/// Navigate Feature/FeatureCollection to find the geometry object.
fn extract_geometry(value: &serde_json::Value) -> Result<serde_json::Value> {
    let obj_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or("GeoJSON missing 'type' field")?;
    match obj_type {
        "Polygon" | "MultiPolygon" => Ok(value.clone()),
        "Feature" => {
            let geom = value
                .get("geometry")
                .ok_or("Feature missing 'geometry' field")?;
            Ok(geom.clone())
        }
        "FeatureCollection" => {
            let features = value
                .get("features")
                .and_then(serde_json::Value::as_array)
                .ok_or("FeatureCollection missing 'features' array")?;
            let first = features.first().ok_or("FeatureCollection has no features")?;
            let geom = first
                .get("geometry")
                .ok_or("first Feature missing 'geometry' field")?;
            Ok(geom.clone())
        }
        other => Err(format!("unsupported GeoJSON type: {other}").into()),
    }
}

/// Dispatch to the right parser based on geometry type.
fn parse_geometry_by_type(
    geo_type: &str,
    coords: &serde_json::Value,
) -> Result<Vec<PolygonRings>> {
    match geo_type {
        "Polygon" => {
            let poly = parse_polygon_coordinates(coords)?;
            Ok(vec![poly])
        }
        "MultiPolygon" => {
            let arr = coords
                .as_array()
                .ok_or("MultiPolygon coordinates must be an array")?;
            let mut polygons = Vec::with_capacity(arr.len());
            for polygon_coords in arr {
                polygons.push(parse_polygon_coordinates(polygon_coords)?);
            }
            Ok(polygons)
        }
        other => Err(format!("unsupported geometry type: {other}").into()),
    }
}

/// Parse one polygon's coordinate array: `[exterior_ring, hole1, hole2, ...]`.
fn parse_polygon_coordinates(coords: &serde_json::Value) -> Result<PolygonRings> {
    let rings = coords
        .as_array()
        .ok_or("polygon coordinates must be an array of rings")?;
    let exterior_val = rings.first().ok_or("polygon must have at least one ring")?;
    let exterior = parse_ring(exterior_val)?;
    let mut holes = Vec::new();
    for hole_val in rings.iter().skip(1) {
        holes.push(parse_ring(hole_val)?);
    }
    Ok(PolygonRings { exterior, holes })
}

/// Parse one ring's coordinate array: `[[lon, lat], ...]`.
fn parse_ring(ring: &serde_json::Value) -> Result<Vec<(f64, f64)>> {
    let points = ring
        .as_array()
        .ok_or("ring must be an array of coordinate pairs")?;
    let mut result = Vec::with_capacity(points.len());
    for point in points {
        let pair = point
            .as_array()
            .ok_or("coordinate must be a [lon, lat] array")?;
        if pair.len() < 2 {
            return Err("coordinate array must have at least 2 elements".into());
        }
        let lon = pair[0]
            .as_f64()
            .ok_or("coordinate lon must be a number")?;
        let lat = pair[1]
            .as_f64()
            .ok_or("coordinate lat must be a number")?;
        result.push((lon, lat));
    }
    Ok(result)
}

/// Compute the enclosing bounding box from all exterior ring vertices.
fn bbox_from_polygons(polygons: &[PolygonRings]) -> Result<Bbox> {
    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut found_any = false;

    for poly in polygons {
        for &(lon, lat) in &poly.exterior {
            found_any = true;
            if lon < min_lon {
                min_lon = lon;
            }
            if lat < min_lat {
                min_lat = lat;
            }
            if lon > max_lon {
                max_lon = lon;
            }
            if lat > max_lat {
                max_lat = lat;
            }
        }
    }

    if !found_any {
        return Err("no exterior ring vertices found for bounding box".into());
    }

    Ok(Bbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

pub struct ExtractStats {
    pub nodes_in_bbox: u64,
    pub nodes_from_ways: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    pub strategy: &'static str,
}

impl ExtractStats {
    pub fn print_summary(&self) {
        eprintln!(
            "Extract ({}): {} nodes ({} in bbox, {} from ways), {} ways, {} relations",
            self.strategy,
            self.nodes_in_bbox + self.nodes_from_ways,
            self.nodes_in_bbox,
            self.nodes_from_ways,
            self.ways_written,
            self.relations_written,
        );
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Extract elements within `region` from `input` and write to `output`.
///
/// If `simple` is true, uses a single-pass strategy (fast but ways may reference
/// nodes outside the extract). Otherwise uses `complete_ways` (two passes, all
/// nodes of matching ways are included).
#[hotpath::measure]
pub fn extract(
    input: &Path,
    output: &Path,
    region: &Region,
    simple: bool,
) -> Result<ExtractStats> {
    if simple {
        extract_simple(input, output, region)
    } else {
        extract_complete_ways(input, output, region)
    }
}

// ---------------------------------------------------------------------------
// Simple strategy (single pass)
// ---------------------------------------------------------------------------

fn extract_simple(input: &Path, output: &Path, region: &Region) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        ways_written: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let mut writer = PbfWriter::to_path(output, Compression::default())?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;

    // OPTIMIZATION: Use sorted Vec<i64> instead of BTreeSet<i64> for matched element IDs.
    //
    // Previously these were BTreeSet<i64>, which stores each entry in a B-tree node
    // with ~40 bytes overhead per entry (node pointers, balance metadata). For large
    // extracts with millions of matched IDs, this wastes significant memory.
    //
    // Sorted Vec<i64> uses exactly 8 bytes per entry (just the i64 itself), a ~5x
    // memory reduction. Lookups use binary_search() which is O(log n) -- the same
    // asymptotic complexity as BTreeSet::contains() -- but with much better cache
    // locality since the data is contiguous in memory.
    //
    // Alternatives considered:
    // - HashSet<i64>: Even worse memory overhead (~72 bytes/entry due to hash table
    //   buckets, load factor headroom, and per-entry hash storage).
    // - roaring::RoaringBitmap: Excellent compression for dense ID ranges, but adds
    //   an external dependency. Overkill for extract-sized sets where the simple
    //   sorted Vec approach is sufficient.
    //
    // The sort+dedup step is deferred until the first lookup via boolean flags. This
    // is safe because OSM PBF files are conventionally ordered: all nodes come before
    // all ways, and all ways come before all relations. So by the time we need to
    // look up node IDs (when processing ways), all nodes have already been collected,
    // and similarly for way IDs when processing relations.
    //
    // sort_unstable() is used instead of sort() because i64 has no meaningful
    // stability requirement (equal elements are identical), and sort_unstable()
    // avoids the temporary allocation that sort() needs for its merge step.
    let mut matched_node_ids: Vec<i64> = Vec::new();
    let mut matched_way_ids: Vec<i64> = Vec::new();
    // Track whether each Vec has been sorted+deduped yet, to sort lazily on first
    // lookup. This avoids sorting after every push (which would be O(n^2) total).
    let mut node_ids_sorted = false;
    let mut way_ids_sorted = false;

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    write_extract_header(region, &header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            if region.contains(dn.lat(), dn.lon()) {
                                matched_node_ids.push(dn.id());
                                write_dense_node(dn, &mut bb, &mut writer)?;
                                stats.nodes_in_bbox += 1;
                            }
                        }
                        Element::Node(n) => {
                            if region.contains(n.lat(), n.lon()) {
                                matched_node_ids.push(n.id());
                                write_node(n, &mut bb, &mut writer)?;
                                stats.nodes_in_bbox += 1;
                            }
                        }
                        Element::Way(w) => {
                            // Sort+dedup node IDs on first way encounter. All nodes
                            // precede ways in OSM PBF file order, so the Vec is
                            // complete by the time we reach the first way.
                            if !node_ids_sorted {
                                matched_node_ids.sort_unstable();
                                matched_node_ids.dedup();
                                node_ids_sorted = true;
                            }
                            if w.refs().any(|r| matched_node_ids.binary_search(&r).is_ok()) {
                                matched_way_ids.push(w.id());
                                write_way(w, &mut bb, &mut writer)?;
                                stats.ways_written += 1;
                            }
                        }
                        Element::Relation(r) => {
                            // Sort+dedup way IDs on first relation encounter. All ways
                            // precede relations in OSM PBF file order, so the Vec is
                            // complete by the time we reach the first relation.
                            // Also ensure node IDs are sorted (in case the file
                            // contained no ways, the sort would not have triggered yet).
                            if !node_ids_sorted {
                                matched_node_ids.sort_unstable();
                                matched_node_ids.dedup();
                                node_ids_sorted = true;
                            }
                            if !way_ids_sorted {
                                matched_way_ids.sort_unstable();
                                matched_way_ids.dedup();
                                way_ids_sorted = true;
                            }
                            if relation_has_matched_member(r, &matched_node_ids, &matched_way_ids) {
                                write_relation(r, &mut bb, &mut writer)?;
                                stats.relations_written += 1;
                            }
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Complete-ways strategy (two passes)
// ---------------------------------------------------------------------------

fn extract_complete_ways(input: &Path, output: &Path, region: &Region) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        ways_written: 0,
        relations_written: 0,
        strategy: "complete_ways",
    };

    // --- Pass 1: Collect matches ---
    //
    // OPTIMIZATION: Use Vec<i64> instead of BTreeSet<i64> for all ID collections.
    //
    // Previously these were BTreeSet<i64>, which has ~40 bytes per-entry overhead
    // from B-tree node allocations (child/parent pointers, balance metadata). For a
    // country-sized extract with millions of matched nodes and ways, this overhead
    // dominates memory usage.
    //
    // Sorted Vec<i64> stores just the raw 8-byte i64 values contiguously, giving
    // ~5x memory reduction. Lookups via binary_search() have the same O(log n)
    // complexity as BTreeSet::contains() but with better cache locality.
    //
    // During pass 1, some Vecs are queried while others are still being built:
    // bbox_node_ids is looked up when processing ways, and matched_way_ids is looked
    // up when processing relations. This works because OSM PBF files are ordered
    // (nodes -> ways -> relations), so each Vec is complete before its first lookup.
    // Lazy sorting via boolean flags ensures each Vec is sorted exactly once, right
    // before its first binary_search.
    //
    // After pass 1 completes, all four Vecs are sorted+deduped for pass 2 lookups.
    //
    // Alternatives considered:
    // - HashSet<i64>: Worse memory (~72 bytes/entry from hash buckets and load factor).
    // - roaring::RoaringBitmap: Great compression for dense ranges but adds an
    //   external dependency; overkill for typical extract sizes.
    //
    // sort_unstable() is preferred over sort() for primitive types -- no stability
    // needed (equal i64 values are identical), and it avoids the temporary buffer
    // allocation that sort() uses internally for its merge step.
    let mut bbox_node_ids: Vec<i64> = Vec::new();
    let mut matched_way_ids: Vec<i64> = Vec::new();
    let mut all_way_node_ids: Vec<i64> = Vec::new();
    let mut matched_relation_ids: Vec<i64> = Vec::new();
    // Lazy-sort flags for within-pass-1 lookups. These persist across blocks so
    // each Vec is sorted at most once during pass 1 (on the first block that needs
    // to look it up). See collect_pass1_matches for details.
    let mut bbox_node_ids_sorted = false;
    let mut way_ids_sorted = false;

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(_) => {}
            BlobDecode::OsmData(block) => {
                collect_pass1_matches(
                    &block,
                    region,
                    &mut bbox_node_ids,
                    &mut matched_way_ids,
                    &mut all_way_node_ids,
                    &mut matched_relation_ids,
                    &mut bbox_node_ids_sorted,
                    &mut way_ids_sorted,
                );
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    // Sort and deduplicate all ID Vecs between pass 1 and pass 2. This is the key
    // step that makes binary_search() valid for pass 2 lookups.
    //
    // bbox_node_ids and matched_way_ids may already be sorted from lazy sorting
    // during pass 1 (see collect_pass1_matches), but sorting an already-sorted Vec
    // is O(n) with sort_unstable's pattern detection, so the redundant sort is cheap.
    //
    // all_way_node_ids is only ever appended to during pass 1 (never looked up), so
    // this is its first sort. matched_relation_ids is similarly only appended to
    // during pass 1.
    bbox_node_ids.sort_unstable();
    bbox_node_ids.dedup();
    matched_way_ids.sort_unstable();
    matched_way_ids.dedup();
    all_way_node_ids.sort_unstable();
    all_way_node_ids.dedup();
    matched_relation_ids.sort_unstable();
    matched_relation_ids.dedup();

    // --- Pass 2: Write matching elements in file order ---
    let mut writer = PbfWriter::to_path(output, Compression::default())?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    write_extract_header(region, &header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                write_pass2_elements(
                    &block,
                    &bbox_node_ids,
                    &all_way_node_ids,
                    &matched_way_ids,
                    &matched_relation_ids,
                    &mut bb,
                    &mut writer,
                    &mut stats,
                )?;
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

/// Collect matching element IDs during pass 1 of the complete-ways strategy.
///
/// This function is called once per PrimitiveBlock. Within pass 1, some Vecs need
/// to be looked up while others are still being populated:
/// - `bbox_node_ids` is queried when processing ways (to check if a way has any
///   node in the region).
/// - `matched_way_ids` is queried when processing relations (to check if a relation
///   references a matched way).
///
/// This works because OSM PBF files are ordered: all node blocks come before way
/// blocks, and all way blocks come before relation blocks. We lazily sort each Vec
/// on the first block that needs to look it up. The `bbox_node_ids_sorted` and
/// `way_ids_sorted` flags persist across block boundaries (passed by the caller)
/// so each Vec is sorted at most once.
///
/// After pass 1 completes, the caller performs a final sort+dedup on all Vecs to
/// prepare them for pass 2 lookups.
#[allow(clippy::too_many_arguments)]
fn collect_pass1_matches(
    block: &crate::PrimitiveBlock,
    region: &Region,
    bbox_node_ids: &mut Vec<i64>,
    matched_way_ids: &mut Vec<i64>,
    all_way_node_ids: &mut Vec<i64>,
    matched_relation_ids: &mut Vec<i64>,
    bbox_node_ids_sorted: &mut bool,
    way_ids_sorted: &mut bool,
) {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                if region.contains(dn.lat(), dn.lon()) {
                    bbox_node_ids.push(dn.id());
                }
            }
            Element::Node(n) => {
                if region.contains(n.lat(), n.lon()) {
                    bbox_node_ids.push(n.id());
                }
            }
            Element::Way(w) => {
                // Sort+dedup bbox_node_ids on first way encounter. All nodes precede
                // ways in OSM PBF file order, so the Vec is complete by now.
                if !*bbox_node_ids_sorted {
                    bbox_node_ids.sort_unstable();
                    bbox_node_ids.dedup();
                    *bbox_node_ids_sorted = true;
                }
                if w.refs().any(|r| bbox_node_ids.binary_search(&r).is_ok()) {
                    matched_way_ids.push(w.id());
                    all_way_node_ids.extend(w.refs());
                }
            }
            Element::Relation(r) => {
                // Sort+dedup matched_way_ids on first relation encounter. All ways
                // precede relations in OSM PBF file order, so the Vec is complete.
                // Also ensure bbox_node_ids is sorted (for relation_has_matched_member
                // to check node membership, and in case the file had no ways).
                if !*bbox_node_ids_sorted {
                    bbox_node_ids.sort_unstable();
                    bbox_node_ids.dedup();
                    *bbox_node_ids_sorted = true;
                }
                if !*way_ids_sorted {
                    matched_way_ids.sort_unstable();
                    matched_way_ids.dedup();
                    *way_ids_sorted = true;
                }
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) {
                    matched_relation_ids.push(r.id());
                }
            }
        }
    }
}

/// Write matching elements during pass 2. All ID Vecs are pre-sorted+deduped by
/// the caller between pass 1 and pass 2, so binary_search() is valid here.
#[allow(clippy::too_many_arguments)]
fn write_pass2_elements(
    block: &crate::PrimitiveBlock,
    bbox_node_ids: &[i64],
    all_way_node_ids: &[i64],
    matched_way_ids: &[i64],
    matched_relation_ids: &[i64],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                // binary_search on sorted slices: O(log n) lookup, same as BTreeSet
                // but with contiguous memory for better cache performance.
                let in_bbox = bbox_node_ids.binary_search(&dn.id()).is_ok();
                let from_way = all_way_node_ids.binary_search(&dn.id()).is_ok();
                if in_bbox || from_way {
                    write_dense_node(dn, bb, writer)?;
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Node(n) => {
                let in_bbox = bbox_node_ids.binary_search(&n.id()).is_ok();
                let from_way = all_way_node_ids.binary_search(&n.id()).is_ok();
                if in_bbox || from_way {
                    write_node(n, bb, writer)?;
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Way(w) => {
                if matched_way_ids.binary_search(&w.id()).is_ok() {
                    write_way(w, bb, writer)?;
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                if matched_relation_ids.binary_search(&r.id()).is_ok() {
                    write_relation(r, bb, writer)?;
                    stats.relations_written += 1;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation member matching
// ---------------------------------------------------------------------------

/// Check if a relation has any member whose ID is in the matched node or way sets.
///
/// Takes sorted slices (not BTreeSet) -- uses binary_search() for O(log n) lookups
/// with contiguous memory layout for better cache performance than tree-based lookups.
fn relation_has_matched_member(
    r: &crate::Relation,
    node_ids: &[i64],
    way_ids: &[i64],
) -> bool {
    r.members().any(|m| match m.id {
        MemberId::Node(id) => node_ids.binary_search(&id).is_ok(),
        MemberId::Way(id) => way_ids.binary_search(&id).is_ok(),
        MemberId::Relation(_) | MemberId::Unknown(_, _) => false,
    })
}

// ---------------------------------------------------------------------------
// Element writers
// ---------------------------------------------------------------------------

fn write_dense_node(
    dn: &crate::DenseNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = dn.tags().collect();
    let meta = dn.info().and_then(|info| {
        let user = info.user().ok()?;
        Some(Metadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: info.changeset(),
            uid: info.uid(),
            user,
            visible: info.visible(),
        })
    });
    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags, meta.as_ref());
    Ok(())
}

fn write_node(
    n: &crate::Node,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = n.tags().collect();
    let info = n.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags, meta.as_ref());
    Ok(())
}

fn write_way(
    w: &crate::Way,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = w.tags().collect();
    let refs: Vec<i64> = w.refs().collect();
    let info = w.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_way(w.id(), &tags, &refs, meta.as_ref());
    Ok(())
}

fn write_relation(
    r: &crate::Relation,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = r.tags().collect();
    let members: Vec<MemberData<'_>> = r
        .members()
        .map(|m| MemberData {
            id: m.id,
            role: m.role().unwrap_or(""),
        })
        .collect();
    let info = r.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_relation(r.id(), &tags, &members, meta.as_ref());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}

fn write_extract_header(
    region: &Region,
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    let bbox = region.bbox();
    let header_bytes = build_header(
        Some((bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)),
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
        &[],
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests -- it immediately fails the test with a clear backtrace pointing
// to the exact call site. Propagating Results via `-> Result<()>` in tests would
// lose the backtrace and produce less actionable error messages. The crate-wide
// `unwrap_used = "deny"` lint is designed for production code where panics are
// unacceptable; test code is exempt via this module-level allow.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    #[test]
    fn parse_valid_bbox() {
        let b = parse_bbox("12.4,55.6,12.7,55.8").unwrap();
        assert!((b.min_lon - 12.4).abs() < 1e-9);
        assert!((b.min_lat - 55.6).abs() < 1e-9);
        assert!((b.max_lon - 12.7).abs() < 1e-9);
        assert!((b.max_lat - 55.8).abs() < 1e-9);
    }

    #[test]
    fn parse_bbox_wrong_count() {
        assert!(parse_bbox("12.4,55.6,12.7").is_err());
        assert!(parse_bbox("12.4,55.6,12.7,55.8,1.0").is_err());
    }

    #[test]
    fn parse_bbox_invalid_number() {
        assert!(parse_bbox("abc,55.6,12.7,55.8").is_err());
    }

    #[test]
    fn parse_bbox_min_ge_max() {
        assert!(parse_bbox("12.7,55.6,12.4,55.8").is_err());
        assert!(parse_bbox("12.4,55.8,12.7,55.6").is_err());
    }

    #[test]
    fn bbox_contains_inside() {
        let b = Bbox {
            min_lon: 12.0,
            min_lat: 55.0,
            max_lon: 13.0,
            max_lat: 56.0,
        };
        assert!(b.contains(55.5, 12.5));
    }

    #[test]
    fn bbox_contains_outside() {
        let b = Bbox {
            min_lon: 12.0,
            min_lat: 55.0,
            max_lon: 13.0,
            max_lat: 56.0,
        };
        assert!(!b.contains(54.0, 12.5));
        assert!(!b.contains(55.5, 14.0));
    }

    #[test]
    fn bbox_contains_edge() {
        let b = Bbox {
            min_lon: 12.0,
            min_lat: 55.0,
            max_lon: 13.0,
            max_lat: 56.0,
        };
        assert!(b.contains(55.0, 12.0));
        assert!(b.contains(56.0, 13.0));
    }

    // -----------------------------------------------------------------------
    // point_in_ring tests
    // -----------------------------------------------------------------------

    #[test]
    fn point_in_square() {
        // Unit square: (0,0), (1,0), (1,1), (0,1), (0,0)
        let square = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)];
        // Inside
        assert!(point_in_ring(0.5, 0.5, &square));
        // Outside
        assert!(!point_in_ring(2.0, 0.5, &square));
        assert!(!point_in_ring(0.5, 2.0, &square));
        assert!(!point_in_ring(-0.5, 0.5, &square));
    }

    #[test]
    fn point_in_triangle() {
        // Triangle: (0,0), (4,0), (2,3), (0,0)
        let triangle = vec![(0.0, 0.0), (4.0, 0.0), (2.0, 3.0), (0.0, 0.0)];
        // Inside
        assert!(point_in_ring(2.0, 1.0, &triangle));
        // Outside
        assert!(!point_in_ring(0.0, 3.0, &triangle));
        assert!(!point_in_ring(5.0, 1.0, &triangle));
    }

    #[test]
    fn point_in_concave() {
        // L-shaped polygon (concave):
        // (0,0), (2,0), (2,1), (1,1), (1,2), (0,2), (0,0)
        let l_shape = vec![
            (0.0, 0.0),
            (2.0, 0.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (1.0, 2.0),
            (0.0, 2.0),
            (0.0, 0.0),
        ];
        // Inside the bottom part
        assert!(point_in_ring(1.5, 0.5, &l_shape));
        // Inside the left part
        assert!(point_in_ring(0.5, 1.5, &l_shape));
        // Outside: in the upper-right concavity
        assert!(!point_in_ring(1.5, 1.5, &l_shape));
        // Fully outside
        assert!(!point_in_ring(3.0, 1.0, &l_shape));
    }

    #[test]
    fn point_in_ring_degenerate() {
        // Empty ring
        assert!(!point_in_ring(0.0, 0.0, &[]));
        // Two-point ring (not a valid polygon)
        assert!(!point_in_ring(0.0, 0.0, &[(0.0, 0.0), (1.0, 1.0)]));
    }

    // -----------------------------------------------------------------------
    // Region::Polygon tests
    // -----------------------------------------------------------------------

    #[test]
    fn polygon_region_contains() {
        // Square polygon from (10, 50) to (12, 52) in (lon, lat)
        let region = Region::Polygon {
            polygons: vec![PolygonRings {
                exterior: vec![
                    (10.0, 50.0),
                    (12.0, 50.0),
                    (12.0, 52.0),
                    (10.0, 52.0),
                    (10.0, 50.0),
                ],
                holes: vec![],
            }],
            bbox: Bbox {
                min_lon: 10.0,
                min_lat: 50.0,
                max_lon: 12.0,
                max_lat: 52.0,
            },
        };
        // Inside: lat=51, lon=11
        assert!(region.contains(51.0, 11.0));
        // Outside
        assert!(!region.contains(53.0, 11.0));
        assert!(!region.contains(51.0, 13.0));
    }

    #[test]
    fn polygon_region_hole() {
        // Square with a hole in the center
        let region = Region::Polygon {
            polygons: vec![PolygonRings {
                exterior: vec![
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ],
                holes: vec![vec![
                    (3.0, 3.0),
                    (7.0, 3.0),
                    (7.0, 7.0),
                    (3.0, 7.0),
                    (3.0, 3.0),
                ]],
            }],
            bbox: Bbox {
                min_lon: 0.0,
                min_lat: 0.0,
                max_lon: 10.0,
                max_lat: 10.0,
            },
        };
        // Inside exterior but outside hole: lat=1, lon=1
        assert!(region.contains(1.0, 1.0));
        // Inside hole: lat=5, lon=5
        assert!(!region.contains(5.0, 5.0));
        // Outside entirely
        assert!(!region.contains(15.0, 5.0));
    }

    #[test]
    fn polygon_region_bbox_rejects() {
        // Point well outside the bbox should be rejected quickly
        let region = Region::Polygon {
            polygons: vec![PolygonRings {
                exterior: vec![
                    (10.0, 50.0),
                    (12.0, 50.0),
                    (12.0, 52.0),
                    (10.0, 52.0),
                    (10.0, 50.0),
                ],
                holes: vec![],
            }],
            bbox: Bbox {
                min_lon: 10.0,
                min_lat: 50.0,
                max_lon: 12.0,
                max_lat: 52.0,
            },
        };
        // lat=0, lon=0 -- outside bbox
        assert!(!region.contains(0.0, 0.0));
    }

    // -----------------------------------------------------------------------
    // Region::Bbox pass-through
    // -----------------------------------------------------------------------

    #[test]
    fn region_bbox_contains() {
        let region = Region::Bbox(Bbox {
            min_lon: 12.0,
            min_lat: 55.0,
            max_lon: 13.0,
            max_lat: 56.0,
        });
        assert!(region.contains(55.5, 12.5));
        assert!(!region.contains(54.0, 12.5));
    }

    #[test]
    fn region_bbox_accessor() {
        let region = Region::Bbox(Bbox {
            min_lon: 1.0,
            min_lat: 2.0,
            max_lon: 3.0,
            max_lat: 4.0,
        });
        let b = region.bbox();
        assert!((b.min_lon - 1.0).abs() < 1e-9);
        assert!((b.min_lat - 2.0).abs() < 1e-9);
        assert!((b.max_lon - 3.0).abs() < 1e-9);
        assert!((b.max_lat - 4.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // parse_geojson tests
    // -----------------------------------------------------------------------

    fn write_temp_geojson(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parse_geojson_bare_polygon() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "Polygon",
            "coordinates": [
                [[10.0, 50.0], [12.0, 50.0], [12.0, 52.0], [10.0, 52.0], [10.0, 50.0]]
            ]
        }"#;
        let path = write_temp_geojson(&dir, "bare.geojson", json);
        let region = parse_geojson(&path).unwrap();
        // Should contain a point inside
        assert!(region.contains(51.0, 11.0));
        // Should not contain a point outside
        assert!(!region.contains(53.0, 11.0));
        // Check bbox
        let b = region.bbox();
        assert!((b.min_lon - 10.0).abs() < 1e-9);
        assert!((b.max_lat - 52.0).abs() < 1e-9);
    }

    #[test]
    fn parse_geojson_feature() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "Feature",
            "properties": {},
            "geometry": {
                "type": "Polygon",
                "coordinates": [
                    [[0.0, 0.0], [5.0, 0.0], [5.0, 5.0], [0.0, 5.0], [0.0, 0.0]]
                ]
            }
        }"#;
        let path = write_temp_geojson(&dir, "feature.geojson", json);
        let region = parse_geojson(&path).unwrap();
        assert!(region.contains(2.5, 2.5));
        assert!(!region.contains(6.0, 2.5));
    }

    #[test]
    fn parse_geojson_feature_collection() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "properties": {},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0], [1.0, 1.0]]
                    ]
                }
            }]
        }"#;
        let path = write_temp_geojson(&dir, "fc.geojson", json);
        let region = parse_geojson(&path).unwrap();
        assert!(region.contains(2.0, 2.0));
        assert!(!region.contains(0.0, 0.0));
    }

    #[test]
    fn parse_geojson_multipolygon() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "MultiPolygon",
            "coordinates": [
                [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]]],
                [[[5.0, 5.0], [7.0, 5.0], [7.0, 7.0], [5.0, 7.0], [5.0, 5.0]]]
            ]
        }"#;
        let path = write_temp_geojson(&dir, "multi.geojson", json);
        let region = parse_geojson(&path).unwrap();
        // Inside first polygon: lat=1, lon=1
        assert!(region.contains(1.0, 1.0));
        // Inside second polygon: lat=6, lon=6
        assert!(region.contains(6.0, 6.0));
        // Between the two polygons: lat=3, lon=3
        assert!(!region.contains(3.0, 3.0));
        // Check bbox spans both
        let b = region.bbox();
        assert!((b.min_lon - 0.0).abs() < 1e-9);
        assert!((b.min_lat - 0.0).abs() < 1e-9);
        assert!((b.max_lon - 7.0).abs() < 1e-9);
        assert!((b.max_lat - 7.0).abs() < 1e-9);
    }

    #[test]
    fn parse_geojson_invalid_type() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "Point",
            "coordinates": [10.0, 50.0]
        }"#;
        let path = write_temp_geojson(&dir, "point.geojson", json);
        assert!(parse_geojson(&path).is_err());
    }
}
