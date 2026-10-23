//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::{Path, PathBuf};

use crate::cat::CleanAttrs;
use crate::coord_fmt::e7_to_deg;
use crate::error::Error;
use crate::write::WriteOptions;

use super::{HeaderOverrides, Result, require_indexdata};

mod common;
mod complete;
mod multi;
mod simple;
mod smart;

// ---------------------------------------------------------------------------
// Bounding box
// ---------------------------------------------------------------------------

pub use crate::geo::Bbox;

/// Parse a bbox string in osmium convention: `minlon,minlat,maxlon,maxlat`.
// The messages show the bad input value, which is more useful to users than the
// underlying ParseFloatError ("invalid float literal").
pub fn parse_bbox(s: &str) -> Result<Bbox> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(Error::invalid_argument(format!(
            "bbox must have 4 comma-separated values, got {}",
            parts.len()
        )));
    }
    let min_lon: f64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid min_lon: {}", parts[0])))?;
    let min_lat: f64 = parts[1]
        .trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid min_lat: {}", parts[1])))?;
    let max_lon: f64 = parts[2]
        .trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid max_lon: {}", parts[2])))?;
    let max_lat: f64 = parts[3]
        .trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid max_lat: {}", parts[3])))?;

    Bbox::try_new(min_lon, min_lat, max_lon, max_lat)
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
            // `bbox_int` is deliberately widened for conservative blob
            // selection. Exact point membership must retain the user's f64
            // boundary, or a point up to one e7 unit outside can match.
            Region::Bbox(bbox) => bbox.contains(e7_to_deg(lat as f64), e7_to_deg(lon as f64)),
            Region::Polygon { polygons, .. } => {
                if !bbox_int.contains(lat, lon) {
                    return false;
                }
                let lat_f64 = e7_to_deg(lat as f64);
                let lon_f64 = e7_to_deg(lon as f64);
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
/// Borrows hole slices without allocating a temporary list. Crossing rings
/// still unwrap per query: these public vectors can be mutated by callers, so
/// a parse-time cache would become stale. Prepared geometry needs an immutable
/// owning API before it can safely replace this representation.
fn polygon_rings_contains(poly: &PolygonRings, px: f64, py: f64) -> bool {
    crate::geo::point_in_polygon_rings(px, py, &poly.exterior, poly.holes.iter().map(Vec::as_slice))
}

// Delegate to geo module; used by tests and bbox_from_polygons.
use crate::geo::ring_crosses_antimeridian;

// ---------------------------------------------------------------------------
// GeoJSON parsing
// ---------------------------------------------------------------------------

/// Parse a GeoJSON file and extract polygon geometry as a `Region`.
///
/// Accepts:
/// - A bare Geometry with type "Polygon" or "MultiPolygon"
/// - A Feature with a Polygon/MultiPolygon geometry
/// - A FeatureCollection whose Polygon/MultiPolygon features are all included
pub fn parse_geojson(path: &Path) -> Result<Region> {
    let data = std::fs::read_to_string(path)?;
    let value: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| Error::invalid_data_with("invalid GeoJSON", e))?;
    let geometries = extract_geometries(&value)?;
    let mut polygons = Vec::new();
    for geometry in geometries {
        let geo_type = geometry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::invalid_data("geometry missing 'type' field"))?;
        let coords = geometry
            .get("coordinates")
            .ok_or_else(|| Error::invalid_data("geometry missing 'coordinates' field"))?;
        polygons.extend(parse_geometry_by_type(geo_type, coords)?);
    }
    let bbox = bbox_from_polygons(&polygons)?;
    Ok(Region::Polygon { polygons, bbox })
}

