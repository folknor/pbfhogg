//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::path::Path;

use rayon::prelude::*;

use crate::block_builder::{HeaderBuilder, BlockBuilder, MemberData};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementReader, MemberId, PrimitiveBlock};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Dense bitset for O(1) element ID membership testing
// ---------------------------------------------------------------------------

/// Chunked sparse bitset for O(1) element ID membership testing.
///
/// Mirrors osmium's `IdSetDense`: a vector of on-demand 4MB byte-array chunks,
/// each covering 33M IDs via bit-level addressing. Lookup and insertion are
/// 3 instructions (chunk index + byte offset + bitmask), with no hashing or
/// sorting overhead.
///
/// Memory: 1 bit per ID present in each allocated chunk, 4MB per chunk, zero
/// for empty ranges. For Denmark's 52M nodes: 2 chunks = 8MB. For planet
/// (12B node IDs): ~364 chunks = 1.5GB.
struct IdSetDense {
    chunks: Vec<Option<Box<[u8; CHUNK_SIZE]>>>,
}

const CHUNK_BITS: usize = 22;
const CHUNK_SIZE: usize = 1 << CHUNK_BITS;

impl IdSetDense {
    fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    #[allow(clippy::cast_sign_loss)]
    fn set(&mut self, id: i64) {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            self.chunks.resize_with(cid + 1, || None);
        }
        let chunk = self.chunks[cid].get_or_insert_with(|| Box::new([0u8; CHUNK_SIZE]));
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        chunk[offset] |= 1u8 << (id & 7);
    }

    #[allow(clippy::cast_sign_loss)]
    fn get(&self, id: i64) -> bool {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            return false;
        }
        match &self.chunks[cid] {
            None => false,
            Some(chunk) => {
                let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
                (chunk[offset] & (1u8 << (id & 7))) != 0
            }
        }
    }

    /// Merge another IdSetDense into this one via bitwise OR.
    #[allow(dead_code)]
    ///
    /// For non-overlapping chunks (common in sorted PBFs where each rayon thread
    /// processes a contiguous ID range), chunks are moved with zero copying.
    /// For overlapping chunks, byte-level OR is applied.
    fn merge(&mut self, other: Self) {
        if other.chunks.len() > self.chunks.len() {
            self.chunks.resize_with(other.chunks.len(), || None);
        }
        for (i, other_chunk) in other.chunks.into_iter().enumerate() {
            if let Some(oc) = other_chunk {
                match &mut self.chunks[i] {
                    Some(sc) => {
                        for (a, b) in sc.iter_mut().zip(oc.iter()) {
                            *a |= *b;
                        }
                    }
                    slot @ None => {
                        *slot = Some(oc);
                    }
                }
            }
        }
    }
}

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
    #[allow(clippy::cast_lossless)]
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
#[hotpath::measure]
pub fn extract(
    input: &Path,
    output: &Path,
    region: &Region,
    strategy: ExtractStrategy,
    compression: Compression,
    direct_io: bool,
) -> Result<ExtractStats> {
    match strategy {
        ExtractStrategy::Simple => extract_simple(input, output, region, compression, direct_io),
        ExtractStrategy::CompleteWays => extract_complete_ways(input, output, region, compression, direct_io),
        ExtractStrategy::Smart => extract_smart(input, output, region, compression, direct_io),
    }
}

// ---------------------------------------------------------------------------
// Parallel batch infrastructure
// ---------------------------------------------------------------------------

const BATCH_SIZE: usize = 64;

fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) -> std::result::Result<(), String> {
    if let Some(bytes) = bb.take().map_err(|e| e.to_string())? {
        output.push(bytes.to_vec());
    }
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

// ---------------------------------------------------------------------------
// Simple strategy (two passes: streaming collection + parallel write)
// ---------------------------------------------------------------------------

fn extract_simple(input: &Path, output: &Path, region: &Region, compression: Compression, direct_io: bool) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    // --- Pass 1: Streaming collection (pipelined decode, sequential consumer) ---
    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    if region.contains_decimicro(&bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) {
                        bbox_node_ids.set(dn.id());
                    }
                }
                Element::Node(n) => {
                    if region.contains_decimicro(&bbox_int, n.decimicro_lat(), n.decimicro_lon()) {
                        bbox_node_ids.set(n.id());
                    }
                }
                Element::Way(w) => {
                    if w.refs().any(|r| bbox_node_ids.get(r)) {
                        matched_way_ids.set(w.id());
                    }
                }
                Element::Relation(r) => {
                    if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                        matched_relation_ids.set(r.id());
                    }
                }
            }
        }
    }

    // --- Pass 2: Parallel write (same pattern as complete-ways) ---
    let all_way_node_ids = IdSetDense::new(); // empty — simple doesn't include extra way nodes
    let reader = ElementReader::open(input, direct_io)?;
    let bbox = region.bbox();
    let header_bytes = HeaderBuilder::from_header(reader.header())
        .bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        .sorted()
        .build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= BATCH_SIZE {
            process_extract_pass2_batch(&batch, &ids, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(&batch, &ids, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Complete-ways strategy (two passes)
// ---------------------------------------------------------------------------

fn extract_complete_ways(input: &Path, output: &Path, region: &Region, compression: Compression, direct_io: bool) -> Result<ExtractStats> {
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
    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut all_way_node_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        collect_pass1_matches(
            &block,
            region,
            &bbox_int,
            &mut bbox_node_ids,
            &mut matched_way_ids,
            &mut all_way_node_ids,
            &mut matched_relation_ids,
        );
    }

    // --- Pass 2: Write matching elements in file order ---
    let reader = ElementReader::open(input, direct_io)?;
    let bbox = region.bbox();
    let header_bytes = HeaderBuilder::from_header(reader.header())
        .bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        .sorted()
        .build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= BATCH_SIZE {
            process_extract_pass2_batch(&batch, &ids, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(&batch, &ids, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    Ok(stats)
}

/// Collect matching element IDs during pass 1 of the complete-ways strategy.
///
/// This function is called once per PrimitiveBlock. Nodes in the region are
/// added to `bbox_node_ids`, ways with any node in the region are added to
/// `matched_way_ids` (and all their node refs to `all_way_node_ids`), and
/// relations referencing matched nodes/ways are added to `matched_relation_ids`.
///
/// All ID sets use `IdSetDense` for O(1) lookup and insertion with no sorting.
fn collect_pass1_matches(
    block: &crate::PrimitiveBlock,
    region: &Region,
    bbox_int: &BboxInt,
    bbox_node_ids: &mut IdSetDense,
    matched_way_ids: &mut IdSetDense,
    all_way_node_ids: &mut IdSetDense,
    matched_relation_ids: &mut IdSetDense,
) {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) {
                    bbox_node_ids.set(dn.id());
                }
            }
            Element::Node(n) => {
                if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) {
                    bbox_node_ids.set(n.id());
                }
            }
            Element::Way(w) => {
                if w.refs().any(|r| bbox_node_ids.get(r)) {
                    matched_way_ids.set(w.id());
                    for r in w.refs() {
                        all_way_node_ids.set(r);
                    }
                }
            }
            Element::Relation(r) => {
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) {
                    matched_relation_ids.set(r.id());
                }
            }
        }
    }
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
fn extract_block_pass2(
    block: &PrimitiveBlock,
    ids: &ExtractPass2IdSets<'_>,
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
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
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
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
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
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
                    if !bb.can_add_way() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.get(r.id()) {
                    if !bb.can_add_relation() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
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
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<Vec<u8>>, ExtractStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<Vec<u8>> = Vec::new();
                let block_stats = extract_block_pass2(block, ids, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge_extract_stats(stats, &block_stats);
        for block_bytes in &blocks {
            writer.write_primitive_block(block_bytes)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Smart strategy (three passes)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn extract_smart(
    input: &Path,
    output: &Path,
    region: &Region,
    compression: Compression,
    direct_io: bool,
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
    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut all_way_node_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();
    let mut extra_way_ids = IdSetDense::new();
    let mut extra_node_ids = IdSetDense::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        collect_pass1_smart(
            &block, region, &bbox_int,
            &mut bbox_node_ids, &mut matched_way_ids,
            &mut all_way_node_ids, &mut matched_relation_ids,
            &mut extra_way_ids, &mut extra_node_ids,
        );
    }

    // --- Pass 2: Resolve extra way node deps ---
    // For each way in extra_way_ids not already in matched_way_ids,
    // collect all node refs into extra_node_ids.
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements() {
            if let Element::Way(w) = &element {
                let wid = w.id();
                if extra_way_ids.get(wid) && !matched_way_ids.get(wid) {
                    for r in w.refs() {
                        extra_node_ids.set(r);
                    }
                }
            }
        }
    }

    // --- Pass 3: Write matching elements in file order ---
    let reader = ElementReader::open(input, direct_io)?;
    let bbox = region.bbox();
    let header_bytes = HeaderBuilder::from_header(reader.header())
        .bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        .sorted()
        .build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let ids = ExtractPass3IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        extra_node_ids: &extra_node_ids,
        matched_way_ids: &matched_way_ids,
        extra_way_ids: &extra_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= BATCH_SIZE {
            process_extract_pass3_batch(&batch, &ids, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass3_batch(&batch, &ids, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    Ok(stats)
}

/// Collect matching element IDs during pass 1 of the smart strategy.
///
/// Same as `collect_pass1_matches` but additionally collects extra way and
/// node IDs from matched multipolygon/boundary relations.
#[allow(clippy::too_many_arguments)]
fn collect_pass1_smart(
    block: &crate::PrimitiveBlock,
    region: &Region,
    bbox_int: &BboxInt,
    bbox_node_ids: &mut IdSetDense,
    matched_way_ids: &mut IdSetDense,
    all_way_node_ids: &mut IdSetDense,
    matched_relation_ids: &mut IdSetDense,
    extra_way_ids: &mut IdSetDense,
    extra_node_ids: &mut IdSetDense,
) {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) {
                    bbox_node_ids.set(dn.id());
                }
            }
            Element::Node(n) => {
                if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) {
                    bbox_node_ids.set(n.id());
                }
            }
            Element::Way(w) => {
                if w.refs().any(|r| bbox_node_ids.get(r)) {
                    matched_way_ids.set(w.id());
                    for r in w.refs() {
                        all_way_node_ids.set(r);
                    }
                }
            }
            Element::Relation(r) => {
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) {
                    matched_relation_ids.set(r.id());
                    // For multipolygon/boundary relations, collect all member
                    // IDs so their ways and nodes are fully included.
                    if is_smart_relation(r) {
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
    }
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
fn extract_block_pass3(
    block: &PrimitiveBlock,
    ids: &ExtractPass3IdSets<'_>,
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
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
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
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
                    if !bb.can_add_node() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
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
                    if !bb.can_add_way() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
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
                    if !bb.can_add_relation() {
                        flush_local(bb, output)?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
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
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<Vec<u8>>, ExtractStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<Vec<u8>> = Vec::new();
                let block_stats = extract_block_pass3(block, ids, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge_extract_stats(stats, &block_stats);
        for block_bytes in &blocks {
            writer.write_primitive_block(block_bytes)?;
        }
    }
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

use super::{dense_node_metadata, element_metadata};


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
