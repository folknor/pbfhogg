//! Extract elements within a geographic bounding box. Equivalent to `osmium extract`.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::block_builder::{build_header, BlockBuilder, MemberData, Metadata};
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

/// Extract elements within `bbox` from `input` and write to `output`.
///
/// If `simple` is true, uses a single-pass strategy (fast but ways may reference
/// nodes outside the extract). Otherwise uses `complete_ways` (two passes, all
/// nodes of matching ways are included).
pub fn extract(input: &Path, output: &Path, bbox: &Bbox, simple: bool) -> Result<ExtractStats> {
    if simple {
        extract_simple(input, output, bbox)
    } else {
        extract_complete_ways(input, output, bbox)
    }
}

// ---------------------------------------------------------------------------
// Simple strategy (single pass)
// ---------------------------------------------------------------------------

fn extract_simple(input: &Path, output: &Path, bbox: &Bbox) -> Result<ExtractStats> {
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

    let mut matched_node_ids: BTreeSet<i64> = BTreeSet::new();
    let mut matched_way_ids: BTreeSet<i64> = BTreeSet::new();

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    write_extract_header(bbox, &header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            if bbox.contains(dn.lat(), dn.lon()) {
                                matched_node_ids.insert(dn.id());
                                write_dense_node(dn, &mut bb, &mut writer)?;
                                stats.nodes_in_bbox += 1;
                            }
                        }
                        Element::Node(n) => {
                            if bbox.contains(n.lat(), n.lon()) {
                                matched_node_ids.insert(n.id());
                                write_node(n, &mut bb, &mut writer)?;
                                stats.nodes_in_bbox += 1;
                            }
                        }
                        Element::Way(w) => {
                            if w.refs().any(|r| matched_node_ids.contains(&r)) {
                                matched_way_ids.insert(w.id());
                                write_way(w, &mut bb, &mut writer)?;
                                stats.ways_written += 1;
                            }
                        }
                        Element::Relation(r) => {
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

fn extract_complete_ways(input: &Path, output: &Path, bbox: &Bbox) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        ways_written: 0,
        relations_written: 0,
        strategy: "complete_ways",
    };

    // --- Pass 1: Collect matches ---
    let mut bbox_node_ids: BTreeSet<i64> = BTreeSet::new();
    let mut matched_way_ids: BTreeSet<i64> = BTreeSet::new();
    let mut all_way_node_ids: BTreeSet<i64> = BTreeSet::new();
    let mut matched_relation_ids: BTreeSet<i64> = BTreeSet::new();

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(_) => {}
            BlobDecode::OsmData(block) => {
                collect_pass1_matches(
                    &block,
                    bbox,
                    &mut bbox_node_ids,
                    &mut matched_way_ids,
                    &mut all_way_node_ids,
                    &mut matched_relation_ids,
                );
            }
            BlobDecode::Unknown(_) => {}
        }
    }

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
                    write_extract_header(bbox, &header, &mut writer)?;
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

fn collect_pass1_matches(
    block: &crate::PrimitiveBlock,
    bbox: &Bbox,
    bbox_node_ids: &mut BTreeSet<i64>,
    matched_way_ids: &mut BTreeSet<i64>,
    all_way_node_ids: &mut BTreeSet<i64>,
    matched_relation_ids: &mut BTreeSet<i64>,
) {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                if bbox.contains(dn.lat(), dn.lon()) {
                    bbox_node_ids.insert(dn.id());
                }
            }
            Element::Node(n) => {
                if bbox.contains(n.lat(), n.lon()) {
                    bbox_node_ids.insert(n.id());
                }
            }
            Element::Way(w) => {
                if w.refs().any(|r| bbox_node_ids.contains(&r)) {
                    matched_way_ids.insert(w.id());
                    all_way_node_ids.extend(w.refs());
                }
            }
            Element::Relation(r) => {
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) {
                    matched_relation_ids.insert(r.id());
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_pass2_elements(
    block: &crate::PrimitiveBlock,
    bbox_node_ids: &BTreeSet<i64>,
    all_way_node_ids: &BTreeSet<i64>,
    matched_way_ids: &BTreeSet<i64>,
    matched_relation_ids: &BTreeSet<i64>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
    stats: &mut ExtractStats,
) -> Result<()> {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let in_bbox = bbox_node_ids.contains(&dn.id());
                let from_way = all_way_node_ids.contains(&dn.id());
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
                let in_bbox = bbox_node_ids.contains(&n.id());
                let from_way = all_way_node_ids.contains(&n.id());
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
                if matched_way_ids.contains(&w.id()) {
                    write_way(w, bb, writer)?;
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                if matched_relation_ids.contains(&r.id()) {
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

fn relation_has_matched_member(
    r: &crate::Relation,
    node_ids: &BTreeSet<i64>,
    way_ids: &BTreeSet<i64>,
) -> bool {
    r.members().any(|m| match m.id {
        MemberId::Node(id) => node_ids.contains(&id),
        MemberId::Way(id) => way_ids.contains(&id),
        MemberId::Relation(_) => false,
    })
}

// ---------------------------------------------------------------------------
// Element writers
// ---------------------------------------------------------------------------

fn write_dense_node(
    dn: &crate::DenseNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
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
    writer: &mut PbfWriter<io::BufWriter<File>>,
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
    writer: &mut PbfWriter<io::BufWriter<File>>,
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
    writer: &mut PbfWriter<io::BufWriter<File>>,
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
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}

fn write_extract_header(
    bbox: &Bbox,
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    let header_bytes = build_header(
        Some((bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)),
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
}
