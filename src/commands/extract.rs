//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::Path;

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, BlockType, Element, MemberId, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

use super::{
    drain_batch_results, flush_local, require_indexdata,
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
    // Try single-pass multi-extract for simple strategy on sorted input.
    if matches!(strategy, ExtractStrategy::Simple) && !clean.any() {
        if let Some(stats) = try_extract_multi_single_pass(
            input, slots, set_bounds, compression, direct_io, overrides,
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
// Single-pass multi-extract (simple strategy, sorted input)
// ---------------------------------------------------------------------------

/// Try single-pass multi-extract: read the PBF once, classify each element
/// against all N regions, write to N output files. Returns `None` to fall
/// back to sequential if the input isn't sorted.
///
/// Simple strategy only (no per-region way/relation ID tracking beyond what
/// fits in memory). Uses sync-mode PbfWriters (no per-writer thread).
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn try_extract_multi_single_pass(
    input: &Path,
    slots: &[ExtractSlot],
    set_bounds: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Option<Vec<ExtractStats>>> {
    use std::io::BufWriter;

    // Check if input is sorted.
    let header = {
        let mut br = crate::BlobReader::open(input, direct_io)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    super::warn_locations_on_ways_loss(&h);
                    if !h.is_sorted() {
                        return Ok(None); // fall back to sequential
                    }
                    h
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }
    };

    let n = slots.len();
    eprintln!("[multi-extract] single-pass: {n} regions, simple strategy");

    // Precompute per-region integer bboxes.
    let bbox_ints: Vec<BboxInt> = slots.iter()
        .map(|s| BboxInt::from_bbox(s.region.bbox()))
        .collect();

    // Union bbox for blob-level spatial skip.
    let union_bbox = BboxInt {
        min_lon: bbox_ints.iter().map(|b| b.min_lon).min().unwrap_or(i32::MIN),
        min_lat: bbox_ints.iter().map(|b| b.min_lat).min().unwrap_or(i32::MIN),
        max_lon: bbox_ints.iter().map(|b| b.max_lon).max().unwrap_or(i32::MAX),
        max_lat: bbox_ints.iter().map(|b| b.max_lat).max().unwrap_or(i32::MAX),
    };
    let spatial_filter = spatial_blob_filter(&union_bbox);

    // Open N sync-mode writers.
    let mut writers: Vec<PbfWriter<BufWriter<std::fs::File>>> = Vec::with_capacity(n);
    for slot in slots {
        let bbox = slot.region.bbox();
        let header_bytes = super::build_output_header(&header, true, overrides, |hb| {
            let hb = if set_bounds {
                hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
            } else {
                hb
            };
            hb.sorted()
        })?;
        let file = BufWriter::new(
            std::fs::File::create(&slot.output)
                .map_err(|e| format!("failed to create {}: {e}", slot.output.display()))?
        );
        let mut w = PbfWriter::new(file, compression);
        w.write_header(&header_bytes)
            .map_err(|e| format!("failed to write header to {}: {e}", slot.output.display()))?;
        writers.push(w);
    }

    // Per-region ID sets and stats.
    let mut bbox_node_ids: Vec<IdSetDense> = (0..n).map(|_| IdSetDense::new()).collect();
    let mut matched_way_ids: Vec<IdSetDense> = (0..n).map(|_| IdSetDense::new()).collect();
    let mut matched_relation_ids: Vec<IdSetDense> = (0..n).map(|_| IdSetDense::new()).collect();
    let mut stats: Vec<ExtractStats> = (0..n).map(|_| ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    }).collect();

    // Build schedules by element type for parallel classification.
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut relation_schedule: Vec<(usize, u64, usize)> = Vec::new();
    // Per-node-blob passthrough metadata.
    let mut node_blob_info: Vec<NodeBlobInfo> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = hdr.index() {
            if !spatial_filter.wants_index(&idx) { continue; }
            match idx.kind {
                crate::blob_index::ElemKind::Node => {
                    // Raw passthrough is only sound for bbox regions — polygon
                    // regions can exclude nodes inside the bbox but outside the
                    // polygon boundary or inside holes.
                    let mut contained_in: Vec<usize> = Vec::new();
                    if let Some(ref blob_bbox) = idx.bbox {
                        for (i, (bi, slot)) in bbox_ints.iter().zip(slots.iter()).enumerate() {
                            if matches!(slot.region, Region::Bbox(_)) {
                                let region_bbox = crate::BlobBbox::new(bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon);
                                if region_bbox.contains(blob_bbox) {
                                    contained_in.push(i);
                                }
                            }
                        }
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let frame_size = (data_offset - frame_offset) as usize + data_size;
                    node_blob_info.push(NodeBlobInfo { contained_in, frame_offset, frame_size, count: idx.count });
                    node_schedule.push((seq, data_offset, data_size));
                }
                crate::blob_index::ElemKind::Way => way_schedule.push((seq, data_offset, data_size)),
                crate::blob_index::ElemKind::Relation => relation_schedule.push((seq, data_offset, data_size)),
            }
        } else {
            // No indexdata — include in all schedules (conservative).
            node_blob_info.push(NodeBlobInfo { contained_in: Vec::new(), frame_offset, frame_size: 0, count: 0 });
            node_schedule.push((seq, data_offset, data_size));
            way_schedule.push((seq, data_offset, data_size));
            relation_schedule.push((seq, data_offset, data_size));
        }
        seq += 1;
    }
    drop(scanner);

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    // Phase 1: Parallel node classification → N bbox_node_ids.
    // For all-bbox regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) with single-pass multi-region classification.
    // Polygon regions fall back to element-by-element iteration.
    let all_bbox = slots.iter().all(|s| matches!(s.region, Region::Bbox(_)));
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_START");
    if all_bbox {
        let bboxes: Vec<(i32, i32, i32, i32)> = bbox_ints.iter()
            .map(|bi| (bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon))
            .collect();
        parallel_classify_phase(
            &shared_file,
            &node_schedule,
            || (crate::read::columnar::DenseNodeColumns::new(), vec![Vec::<i64>::new(); n]),
            |block, (columns, scratch)| {
                block.decode_dense_columns(columns);
                for v in scratch.iter_mut() { v.clear(); }
                columns.collect_matching_ids_multi_bbox(&bboxes, scratch);
                scratch.iter_mut().map(|v| v.drain(..).collect::<Vec<i64>>()).collect::<Vec<_>>()
            },
            |region_ids: Vec<Vec<i64>>| {
                for (i, ids) in region_ids.into_iter().enumerate() {
                    for id in ids { bbox_node_ids[i].set(id); }
                }
            },
        )?;
    } else {
        parallel_classify_phase(
            &shared_file,
            &node_schedule,
            || vec![Vec::<i64>::new(); n],
            |block, scratch| {
                for v in scratch.iter_mut() { v.clear(); }
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn) => {
                            let lat = dn.decimicro_lat();
                            let lon = dn.decimicro_lon();
                            for i in 0..n {
                                if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                                    scratch[i].push(dn.id());
                                }
                            }
                        }
                        Element::Node(nd) => {
                            let lat = nd.decimicro_lat();
                            let lon = nd.decimicro_lon();
                            for i in 0..n {
                                if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                                    scratch[i].push(nd.id());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                scratch.iter_mut().map(|v| v.drain(..).collect::<Vec<i64>>()).collect::<Vec<_>>()
            },
            |region_ids: Vec<Vec<i64>>| {
                for (i, ids) in region_ids.into_iter().enumerate() {
                    for id in ids { bbox_node_ids[i].set(id); }
                }
            },
        )?;
    }
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_END");

    // Phase 1 write: parallel decode with raw passthrough for fully-contained node blobs.
    crate::debug::emit_marker("MULTI_NODE_WRITE_START");
    multi_extract_pread_write_nodes(
        &shared_file,
        &node_schedule,
        &node_blob_info,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            for element in block.elements() {
                match &element {
                    Element::DenseNode(dn) if bbox_node_ids.iter().any(|s| s.get(dn.id())) => {
                        let id = dn.id();
                        let meta = dense_node_metadata(dn);
                        for i in 0..n {
                            if bbox_node_ids[i].get(id) {
                                ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                                bbs[i].add_node(id, dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                                counts[i] += 1;
                            }
                        }
                    }
                    Element::Node(nd) if bbox_node_ids.iter().any(|s| s.get(nd.id())) => {
                        let id = nd.id();
                        let meta = element_metadata(&nd.info());
                        for i in 0..n {
                            if bbox_node_ids[i].get(id) {
                                ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                                bbs[i].add_node(id, nd.decimicro_lat(), nd.decimicro_lon(), nd.tags(), meta.as_ref());
                                counts[i] += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
    )?;
    crate::debug::emit_marker("MULTI_NODE_WRITE_END");

    // Phase 2: Parallel way classification → N matched_way_ids.
    crate::debug::emit_marker("MULTI_WAY_CLASSIFY_START");
    parallel_classify_phase(
        &shared_file,
        &way_schedule,
        || (),
        |block, _s| {
            let mut region_ids: Vec<Vec<i64>> = vec![Vec::new(); n];
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    for i in 0..n {
                        if w.refs().any(|r| bbox_node_ids[i].get(r)) {
                            region_ids[i].push(w.id());
                        }
                    }
                }
            }
            region_ids
        },
        |region_ids| {
            for (i, ids) in region_ids.into_iter().enumerate() {
                for id in ids { matched_way_ids[i].set(id); }
            }
        },
    )?;
    crate::debug::emit_marker("MULTI_WAY_CLASSIFY_END");

    // Phase 2 write: parallel decode, write matching ways to N writers.
    crate::debug::emit_marker("MULTI_WAY_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &way_schedule,
        n,
        |block, bbs, output, scratch| {
            let mut counts = vec![0u64; n];
            for element in block.elements() {
                if let Element::Way(w) = &element {
                    let wid = w.id();
                    if !matched_way_ids.iter().any(|s| s.get(wid)) { continue; }
                    scratch.clear();
                    scratch.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    for i in 0..n {
                        if matched_way_ids[i].get(wid) {
                            ensure_way_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_way(wid, w.tags(), scratch, meta.as_ref());
                            counts[i] += 1;
                        }
                    }
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
        |s| &mut s.ways_written,
    )?;
    crate::debug::emit_marker("MULTI_WAY_WRITE_END");

    // Phase 3: Parallel relation classification → N matched_relation_ids.
    crate::debug::emit_marker("MULTI_REL_CLASSIFY_START");
    parallel_classify_accumulate(
        &shared_file,
        &relation_schedule,
        || (0..n).map(|_| IdSetDense::new()).collect::<Vec<_>>(),
        |block, region_ids| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    for i in 0..n {
                        if relation_has_matched_member(r, &bbox_node_ids[i], &matched_way_ids[i]) {
                            region_ids[i].set(r.id());
                        }
                    }
                }
            }
        },
        |region_ids| {
            for (i, worker_set) in region_ids.into_iter().enumerate() {
                matched_relation_ids[i].merge(worker_set);
            }
        },
    )?;
    crate::debug::emit_marker("MULTI_REL_CLASSIFY_END");

    // Phase 3 write: parallel decode, write matching relations to N writers.
    crate::debug::emit_marker("MULTI_REL_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &relation_schedule,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            let mut members_buf: Vec<MemberData<'_>> = Vec::new();
            for element in block.elements() {
                if let Element::Relation(r) = &element {
                    let rid = r.id();
                    if !matched_relation_ids.iter().any(|s| s.get(rid)) { continue; }
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    for i in 0..n {
                        if matched_relation_ids[i].get(rid) {
                            ensure_relation_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_relation(rid, r.tags(), &members_buf, meta.as_ref());
                            counts[i] += 1;
                        }
                    }
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
        |s| &mut s.relations_written,
    )?;
    crate::debug::emit_marker("MULTI_REL_WRITE_END");

    // Flush all writers (workers already flushed their BlockBuilders per blob).
    for (i, slot) in slots.iter().enumerate() {
        writers[i].flush()
            .map_err(|e| format!("failed to flush {}: {e}", slot.output.display()))?;
    }

    // Print per-region stats.
    for (i, slot) in slots.iter().enumerate() {
        let s = &stats[i];
        let total = s.nodes_in_bbox + s.ways_written + s.relations_written;
        eprintln!(
            "  [{}] {}: {} elements ({} nodes, {} ways, {} relations)",
            i + 1,
            slot.output.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            total, s.nodes_in_bbox, s.ways_written, s.relations_written,
        );
    }

    Ok(Some(stats))
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

/// Node write phase with raw passthrough for fully-contained blobs.
///
/// Blobs fully contained in ALL N regions are written as raw frames to all
/// N writers without decode. Other blobs go through parallel decode workers.
/// Both streams are interleaved in sequence order via ReorderBuffer.
#[allow(clippy::too_many_lines)]
/// Per-node-blob passthrough metadata: (contained_regions, frame_offset, frame_size, count).
struct NodeBlobInfo {
    contained_in: Vec<usize>,
    frame_offset: u64,
    frame_size: usize,
    count: u64,
}

#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn multi_extract_pread_write_nodes<F>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    blob_info: &[NodeBlobInfo],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut Vec<BlockBuilder>, &mut Vec<Vec<OwnedBlock>>, &mut Vec<i64>)
        -> std::result::Result<Vec<u64>, String> + Send + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    // Blobs fully contained in ALL N regions skip decode entirely — write raw
    // frame to all N writers. Other blobs go through parallel decode workers.
    let mut decode_items: Vec<(usize, u64, usize)> = Vec::new();
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new();
    for (local_seq, ((_global_seq, data_offset, data_size), info)) in
        schedule.iter().zip(blob_info.iter()).enumerate()
    {
        if info.contained_in.len() == n {
            passthrough_items.push((local_seq, info.frame_offset, info.frame_size, info.count));
        } else {
            decode_items.push((local_seq, *data_offset, *data_size));
        }
    }

    if !passthrough_items.is_empty() {
        let pt = passthrough_items.len();
        let dc = decode_items.len();
        eprintln!("  node blobs: {pt} passthrough, {dc} decoded");
    }

    // If everything is passthrough, skip the thread scope entirely.
    if decode_items.is_empty() {
        let mut frame_buf: Vec<u8> = Vec::new();
        for &(_, frame_offset, frame_size, count) in &passthrough_items {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(&mut frame_buf, frame_offset)
                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
            for i in 0..n {
                writers[i].write_raw(&frame_buf)?;
                stats[i].nodes_in_bbox += count;
            }
        }
        return Ok(());
    }

    let decode_threads = std::thread::available_parallelism()
        .map(|t| t.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, MultiNodeCI)>(32);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for item in decode_items {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bbs: Vec<BlockBuilder> = (0..n).map(|_| BlockBuilder::new()).collect();
                let mut output: Vec<Vec<OwnedBlock>> = (0..n).map(|_| Vec::new()).collect();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut i64_scratch: Vec<i64> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: WorkerResult = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        for v in &mut output { v.clear(); }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            ))?;
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i]).map_err(|e| {
                                crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e))
                                )
                            })?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> = output.iter_mut()
                            .map(|v| v.drain(..).collect())
                            .collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, MultiNodeCI::Decoded(r))).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Pre-insert passthrough items into the reorder buffer.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<MultiNodeCI> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);
        for &(local_seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(local_seq, MultiNodeCI::Passthrough(frame_offset, frame_size, count));
        }

        let mut frame_buf: Vec<u8> = Vec::new();
        for (s, item) in result_rx {
            reorder.push(s, item);
            while let Some(ci) = reorder.pop_ready() {
                write_consumer_item(ci, n, shared_file, &mut frame_buf, writers, stats)?;
            }
        }
        while let Some(ci) = reorder.pop_ready() {
            write_consumer_item(ci, n, shared_file, &mut frame_buf, writers, stats)?;
        }
        Ok(())
    })?;

    Ok(())
}

