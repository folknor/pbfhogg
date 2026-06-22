//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::Path;

use crate::cat::CleanAttrs;
use crate::writer::Compression;

use super::{HeaderOverrides, Result, require_indexdata};

mod common;
mod complete;
mod multi;
mod simple;
mod smart;

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
// String errors are intentional for CLI arg parsing - the bad input value is more
// useful to users than the underlying ParseFloatError ("invalid float literal").
pub fn parse_bbox(s: &str) -> Result<Bbox> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(format!(
            "bbox must have 4 comma-separated values, got {}",
            parts.len()
        )
        .into());
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

    /// Fast containment test using decimicrodegree integer coordinates.
    ///
    /// For bbox regions, uses pure integer comparison (4 i32 compares) - avoids
    /// the i64→f64 conversion that `contains()` requires per node. For polygon
    /// regions, the bbox fast-rejection uses integers; only points passing the
    /// bbox test fall through to the f64 polygon ray-casting (with i32→f64
    /// conversion done only for those points).
    fn contains_decimicro(&self, bbox_int: &common::BboxInt, lat: i32, lon: i32) -> bool {
        match self {
            Region::Bbox(_) => bbox_int.contains(lat, lon),
            Region::Polygon { polygons, .. } => {
                if !bbox_int.contains(lat, lon) {
                    return false;
                }
                let lat_f64 = lat as f64 * 1e-7;
                let lon_f64 = lon as f64 * 1e-7;
                polygon_contains(polygons, lon_f64, lat_f64)
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
/// Calls geo primitives directly to avoid per-point allocation.
fn polygon_rings_contains(poly: &PolygonRings, px: f64, py: f64) -> bool {
    if !crate::geo::point_in_ring_with_antimeridian(px, py, &poly.exterior) {
        return false;
    }
    !poly
        .holes
        .iter()
        .any(|hole| crate::geo::point_in_ring_with_antimeridian(px, py, hole))
}

// Delegate to geo module - used by tests and polygon_bbox_f64
use crate::geo::ring_crosses_antimeridian;

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
            let first = features
                .first()
                .ok_or("FeatureCollection has no features")?;
            let geom = first
                .get("geometry")
                .ok_or("first Feature missing 'geometry' field")?;
            Ok(geom.clone())
        }
        other => Err(format!("unsupported GeoJSON type: {other}").into()),
    }
}

