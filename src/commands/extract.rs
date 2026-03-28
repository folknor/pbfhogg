//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::Path;

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, BlockType, Element, ElementReader, MemberId, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

use super::{
    drain_batch_results, flush_local, for_each_primitive_block_batch, require_indexdata,
    writer_from_header, ensure_node_capacity_local, ensure_way_capacity_local,
    ensure_relation_capacity_local, HeaderOverrides,
};
use super::id_set_dense::IdSetDense;

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

/// Precomputed integer bounding box in decimicrodegrees (10^-7) for fast containment testing.
///
/// Avoids the i64→f64 conversion and float comparison that `Bbox::contains` requires
/// on every node. The integer bbox is computed once from the f64 Bbox at startup.
struct BboxInt {
    min_lon: i32,
    min_lat: i32,
    max_lon: i32,
    max_lat: i32,
}

impl BboxInt {
    /// Convert a float Bbox to integer decimicrodegrees.
    #[allow(clippy::cast_possible_truncation)]
    fn from_bbox(bbox: &Bbox) -> Self {
        Self {
            min_lon: (bbox.min_lon * 1e7).floor() as i32,
            min_lat: (bbox.min_lat * 1e7).floor() as i32,
            max_lon: (bbox.max_lon * 1e7).ceil() as i32,
            max_lat: (bbox.max_lat * 1e7).ceil() as i32,
        }
    }

    /// Returns `true` if the point (lat, lon) in decimicrodegrees falls within this bbox.
    fn contains(&self, lat: i32, lon: i32) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

/// Build a [`BlobFilter`] that accepts all element types but spatially filters
/// node blobs: only node blobs whose coordinate bbox intersects the extraction
/// bbox are decompressed. Way and relation blobs always pass through.
///
/// Requires v2 indexdata with spatial bounds. Blobs without spatial indexdata
/// are conservatively passed through.
fn spatial_blob_filter(bbox_int: &BboxInt) -> BlobFilter {
    BlobFilter::new(true, true, true).with_node_bbox(crate::BlobBbox::new(
        bbox_int.min_lat,
        bbox_int.max_lat,
        bbox_int.min_lon,
        bbox_int.max_lon,
    ))
}

/// Parse a bbox string in osmium convention: `minlon,minlat,maxlon,maxlat`.
// String errors are intentional for CLI arg parsing — the bad input value is more
// useful to users than the underlying ParseFloatError ("invalid float literal").
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