/// Write one consumer item (decoded or passthrough) to N writers.
fn write_consumer_item(
    item: MultiNodeCI,
    n: usize,
    shared_file: &std::sync::Arc<std::fs::File>,
    frame_buf: &mut Vec<u8>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;
    match item {
        MultiNodeCI::Decoded(r) => {
            let (region_blocks, counts) = r?;
            for (i, blocks) in region_blocks.into_iter().enumerate() {
                stats[i].nodes_in_bbox += counts[i];
                for (block_bytes, index, tagdata) in blocks {
                    writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                }
            }
        }
        MultiNodeCI::Passthrough(frame_offset, frame_size, count) => {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(frame_buf, frame_offset)
                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
            for i in 0..n {
                writers[i].write_raw(frame_buf)?;
                stats[i].nodes_in_bbox += count;
            }
        }
    }
    Ok(())
}

enum MultiNodeCI {
    Decoded(crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>),
    Passthrough(u64, usize, u64),
}

/// Multi-region pread-from-workers write pass.
///
/// Workers pread blob data, decompress, parse into PrimitiveBlock, then call
/// the provided closure to classify elements against N regions and produce
/// N × Vec<OwnedBlock>. The consumer writes each region's blocks to its
/// writer in sequence order.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn multi_extract_pread_write<F>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut Vec<BlockBuilder>, &mut Vec<Vec<OwnedBlock>>, &mut Vec<i64>)
        -> std::result::Result<Vec<u64>, String> + Send + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = std::thread::available_parallelism()
        .map(|t| t.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // Worker result: per-region OwnedBlocks + per-region counts.
    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, WorkerResult)>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule items to workers with local sequence index.
        scope.spawn(move || {
            for (local_seq, &(_global_seq, data_offset, data_size)) in schedule.iter().enumerate() {
                if desc_tx.send((local_seq, data_offset, data_size)).is_err() { break; }
            }
        });

        // Workers: pread → decompress → PrimitiveBlock → classify N regions → N × OwnedBlocks.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bbs: Vec<BlockBuilder> = (0..n).map(|_| BlockBuilder::new()).collect();
                let mut output: Vec<Vec<OwnedBlock>> = (0..n).map(|_| Vec::new()).collect();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut i64_scratch: Vec<i64> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: WorkerResult = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        for v in &mut output { v.clear(); }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            ))?;
                        // Flush remaining elements in each BlockBuilder.
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i]).map_err(|e| {
                                crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e))
                                )
                            })?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> = output.iter_mut()
                            .map(|v| v.drain(..).collect())
                            .collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, r)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Consumer: receive N-region results in order, write to N writers.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<WorkerResult> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        for (s, item) in result_rx {
            reorder.push(s, item);

            while let Some(result) = reorder.pop_ready() {
                let (region_blocks, counts) = result?;
                for (i, blocks) in region_blocks.into_iter().enumerate() {
                    *stat_field(&mut stats[i]) += counts[i];
                    for (block_bytes, index, tagdata) in blocks {
                        writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                }
            }
        }
        // Drain remaining.
        while let Some(result) = reorder.pop_ready() {
            let (region_blocks, counts) = result?;
            for (i, blocks) in region_blocks.into_iter().enumerate() {
                *stat_field(&mut stats[i]) += counts[i];
                for (block_bytes, index, tagdata) in blocks {
                    writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}

fn merge_extract_stats(target: &mut ExtractStats, source: &ExtractStats) {
    target.nodes_in_bbox += source.nodes_in_bbox;
    target.nodes_from_ways += source.nodes_from_ways;
    target.nodes_from_relations += source.nodes_from_relations;
    target.ways_written += source.ways_written;
    target.ways_from_relations += source.ways_from_relations;
    target.relations_written += source.relations_written;
}

/// Shared pread-from-workers write pass used by complete pass 2 and smart pass 3.
///
/// Workers pread blob data from a shared file descriptor, decompress, parse into
/// `PrimitiveBlock`, call the provided `block_fn` closure to extract matching
/// elements, and send `OwnedBlock`s back for ordered writing.
///
/// The closure captures the caller's ID sets and clean reference.
/// Blob descriptor for pread schedule.
#[derive(Clone, Copy)]
struct BlobDesc {
    /// Byte offset of the 4-byte frame length prefix (start of the entire blob frame).
    frame_offset: u64,
    /// Total size of the blob frame (4-byte len + header + blob body).
    frame_size: usize,
    offset: u64,
    size: usize,
    kind: Option<crate::blob_index::ElemKind>,
    /// Spatial bbox from indexdata (node blobs only, v2 format).
    bbox: Option<crate::BlobBbox>,
    /// Element count from indexdata (for stats on raw passthrough blobs).
    count: u64,
    /// True if this blob can be passed through raw (no decode/re-encode).
    /// Set by the schedule builder based on blob bbox containment.
    raw_passthrough: bool,
}

/// Build a blob schedule from a header-only scan.
fn build_blob_schedule(input: &Path) -> Result<Vec<BlobDesc>> {
    build_blob_schedule_with_passthrough(input, None)
}

/// Build a blob schedule, optionally tagging node blobs eligible for raw passthrough.
/// A node blob is eligible if its bbox is fully contained in the extract bbox.
fn build_blob_schedule_with_passthrough(
    input: &Path,
    extract_bbox: Option<&crate::BlobBbox>,
) -> Result<Vec<BlobDesc>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut schedule = Vec::new();
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        let idx = hdr.index();
        let kind = idx.as_ref().map(|i| i.kind);

        // Tag node blobs for raw passthrough if fully contained in extract bbox.
        let raw_passthrough = extract_bbox.is_some_and(|ebbox| {
            idx.as_ref().is_some_and(|i|
                matches!(i.kind, crate::blob_index::ElemKind::Node)
                && i.bbox.as_ref().is_some_and(|bb| ebbox.contains(bb))
            )
        });

        let bbox = idx.as_ref().and_then(|i| i.bbox);
        let count = idx.as_ref().map_or(0, |i| i.count);

        #[allow(clippy::cast_possible_truncation)]
        let frame_size = (data_offset - frame_offset) as usize + data_size;
        schedule.push(BlobDesc { frame_offset, frame_size, offset: data_offset, size: data_size, kind, bbox, count, raw_passthrough });
    }
    Ok(schedule)
}

/// Execute a pread-from-workers write pass on a pre-built schedule.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn pread_execute<F>(
    input: &Path,
    schedule: &[BlobDesc],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    block_fn: F,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> std::result::Result<ExtractStats, String> + Send + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    // Shared file for pread. Uses buffered (non-O_DIRECT) I/O because O_DIRECT
    // requires aligned buffers for pread, which we don't have — our read buffers
    // are plain Vec<u8> without alignment guarantees.
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    type WorkerResult = (usize, crate::error::Result<(Vec<OwnedBlock>, ExtractStats)>);

    // Split schedule: decode blobs go to workers, passthrough blobs handled by consumer.
    // Both are re-sequenced for the reorder buffer.
    let mut decode_items: Vec<(usize, u64, usize)> = Vec::new(); // (global_seq, data_offset, data_size)
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new(); // (global_seq, frame_offset, frame_size, count)
    for (i, d) in schedule.iter().enumerate() {
        if d.raw_passthrough {
            passthrough_items.push((i, d.frame_offset, d.frame_size, d.count));
        } else {
            decode_items.push((i, d.offset, d.size));
        }
    }

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<WorkerResult>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed decode-only blobs to workers.
        // Passthrough blobs are handled directly by the consumer.
        scope.spawn(move || {
            for item in decode_items {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        // Workers: pread → decompress → PrimitiveBlock → extract → OwnedBlocks.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bb = BlockBuilder::new();
                let mut output_blocks: Vec<OwnedBlock> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: crate::error::Result<(Vec<OwnedBlock>, ExtractStats)> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;

                        // Decode path: full PrimitiveBlock → extract → OwnedBlocks.
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        output_blocks.clear();
                        let block_stats = block_fn_ref(
                            &block, &mut bb, &mut output_blocks,
                        ).map_err(|e| crate::error::new_error(
                            crate::error::ErrorKind::Io(std::io::Error::other(e))
                        ))?;
                        flush_local(&mut bb, &mut output_blocks).map_err(|e| {
                            crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            )
                        })?;
                        Ok((std::mem::take(&mut output_blocks), block_stats))
                    })();
                    if tx.send((s, r)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Consumer: merge two streams — worker OwnedBlocks + passthrough raw frames.
        // Both use the reorder buffer keyed by global sequence number.
        // Passthrough blobs: consumer reads raw frame directly, writes via write_raw_owned.
        // Worker blobs: consumer receives OwnedBlocks, writes via write_primitive_block_owned.

        enum ConsumerItem {
            Decoded(crate::error::Result<(Vec<OwnedBlock>, ExtractStats)>),
            Passthrough(u64, usize, u64), // (frame_offset, frame_size, element_count)
        }

        let _total_blobs = schedule.len();
        let mut reorder: crate::reorder_buffer::ReorderBuffer<ConsumerItem> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        // Pre-insert passthrough items into the reorder buffer.
        for &(seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(seq, ConsumerItem::Passthrough(frame_offset, frame_size, count));
        }

        // Drain worker results into the reorder buffer.
        let mut frame_read_buf: Vec<u8> = Vec::new();
        for (s, item) in result_rx {
            reorder.push(s, ConsumerItem::Decoded(item));

            while let Some(ci) = reorder.pop_ready() {
                match ci {
                    ConsumerItem::Decoded(r) => {
                        let (blocks, block_stats) = r?;
                        merge_extract_stats(stats, &block_stats);
                        for (block_bytes, index, tagdata) in blocks {
                            writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                        }
                    }
                    ConsumerItem::Passthrough(frame_offset, frame_size, count) => {
                        // Read raw frame directly and write without decode/re-encode.
                        frame_read_buf.resize(frame_size, 0);
                        shared_file.read_exact_at(&mut frame_read_buf, frame_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                        stats.nodes_in_bbox += count;
                    }
                }
            }
        }

        // Drain any remaining passthrough items after workers are done.
        while let Some(ci) = reorder.pop_ready() {
            match ci {
                ConsumerItem::Decoded(r) => {
                    let (blocks, block_stats) = r?;
                    merge_extract_stats(stats, &block_stats);
                    for (block_bytes, index, tagdata) in blocks {
                        writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                }
                ConsumerItem::Passthrough(frame_offset, frame_size, count) => {
                    frame_read_buf.resize(frame_size, 0);
                    shared_file.read_exact_at(&mut frame_read_buf, frame_offset)
                        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                    writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                    stats.nodes_in_bbox += count;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}

/// Convenience: build schedule + execute + flush. Used by complete/smart write passes.
/// Flushes the writer after execution (assumes single-use — don't call on a writer
/// that will be reused for subsequent phases).
fn pread_write_pass<F>(
    input: &Path,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    block_fn: F,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> std::result::Result<ExtractStats, String> + Send + Sync,
{
    let schedule = build_blob_schedule(input)?;
    pread_execute(input, &schedule, writer, stats, block_fn)?;
    writer.flush()?;
    Ok(())
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
    // to read just the first blob (header).
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
    let spatial_filter = spatial_blob_filter(&bbox_int);

    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let decompress_pool = crate::blob::DecompressPool::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !spatial_filter.wants_index(&idx) { continue; }
        }
        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        let block = PrimitiveBlock::new(decompressed)?;
        classify_block_simple(
            &block, region, &bbox_int,
            &mut bbox_node_ids, &mut matched_way_ids, &mut matched_relation_ids,
        );
    }
    crate::debug::emit_marker("EXTRACT_PASS1_END");

    crate::debug::emit_marker("EXTRACT_PASS2_START");
    let all_way_node_ids = IdSetDense::new();

    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let decompress_pool = crate::blob::DecompressPool::new();
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        let block = PrimitiveBlock::new(decompressed)?;
        batch.push(block);
        if batch.len() >= BATCH_SIZE {
            process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_PASS2_END");
    Ok(stats)
}

/// 3-phase barrier pipeline for sorted simple extract.
///
/// Exploits the sorted PBF guarantee (nodes → ways → relations) to parallelize
/// both classification and writing. Each phase runs pread-from-workers:
///
/// Phase 1 (nodes): workers classify (bbox check, pure function) + write matches.
///   Consumer collects bbox_node_ids. No shared mutable state needed by workers.
/// Phase 2 (ways): workers check refs against frozen &bbox_node_ids + write matches.
///   Consumer collects matched_way_ids.
/// Phase 3 (relations): workers check members against frozen ID sets + write matches.
///
/// ID sets become read-only after each phase barrier. Workers share them via
/// references in thread::scope. Single file scan (schedule built once from
/// header-only pass), three execution phases.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
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
    let spatial_filter = spatial_blob_filter(&bbox_int);

    // Build schedule once, partition by element type.
    // Tag node blobs for raw passthrough if bbox region + no clean.
    let passthrough_bbox = if matches!(region, Region::Bbox(_)) && !clean.any() {
        Some(crate::BlobBbox::new(
            bbox_int.min_lat, bbox_int.max_lat, bbox_int.min_lon, bbox_int.max_lon,
        ))
    } else {
        None
    };
    let full_schedule = build_blob_schedule_with_passthrough(input, passthrough_bbox.as_ref())?;
    let node_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| {
            match d.kind {
                Some(crate::blob_index::ElemKind::Node) => {
                    // Apply spatial bbox filter to skip node blobs outside extract region.
                    if let Some(ref filter_bbox) = spatial_filter.node_bbox {
                        match d.bbox {
                            Some(ref bb) => filter_bbox.intersects(bb),
                            None => true, // no bbox in indexdata — must include
                        }
                    } else {
                        true // no spatial filter configured
                    }
                }
                None => true, // no indexdata — must include
                _ => false,
            }
        })
        .collect();
    // Non-indexed blobs (kind == None) are included in all three schedules
    // because we can't determine their type without decompressing. Each phase's
    // classify closure only processes its matching element type, so elements of
    // other types are silently skipped. This means non-indexed blobs are
    // decompressed up to 3 times — acceptable since indexed PBFs (production
    // path) always have kind set and this path is only reachable via --force.
    let way_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| matches!(d.kind, Some(crate::blob_index::ElemKind::Way) | None))
        .collect();
    let relation_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| matches!(d.kind, Some(crate::blob_index::ElemKind::Relation) | None))
        .collect();

    // Open writer.
    let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = header_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    super::warn_locations_on_ways_loss(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let empty_relation_ids = IdSetDense::new(); // placeholder for node/way phases
    let all_way_node_ids = IdSetDense::new();

    // --- Phase 1: Classify nodes (parallel pread + scanner) ---
    // Workers pread node blobs, decompress, scan with node-only scanner,
    // check bbox (pure function), send matching IDs to consumer.
    // Consumer merges into bbox_node_ids. No shared mutable state in workers.
    crate::debug::emit_marker("SIMPLE_NODE_CLASSIFY_START");
    {
        use std::os::unix::fs::FileExt as _;

        // node_schedule already filtered to node blobs.

        let classify_file = std::sync::Arc::new(
            std::fs::File::open(input)
                .map_err(|e| format!("failed to open {}: {e}", input.display()))?
        );

        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        type ClassifyResult = (usize, crate::error::Result<Vec<i64>>);
        let (cls_tx, cls_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
        let cls_rx = std::sync::Arc::new(std::sync::Mutex::new(cls_rx));
        let (ids_tx, ids_rx) = std::sync::mpsc::sync_channel::<ClassifyResult>(32);

        std::thread::scope(|scope| -> Result<()> {
            // Dispatcher: send node blob descriptors.
            let descs: Vec<(usize, u64, usize)> = node_schedule.iter()
                .enumerate()
                .map(|(i, d)| (i, d.offset, d.size))
                .collect();
            scope.spawn(move || {
                for item in descs {
                    if cls_tx.send(item).is_err() { break; }
                }
            });

            // Workers: pread → decompress → node scanner → bbox check → Vec<i64>.
            let region_ref = region;
            let bbox_int_ref = &bbox_int;
            for _ in 0..decode_threads {
                let rx = std::sync::Arc::clone(&cls_rx);
                let tx = ids_tx.clone();
                let file = std::sync::Arc::clone(&classify_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut tuples: Vec<super::node_scanner::NodeTuple> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let (seq, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => break,
                            }
                        };

                        let r: crate::error::Result<Vec<i64>> = (|| {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)
                                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                            tuples.clear();
                            super::node_scanner::extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)
                                .map_err(|e| crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                                ))?;
                            let matching: Vec<i64> = tuples.iter()
                                .filter(|t| region_ref.contains_decimicro(bbox_int_ref, t.lat, t.lon))
                                .map(|t| t.id)
                                .collect();
                            Ok(matching)
                        })();
                        if tx.send((seq, r)).is_err() { break; }
                    }
                });
            }
            drop(cls_rx);
            drop(ids_tx);

            // Consumer: merge matching IDs into bbox_node_ids.
            for (_seq, result) in ids_rx {
                let matching_ids = result?;
                for id in matching_ids {
                    bbox_node_ids.set(id);
                }
            }
            Ok(())
        })?;
    }
    crate::debug::emit_marker("SIMPLE_NODE_CLASSIFY_END");
    // bbox_node_ids frozen. Write matching nodes via pread-from-workers.
    crate::debug::emit_marker("SIMPLE_NODE_WRITE_START");
    let node_descs: Vec<BlobDesc> = node_schedule.iter().map(|d| **d).collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(input, &node_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_NODE_WRITE_END");
    // --- Phase 2: Classify ways (scanner) + write ways (pread-from-workers) ---
    crate::debug::emit_marker("SIMPLE_WAY_CLASSIFY_START");
    {
        use std::os::unix::fs::FileExt as _;

        let classify_file = std::sync::Arc::new(
            std::fs::File::open(input)
                .map_err(|e| format!("failed to open {}: {e}", input.display()))?
        );
        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        type WayClassifyResult = (usize, crate::error::Result<Vec<i64>>);
        let (cls_tx, cls_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
        let cls_rx = std::sync::Arc::new(std::sync::Mutex::new(cls_rx));
        let (ids_tx, ids_rx) = std::sync::mpsc::sync_channel::<WayClassifyResult>(32);

        std::thread::scope(|scope| -> Result<()> {
            let descs: Vec<(usize, u64, usize)> = way_schedule.iter()
                .enumerate()
                .map(|(i, d)| (i, d.offset, d.size))
                .collect();
            scope.spawn(move || {
                for item in descs {
                    if cls_tx.send(item).is_err() { break; }
                }
            });

            // Workers: pread → decompress → way-ref scanner → check bbox_node_ids → Vec<i64>.
            let bbox_ids_ref = &bbox_node_ids;
            for _ in 0..decode_threads {
                let rx = std::sync::Arc::clone(&cls_rx);
                let tx = ids_tx.clone();
                let file = std::sync::Arc::clone(&classify_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let (seq, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => break,
                            }
                        };

                        let r: crate::error::Result<Vec<i64>> = (|| {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)
                                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                            let mut matching: Vec<i64> = Vec::new();
                            super::way_scanner::scan_way_refs(&decompress_buf, &mut refs_buf, &mut group_starts, |way_id, refs| {
                                if refs.iter().any(|&r| bbox_ids_ref.get(r)) {
                                    matching.push(way_id);
                                }
                            }).map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                            ))?;
                            Ok(matching)
                        })();
                        if tx.send((seq, r)).is_err() { break; }
                    }
                });
            }
            drop(cls_rx);
            drop(ids_tx);

            for (_seq, result) in ids_rx {
                let matching_ids = result?;
                for id in matching_ids {
                    matched_way_ids.set(id);
                }
            }
            Ok(())
        })?;
    }
    crate::debug::emit_marker("SIMPLE_WAY_CLASSIFY_END");
    // matched_way_ids frozen. Write matching ways via pread-from-workers.
    crate::debug::emit_marker("SIMPLE_WAY_WRITE_START");
    let way_descs: Vec<BlobDesc> = way_schedule.iter()
        .map(|d| BlobDesc { raw_passthrough: false, ..**d })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(input, &way_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_WAY_WRITE_END");
    // --- Phase 3: Classify relations + write (pread-from-workers) ---
    crate::debug::emit_marker("SIMPLE_REL_CLASSIFY_START");
    let mut matched_relation_ids = IdSetDense::new();
    {
        let (rel_classify_schedule, rel_classify_file) = super::build_classify_schedule(
            input, Some(crate::blob_index::ElemKind::Relation),
        )?;
        parallel_classify_accumulate(
            &rel_classify_file,
            &rel_classify_schedule,
            IdSetDense::new,
            |block, ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element {
                        if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                            ids.set(r.id());
                        }
                    }
                }
            },
            |worker_ids| {
                matched_relation_ids.merge(worker_ids);
            },
        )?;
    }
    crate::debug::emit_marker("SIMPLE_REL_CLASSIFY_END");
    crate::debug::emit_marker("SIMPLE_REL_WRITE_START");
    let rel_descs: Vec<BlobDesc> = relation_schedule.iter()
        .map(|d| BlobDesc { raw_passthrough: false, ..**d })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &matched_relation_ids,
        };
        pread_execute(input, &rel_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_REL_WRITE_END");
    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_SCAN_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Complete-ways strategy (two passes)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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

    // --- Pass 2: Write matching elements via pread-from-workers ---
    crate::debug::emit_marker("EXTRACT_PASS2_START");

    let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = header_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    super::warn_locations_on_ways_loss(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        matched_way_ids: &result.matched_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    pread_write_pass(input, &mut writer, &mut stats, |block, bb, output_blocks| {
        extract_block_pass2(block, &ids, clean, bb, output_blocks)
    })?;

    crate::debug::emit_marker("EXTRACT_PASS2_END");
    Ok(stats)
}

use super::{parallel_classify_phase, parallel_classify_accumulate};

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
    /// Whether workers should collect extra way/node member IDs from smart
    /// relations (multipolygon/boundary). Compile-time constant — the compiler
    /// eliminates the dead branch in `CompleteRelationHandler`.
    const COLLECT_MEMBER_IDS: bool;

    /// Process a single matched relation in the unsorted/mixed fallback path.
    /// Called after the relation ID has already been added to `matched_relation_ids`.
    fn handle_relation(&mut self, r: &crate::Relation);

    /// Merge extra way/node IDs from parallel workers (sorted path phase 3).
    fn merge_worker_extras(&mut self, extra_way_ids: IdSetDense, extra_node_ids: IdSetDense);
}

