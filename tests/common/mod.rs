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
//!
//! [`cli`] hosts [`CliInvoker`] for tests that drive the pbfhogg
//! binary via `std::process::Command` instead of calling into the
//! library directly.
//!
//! [`CliInvoker`]: cli::CliInvoker

pub mod adversarial;
pub mod cli;

use std::collections::BTreeMap;
use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, ElementReader, MemberId};

// ---------------------------------------------------------------------------
// Test element structs - lightweight descriptions of OSM elements for building
// test PBF files. These mirror the PBF data model but use static strings for
// convenience.
// ---------------------------------------------------------------------------

/// Optional test-side element metadata. Every field mirrors
/// [`pbfhogg::block_builder::Metadata`] but uses `&'static str` for `user` so
/// struct literals stay cheap. `Default` gives a valid, stable set of values
/// suitable for tests that just want "some metadata" (version 1, epoch 0,
/// changeset 0, uid 0, user "", visible).
#[derive(Clone, Copy, Debug)]
pub struct TestMeta {
    pub version: i32,
    pub timestamp: i64,
    pub changeset: i64,
    pub uid: i32,
    pub user: &'static str,
    pub visible: bool,
}

impl Default for TestMeta {
    fn default() -> Self {
        Self {
            version: 1,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        }
    }
}

fn to_block_meta(m: &TestMeta) -> block_builder::Metadata<'_> {
    block_builder::Metadata {
        version: m.version,
        timestamp: m.timestamp,
        changeset: m.changeset,
        uid: m.uid,
        user: m.user,
        visible: m.visible,
    }
}

/// A test node with id, coordinates (in decimicrodegrees), tags, and
/// optional metadata.
pub struct TestNode {
    pub id: i64,
    /// Latitude in decimicrodegrees (10^-7 degrees).
    pub lat: i32,
    /// Longitude in decimicrodegrees (10^-7 degrees).
    pub lon: i32,
    pub tags: Vec<(&'static str, &'static str)>,
    /// Optional per-element metadata (version/timestamp/changeset/uid/user/visible).
    /// `None` writes a node with no OSM metadata - the default for most tests.
    pub meta: Option<TestMeta>,
}

/// A test way with id, node references, tags, and optional metadata.
pub struct TestWay {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Vec<(&'static str, &'static str)>,
    pub meta: Option<TestMeta>,
}

/// A test relation with id, members, tags, and optional metadata.
///
/// Members use the [`TestMember`] struct which pairs a [`MemberId`] with a role
/// string.
pub struct TestRelation {
    pub id: i64,
    pub members: Vec<TestMember>,
    pub tags: Vec<(&'static str, &'static str)>,
    pub meta: Option<TestMeta>,
}

/// A single member of a test relation: a typed member id plus a role string.
pub struct TestMember {
    pub id: MemberId,
    pub role: &'static str,
}

// ---------------------------------------------------------------------------
// PBF writing helper - builds a complete PBF file from test element slices.
// ---------------------------------------------------------------------------