    /// Fast containment test using decimicrodegree integer coordinates.
    ///
    /// For bbox regions, uses pure integer comparison (4 i32 compares) — avoids
    /// the i64→f64 conversion that `contains()` requires per node. For polygon
    /// regions, the bbox fast-rejection uses integers; only points passing the
    /// bbox test fall through to the f64 polygon ray-casting (with i32→f64
    /// conversion done only for those points).
    fn contains_decimicro(&self, bbox_int: &BboxInt, lat: i32, lon: i32) -> bool {
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

// Delegate to geo module — used by tests and polygon_bbox_f64
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

    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

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

    let geo_count = usize::from(has_bbox) + usize::from(has_polygon) + usize::from(has_polygon_file);
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
        require_indexdata(input, direct_io, force,
            "input PBF has no blob-level indexdata. Without indexdata, the spatial bbox \
             filter is a no-op — all blobs are decompressed (significantly slower).")?;
    }
    let result = match strategy {
        ExtractStrategy::Simple => extract_simple(input, output, region, set_bounds, clean, compression, direct_io, overrides),
        ExtractStrategy::CompleteWays => extract_complete_ways(input, output, region, set_bounds, clean, compression, direct_io, overrides),
        ExtractStrategy::Smart => extract_smart(input, output, region, set_bounds, clean, compression, direct_io, overrides),
    }?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extract_nodes_in_bbox", result.nodes_in_bbox as i64);
        crate::debug::emit_counter("extract_nodes_from_ways", result.nodes_from_ways as i64);
        crate::debug::emit_counter("extract_nodes_from_relations", result.nodes_from_relations as i64);
        crate::debug::emit_counter("extract_ways_written", result.ways_written as i64);
        crate::debug::emit_counter("extract_ways_from_relations", result.ways_from_relations as i64);
        crate::debug::emit_counter("extract_relations_written", result.relations_written as i64);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Parallel batch infrastructure
// ---------------------------------------------------------------------------


fn merge_extract_stats(target: &mut ExtractStats, source: &ExtractStats) {
    target.nodes_in_bbox += source.nodes_in_bbox;
    target.nodes_from_ways += source.nodes_from_ways;
    target.nodes_from_relations += source.nodes_from_relations;
    target.ways_written += source.ways_written;
    target.ways_from_relations += source.ways_from_relations;
    target.relations_written += source.relations_written;
}

// ---------------------------------------------------------------------------
// Simple strategy (single pass for sorted inputs, two-pass fallback for unsorted)
// ---------------------------------------------------------------------------

/// Classify elements in a single block for simple extract (populate ID sets).
///
/// Iterates elements without metadata (faster) and marks matching IDs:
/// - Nodes: bbox containment → set `bbox_node_ids`
/// - Ways: any ref in `bbox_node_ids` → set `matched_way_ids`
/// - Relations: matched node/way member → set `matched_relation_ids`
///
/// Returns `true` if any element in the block matched (the block should be
/// included in the write batch). Returns `false` if the block is empty for
/// this extract — callers can skip it to avoid parsing elements with full
/// metadata in the write path.
///
/// Uses `block_type()` (1 byte per group) to branch by type phase,
/// eliminating dead match arms in the hot inner loop for sorted PBFs.
#[hotpath::measure]
fn classify_block_simple(
    block: &PrimitiveBlock,
    region: &Region,
    bbox_int: &BboxInt,
    bbox_node_ids: &mut IdSetDense,
    matched_way_ids: &mut IdSetDense,
    matched_relation_ids: &mut IdSetDense,
) -> bool {
    let mut matched = false;
    match block.block_type() {
        BlockType::DenseNodes | BlockType::Nodes => {
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                        matched = true;
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                        matched = true;
                    }
                    _ => {}
                }
            }
        }
        BlockType::Ways => {
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element
                    && w.refs().any(|r| bbox_node_ids.get(r))
                {
                    matched_way_ids.set(w.id());
                    matched = true;
                }
            }
        }
        BlockType::Relations => {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element
                    && relation_has_matched_member(r, bbox_node_ids, matched_way_ids)
                {
                    matched_relation_ids.set(r.id());
                    matched = true;
                }
            }
        }
        BlockType::Empty => {
            // Empty blocks have no elements — skip.
        }
        BlockType::Mixed => {
            // Fallback for mixed blocks — check all element types.
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                        matched = true;
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                        matched = true;
                    }
                    Element::Way(w) if w.refs().any(|r| bbox_node_ids.get(r)) => {
                        matched_way_ids.set(w.id());
                        matched = true;
                    }
                    Element::Relation(r)
                        if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) =>
                    {
                        matched_relation_ids.set(r.id());
                        matched = true;
                    }
                    _ => {}
                }
            }
        }
    }
    matched
}