struct CompleteRelationHandler;

impl RelationHandler for CompleteRelationHandler {
    const COLLECT_MEMBER_IDS: bool = false;

    fn handle_relation(&mut self, _r: &crate::Relation) {}

    fn merge_worker_extras(&mut self, _extra_way_ids: IdSetDense, _extra_node_ids: IdSetDense) {}
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
    const COLLECT_MEMBER_IDS: bool = true;

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

    fn merge_worker_extras(&mut self, extra_way_ids: IdSetDense, extra_node_ids: IdSetDense) {
        self.extra_way_ids.merge(extra_way_ids);
        self.extra_node_ids.merge(extra_node_ids);
    }
}

/// Collect pass 1 ID sets with strategy-specific relation handling.
///
/// Reads all elements via sequential BlobReader + DecompressPool, collecting:
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

    // Sequential reader to avoid PrimitiveBlock cross-thread retention OOM.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let is_sorted = header_blob.to_headerblock()?.is_sorted();
    let filter = spatial_blob_filter(bbox_int);
    let decompress_pool = crate::blob::DecompressPool::new();

    if !is_sorted {
        for blob_result in &mut blob_reader {
            let blob = blob_result?;
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
            if let Some(idx) = blob.index() {
                if !filter.wants_index(&idx) { continue; }
            }
            let decompressed = blob.decompress_pooled(&decompress_pool)?;
            let block = PrimitiveBlock::new(decompressed)?;
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

    // Sorted path: parallel three-phase classification via pread-from-workers.
    // Phase 1: nodes (bbox check) → bbox_node_ids
    // Phase 2: ways (ref check against bbox_node_ids) → matched_way_ids + all_way_node_ids
    // Phase 3: relations (member check) → matched_relation_ids + handler extras
    drop(blob_reader);
    drop(decompress_pool);

    // Build per-type schedules from header-only scan.
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut relation_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = hdr.index() {
            if !filter.wants_index(&idx) { continue; }
            match idx.kind {
                crate::blob_index::ElemKind::Node => node_schedule.push((seq, data_offset, data_size)),
                crate::blob_index::ElemKind::Way => way_schedule.push((seq, data_offset, data_size)),
                crate::blob_index::ElemKind::Relation => relation_schedule.push((seq, data_offset, data_size)),
            }
        }
        seq += 1;
    }
    drop(scanner);

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    // Phase 1: Classify nodes by region containment.
    // For bbox-only regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) for cache-friendly classification. Polygon regions
    // fall back to element-by-element iteration.
    let use_columnar = matches!(region, Region::Bbox(_));
    parallel_classify_phase(
        &shared_file,
        &node_schedule,
        || (crate::read::columnar::DenseNodeColumns::new(), Vec::<i64>::new()),
        |block, (columns, ids)| {
            ids.clear();
            if use_columnar {
                block.decode_dense_columns(columns);
                columns.collect_matching_ids_bbox(
                    bbox_int.min_lat, bbox_int.max_lat,
                    bbox_int.min_lon, bbox_int.max_lon,
                    ids,
                );
            } else {
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn)
                            if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                        {
                            ids.push(dn.id());
                        }
                        Element::Node(n)
                            if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                        {
                            ids.push(n.id());
                        }
                        _ => {}
                    }
                }
            }
            ids.drain(..).collect::<Vec<i64>>()
        },
        |ids| {
            for id in ids { bbox_node_ids.set(id); }
        },
    )?;

    // Phase 2: Classify ways by ref intersection with bbox nodes.
    parallel_classify_phase(
        &shared_file,
        &way_schedule,
        || (),
        |block, _s| {
            let mut way_ids = Vec::new();
            let mut node_ids = Vec::new();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    if w.refs().any(|r| bbox_node_ids.get(r)) {
                        way_ids.push(w.id());
                        node_ids.extend(w.refs());
                    }
                }
            }
            (way_ids, node_ids)
        },
        |(way_ids, node_ids)| {
            for id in way_ids { matched_way_ids.set(id); }
            for id in node_ids { all_way_node_ids.set(id); }
        },
    )?;

    // Phase 3: Classify relations by member intersection.
    let collect_member_ids = H::COLLECT_MEMBER_IDS;
    parallel_classify_accumulate(
        &shared_file,
        &relation_schedule,
        || (IdSetDense::new(), IdSetDense::new(), IdSetDense::new()),
        |block, (rel_ids, extra_way_ids, extra_node_ids)| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                        rel_ids.set(r.id());
                        if collect_member_ids && is_smart_relation(r) {
                            for m in r.members() {
                                match m.id {
                                    MemberId::Way(id) => extra_way_ids.set(id),
                                    MemberId::Node(id) => extra_node_ids.set(id),
                                    MemberId::Relation(_) | MemberId::Unknown(_, _) => {}
                                }
                            }
                        }
                    }
                }
            }
        },
        |(worker_rel_ids, worker_extra_way_ids, worker_extra_node_ids)| {
            matched_relation_ids.merge(worker_rel_ids);
            handler.merge_worker_extras(worker_extra_way_ids, worker_extra_node_ids);
        },
    )?;

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
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let in_bbox = ids.bbox_node_ids.get(dn.id());
                let from_way = ids.all_way_node_ids.get(dn.id());
                if in_bbox || from_way {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
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
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
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
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = clean_metadata(element_metadata(&w.info()), clean);
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.get(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = clean_metadata(element_metadata(&r.info()), clean);
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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

    // --- Pass 2: Resolve extra way node deps (parallel pread) ---
    crate::debug::emit_marker("EXTRACT_PASS2_START");
    // For each way in extra_way_ids not already in matched_way_ids,
    // collect all node refs into extra_node_ids.
    // Only way blobs are needed here — skip node and relation blobs via index.
    {
    let (way_schedule, shared_file) = super::build_classify_schedule(
        input, Some(crate::blob_index::ElemKind::Way),
    )?;

    let extra_way_ids_ref = &handler.extra_way_ids;
    let matched_way_ids_ref = &result.matched_way_ids;
    parallel_classify_accumulate(
        &shared_file,
        &way_schedule,
        IdSetDense::new,
        |block, node_ids| {
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    let wid = w.id();
                    if extra_way_ids_ref.get(wid) && !matched_way_ids_ref.get(wid) {
                        for r in w.refs() { node_ids.set(r); }
                    }
                }
            }
        },
        |worker_node_ids| {
            extra_node_ids.merge(worker_node_ids);
        },
    )?;
    }

    crate::debug::emit_marker("EXTRACT_PASS2_END");

    // --- Pass 3: Write matching elements via pread-from-workers ---
    crate::debug::emit_marker("EXTRACT_PASS3_START");

    let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = header_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    super::warn_locations_on_ways_loss(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    let ids = ExtractPass3IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        extra_node_ids: &extra_node_ids,
        matched_way_ids: &result.matched_way_ids,
        extra_way_ids: &handler.extra_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    pread_write_pass(input, &mut writer, &mut stats, |block, bb, output_blocks| {
        extract_block_pass3(block, &ids, clean, bb, output_blocks)
    })?;

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
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
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
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
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
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = clean_metadata(element_metadata(&w.info()), clean);
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
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
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = clean_metadata(element_metadata(&r.info()), clean);
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }
    Ok(stats)
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