/// Write a complete PBF file containing the given nodes, ways, and relations.
///
/// Creates a header blob followed by one or more primitive blocks. Elements are
/// written in order (all nodes, then all ways, then all relations) and the
/// builder is flushed between element types and whenever a block reaches its
/// capacity limit (8000 entities by default).
///
/// When `sorted` is true, the header declares `Sort.Type_then_ID`.
///
/// `block_size_override` forces a block flush every N elements within each
/// type section instead of waiting for the 8000-entity cap. Use this to
/// produce multi-block fixtures that exercise batch boundaries, blob
/// classification, and passthrough coalescing without allocating thousands
/// of elements. `None` keeps the natural 8000/block behaviour.
///
/// This is the canonical test PBF writer shared across most integration tests.
/// The `add_locations_to_ways` tests use a local variant because their
/// `TestRelation` type uses a different member representation.
pub fn write_test_pbf_impl(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    sorted: bool,
    block_size_override: Option<usize>,
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let mut hb = block_builder::HeaderBuilder::new();
    if sorted {
        hb = hb.sorted();
    }
    let header = hb.build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    let mut batch_count = 0usize;

    // Nodes
    for n in nodes {
        let hit_override = matches!(block_size_override, Some(s) if batch_count >= s);
        if (!bb.can_add_node() || hit_override)
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
            batch_count = 0;
        }
        let meta = n.meta.as_ref().map(to_block_meta);
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), meta.as_ref());
        batch_count += 1;
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }
    batch_count = 0;

    // Ways
    for w in ways {
        let hit_override = matches!(block_size_override, Some(s) if batch_count >= s);
        if (!bb.can_add_way() || hit_override)
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
            batch_count = 0;
        }
        let meta = w.meta.as_ref().map(to_block_meta);
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, meta.as_ref());
        batch_count += 1;
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }
    batch_count = 0;

    // Relations
    for r in relations {
        let hit_override = matches!(block_size_override, Some(s) if batch_count >= s);
        if (!bb.can_add_relation() || hit_override)
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
            batch_count = 0;
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        let meta = r.meta.as_ref().map(to_block_meta);
        bb.add_relation(r.id, r.tags.iter().copied(), &members, meta.as_ref());
        batch_count += 1;
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// Write a test PBF without the sorted header flag. See [`write_test_pbf_impl`].
pub fn write_test_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    write_test_pbf_impl(path, nodes, ways, relations, false, None);
}

/// Write a test PBF with `Sort.Type_then_ID` header flag. See [`write_test_pbf_impl`].
pub fn write_test_pbf_sorted(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    write_test_pbf_impl(path, nodes, ways, relations, true, None);
}

/// Write a sorted test PBF that forces a block flush every `block_size`
/// elements within each type section. Produces a multi-block fixture with
/// a small, predictable number of blobs, useful for exercising batch
/// boundaries, blob classification, passthrough coalescing, and
/// merge-join block-pair logic without needing ~8000+ elements per type.
///
/// Example: `write_multi_block_test_pbf(path, &nodes_100, &ways_20, &rels_5, 25)`
/// lays the nodes out as four blocks of 25 (or 24+24+24+28 tail), ways as one
/// block, relations as one block - seven data blobs total.
pub fn write_multi_block_test_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    block_size: usize,
) {
    assert!(block_size > 0, "block_size must be > 0");
    write_test_pbf_impl(path, nodes, ways, relations, true, Some(block_size));
}

/// Write a sorted test PBF whose OsmData blobs lack the `indexdata`
/// `BlobHeader` field. Use this to exercise read paths (most visibly
/// `diff_element_stream` in `commands::diff`) that only fire when one
/// or both inputs are non-indexed. The `PrimitiveBlock` payload is
/// identical to `write_test_pbf_sorted`; only the outer blob header
/// differs. The header declares `Sort.Type_then_ID` so consumers that
/// require sorted inputs still accept the output.
pub fn write_test_pbf_non_indexed(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
        let meta = n.meta.as_ref().map(to_block_meta);
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), meta.as_ref());
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write block");
    }

    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
        let meta = w.meta.as_ref().map(to_block_meta);
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, meta.as_ref());
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        let meta = r.meta.as_ref().map(to_block_meta);
        bb.add_relation(r.id, r.tags.iter().copied(), &members, meta.as_ref());
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write block");
    }

    writer.flush().expect("flush");
}

// ---------------------------------------------------------------------------
// Fixture generation helpers
// ---------------------------------------------------------------------------
//
// These produce sequential, id-sorted element vectors intended to be paired
// with `write_multi_block_test_pbf` when a test wants to exercise multi-blob
// paths without hand-writing dozens of literals. Coordinates are chosen to
// stay inside the WGS84 range and to produce visibly distinct lat/lon per
// element so failures aren't hidden by coordinate collisions.