#[allow(clippy::too_many_arguments)]
fn extract_simple(input: &Path, output: &Path, region: &Region, set_bounds: bool, clean: &CleanAttrs, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<ExtractStats> {
    // Check if input is sorted — if so, classify + write in a single file pass.
    // We need a quick header check without keeping the reader open. Use BlobReader
    // to read just the first blob (header) instead of a full ElementReader.
    let is_sorted = {
        let mut br = crate::BlobReader::open(input, direct_io)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    super::warn_locations_on_ways_loss(&h);
                    h.is_sorted()
                }
                _ => false,
            },
            _ => false,
        }
    };

    if is_sorted {
        return extract_simple_single_pass(input, output, region, set_bounds, clean, compression, direct_io, overrides);
    }

    // --- Unsorted fallback: two passes (collect IDs, then write) ---
    crate::debug::emit_marker("EXTRACT_PASS1_START");
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(spatial_blob_filter(&bbox_int));
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        classify_block_simple(
            &block, region, &bbox_int,
            &mut bbox_node_ids, &mut matched_way_ids, &mut matched_relation_ids,
        );
    }
    crate::debug::emit_marker("EXTRACT_PASS1_END");

    crate::debug::emit_marker("EXTRACT_PASS2_START");
    let all_way_node_ids = IdSetDense::new();
    let reader = ElementReader::open(input, direct_io)?;
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, reader.header(), false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    })?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_extract_pass2_batch(batch, &ids, clean, &mut writer, &mut stats)
    })?;

    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_PASS2_END");
    Ok(stats)
}

/// Single-pass simple extract for sorted inputs.
///
/// For PBFs with `Sort.Type_then_ID`, classification and writing happen in one
/// file pass. Blocks arrive in type order (nodes → ways → relations), so by
/// the time ways appear, all bbox node IDs are known; by the time relations
/// appear, all matched way IDs are known.
///
/// Each batch of blocks is classified sequentially (populating ID sets), then
/// dispatched for parallel writing via `process_extract_pass2_batch`. The
/// pipeline (dedicated rayon pool) runs concurrently with the parallel write
/// (global rayon pool) without contention.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn extract_simple_single_pass(
    input: &Path,
    output: &Path,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<ExtractStats> {
    crate::debug::emit_marker("EXTRACT_SCAN_START");
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();
    let all_way_node_ids = IdSetDense::new(); // empty — simple doesn't include extra way nodes

    // Sequential reader with node-only scanner for bbox classification.
    // Node blobs: use extract_node_tuples (no string table) for fast bbox check.
    // If matches found, decompress again into PrimitiveBlock for the write batch.
    // Way/relation blobs: PrimitiveBlock directly.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    super::warn_locations_on_ways_loss(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    })?;

    let spatial_filter = spatial_blob_filter(&bbox_int);
    let decompress_pool = crate::blob::DecompressPool::new();
    let mut node_tuples: Vec<super::node_scanner::NodeTuple> = Vec::new();

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }

        // Spatial blob filter: skip blobs whose bbox doesn't intersect.
        if let Some(idx) = blob.index() {
            if matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                if !spatial_filter.want_nodes { continue; }
                if let Some(ref filter_bbox) = spatial_filter.node_bbox {
                    if let Some(ref blob_bbox) = idx.bbox {
                        if !filter_bbox.intersects(blob_bbox) { continue; }
                    }
                }
            }
        }

        // Node blobs: fast-path classification via node-only scanner.
        let is_node_blob = blob.index()
            .is_some_and(|idx| matches!(idx.kind, crate::blob_index::ElemKind::Node));
        if is_node_blob {
            // Decompress once for classification (no PrimitiveBlock, no string table).
            let decompressed = blob.decompress_pooled(&decompress_pool)?;
            node_tuples.clear();
            super::node_scanner::extract_node_tuples(&decompressed, &mut node_tuples)
                .map_err(|e| crate::error::new_error(
                    crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                ))?;
            let has_matches = node_tuples.iter().any(|t| {
                region.contains_decimicro(&bbox_int, t.lat, t.lon)
            });
            if has_matches {
                // Populate IdSetDense for way/relation classification.
                for t in &node_tuples {
                    if region.contains_decimicro(&bbox_int, t.lat, t.lon) {
                        bbox_node_ids.set(t.id);
                    }
                }
                // Need full PrimitiveBlock for writing — construct from same decompressed data.
                let block = PrimitiveBlock::new(decompressed)?;
                batch.push(block);
            }
        } else {
            // Way/relation blobs: full PrimitiveBlock for classification + writing.
            let decompressed = blob.decompress_pooled(&decompress_pool)?;
            let block = PrimitiveBlock::new(decompressed)?;
            let has_matches = classify_block_simple(
                &block, region, &bbox_int,
                &mut bbox_node_ids, &mut matched_way_ids, &mut matched_relation_ids,
            );
            if has_matches {
                batch.push(block);
            }
        }

        if batch.len() >= BATCH_SIZE {
            let ids = ExtractPass2IdSets {
                bbox_node_ids: &bbox_node_ids,
                all_way_node_ids: &all_way_node_ids,
                matched_way_ids: &matched_way_ids,
                matched_relation_ids: &matched_relation_ids,
            };
            process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &matched_relation_ids,
        };
        process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_SCAN_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Complete-ways strategy (two passes)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn extract_complete_ways(input: &Path, output: &Path, region: &Region, set_bounds: bool, clean: &CleanAttrs, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "complete_ways",
    };

    // --- Pass 1: Collect matches ---
    crate::debug::emit_marker("EXTRACT_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = CompleteRelationHandler;
    let result = collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler)?;
    crate::debug::emit_marker("EXTRACT_PASS1_END");

    // --- Pass 2: Write matching elements in file order ---
    crate::debug::emit_marker("EXTRACT_PASS2_START");
    let reader = ElementReader::open(input, direct_io)?;
    super::warn_locations_on_ways_loss(reader.header());
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, reader.header(), false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    })?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        matched_way_ids: &result.matched_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_extract_pass2_batch(batch, &ids, clean, &mut writer, &mut stats)
    })?;

    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_PASS2_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Pass 1 ID collection helpers
