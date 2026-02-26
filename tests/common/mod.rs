// Each test binary includes this module but only uses a subset of helpers,
// so unused warnings are expected and harmless.
#![allow(dead_code)]
//! Shared test helpers used across integration tests.
//!
//! This module centralizes the duplicated helper structs and functions that were
//! previously copy-pasted across merge.rs, derive_changes.rs, extract.rs,
//! getid.rs, tags_filter.rs, diff.rs, and add_locations_to_ways.rs.
//!
//! There are two variants of PBF-reading helpers because some tests need node
//! coordinates (lat/lon) while others only need IDs and tags:
//!
//! - [`PbfContentsWithCoords`] / [`read_all_elements_with_coords`]: used by
//!   merge and derive_changes tests that verify coordinate values.
//! - [`PbfContentsIdOnly`] / [`read_all_elements_id_only`]: used by extract,
//!   getid, and tags_filter tests that only need element IDs and tags.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};

// ---------------------------------------------------------------------------
// Test element structs — lightweight descriptions of OSM elements for building
// test PBF files. These mirror the PBF data model but use static strings for
// convenience.
// ---------------------------------------------------------------------------

/// A test node with id, coordinates (in decimicrodegrees), and tags.
pub struct TestNode {
    pub id: i64,
    /// Latitude in decimicrodegrees (10^-7 degrees).
    pub lat: i32,
    /// Longitude in decimicrodegrees (10^-7 degrees).
    pub lon: i32,
    pub tags: Vec<(&'static str, &'static str)>,
}

/// A test way with id, node references, and tags.
pub struct TestWay {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Vec<(&'static str, &'static str)>,
}

/// A test relation with id, members, and tags.
///
/// Members use the [`TestMember`] struct which pairs a [`MemberId`] with a role
/// string.
pub struct TestRelation {
    pub id: i64,
    pub members: Vec<TestMember>,
    pub tags: Vec<(&'static str, &'static str)>,
}

/// A single member of a test relation: a typed member id plus a role string.
pub struct TestMember {
    pub id: MemberId,
    pub role: &'static str,
}

// ---------------------------------------------------------------------------
// PBF writing helper — builds a complete PBF file from test element slices.
// ---------------------------------------------------------------------------

/// Write a complete PBF file containing the given nodes, ways, and relations.
///
/// Creates a header blob followed by one or more primitive blocks. Elements are
/// written in order (all nodes, then all ways, then all relations) and the
/// builder is flushed between element types and whenever a block reaches its
/// capacity limit (8000 entities).
///
/// This is the canonical test PBF writer shared across most integration tests.
/// The `add_locations_to_ways` tests use a local variant because their
/// `TestRelation` type uses a different member representation.
pub fn write_test_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None, &[]).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Nodes
    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, &n.tags, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    // Ways
    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_way(w.id, &w.tags, &w.refs, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    // Relations
    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, &r.tags, &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

// ---------------------------------------------------------------------------
// PBF header reading
// ---------------------------------------------------------------------------

/// Read the header from a PBF file.
pub fn read_header(path: &Path) -> pbfhogg::HeaderBlock {
    let reader = BlobReader::from_path(path).expect("open pbf");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmHeader(header) = blob.decode().expect("decode blob") {
            return *header;
        }
    }
    panic!("no header found in PBF file");
}

// ---------------------------------------------------------------------------
// PBF reading helpers — "with coords" variant
// ---------------------------------------------------------------------------

/// Collected PBF elements including node coordinates. Used by merge and
/// derive_changes tests that need to verify lat/lon values.
///
/// Tuple layouts:
/// - nodes: `(id, lat, lon, tags)`
/// - ways: `(id, refs, tags)`
/// - relations: `(id, members_as_(id, type_str, role), tags)`
#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct PbfContentsWithCoords {
    pub nodes: Vec<(i64, i32, i32, Vec<(String, String)>)>,
    pub ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    pub relations: Vec<(i64, Vec<(i64, String, String)>, Vec<(String, String)>)>,
}

/// Read all elements from a PBF file, preserving node coordinates.
///
/// Handles both `DenseNode` and `Node` element variants. Returns a
/// [`PbfContentsWithCoords`] with all elements in file order.
pub fn read_all_elements_with_coords(path: &Path) -> PbfContentsWithCoords {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = PbfContentsWithCoords {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let tags: Vec<(String, String)> = dn
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents
                            .nodes
                            .push((dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), tags));
                    }
                    Element::Node(n) => {
                        let tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents
                            .nodes
                            .push((n.id(), n.decimicro_lat(), n.decimicro_lon(), tags));
                    }
                    Element::Way(w) => {
                        let tags: Vec<(String, String)> =
                            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> =
                            r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        let members: Vec<(i64, String, String)> = r
                            .members()
                            .map(|m| {
                                let type_str = match m.id.member_type() {
                                    pbfhogg::MemberType::Node => "node",
                                    pbfhogg::MemberType::Way => "way",
                                    pbfhogg::MemberType::Relation => "relation",
                                    pbfhogg::MemberType::Unknown(_) => "unknown",
                                    _ => "unknown",
                                };
                                (
                                    m.id.id(),
                                    type_str.to_string(),
                                    m.role().unwrap_or("").to_string(),
                                )
                            })
                            .collect();
                        contents.relations.push((r.id(), members, tags));
                    }
                    _ => {}
                }
            }
        }
    }

    contents
}

/// Extract just the node IDs from a [`PbfContentsWithCoords`].
pub fn node_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.nodes.iter().map(|(id, _, _, _)| *id).collect()
}

/// Extract just the way IDs from a [`PbfContentsWithCoords`].
pub fn way_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

/// Extract just the relation IDs from a [`PbfContentsWithCoords`].
pub fn relation_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.relations.iter().map(|(id, _, _)| *id).collect()
}

// ---------------------------------------------------------------------------
// PBF reading helpers — "id only" variant (no coordinates on nodes)
// ---------------------------------------------------------------------------

/// Collected PBF elements without node coordinates. Used by extract, getid,
/// and tags_filter tests that only need IDs and tags.
///
/// Tuple layouts:
/// - nodes: `(id, tags)`
/// - ways: `(id, refs, tags)`
/// - relations: `(id, tags)`
#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct PbfContentsIdOnly {
    pub nodes: Vec<(i64, Vec<(String, String)>)>,
    pub ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    pub relations: Vec<(i64, Vec<(String, String)>)>,
}

/// Read all elements from a PBF file, discarding node coordinates.
///
/// Handles both `DenseNode` and `Node` element variants. Returns a
/// [`PbfContentsIdOnly`] with all elements in file order.
pub fn read_all_elements_id_only(path: &Path) -> PbfContentsIdOnly {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = PbfContentsIdOnly {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let tags: Vec<(String, String)> = dn
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((dn.id(), tags));
                    }
                    Element::Node(n) => {
                        let tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((n.id(), tags));
                    }
                    Element::Way(w) => {
                        let tags: Vec<(String, String)> =
                            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> =
                            r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        contents.relations.push((r.id(), tags));
                    }
                    _ => {}
                }
            }
        }
    }

    contents
}

/// Extract just the node IDs from a [`PbfContentsIdOnly`].
pub fn node_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.nodes.iter().map(|(id, _)| *id).collect()
}

/// Extract just the way IDs from a [`PbfContentsIdOnly`].
pub fn way_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

/// Extract just the relation IDs from a [`PbfContentsIdOnly`].
pub fn relation_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.relations.iter().map(|(id, _)| *id).collect()
}