/// Generate `count` nodes with ids `start_id..start_id+count`.
///
/// Latitudes and longitudes are a deterministic function of **id**, not
/// of the position in the returned vector: `lat = lon = 1000 * (id %
/// 10_000)` decimicrodegrees. Keying off id means two `generate_nodes`
/// calls that produce the same id (even with different `count` /
/// `start_id` windows) agree on the coord - important for tests that
/// splice overlapping id ranges between old/new fixtures and rely on
/// element-equality semantics (diff, merge). Coordinates wrap every
/// 10 000 ids to stay inside the WGS84 range for arbitrarily large
/// inputs. No tags, no metadata - map over the result when a test
/// needs specific tags.
pub fn generate_nodes(count: usize, start_id: i64) -> Vec<TestNode> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("node count fits in i64");
            let id = start_id + id_offset;
            // decimicrodegrees = 1e-7 degrees, so 1e-4° = 1000 decimicrodegrees.
            let step = 1000_i32;
            let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
            let coord = step.saturating_mul(wrap);
            TestNode {
                id,
                lat: coord,
                lon: coord,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `count` ways with ids `start_id..start_id+count`, each
/// referencing `refs_per_way` consecutive node ids starting at
/// `node_start_id`. Useful for testing blob classification on ways whose
/// refs point into a specific node-id range.
pub fn generate_ways(
    count: usize,
    start_id: i64,
    refs_per_way: usize,
    node_start_id: i64,
) -> Vec<TestWay> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("way count fits in i64");
            let refs: Vec<i64> = (0..refs_per_way)
                .map(|r| {
                    let r = i64::try_from(r).expect("refs_per_way fits in i64");
                    node_start_id + r
                })
                .collect();
            TestWay {
                id: start_id + id_offset,
                refs,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `count` relations with ids `start_id..start_id+count`, each
/// containing `members_per_rel` way members starting at `way_start_id`
/// with the role `"outer"`.
pub fn generate_relations(
    count: usize,
    start_id: i64,
    members_per_rel: usize,
    way_start_id: i64,
) -> Vec<TestRelation> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("relation count fits in i64");
            let members: Vec<TestMember> = (0..members_per_rel)
                .map(|m| {
                    let m = i64::try_from(m).expect("members_per_rel fits in i64");
                    TestMember {
                        id: MemberId::Way(way_start_id + m),
                        role: "outer",
                    }
                })
                .collect();
            TestRelation {
                id: start_id + id_offset,
                members,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `n_neg + n_pos` nodes with mixed-sign ids in canonical OSM
/// order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Zero is not a valid
/// OSM id and is omitted. Coordinates and tags follow the
/// [`generate_nodes`] pattern.
///
/// Canonical OSM order is `0, -1, -2, ...` (negatives by ascending
/// absolute value) `then 1, 2, ...` (positives ascending) - see
/// `osm_id_cmp` in `src/osm_id.rs`. Used by the negative-ID sweep
/// (`cli_negative_id_invariants.rs`) to drive every command through
/// a mixed-sign fixture in the order canonical-sorted readers expect.
pub fn generate_nodes_with_negatives(n_neg: usize, n_pos: usize) -> Vec<TestNode> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        let step = 1000_i32;
        let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
        let coord = step.saturating_mul(wrap);
        out.push(TestNode {
            id,
            lat: coord,
            lon: coord,
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        let step = 1000_i32;
        let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
        let coord = step.saturating_mul(wrap);
        out.push(TestNode {
            id,
            lat: coord,
            lon: coord,
            tags: vec![],
            meta: None,
        });
    }
    out
}

/// Generate `n_neg + n_pos` ways with mixed-sign ids in canonical OSM
/// order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Each way references
/// `refs_per_way` consecutive positive node ids starting at `1` -
/// callers that want way refs targeting negative nodes can layer them
/// on after.
pub fn generate_ways_with_negatives(
    n_neg: usize,
    n_pos: usize,
    refs_per_way: usize,
) -> Vec<TestWay> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    let make_refs = |seed: i64| -> Vec<i64> {
        (0..refs_per_way)
            .map(|r| {
                let r = i64::try_from(r).expect("refs_per_way fits in i64");
                seed.saturating_add(r)
            })
            .collect()
    };
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        out.push(TestWay {
            id,
            refs: make_refs(1),
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        out.push(TestWay {
            id,
            refs: make_refs(1),
            tags: vec![],
            meta: None,
        });
    }
    out
}

/// Generate `n_neg + n_pos` relations with mixed-sign ids in canonical
/// OSM order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Each relation
/// has `members_per_rel` way members with role `"outer"`, referencing
/// positive way ids starting at `1`.
pub fn generate_relations_with_negatives(
    n_neg: usize,
    n_pos: usize,
    members_per_rel: usize,
) -> Vec<TestRelation> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    let make_members = || -> Vec<TestMember> {
        (0..members_per_rel)
            .map(|m| {
                let m = i64::try_from(m).expect("members_per_rel fits in i64");
                TestMember {
                    id: MemberId::Way(1 + m),
                    role: "outer",
                }
            })
            .collect()
    };
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        out.push(TestRelation {
            id,
            members: make_members(),
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        out.push(TestRelation {
            id,
            members: make_members(),
            tags: vec![],
            meta: None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Indexdata assertions
// ---------------------------------------------------------------------------

/// Assert the PBF's first OsmData blob carries indexdata. Matches the
/// representativeness assumption of `pbfhogg::has_indexdata`: the first
/// data blob is trusted to speak for the rest of the file. `write_test_pbf`
/// and friends always emit indexed blobs, so this is a fast smoke check
/// that a helper hasn't accidentally dropped indexdata.
pub fn assert_indexed(path: &Path) {
    let indexed = pbfhogg::has_indexdata(path, false).expect("probe indexdata");
    assert!(
        indexed,
        "expected {} to carry indexdata on its first data blob",
        path.display()
    );
}

/// Assert the PBF's first OsmData blob has no indexdata. Pair with
/// `write_test_pbf_non_indexed` (see the non-indexed writer helper) to
/// confirm the blob really landed without the indexdata field.
pub fn assert_non_indexed(path: &Path) {
    let indexed = pbfhogg::has_indexdata(path, false).expect("probe indexdata");
    assert!(
        !indexed,
        "expected {} to have no indexdata on its first data blob",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// PBF header reading
// ---------------------------------------------------------------------------

/// Read the header from a PBF file.
pub fn read_header(path: &Path) -> pbfhogg::HeaderBlock {
    let reader = ElementReader::from_path(path).expect("open pbf");
    reader.header().clone()
}

// ---------------------------------------------------------------------------
// PBF reading helpers - "with coords" variant
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
                        contents.nodes.push((
                            dn.id(),
                            dn.decimicro_lat(),
                            dn.decimicro_lon(),
                            tags,
                        ));
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
                        let tags: Vec<(String, String)> = w
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> = r
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
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
// PBF reading helpers - "id only" variant (no coordinates on nodes)
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
                        let tags: Vec<(String, String)> = w
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> = r
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
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

// ---------------------------------------------------------------------------
// PBF reading helpers - "normalized" variant for element-equivalence cross-checks
// ---------------------------------------------------------------------------
//
// Used by tests that need to assert two PBFs contain the same elements without
// requiring byte-identical output. Byte-identical comparison is too strict: two
// semantically-equivalent PBFs can differ in string-table ordering, DenseNodes
// delta packing, and block flush boundaries without any actual element
// difference. The normalized form captures what callers actually care about:
//
// - ID, coordinates (nodes), tags as a BTreeMap (order-insensitive), refs/
//   members as a Vec (order-sensitive, since OSM semantics depend on it),
//   metadata per element.
//
// Each type section is sorted by id so two PBFs with the same element set but
// different file orderings compare equal. Used by the renumber external tests
// to cross-check against the in-memory path.

/// Normalized element metadata. Matches `pbfhogg::block_builder::Metadata`
/// shape with owned strings. `None` means the element has no metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedMeta {
    pub version: i32,
    pub timestamp: i64,
    pub changeset: i64,
    pub uid: i32,
    pub user: String,
    pub visible: bool,
}

/// Normalized node: id + coords + tags (order-insensitive) + metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedNode {
    pub id: i64,
    pub lat: i32,
    pub lon: i32,
    pub tags: BTreeMap<String, String>,
    pub meta: Option<NormalizedMeta>,
}

/// Normalized way: id + refs (order-sensitive) + tags + metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedWay {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
    pub meta: Option<NormalizedMeta>,
}

/// Normalized relation member: type + ref id + role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedMember {
    pub member_type: String,
    pub ref_id: i64,
    pub role: String,
}

/// Normalized relation: id + members (order-sensitive) + tags + metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRelation {
    pub id: i64,
    pub members: Vec<NormalizedMember>,
    pub tags: BTreeMap<String, String>,
    pub meta: Option<NormalizedMeta>,
}

/// Normalized view of a complete PBF file. Each section sorted by id.
#[derive(Debug)]
pub struct NormalizedPbf {
    pub nodes: Vec<NormalizedNode>,
    pub ways: Vec<NormalizedWay>,
    pub relations: Vec<NormalizedRelation>,
}

/// Build a `NormalizedMeta` from a Node/Way/Relation `Info`. Returns `None`
/// when the info has no version (i.e. no metadata block was present, matching
/// `commands::element_metadata`).
fn meta_from_info(info: &pbfhogg::Info<'_>) -> Option<NormalizedMeta> {
    info.version().map(|v| NormalizedMeta {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or("")
            .to_string(),
        visible: info.visible(),
    })
}

/// Build a `NormalizedMeta` from a `DenseNode`. Returns `None` when no info
/// block is present (matching `commands::dense_node_metadata`).
fn meta_from_dense_node(dn: &pbfhogg::DenseNode<'_>) -> Option<NormalizedMeta> {
    dn.info()
        .filter(|i| i.version() != -1)
        .map(|i| NormalizedMeta {
            version: i.version(),
            timestamp: i.milli_timestamp() / 1000,
            changeset: i.changeset(),
            uid: i.uid(),
            user: i.user().unwrap_or("").to_string(),
            visible: i.visible(),
        })
}

/// Convert a `Node`/`DenseNode` into a `NormalizedNode`.
fn normalize_node(n: &pbfhogg::Node<'_>) -> NormalizedNode {
    let tags: BTreeMap<String, String> = n
        .tags()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    NormalizedNode {
        id: n.id(),
        lat: n.decimicro_lat(),
        lon: n.decimicro_lon(),
        tags,
        meta: meta_from_info(&n.info()),
    }
}

fn normalize_dense_node(dn: &pbfhogg::DenseNode<'_>) -> NormalizedNode {
    let tags: BTreeMap<String, String> = dn
        .tags()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    NormalizedNode {
        id: dn.id(),
        lat: dn.decimicro_lat(),
        lon: dn.decimicro_lon(),
        tags,
        meta: meta_from_dense_node(dn),
    }
}

fn normalize_way(w: &pbfhogg::Way<'_>) -> NormalizedWay {
    let tags: BTreeMap<String, String> = w
        .tags()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    NormalizedWay {
        id: w.id(),
        refs: w.refs().collect(),
        tags,
        meta: meta_from_info(&w.info()),
    }
}

fn normalize_relation(r: &pbfhogg::Relation<'_>) -> NormalizedRelation {
    let tags: BTreeMap<String, String> = r
        .tags()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let members: Vec<NormalizedMember> = r
        .members()
        .map(|m| {
            let member_type = match m.id.member_type() {
                pbfhogg::MemberType::Node => "node",
                pbfhogg::MemberType::Way => "way",
                pbfhogg::MemberType::Relation => "relation",
                _ => "unknown",
            }
            .to_string();
            NormalizedMember {
                member_type,
                ref_id: m.id.id(),
                role: m.role().unwrap_or("").to_string(),
            }
        })
        .collect();
    NormalizedRelation {
        id: r.id(),
        members,
        tags,
        meta: meta_from_info(&r.info()),
    }
}

/// Read a PBF file into its normalized, element-equivalence form.
///
/// Both `DenseNode` and `Node` element variants are coalesced into the same
/// `NormalizedNode` shape so the serialization choice doesn't affect
/// comparison. Each section is sorted by id on return.
pub fn read_normalized(path: &Path) -> NormalizedPbf {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = NormalizedPbf {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => contents.nodes.push(normalize_dense_node(&dn)),
                    Element::Node(n) => contents.nodes.push(normalize_node(&n)),
                    Element::Way(w) => contents.ways.push(normalize_way(&w)),
                    Element::Relation(r) => contents.relations.push(normalize_relation(&r)),
                    _ => {}
                }
            }
        }
    }

    contents.nodes.sort_by_key(|n| n.id);
    contents.ways.sort_by_key(|w| w.id);
    contents.relations.sort_by_key(|r| r.id);
    contents
}

/// Assert that a PBF file is sorted by `Sort.Type_then_ID`:
///
/// - The header block declares `is_sorted() == true`.
/// - Elements appear in file order as nodes → ways → relations (no
///   out-of-order type transitions).
/// - Within each type, ids are monotonically non-decreasing.
///
/// `read_normalized` sorts sections internally, so bugs that emit
/// elements in the wrong file order can't be caught by element-
/// equivalence alone. This helper reads the file in raw order via
/// `read_all_elements_with_coords` (which preserves blob order) and
/// asserts the sortedness contract.
pub fn assert_sorted_file(path: &Path) {
    // Header flag check.
    let header = read_header(path);
    assert!(
        header.is_sorted(),
        "PBF header is not declared sorted (Sort.Type_then_ID missing) for {}",
        path.display()
    );

    // File-order contents via the non-normalized reader.
    let contents = read_all_elements_with_coords(path);

    // Within each section, ids must be monotonically non-decreasing.
    // (The reader already walks per-type sections in file order, so
    // this also implicitly asserts no out-of-type interleavings within
    // a section.)
    let mut last = i64::MIN;
    for (id, _, _, _) in &contents.nodes {
        assert!(
            *id >= last,
            "node ids not sorted in {}: {} followed by {}",
            path.display(),
            last,
            id
        );
        last = *id;
    }
    let mut last = i64::MIN;
    for (id, _, _) in &contents.ways {
        assert!(
            *id >= last,
            "way ids not sorted in {}: {} followed by {}",
            path.display(),
            last,
            id
        );
        last = *id;
    }
    let mut last = i64::MIN;
    for (id, _, _) in &contents.relations {
        assert!(
            *id >= last,
            "relation ids not sorted in {}: {} followed by {}",
            path.display(),
            last,
            id
        );
        last = *id;
    }
}

/// Assert that two PBF files are element-equivalent.
///
/// Reads both via `read_normalized` and compares section-by-section. On
/// mismatch, panics with the standard `assert_eq!` pretty-printed diff so the
/// first diverging element is visible. Compares sizes separately from
/// contents so a count mismatch surfaces before an iterator zip stops early.
pub fn assert_elements_equivalent(path_a: &Path, path_b: &Path) {
    let a = read_normalized(path_a);
    let b = read_normalized(path_b);

    assert_eq!(
        a.nodes.len(),
        b.nodes.len(),
        "node count differs: {} vs {}",
        a.nodes.len(),
        b.nodes.len()
    );
    assert_eq!(
        a.ways.len(),
        b.ways.len(),
        "way count differs: {} vs {}",
        a.ways.len(),
        b.ways.len()
    );
    assert_eq!(
        a.relations.len(),
        b.relations.len(),
        "relation count differs: {} vs {}",
        a.relations.len(),
        b.relations.len()
    );

    for (i, (na, nb)) in a.nodes.iter().zip(b.nodes.iter()).enumerate() {
        assert_eq!(na, nb, "node at sorted index {i} differs");
    }
    for (i, (wa, wb)) in a.ways.iter().zip(b.ways.iter()).enumerate() {
        assert_eq!(wa, wb, "way at sorted index {i} differs");
    }
    for (i, (ra, rb)) in a.relations.iter().zip(b.relations.iter()).enumerate() {
        assert_eq!(ra, rb, "relation at sorted index {i} differs");
    }
}

// ---------------------------------------------------------------------------
// Error helpers for platform-specific I/O tests
// ---------------------------------------------------------------------------

/// Check if an error is EINVAL (used to skip O_DIRECT tests on unsupported filesystems).
#[cfg(feature = "linux-direct-io")]
pub fn is_einval(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        return io_err.raw_os_error() == Some(libc::EINVAL);
    }
    if let Some(pbf_err) = err.downcast_ref::<pbfhogg::Error>()
        && let pbfhogg::ErrorKind::Io(io_err) = pbf_err.kind()
    {
        return io_err.raw_os_error() == Some(libc::EINVAL);
    }
    false
}