/// Dispatch to the right parser based on geometry type.
fn parse_geometry_by_type(geo_type: &str, coords: &serde_json::Value) -> Result<Vec<PolygonRings>> {
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
        let lon = pair[0].as_f64().ok_or("coordinate lon must be a number")?;
        let lat = pair[1].as_f64().ok_or("coordinate lat must be a number")?;
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
    let mut crosses_antimeridian = false;

    for poly in polygons {
        if ring_crosses_antimeridian(&poly.exterior) {
            crosses_antimeridian = true;
        }
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

    if crosses_antimeridian {
        min_lon = -180.0;
        max_lon = 180.0;
    }

    Ok(Bbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

// ---------------------------------------------------------------------------
// Config file parsing (multi-extract)
// ---------------------------------------------------------------------------

/// A single extract slot parsed from a config file.
pub struct ExtractSlot {
    pub region: Region,
    pub output: std::path::PathBuf,
}

/// Parse a multi-extract JSON config file.
///
/// Returns `(directory, extracts)` where `directory` is the optional output
/// directory from the config and `extracts` is the list of extract slots.
///
/// Config format:
/// ```json
/// {
///   "directory": "/output",
///   "extracts": [
///     { "output": "denmark.osm.pbf", "bbox": [8.09, 54.80, 12.69, 57.73] },
///     { "output": "berlin.osm.pbf", "polygon": { "type": "Polygon", "coordinates": [...] } },
///     { "output": "hamburg.osm.pbf", "polygon_file": "hamburg.geojson" }
///   ]
/// }
/// ```
pub fn parse_extract_config(
    config_path: &Path,
) -> Result<(Option<std::path::PathBuf>, Vec<ExtractSlot>)> {
    let data = std::fs::read_to_string(config_path)?;
    let value: serde_json::Value = serde_json::from_str(&data)?;

    let directory = value
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from);

    let extracts_arr = value
        .get("extracts")
        .and_then(serde_json::Value::as_array)
        .ok_or("config must have an 'extracts' array")?;

    if extracts_arr.is_empty() {
        return Err("'extracts' array must not be empty".into());
    }
    if extracts_arr.len() > 500 {
        return Err(format!("too many extracts: {} (max 500)", extracts_arr.len()).into());
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let resolve_dir = directory.as_deref().unwrap_or(config_dir);

    let mut slots = Vec::with_capacity(extracts_arr.len());
    let mut output_paths: Vec<std::path::PathBuf> = Vec::with_capacity(extracts_arr.len());

    for (i, entry) in extracts_arr.iter().enumerate() {
        let output_name = entry
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("extract[{i}] missing 'output' field"))?;

        let output_path = resolve_dir.join(output_name);
        if output_paths.contains(&output_path) {
            return Err(format!("duplicate output path: {}", output_path.display()).into());
        }
        output_paths.push(output_path.clone());

        let region = parse_extract_geometry(entry, i, config_dir)?;
        slots.push(ExtractSlot {
            region,
            output: output_path,
        });
    }

    Ok((directory, slots))
}

/// Parse the geometry for a single extract entry in a config file.
fn parse_extract_geometry(
    entry: &serde_json::Value,
    index: usize,
    config_dir: &Path,
) -> Result<Region> {
    let has_bbox = entry.get("bbox").is_some();
    let has_polygon = entry.get("polygon").is_some();
    let has_polygon_file = entry.get("polygon_file").is_some();

    let geo_count =
        usize::from(has_bbox) + usize::from(has_polygon) + usize::from(has_polygon_file);
    if geo_count == 0 {
        return Err(format!(
            "extract[{index}] must have exactly one of 'bbox', 'polygon', or 'polygon_file'"
        )
        .into());
    }
    if geo_count > 1 {
        return Err(format!(
            "extract[{index}] has multiple geometry fields; use exactly one of 'bbox', 'polygon', or 'polygon_file'"
        )
        .into());
    }

    if has_bbox {
        let arr = entry
            .get("bbox")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("extract[{index}] 'bbox' must be an array"))?;
        if arr.len() != 4 {
            return Err(format!(
                "extract[{index}] 'bbox' must have 4 elements, got {}",
                arr.len()
            )
            .into());
        }
        let min_lon = arr[0]
            .as_f64()
            .ok_or_else(|| format!("extract[{index}] bbox[0] must be a number"))?;
        let min_lat = arr[1]
            .as_f64()
            .ok_or_else(|| format!("extract[{index}] bbox[1] must be a number"))?;
        let max_lon = arr[2]
            .as_f64()
            .ok_or_else(|| format!("extract[{index}] bbox[2] must be a number"))?;
        let max_lat = arr[3]
            .as_f64()
            .ok_or_else(|| format!("extract[{index}] bbox[3] must be a number"))?;
        if min_lon >= max_lon {
            return Err(format!(
                "extract[{index}] bbox min_lon ({min_lon}) must be less than max_lon ({max_lon})"
            )
            .into());
        }
        if min_lat >= max_lat {
            return Err(format!(
                "extract[{index}] bbox min_lat ({min_lat}) must be less than max_lat ({max_lat})"
            )
            .into());
        }
        return Ok(Region::Bbox(Bbox {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }));
    }

    if has_polygon {
        let geom = entry
            .get("polygon")
            .ok_or_else(|| format!("extract[{index}] missing 'polygon'"))?;
        let geometry = extract_geometry(geom)?;
        let geo_type = geometry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("extract[{index}] polygon missing 'type' field"))?;
        let coords = geometry
            .get("coordinates")
            .ok_or_else(|| format!("extract[{index}] polygon missing 'coordinates' field"))?;
        let polygons = parse_geometry_by_type(geo_type, coords)?;
        let bbox = bbox_from_polygons(&polygons)?;
        return Ok(Region::Polygon { polygons, bbox });
    }

    // has_polygon_file
    let file_str = entry
        .get("polygon_file")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("extract[{index}] 'polygon_file' must be a string"))?;
    let polygon_path = config_dir.join(file_str);
    parse_geojson(&polygon_path)
}