/// Navigate Feature/FeatureCollection to find the geometry object.
fn extract_geometries(value: &serde_json::Value) -> Result<Vec<&serde_json::Value>> {
    let obj_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::invalid_data("GeoJSON missing 'type' field"))?;
    match obj_type {
        "Polygon" | "MultiPolygon" => Ok(vec![value]),
        "Feature" => {
            let geom = value
                .get("geometry")
                .ok_or_else(|| Error::invalid_data("Feature missing 'geometry' field"))?;
            Ok(vec![geom])
        }
        "FeatureCollection" => {
            let features = value
                .get("features")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| Error::invalid_data("FeatureCollection missing 'features' array"))?;
            if features.is_empty() {
                return Err(Error::invalid_data("FeatureCollection has no features"));
            }
            features
                .iter()
                .enumerate()
                .map(|(i, feature)| {
                    feature.get("geometry").ok_or_else(|| {
                        Error::invalid_data(format!(
                            "FeatureCollection feature {i} missing 'geometry' field"
                        ))
                    })
                })
                .collect()
        }
        other => Err(Error::invalid_data(format!(
            "unsupported GeoJSON type: {other}"
        ))),
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
                .ok_or_else(|| Error::invalid_data("MultiPolygon coordinates must be an array"))?;
            let mut polygons = Vec::with_capacity(arr.len());
            for polygon_coords in arr {
                polygons.push(parse_polygon_coordinates(polygon_coords)?);
            }
            Ok(polygons)
        }
        other => Err(Error::invalid_data(format!(
            "unsupported geometry type: {other}"
        ))),
    }
}

/// Parse one polygon's coordinate array: `[exterior_ring, hole1, hole2, ...]`.
fn parse_polygon_coordinates(coords: &serde_json::Value) -> Result<PolygonRings> {
    let rings = coords
        .as_array()
        .ok_or_else(|| Error::invalid_data("polygon coordinates must be an array of rings"))?;
    let exterior_val = rings
        .first()
        .ok_or_else(|| Error::invalid_data("polygon must have at least one ring"))?;
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
        .ok_or_else(|| Error::invalid_data("ring must be an array of coordinate pairs"))?;
    // A one- or two-point ring has a bounding box but no interior, so
    // accepting it turns a malformed polygon into a successful empty
    // extract. Open rings are accepted on purpose: RFC 7946 asks for a
    // repeated first position, but the point-in-ring and antimeridian tests
    // close a ring implicitly, and osmium accepts three-position rings too
    // (its .poly parser closes them). Refusing them would break region
    // files that have always worked.
    if points.len() < 3 {
        return Err(Error::invalid_data(
            "polygon ring must contain at least 3 coordinate positions",
        ));
    }
    let mut result = Vec::with_capacity(points.len());
    for point in points {
        let pair = point
            .as_array()
            .ok_or_else(|| Error::invalid_data("coordinate must be a [lon, lat] array"))?;
        if pair.len() < 2 {
            return Err(Error::invalid_data(
                "coordinate array must have at least 2 elements",
            ));
        }
        let lon = pair[0]
            .as_f64()
            .ok_or_else(|| Error::invalid_data("coordinate lon must be a number"))?;
        let lat = pair[1]
            .as_f64()
            .ok_or_else(|| Error::invalid_data("coordinate lat must be a number"))?;
        crate::coord_pack::validate_lon("polygon longitude", lon)
            .map_err(Error::invalid_data_from)?;
        crate::coord_pack::validate_lat("polygon latitude", lat)
            .map_err(Error::invalid_data_from)?;
        result.push((lon, lat));
    }
    // Self-intersections are deliberately not rejected here. osmium extract
    // also accepts rings without a simplicity check and applies even-odd
    // ray crossing when selecting points; rejecting crossings would diverge
    // from that behavior and requires a separate geometry-validation pass.
    // The signed-area check below still rejects rings whose algebraic area
    // cancels to zero, including a symmetric bow-tie.
    // A ring can still collapse to a line or point (fewer than three distinct
    // positions, or all collinear) and so have no interior to select. Taking
    // the shoelace sum relative to the first position makes the closing
    // edge's term zero, so this is the full area whether or not the input
    // repeats its first position.
    let origin = result[0];
    let twice_area = result
        .windows(2)
        .map(|edge| {
            (edge[0].0 - origin.0) * (edge[1].1 - origin.1)
                - (edge[1].0 - origin.0) * (edge[0].1 - origin.1)
        })
        .sum::<f64>();
    if twice_area == 0.0 {
        return Err(Error::invalid_data(
            "polygon ring must enclose a non-zero area",
        ));
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
        return Err(Error::invalid_data(
            "no exterior ring vertices found for bounding box",
        ));
    }

    // Region::bbox and blob scheduling accept one ordinary interval. A crossing
    // polygon needs both longitude lobes; widening is conservative and loses
    // skip efficiency, not points. Two boxes require a union-aware scheduling
    // interface, including multi-extract, rather than narrowing this interval.
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
    /// The GeoJSON file this slot's region was read from (its `polygon_file`
    /// entry resolved against the config's directory), when it names one. An
    /// input of the run, so callers refuse an output that aliases it.
    pub polygon_file: Option<std::path::PathBuf>,
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
    parse_extract_config_with_output_directory(config_path, None)
}