/// Check if an error indicates io_uring is unavailable.
#[cfg(feature = "linux-io-uring")]
pub fn is_uring_unavailable(err: &(dyn std::error::Error + 'static)) -> bool {
    if err.downcast_ref::<std::io::Error>().is_some() {
        return true;
    }
    if let Some(pbf_err) = err.downcast_ref::<pbfhogg::Error>() {
        return matches!(pbf_err.kind(), pbfhogg::ErrorKind::Io(_));
    }
    false
}

// ---------------------------------------------------------------------------
// Scratch-dir tracking (fault-injection test support).
//
// Snapshot a directory's recursive contents before and after a command
// runs; the test asserts no leaked temp files on the error path. Pairs
// with the `test-hooks` fault-injection harness in the pipeline modules.
// ---------------------------------------------------------------------------

/// Recursive listing of all regular-file paths under `root`, relative
/// to `root`. Directories are included as keys in the set so a removed
/// empty subdir also shows up in the diff.
pub fn snapshot_dir(root: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    snapshot_dir_inner(root, root, &mut out);
    out
}

fn snapshot_dir_inner(
    base: &Path,
    dir: &Path,
    out: &mut std::collections::BTreeSet<std::path::PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        out.insert(rel);
        if path.is_dir() {
            snapshot_dir_inner(base, &path, out);
        }
    }
}