/// Run multiple extracts from a parsed config, calling [`extract`] for each slot.
#[allow(clippy::too_many_arguments)]
pub fn extract_multi(
    input: &Path,
    slots: &[ExtractSlot],
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<ExtractStats>> {
    // Try single-pass multi-extract for simple strategy on sorted input.
    if matches!(strategy, ExtractStrategy::Simple) && !clean.any() {
        if let Some(stats) = multi::try_extract_multi_single_pass(
            input,
            slots,
            set_bounds,
            compression,
            direct_io,
            overrides,
        )? {
            return Ok(stats);
        }
    }

    // Sequential fallback: one extract at a time.
    let mut all_stats = Vec::with_capacity(slots.len());
    for (i, slot) in slots.iter().enumerate() {
        eprintln!(
            "[{}/{}] Extracting to {}",
            i + 1,
            slots.len(),
            slot.output.display()
        );
        let stats = extract(
            input,
            &slot.output,
            &slot.region,
            strategy,
            set_bounds,
            clean,
            compression,
            direct_io,
            force,
            overrides,
        )?;
        all_stats.push(stats);
    }
    Ok(all_stats)
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Extraction strategy determining how referential completeness is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractStrategy {
    /// Single pass. Fast but ways may reference nodes outside the extract.
    Simple,
    /// Two passes. All nodes of matching ways are included.
    CompleteWays,
    /// Three passes. Like CompleteWays, but additionally pulls in all way
    /// members (and their nodes) of matched multipolygon/boundary relations,
    /// even if those ways are outside the extract region.
    Smart,
}

pub struct ExtractStats {
    pub nodes_in_bbox: u64,
    pub nodes_from_ways: u64,
    pub nodes_from_relations: u64,
    pub ways_written: u64,
    pub ways_from_relations: u64,
    pub relations_written: u64,
    pub strategy: &'static str,
}

impl ExtractStats {
    pub fn print_summary(&self) {
        let total_nodes = self.nodes_in_bbox + self.nodes_from_ways + self.nodes_from_relations;
        let total_ways = self.ways_written + self.ways_from_relations;
        if self.nodes_from_relations > 0 || self.ways_from_relations > 0 {
            eprintln!(
                "Extract ({}): {} nodes ({} in bbox, {} from ways, {} from relations), \
                 {} ways ({} from relations), {} relations",
                self.strategy,
                total_nodes,
                self.nodes_in_bbox,
                self.nodes_from_ways,
                self.nodes_from_relations,
                total_ways,
                self.ways_from_relations,
                self.relations_written,
            );
        } else {
            eprintln!(
                "Extract ({}): {} nodes ({} in bbox, {} from ways), {} ways, {} relations",
                self.strategy,
                total_nodes,
                self.nodes_in_bbox,
                self.nodes_from_ways,
                total_ways,
                self.relations_written,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Extract elements within `region` from `input` and write to `output`.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub fn extract(
    input: &Path,
    output: &Path,
    region: &Region,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<ExtractStats> {
    if !matches!(strategy, ExtractStrategy::Simple) {
        require_indexdata(
            input,
            direct_io,
            force,
            "input PBF has no blob-level indexdata. Without indexdata, the spatial bbox \
             filter is a no-op - all blobs are decompressed (significantly slower).",
        )?;
    }
    let result = match strategy {
        ExtractStrategy::Simple => simple::extract_simple(
            input,
            output,
            region,
            set_bounds,
            clean,
            compression,
            direct_io,
            overrides,
        ),
        ExtractStrategy::CompleteWays => complete::extract_complete_ways(
            input,
            output,
            region,
            set_bounds,
            clean,
            compression,
            direct_io,
            overrides,
        ),
        ExtractStrategy::Smart => smart::extract_smart(
            input,
            output,
            region,
            set_bounds,
            clean,
            compression,
            direct_io,
            overrides,
        ),
    }?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extract_nodes_in_bbox", result.nodes_in_bbox as i64);
        crate::debug::emit_counter("extract_nodes_from_ways", result.nodes_from_ways as i64);
        crate::debug::emit_counter(
            "extract_nodes_from_relations",
            result.nodes_from_relations as i64,
        );
        crate::debug::emit_counter("extract_ways_written", result.ways_written as i64);
        crate::debug::emit_counter(
            "extract_ways_from_relations",
            result.ways_from_relations as i64,
        );
        crate::debug::emit_counter("extract_relations_written", result.relations_written as i64);
    }
    Ok(result)
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
    use crate::geo::{point_in_ring, point_in_ring_with_antimeridian};
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

    #[test]
    fn point_in_ring_antimeridian() {
        // Rectangle crossing the dateline.
        let ring = vec![
            (179.0, 10.0),
            (-179.0, 10.0),
            (-179.0, 12.0),
            (179.0, 12.0),
            (179.0, 10.0),
        ];
        assert!(point_in_ring_with_antimeridian(179.5, 11.0, &ring));
        assert!(point_in_ring_with_antimeridian(-179.5, 11.0, &ring));
        assert!(!point_in_ring_with_antimeridian(0.0, 11.0, &ring));
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

    #[test]
    fn polygon_region_antimeridian_contains() {
        let region = Region::Polygon {
            polygons: vec![PolygonRings {
                exterior: vec![
                    (179.0, 10.0),
                    (-179.0, 10.0),
                    (-179.0, 12.0),
                    (179.0, 12.0),
                    (179.0, 10.0),
                ],
                holes: vec![],
            }],
            bbox: Bbox {
                min_lon: -180.0,
                min_lat: 10.0,
                max_lon: 180.0,
                max_lat: 12.0,
            },
        };
        assert!(region.contains(11.0, 179.5));
        assert!(region.contains(11.0, -179.5));
        assert!(!region.contains(11.0, 0.0));
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
    fn parse_geojson_antimeridian_polygon() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "type": "Polygon",
            "coordinates": [
                [[179.0, 10.0], [-179.0, 10.0], [-179.0, 12.0], [179.0, 12.0], [179.0, 10.0]]
            ]
        }"#;
        let path = write_temp_geojson(&dir, "antimeridian.geojson", json);
        let region = parse_geojson(&path).unwrap();
        assert!(region.contains(11.0, 179.5));
        assert!(region.contains(11.0, -179.5));
        assert!(!region.contains(11.0, 0.0));
        let b = region.bbox();
        assert!((b.min_lon + 180.0).abs() < 1e-9);
        assert!((b.max_lon - 180.0).abs() < 1e-9);
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

    // -----------------------------------------------------------------------
    // Config file parsing tests
    // -----------------------------------------------------------------------

    fn write_temp_json(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn config_bbox_extracts() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [
                { "output": "a.osm.pbf", "bbox": [8.0, 54.0, 13.0, 58.0] },
                { "output": "b.osm.pbf", "bbox": [0.0, 0.0, 5.0, 5.0] }
            ]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        let (directory, slots) = parse_extract_config(&path).unwrap();
        assert!(directory.is_none());
        assert_eq!(slots.len(), 2);
        assert!(slots[0].output.ends_with("a.osm.pbf"));
        assert!(slots[1].output.ends_with("b.osm.pbf"));
        // First extract should contain Copenhagen area
        assert!(slots[0].region.contains(55.6, 12.5));
        assert!(!slots[0].region.contains(1.0, 1.0));
        // Second extract should contain (1,1)
        assert!(slots[1].region.contains(1.0, 1.0));
        assert!(!slots[1].region.contains(55.6, 12.5));
    }

    #[test]
    fn config_with_directory() {
        let dir = TempDir::new().unwrap();
        let outdir = dir.path().join("out");
        std::fs::create_dir(&outdir).unwrap();
        let json = format!(
            r#"{{
                "directory": "{}",
                "extracts": [
                    {{ "output": "test.osm.pbf", "bbox": [0.0, 0.0, 1.0, 1.0] }}
                ]
            }}"#,
            outdir.display()
        );
        let path = write_temp_json(&dir, "config.json", &json);
        let (directory, slots) = parse_extract_config(&path).unwrap();
        assert!(directory.is_some());
        assert_eq!(slots[0].output, outdir.join("test.osm.pbf"));
    }

    #[test]
    fn config_inline_polygon() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [{
                "output": "poly.osm.pbf",
                "polygon": {
                    "type": "Polygon",
                    "coordinates": [
                        [[10.0, 50.0], [12.0, 50.0], [12.0, 52.0], [10.0, 52.0], [10.0, 50.0]]
                    ]
                }
            }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        let (_, slots) = parse_extract_config(&path).unwrap();
        assert_eq!(slots.len(), 1);
        assert!(slots[0].region.contains(51.0, 11.0));
        assert!(!slots[0].region.contains(53.0, 11.0));
    }

    #[test]
    fn config_polygon_file() {
        let dir = TempDir::new().unwrap();
        let geojson = r#"{
            "type": "Polygon",
            "coordinates": [
                [[10.0, 50.0], [12.0, 50.0], [12.0, 52.0], [10.0, 52.0], [10.0, 50.0]]
            ]
        }"#;
        write_temp_geojson(&dir, "area.geojson", geojson);
        let json = r#"{
            "extracts": [{
                "output": "from_file.osm.pbf",
                "polygon_file": "area.geojson"
            }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        let (_, slots) = parse_extract_config(&path).unwrap();
        assert_eq!(slots.len(), 1);
        assert!(slots[0].region.contains(51.0, 11.0));
    }

    #[test]
    fn config_no_geometry_fails() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [{ "output": "bad.osm.pbf" }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_duplicate_output_fails() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [
                { "output": "same.osm.pbf", "bbox": [0.0, 0.0, 1.0, 1.0] },
                { "output": "same.osm.pbf", "bbox": [2.0, 2.0, 3.0, 3.0] }
            ]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_empty_extracts_fails() {
        let dir = TempDir::new().unwrap();
        let json = r#"{ "extracts": [] }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_missing_output_fails() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [{ "bbox": [0.0, 0.0, 1.0, 1.0] }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_multiple_geometry_fails() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "extracts": [{
                "output": "bad.osm.pbf",
                "bbox": [0.0, 0.0, 1.0, 1.0],
                "polygon": { "type": "Polygon", "coordinates": [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]] }
            }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }
}