/// Parse a multi-extract config, optionally rebasing its outputs under a CLI
/// output-directory override.
pub fn parse_extract_config_with_output_directory(
    config_path: &Path,
    directory_override: Option<&Path>,
) -> Result<(Option<PathBuf>, Vec<ExtractSlot>)> {
    let data = std::fs::read_to_string(config_path)?;
    let value: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| Error::invalid_data_with("invalid extract config JSON", e))?;

    let directory = value
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);

    let extracts_arr = value
        .get("extracts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::invalid_data("config must have an 'extracts' array"))?;

    if extracts_arr.is_empty() {
        return Err(Error::invalid_data("'extracts' array must not be empty"));
    }
    if extracts_arr.len() > crate::tunables::MAX_EXTRACTS {
        return Err(Error::invalid_data(format!(
            "too many extracts: {} (max {})",
            extracts_arr.len(),
            crate::tunables::MAX_EXTRACTS
        )));
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    // A relative config directory is anchored at the config file, by design.
    // osmium extract uses a relative directory from the process working
    // directory, so the same config can select different output roots there.
    let configured_output_dir = directory.as_deref().map(|dir| {
        if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            config_dir.join(dir)
        }
    });
    let default_output_dir = configured_output_dir.as_deref().unwrap_or(config_dir);
    let resolve_dir = directory_override.unwrap_or(default_output_dir);

    let mut slots = Vec::with_capacity(extracts_arr.len());
    let mut output_paths: Vec<std::path::PathBuf> = Vec::with_capacity(extracts_arr.len());

    for (i, entry) in extracts_arr.iter().enumerate() {
        let output_name = entry
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::invalid_data(format!("extract[{i}] missing 'output' field")))?;

        let output_name_path = Path::new(output_name);
        let output_path = if directory_override.is_some() {
            rebase_config_output(output_name_path, default_output_dir, resolve_dir)
        } else {
            resolve_dir.join(output_name_path)
        };
        if output_paths.contains(&output_path) {
            return Err(Error::invalid_data(format!(
                "duplicate output path: {}",
                output_path.display()
            )));
        }
        output_paths.push(output_path.clone());

        let region = parse_extract_geometry(entry, i, config_dir)?;
        let polygon_file = entry
            .get("polygon_file")
            .and_then(serde_json::Value::as_str)
            .map(|name| config_dir.join(name));
        slots.push(ExtractSlot {
            region,
            output: output_path,
            polygon_file,
        });
    }

    Ok((directory, slots))
}

fn rebase_config_output(output: &Path, old_root: &Path, new_root: &Path) -> PathBuf {
    // osmium extract concatenates its output-directory string and configured
    // output string, including when the latter is absolute. Under pbfhogg's
    // -d override, absolute paths below the configured root keep their
    // relative suffix, while paths outside that root are reduced to their
    // filename so the result stays under the override directory.
    let relative = if output.is_absolute() {
        output
            .strip_prefix(old_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| {
                output
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| output.to_path_buf())
            })
    } else {
        output.to_path_buf()
    };
    new_root.join(relative)
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
        return Err(Error::invalid_data(format!(
            "extract[{index}] must have exactly one of 'bbox', 'polygon', or 'polygon_file'"
        )));
    }
    if geo_count > 1 {
        return Err(Error::invalid_data(format!(
            "extract[{index}] has multiple geometry fields; use exactly one of 'bbox', 'polygon', or 'polygon_file'"
        )));
    }

    if has_bbox {
        let arr = entry
            .get("bbox")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                Error::invalid_data(format!("extract[{index}] 'bbox' must be an array"))
            })?;
        if arr.len() != 4 {
            return Err(Error::invalid_data(format!(
                "extract[{index}] 'bbox' must have 4 elements, got {}",
                arr.len()
            )));
        }
        let number = |slot: usize| {
            arr[slot].as_f64().ok_or_else(|| {
                Error::invalid_data(format!("extract[{index}] bbox[{slot}] must be a number"))
            })
        };
        let min_lon = number(0)?;
        let min_lat = number(1)?;
        let max_lon = number(2)?;
        let max_lat = number(3)?;
        // The config is the input here, so a bad box is invalid data, not an
        // invalid argument: a deliberate reclassification that keeps the
        // `Bbox` refusal as the cause.
        return Bbox::try_new(min_lon, min_lat, max_lon, max_lat)
            .map(Region::Bbox)
            .map_err(|e| Error::invalid_data_with(format!("extract[{index}] bbox"), e));
    }

    if has_polygon {
        let geom = entry
            .get("polygon")
            .ok_or_else(|| Error::invalid_data(format!("extract[{index}] missing 'polygon'")))?;
        let mut polygons = Vec::new();
        for geometry in extract_geometries(geom)? {
            let geo_type = geometry
                .get("type")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_data(format!("extract[{index}] polygon missing 'type' field"))
                })?;
            let coords = geometry.get("coordinates").ok_or_else(|| {
                Error::invalid_data(format!(
                    "extract[{index}] polygon missing 'coordinates' field"
                ))
            })?;
            polygons.extend(parse_geometry_by_type(geo_type, coords)?);
        }
        let bbox = bbox_from_polygons(&polygons)?;
        return Ok(Region::Polygon { polygons, bbox });
    }

    // has_polygon_file
    let file_str = entry
        .get("polygon_file")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::invalid_data(format!("extract[{index}] 'polygon_file' must be a string"))
        })?;
    let polygon_path = config_dir.join(file_str);
    parse_geojson(&polygon_path)
}