/// Assert that a post-run scratch snapshot matches the pre-run snapshot.
/// Panics with a diff listing added / removed entries if not equal.
/// Use around commands that manage scratch files internally: any
/// difference indicates a leaked temp file on the test's execution
/// path (typically an error-path leak the cleanup rule missed).
pub fn assert_scratch_unchanged(
    before: &std::collections::BTreeSet<std::path::PathBuf>,
    after: &std::collections::BTreeSet<std::path::PathBuf>,
) {
    let added: Vec<_> = after.difference(before).collect();
    let removed: Vec<_> = before.difference(after).collect();
    if !added.is_empty() || !removed.is_empty() {
        panic!(
            "scratch directory changed across command invocation\n\
             added (leaked): {added:?}\n\
             removed (stolen): {removed:?}"
        );
    }
}

/// Run a closure inside a tracked scratch directory.
///
/// Snapshots `scratch_root` before and after the closure runs, removes
/// every path in `expected_new_paths` from the post-snapshot, and panics
/// if any other paths were added or removed. Returns the closure result.
///
/// Use this to wrap a command invocation that produces known outputs
/// (relative paths under `scratch_root`) but should not leak any
/// intermediate scratch files.
///
/// ```ignore
/// with_tracked_scratch_dir(dir.path(), &[Path::new("output.osm.pbf")], || {
///     CliInvoker::new()
///         .arg("sort")
///         .arg(&input)
///         .arg("-o")
///         .arg(&output)
///         .assert_success();
/// });
/// ```
pub fn with_tracked_scratch_dir<F, R>(scratch_root: &Path, expected_new_paths: &[&Path], f: F) -> R
where
    F: FnOnce() -> R,
{
    let before = snapshot_dir(scratch_root);
    let result = f();
    let mut after = snapshot_dir(scratch_root);
    for ex in expected_new_paths {
        after.remove(*ex);
    }
    assert_scratch_unchanged(&before, &after);
    result
}