// ---------------------------------------------------------------------------

#[hotpath::measure]
fn merge_way_batch_parallel(
    batch: &[PrimitiveBlock],
    bbox_node_ids: &IdSetDense,
    matched_way_ids: &mut IdSetDense,
    all_way_node_ids: &mut IdSetDense,
) {
    // Per-worker aggregate Vecs via fold: one (Vec, Vec) per rayon worker
    // accumulates across all blocks in the batch. ~6 Vecs instead of ~64
    // (one per block), cutting allocation churn ~10x while keeping
    // O(matched_elements) work (no IdSetDense::merge).
    let partials: Vec<(Vec<i64>, Vec<i64>)> = batch
        .par_iter()
        .fold(
            || (Vec::new(), Vec::new()),
            |(mut local_way_ids, mut local_node_ids), block| {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(w) = &element
                        && w.refs().any(|r| bbox_node_ids.get(r))
                    {
                        local_way_ids.push(w.id());
                        local_node_ids.extend(w.refs());
                    }
                }
                (local_way_ids, local_node_ids)
            },
        )
        .collect();

    for (way_ids, node_ids) in partials {
        for id in way_ids {
            matched_way_ids.set(id);
        }
        for id in node_ids {
            all_way_node_ids.set(id);
        }
    }
}

#[hotpath::measure]
fn merge_relation_batch_parallel(
    batch: &[PrimitiveBlock],
    bbox_node_ids: &IdSetDense,
    matched_way_ids: &IdSetDense,
    matched_relation_ids: &mut IdSetDense,
) {
    let partials: Vec<Vec<i64>> = batch
        .par_iter()
        .fold(
            Vec::new,
            |mut local, block| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element
                        && relation_has_matched_member(r, bbox_node_ids, matched_way_ids)
                    {
                        local.push(r.id());
                    }
                }
                local
            },
        )
        .collect();

    for relation_ids in partials {
        for id in relation_ids {
            matched_relation_ids.set(id);
        }
    }
}