/// Run one extract per region, writing region `i` to member `i` of
/// `outputs`, and publish the whole set once every extract has succeeded.
///
/// `outputs` is checked in full when it is built, so a refusal for any
/// region costs no extraction work, and a failure in any region publishes
/// nothing.
#[allow(clippy::too_many_arguments)]
pub fn extract_multi(
    input: &Path,
    regions: &[Region],
    outputs: crate::output::OutputSet,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Vec<ExtractStats>> {
    host.cancel.check()?;
    if regions.len() != outputs.len() {
        return Err(Error::invalid_argument(format!(
            "got {} outputs for {} extract regions",
            outputs.len(),
            regions.len()
        )));
    }
    let stats = extract_multi_to(
        input, regions, &outputs, strategy, set_bounds, clean, write, overrides, host,
    )?;
    outputs.publish(&host.cancel)?;
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn extract_multi_to(
    input: &Path,
    regions: &[Region],
    outputs: &crate::output::OutputSet,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Vec<ExtractStats>> {
    // Try single-pass multi-extract for simple strategy on sorted input. It
    // writes through the synchronous writer whatever the backend, so only the
    // compression and the input-side direct-IO flag reach it.
    if matches!(strategy, ExtractStrategy::Simple)
        && !clean.any()
        && let Some(stats) = multi::try_extract_multi_single_pass(
            input,
            regions,
            outputs,
            set_bounds,
            write.compression,
            write.direct_reads,
            overrides,
            host,
        )?
    {
        return Ok(stats);
    }

    if matches!(strategy, ExtractStrategy::Simple) && !clean.any() {
        // The single-pass probe declines unsorted, empty or missing-header
        // input and a first-blob read error. Keep the fallback visible; the
        // probe reports a swallowed read error under its own event first.
        crate::host::diag!(
            host.diagnostics,
            Note,
            "extract.multi.sequential_fallback",
            "Multi-extract single-pass probe declined the input; using one full pass per region"
        );
        crate::debug::emit_counter("multi_extract_sequential_fallback", 1);
    }

    // Sequential fallback: one extract at a time, each into its borrowed
    // member. Nothing is published until every region has succeeded.
    let mut all_stats = Vec::with_capacity(regions.len());
    for (i, (region, output)) in regions.iter().zip(outputs.iter()).enumerate() {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "extract.multi.slot",
            "[{}/{}] Extracting to {}",
            i + 1,
            regions.len(),
            output.destination().display()
        );
        let stats = extract_to(
            input, output, region, strategy, set_bounds, clean, write, overrides, host,
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

/// Element counts for one extract. Every counter is observed - counted from
/// elements this command decoded - never taken from `BlobHeader.indexdata`.
pub struct ExtractStats {
    /// Nodes inside the region. A blob copied through raw contributes the
    /// node count its classify-phase decode observed, not its indexdata
    /// `count`; see `common::Passthrough`.
    pub nodes_in_bbox: u64,
    pub nodes_from_ways: u64,
    pub nodes_from_relations: u64,
    pub ways_written: u64,
    pub ways_from_relations: u64,
    pub relations_written: u64,
    pub strategy: &'static str,
}

impl ExtractStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total_nodes = self.nodes_in_bbox + self.nodes_from_ways + self.nodes_from_relations;
        let total_ways = self.ways_written + self.ways_from_relations;
        if self.nodes_from_relations > 0 || self.ways_from_relations > 0 {
            writeln!(
                w,
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
            )?;
        } else {
            writeln!(
                w,
                "Extract ({}): {} nodes ({} in bbox, {} from ways), {} ways, {} relations",
                self.strategy,
                total_nodes,
                self.nodes_in_bbox,
                self.nodes_from_ways,
                total_ways,
                self.relations_written,
            )?;
        }

        Ok(())
    }
}

/// What every extract strategy states about its output header.
///
/// Order is the input's: every strategy writes the selected elements in input
/// order (per kind, or in file order on the unsorted simple fallback), so an
/// extract of a sorted file is sorted and an extract of an unsorted file must
/// not say it is. Ways are re-encoded (only node blobs are ever copied raw),
/// so no way enrichment survives. The bbox is the input's, which contains
/// every extracted coordinate, unless `--set-bounds` asks for the region's
/// envelope - which complete_ways and smart exceed by design with the
/// out-of-region nodes of selected ways (osmium's `--set-bounds` does the
/// same).
fn output_facts(set_bounds: bool, bbox: &Bbox) -> crate::commands::output_header::OutputFacts {
    use crate::commands::output_header::{Bbox as HeaderBbox, OutputFacts, Ways};
    OutputFacts {
        bbox: if set_bounds {
            HeaderBbox::Region(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            HeaderBbox::Carry
        },
        ..OutputFacts::carry(Ways::Reencoded)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Extract elements within `region` from `input` and write to `output`.
#[allow(clippy::too_many_arguments)]
pub fn extract(
    input: &Path,
    output: crate::output::Output,
    region: &Region,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<ExtractStats> {
    host.cancel.check()?;
    let result = extract_to(
        input, &output, region, strategy, set_bounds, clean, write, overrides, host,
    )?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`extract`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
pub(crate) fn extract_to(
    input: &Path,
    output: &crate::output::Output,
    region: &Region,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<ExtractStats> {
    if !matches!(strategy, ExtractStrategy::Simple) {
        require_indexdata(
            input,
            write.direct_reads,
            write.force,
            "input PBF has no blob-level indexdata. Without indexdata, the spatial bbox \
             filter is a no-op - all blobs are decompressed (significantly slower).",
            host,
        )?;
    }
    let result = match strategy {
        ExtractStrategy::Simple => simple::extract_simple(
            input, output, region, set_bounds, clean, write, overrides, host,
        ),
        ExtractStrategy::CompleteWays => complete::extract_complete_ways(
            input, output, region, set_bounds, clean, write, overrides, host,
        ),
        ExtractStrategy::Smart => smart::extract_smart(
            input, output, region, set_bounds, clean, write, overrides, host,
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

    /// A bbox outside the geographic domain used to parse cleanly, then
    /// saturate to `i32::MAX` inside the old `BboxInt::from_bbox` (Rust
    /// float-to-int casts saturate rather than wrap) and produce an empty
    /// extract with exit code 0 - a wrong answer reported as a successful
    /// run.
    ///
    /// Latitude and longitude are checked separately because their domains
    /// differ: 100 degrees is a perfectly good longitude and not a latitude
    /// at all.
    #[test]
    fn parse_bbox_rejects_values_outside_the_geographic_domain() {
        for (input, expected) in [
            ("200,0,300,1", "min_lon"),
            ("0,0,181,1", "max_lon"),
            ("0,-91,1,1", "min_lat"),
            ("0,0,1,91", "max_lat"),
            ("0,0,1,inf", "max_lat"),
            ("-181,0,1,1", "min_lon"),
        ] {
            let err = parse_bbox(input)
                .err()
                .unwrap_or_else(|| panic!("{input} must be refused"))
                .to_string();
            assert!(
                err.contains(expected) && err.contains("degrees"),
                "expected the domain guard naming {expected} for {input}. Got: {err}"
            );
        }

        // Passing counterparts: the exact corners of the domain are legal
        // places, so the guard must not be an off-by-one on the boundary.
        assert!(parse_bbox("-180,-90,180,90").is_ok());
        // And 100 degrees east is a real longitude even though 100 degrees
        // north is not a latitude.
        assert!(parse_bbox("99,0,100,1").is_ok());
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
    // Grid-pruned vs linear classify parity (real contains_decimicro)
    // -----------------------------------------------------------------------

    /// The grid narrows the candidate regions per point; every candidate is
    /// still re-tested with the exact `contains_decimicro`. This pins that the
    /// grid-pruned classify reproduces the brute-force linear classify exactly,
    /// including per-region push order, over a mix of bbox and polygon regions
    /// (with a hole, an antimeridian full-width polygon, and a pole-adjacent
    /// polygon) using the real containment test - so conservative-cover false
    /// positives are rejected identically.
    /// Build the 24-region mix (bbox band, polygon squares, a holed polygon,
    /// an antimeridian full-width polygon, and a pole-adjacent polygon) used by
    /// [`polygon_mixed_grid_parity`]. Split out to keep that test's body within
    /// the line budget.
    fn mixed_parity_regions() -> Vec<Region> {
        fn square(lon0: f64, lat0: f64, size: f64) -> Vec<(f64, f64)> {
            vec![
                (lon0, lat0),
                (lon0 + size, lat0),
                (lon0 + size, lat0 + size),
                (lon0, lat0 + size),
                (lon0, lat0),
            ]
        }
        fn poly(exterior: Vec<(f64, f64)>, holes: Vec<Vec<(f64, f64)>>) -> Region {
            let mut min_lon = f64::MAX;
            let mut min_lat = f64::MAX;
            let mut max_lon = f64::MIN;
            let mut max_lat = f64::MIN;
            for &(x, y) in &exterior {
                min_lon = min_lon.min(x);
                max_lon = max_lon.max(x);
                min_lat = min_lat.min(y);
                max_lat = max_lat.max(y);
            }
            Region::Polygon {
                polygons: vec![PolygonRings { exterior, holes }],
                bbox: Bbox {
                    min_lon,
                    min_lat,
                    max_lon,
                    max_lat,
                },
            }
        }

        let mut regions: Vec<Region> = Vec::new();
        // 12 bbox regions tiling a longitude band.
        for k in 0..12 {
            let lon0 = -30.0 + f64::from(k) * 6.0;
            regions.push(Region::Bbox(Bbox {
                min_lon: lon0,
                min_lat: 0.0,
                max_lon: lon0 + 5.0,
                max_lat: 20.0,
            }));
        }
        // 8 polygon squares.
        for k in 0..8 {
            let lon0 = -20.0 + f64::from(k) * 5.0;
            regions.push(poly(square(lon0, -10.0, 6.0), vec![]));
        }
        // Polygon with a hole.
        regions.push(poly(square(0.0, 0.0, 10.0), vec![square(3.0, 3.0, 4.0)]));
        // Antimeridian-crossing polygon -> full-width bbox.
        regions.push(Region::Polygon {
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
        });
        // Pole-adjacent polygon.
        regions.push(poly(square(0.0, 85.0, 4.0), vec![]));
        // One more bbox in the southern hemisphere to reach 24.
        regions.push(Region::Bbox(Bbox {
            min_lon: 100.0,
            min_lat: -80.0,
            max_lon: 120.0,
            max_lat: -60.0,
        }));
        assert_eq!(regions.len(), 24);
        regions
    }

    // The two `(-.. + (xs(..) % ..) as i64) as i32` narrowings sample a point
    // whose latitude stays in +/-9e8 and longitude in +/-1.8e9 - both inside the
    // i32 decimicrodegree domain - so the u64->i64 and i64->i32 casts are lossless.
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    #[test]
    fn polygon_mixed_grid_parity() {
        use crate::read::region_grid::RegionGrid;

        fn xs(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }

        let regions = mixed_parity_regions();

        let bbox_ints: Vec<common::BboxInt> = regions
            .iter()
            .map(|r| common::BboxInt::from_bbox(r.bbox()))
            .collect();
        let region_bboxes: Vec<(i32, i32, i32, i32)> = bbox_ints
            .iter()
            .map(|b| (b.min_lat, b.max_lat, b.min_lon, b.max_lon))
            .collect();
        let grid = RegionGrid::build(&region_bboxes).expect("fixture within budget");

        let n = regions.len();
        let mut linear = vec![Vec::<i64>::new(); n];
        let mut pruned = vec![Vec::<i64>::new(); n];
        let mut state = 0xDEAD_BEEF_CAFE_F00D_u64;
        for id in 0..2_000_i64 {
            let lat = (-900_000_000_i64 + (xs(&mut state) % 1_800_000_001) as i64) as i32;
            let lon = (-1_800_000_000_i64 + (xs(&mut state) % 3_600_000_001) as i64) as i32;
            for i in 0..n {
                if regions[i].contains_decimicro(&bbox_ints[i], lat, lon) {
                    linear[i].push(id);
                }
            }
            for &j in grid.candidates(lat, lon) {
                let j = j as usize;
                if regions[j].contains_decimicro(&bbox_ints[j], lat, lon) {
                    pruned[j].push(id);
                }
            }
        }
        assert_eq!(linear, pruned);
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
        let dir = crate::path_guard::tests::scratch();
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
    fn parse_ring_rejects_degenerate_rings_and_accepts_open_ones() {
        for ring in [
            serde_json::json!([]),
            serde_json::json!([[12.0, 55.0], [13.0, 56.0]]),
        ] {
            let err = parse_ring(&ring)
                .expect_err("rings with fewer than three positions must be rejected");
            assert!(err.to_string().contains("at least 3"), "wrong error: {err}");
        }

        // Implicitly closed: the point-in-ring test supplies the last edge.
        let open_ring = serde_json::json!([[12.0, 55.0], [13.0, 55.0], [13.0, 56.0], [12.0, 56.0]]);
        let ring = parse_ring(&open_ring).expect("open rings are closed implicitly");
        assert!(point_in_ring(12.5, 55.5, &ring));
        let open_triangle = serde_json::json!([[12.0, 55.0], [13.0, 55.0], [12.0, 56.0]]);
        parse_ring(&open_triangle).expect("an open triangle has an interior");

        for ring in [
            // Closed but collinear.
            serde_json::json!([[12.0, 55.0], [13.0, 56.0], [14.0, 57.0], [12.0, 55.0]]),
            // Open and collinear.
            serde_json::json!([[12.0, 55.0], [13.0, 56.0], [14.0, 57.0]]),
            // Three positions, two distinct.
            serde_json::json!([[12.0, 55.0], [13.0, 56.0], [12.0, 55.0]]),
        ] {
            let err = parse_ring(&ring).expect_err("zero-area rings must be rejected");
            assert!(
                err.to_string().contains("non-zero area"),
                "wrong error: {err}"
            );
        }
    }

    #[test]
    fn parse_geojson_feature() {
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
            }, {
                "type": "Feature",
                "properties": {},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [[10.0, 10.0], [12.0, 10.0], [12.0, 12.0], [10.0, 12.0], [10.0, 10.0]]
                    ]
                }
            }]
        }"#;
        let path = write_temp_geojson(&dir, "fc.geojson", json);
        let region = parse_geojson(&path).unwrap();
        assert!(region.contains(2.0, 2.0));
        assert!(region.contains(11.0, 11.0));
        assert!(!region.contains(0.0, 0.0));
    }

    #[test]
    fn parse_geojson_rejects_out_of_domain_ring_coordinate() {
        let dir = crate::path_guard::tests::scratch();
        let path = write_temp_geojson(
            &dir,
            "bad-domain.geojson",
            r#"{"type":"Polygon","coordinates":[[[0,0],[181,0],[0,1],[0,0]]]}"#,
        );
        let err = parse_geojson(&path)
            .err()
            .expect("longitude must be rejected");
        assert!(
            err.to_string()
                .contains("polygon longitude (181) must be within -180 and 180 degrees"),
            "wrong guard: {err}"
        );
    }

    #[test]
    fn parse_geojson_multipolygon() {
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
        let json = r#"{
            "extracts": [{ "output": "bad.osm.pbf" }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_duplicate_output_fails() {
        let dir = crate::path_guard::tests::scratch();
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
        let dir = crate::path_guard::tests::scratch();
        let json = r#"{ "extracts": [] }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_missing_output_fails() {
        let dir = crate::path_guard::tests::scratch();
        let json = r#"{
            "extracts": [{ "bbox": [0.0, 0.0, 1.0, 1.0] }]
        }"#;
        let path = write_temp_json(&dir, "config.json", json);
        assert!(parse_extract_config(&path).is_err());
    }

    #[test]
    fn config_multiple_geometry_fails() {
        let dir = crate::path_guard::tests::scratch();
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