#[hotpath::measure]
fn merge_relation_batch_smart_parallel(
    batch: &[PrimitiveBlock],
    bbox_node_ids: &IdSetDense,
    matched_way_ids: &IdSetDense,
    matched_relation_ids: &mut IdSetDense,
    extra_way_ids: &mut IdSetDense,
    extra_node_ids: &mut IdSetDense,
) {
    let partials: Vec<(Vec<i64>, Vec<i64>, Vec<i64>)> = batch
        .par_iter()
        .fold(
            || (Vec::new(), Vec::new(), Vec::new()),
            |(mut local_rels, mut local_ways, mut local_nodes), block| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element
                        && relation_has_matched_member(r, bbox_node_ids, matched_way_ids)
                    {
                        local_rels.push(r.id());
                        if is_smart_relation(r) {
                            for m in r.members() {
                                match m.id {
                                    MemberId::Way(id) => local_ways.push(id),
                                    MemberId::Node(id) => local_nodes.push(id),
                                    MemberId::Relation(_) | MemberId::Unknown(_, _) => {}
                                }
                            }
                        }
                    }
                }
                (local_rels, local_ways, local_nodes)
            },
        )
        .collect();

    for (relation_ids, way_ids, node_ids) in partials {
        for id in relation_ids {
            matched_relation_ids.set(id);
        }
        for id in way_ids {
            extra_way_ids.set(id);
        }
        for id in node_ids {
            extra_node_ids.set(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 1: Generic ID collection with pluggable relation handling
// ---------------------------------------------------------------------------

/// Output of pass 1 ID collection, shared between complete-ways and smart strategies.
struct Pass1Result {
    bbox_node_ids: IdSetDense,
    matched_way_ids: IdSetDense,
    all_way_node_ids: IdSetDense,
    matched_relation_ids: IdSetDense,
}

/// Strategy-specific relation handling for pass 1.
///
/// Implementations control what happens after a relation is matched:
/// - `CompleteRelationHandler`: no-op (just collects relation IDs)
/// - `SmartRelationHandler`: additionally collects way/node member IDs from
///   multipolygon/boundary relations
trait RelationHandler {
    /// Process a single matched relation in the unsorted/mixed fallback path.
    /// Called after the relation ID has already been added to `matched_relation_ids`.
    fn handle_relation(&mut self, r: &crate::Relation);

    /// Merge a batch of relation blocks in parallel (sorted path).
    fn merge_relation_batch(
        &mut self,
        batch: &[PrimitiveBlock],
        bbox_node_ids: &IdSetDense,
        matched_way_ids: &IdSetDense,
        matched_relation_ids: &mut IdSetDense,
    );
}

struct CompleteRelationHandler;

impl RelationHandler for CompleteRelationHandler {
    fn handle_relation(&mut self, _r: &crate::Relation) {}

    fn merge_relation_batch(
        &mut self,
        batch: &[PrimitiveBlock],
        bbox_node_ids: &IdSetDense,
        matched_way_ids: &IdSetDense,
        matched_relation_ids: &mut IdSetDense,
    ) {
        merge_relation_batch_parallel(batch, bbox_node_ids, matched_way_ids, matched_relation_ids);
    }
}

struct SmartRelationHandler {
    extra_way_ids: IdSetDense,
    extra_node_ids: IdSetDense,
}

impl SmartRelationHandler {
    fn new() -> Self {
        Self {
            extra_way_ids: IdSetDense::new(),
            extra_node_ids: IdSetDense::new(),
        }
    }
}

impl RelationHandler for SmartRelationHandler {
    fn handle_relation(&mut self, r: &crate::Relation) {
        if is_smart_relation(r) {
            for m in r.members() {
                match m.id {
                    MemberId::Way(id) => self.extra_way_ids.set(id),
                    MemberId::Node(id) => self.extra_node_ids.set(id),
                    MemberId::Relation(_) | MemberId::Unknown(_, _) => {}
                }
            }
        }
    }

    fn merge_relation_batch(
        &mut self,
        batch: &[PrimitiveBlock],
        bbox_node_ids: &IdSetDense,
        matched_way_ids: &IdSetDense,
        matched_relation_ids: &mut IdSetDense,
    ) {
        merge_relation_batch_smart_parallel(
            batch,
            bbox_node_ids,
            matched_way_ids,
            matched_relation_ids,
            &mut self.extra_way_ids,
            &mut self.extra_node_ids,
        );
    }
}

/// Collect pass 1 ID sets with strategy-specific relation handling.
///
/// Reads all elements via pipelined decode, collecting:
/// - `bbox_node_ids`: nodes within the bounding box
/// - `matched_way_ids`: ways referencing at least one bbox node
/// - `all_way_node_ids`: all node refs from matched ways (for pass 2)
/// - `matched_relation_ids`: relations with matched node/way members
///
/// The `handler` controls additional per-relation processing (e.g. smart
/// strategy collects extra way/node IDs from multipolygon/boundary relations).
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
#[hotpath::measure]
fn collect_pass1_generic<H: RelationHandler>(
    input: &Path,
    region: &Region,
    bbox_int: &BboxInt,
    direct_io: bool,
    handler: &mut H,
) -> Result<Pass1Result> {
    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut all_way_node_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();

    let reader = ElementReader::open(input, direct_io)?;
    let is_sorted = reader.header().is_sorted();
    let filter = spatial_blob_filter(bbox_int);
    if !is_sorted {
        for block_result in reader.with_blob_filter(filter).into_blocks_pipelined() {
            let block = block_result?;
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                    }
                    Element::Way(w)
                        if w.refs().any(|r| bbox_node_ids.get(r)) =>
                    {
                        matched_way_ids.set(w.id());
                        for r in w.refs() {
                            all_way_node_ids.set(r);
                        }
                    }
                    Element::Relation(r)
                        if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) =>
                    {
                        matched_relation_ids.set(r.id());
                        handler.handle_relation(r);
                    }
                    _ => {}
                }
            }
        }
        return Ok(Pass1Result {
            bbox_node_ids, matched_way_ids, all_way_node_ids, matched_relation_ids,
        });
    }

    let mut way_batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    let mut relation_batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for block_result in reader
        .with_blob_filter(filter)
        .decode_threads(1)
        .into_blocks_pipelined()
    {
        let block = block_result?;
        match block.block_type() {
            BlockType::DenseNodes | BlockType::Nodes => {
                if !way_batch.is_empty() {
                    merge_way_batch_parallel(
                        &way_batch,
                        &bbox_node_ids,
                        &mut matched_way_ids,
                        &mut all_way_node_ids,
                    );
                    way_batch.clear();
                }
                if !relation_batch.is_empty() {
                    handler.merge_relation_batch(
                        &relation_batch,
                        &bbox_node_ids,
                        &matched_way_ids,
                        &mut matched_relation_ids,
                    );
                    relation_batch.clear();
                }
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn)
                            if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                        {
                            bbox_node_ids.set(dn.id());
                        }
                        Element::Node(n)
                            if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                        {
                            bbox_node_ids.set(n.id());
                        }
                        _ => {}
                    }
                }
            }
            BlockType::Ways => {
                if !relation_batch.is_empty() {
                    handler.merge_relation_batch(
                        &relation_batch,
                        &bbox_node_ids,
                        &matched_way_ids,
                        &mut matched_relation_ids,
                    );
                    relation_batch.clear();
                }
                way_batch.push(block);
                if way_batch.len() >= BATCH_SIZE {
                    merge_way_batch_parallel(
                        &way_batch,
                        &bbox_node_ids,
                        &mut matched_way_ids,
                        &mut all_way_node_ids,
                    );
                    way_batch.clear();
                }
            }
            BlockType::Relations => {
                if !way_batch.is_empty() {
                    merge_way_batch_parallel(
                        &way_batch,
                        &bbox_node_ids,
                        &mut matched_way_ids,
                        &mut all_way_node_ids,
                    );
                    way_batch.clear();
                }
                relation_batch.push(block);
                if relation_batch.len() >= BATCH_SIZE {
                    handler.merge_relation_batch(
                        &relation_batch,
                        &bbox_node_ids,
                        &matched_way_ids,
                        &mut matched_relation_ids,
                    );
                    relation_batch.clear();
                }
            }
            BlockType::Empty => {
                // Empty blocks have no elements — skip without flushing batches.
            }
            BlockType::Mixed => {
                if !way_batch.is_empty() {
                    merge_way_batch_parallel(
                        &way_batch,
                        &bbox_node_ids,
                        &mut matched_way_ids,
                        &mut all_way_node_ids,
                    );
                    way_batch.clear();
                }
                if !relation_batch.is_empty() {
                    handler.merge_relation_batch(
                        &relation_batch,
                        &bbox_node_ids,
                        &matched_way_ids,
                        &mut matched_relation_ids,
                    );
                    relation_batch.clear();
                }
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn)
                            if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                        {
                            bbox_node_ids.set(dn.id());
                        }
                        Element::Node(n)
                            if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                        {
                            bbox_node_ids.set(n.id());
                        }
                        Element::Way(w)
                            if w.refs().any(|r| bbox_node_ids.get(r)) =>
                        {
                            matched_way_ids.set(w.id());
                            for r in w.refs() {
                                all_way_node_ids.set(r);
                            }
                        }
                        Element::Relation(r)
                            if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) =>
                        {
                            matched_relation_ids.set(r.id());
                            handler.handle_relation(r);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if !way_batch.is_empty() {
        merge_way_batch_parallel(
            &way_batch,
            &bbox_node_ids,
            &mut matched_way_ids,
            &mut all_way_node_ids,
        );
    }
    if !relation_batch.is_empty() {
        handler.merge_relation_batch(
            &relation_batch,
            &bbox_node_ids,
            &matched_way_ids,
            &mut matched_relation_ids,
        );
    }

    Ok(Pass1Result {
        bbox_node_ids, matched_way_ids, all_way_node_ids, matched_relation_ids,
    })
}

// ---------------------------------------------------------------------------
// Complete-ways Pass 2: Parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 2 of complete-ways strategy, shared across rayon threads.
struct ExtractPass2IdSets<'a> {
    bbox_node_ids: &'a IdSetDense,
    all_way_node_ids: &'a IdSetDense,
    matched_way_ids: &'a IdSetDense,
    matched_relation_ids: &'a IdSetDense,
}

/// Process a single block for Pass 2 of complete-ways: write elements whose IDs
/// were collected in Pass 1. Uses thread-local BlockBuilder and output buffer.
#[hotpath::measure]
fn extract_block_pass2(
    block: &PrimitiveBlock,
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<ExtractStats, String> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "",
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let in_bbox = ids.bbox_node_ids.get(dn.id());
                let from_way = ids.all_way_node_ids.get(dn.id());
                if in_bbox || from_way {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Node(n) => {
                let in_bbox = ids.bbox_node_ids.get(n.id());
                let from_way = ids.all_way_node_ids.get(n.id());
                if in_bbox || from_way {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Way(w) => {
                if ids.matched_way_ids.get(w.id()) {
                    ensure_way_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = clean_metadata(element_metadata(&w.info()), clean);
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.get(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = clean_metadata(element_metadata(&r.info()), clean);
                    bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Process a batch of blocks in parallel for Pass 2 of complete-ways extraction.
fn process_extract_pass2_batch(
    batch: &[PrimitiveBlock],
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, ExtractStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = extract_block_pass2(block, ids, clean, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Smart strategy (three passes)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn extract_smart(
    input: &Path,
    output: &Path,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "smart",
    };

    // --- Pass 1: Collect matches + smart relation deps ---
    crate::debug::emit_marker("EXTRACT_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = SmartRelationHandler::new();
    let result = collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler)?;
    let mut extra_node_ids = handler.extra_node_ids;
    crate::debug::emit_marker("EXTRACT_PASS1_END");

    // --- Pass 2: Resolve extra way node deps ---
    crate::debug::emit_marker("EXTRACT_PASS2_START");
    // For each way in extra_way_ids not already in matched_way_ids,
    // collect all node refs into extra_node_ids.
    // BlobFilter skips node and relation blobs (only ways are needed here).
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.with_blob_filter(BlobFilter::new(false, true, false)).into_blocks_pipelined() {
        let block = block?;
        for group in block.groups() {
            for w in group.ways() {
                let wid = w.id();
                if handler.extra_way_ids.get(wid) && !result.matched_way_ids.get(wid) {
                    for r in w.refs() {
                        extra_node_ids.set(r);
                    }
                }
            }
        }
    }

    crate::debug::emit_marker("EXTRACT_PASS2_END");

    // --- Pass 3: Write matching elements in file order ---
    crate::debug::emit_marker("EXTRACT_PASS3_START");
    let reader = ElementReader::open(input, direct_io)?;
    super::warn_locations_on_ways_loss(reader.header());
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, reader.header(), false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    })?;

    let ids = ExtractPass3IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        extra_node_ids: &extra_node_ids,
        matched_way_ids: &result.matched_way_ids,
        extra_way_ids: &handler.extra_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_extract_pass3_batch(batch, &ids, clean, &mut writer, &mut stats)
    })?;

    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_PASS3_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Smart Pass 3: Parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 3 of smart strategy, shared across rayon threads.
struct ExtractPass3IdSets<'a> {
    bbox_node_ids: &'a IdSetDense,
    all_way_node_ids: &'a IdSetDense,
    extra_node_ids: &'a IdSetDense,
    matched_way_ids: &'a IdSetDense,
    extra_way_ids: &'a IdSetDense,
    matched_relation_ids: &'a IdSetDense,
}

/// Process a single block for Pass 3 of smart extraction: write elements whose IDs
/// were collected in Passes 1+2. Uses thread-local BlockBuilder and output buffer.
#[hotpath::measure]
fn extract_block_pass3(
    block: &PrimitiveBlock,
    ids: &ExtractPass3IdSets<'_>,
    clean: &CleanAttrs,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<ExtractStats, String> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "",
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.all_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Node(n) => {
                let id = n.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.all_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Way(w) => {
                let in_matched = ids.matched_way_ids.get(w.id());
                let in_extra = ids.extra_way_ids.get(w.id());
                if in_matched || in_extra {
                    ensure_way_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = clean_metadata(element_metadata(&w.info()), clean);
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    if in_extra && !in_matched {
                        stats.ways_from_relations += 1;
                    } else {
                        stats.ways_written += 1;
                    }
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.get(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = clean_metadata(element_metadata(&r.info()), clean);
                    bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Process a batch of blocks in parallel for Pass 3 of smart extraction.
fn process_extract_pass3_batch(
    batch: &[PrimitiveBlock],
    ids: &ExtractPass3IdSets<'_>,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, ExtractStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = extract_block_pass3(block, ids, clean, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation member matching
// ---------------------------------------------------------------------------

/// Check if a relation has any member whose ID is in the matched node or way sets.
fn relation_has_matched_member(
    r: &crate::Relation,
    node_ids: &IdSetDense,
    way_ids: &IdSetDense,
) -> bool {
    r.members().any(|m| match m.id {
        MemberId::Node(id) => node_ids.get(id),
        MemberId::Way(id) => way_ids.get(id),
        MemberId::Relation(_) | MemberId::Unknown(_, _) => false,
    })
}

/// Returns true if the relation has a `type=multipolygon` or `type=boundary` tag.
///
/// These are the relation types whose way members should be fully included
/// in the smart extraction strategy, along with all nodes those ways reference.
fn is_smart_relation(r: &crate::Relation) -> bool {
    r.tags().any(|(k, v)| k == "type" && (v == "multipolygon" || v == "boundary"))
}

// Helpers
// ---------------------------------------------------------------------------

use super::{clean_metadata, dense_node_metadata, element_metadata};


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
