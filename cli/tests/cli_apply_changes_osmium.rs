//! Cross-validation of `apply-changes --locations-on-ways` against
//! `osmium apply-changes --locations-on-ways`.
//!
//! **Escape hatch**, `#[ignore = "external"]`d like the other in-tree osmium
//! checks (`reference/testing.md` "External cross-validation"): it skips and
//! passes on a host without osmium, and runs under `brokkr check --profile
//! full` or `brokkr test -p pbfhogg-cli apply_changes_locations_on_ways_agrees_with_osmium`.
//! Migration target: a `brokkr verify merge` arm that runs both tools with
//! the flag on a handcrafted fixture.
//!
//! What it pins: every surviving base way takes the OSC coordinate of each
//! node the OSC modifies (a base way cannot reference a created node), ways
//! the OSC does not touch included, on a way blob whose ID range misses the
//! diff and on one the merge rewrites. pbfhogg's refresh summary must show
//! those routes: the disjoint blobs searched and rewritten only for the
//! refresh, the rewritten one not searched.
//!
//! # Why the fixture is restricted
//!
//! osmium's location index is an append-only vector of `(id, Location)`
//! pairs searched with `std::lower_bound` (`SparseMemArray`, see
//! `research/osmium-tool/src/command_apply_changes.cpp` and
//! `research/libosmium/include/osmium/index/detail/vector_map.hpp`). It is
//! filled with every raw node version of the change file in file order, and
//! during the base node pass it is searched, and appended to, for each base
//! node some raw OSC way version references. It is sorted only at the base's
//! first way after a node; after that sort a key with several entries
//! resolves to its smallest location. An unresolved OSC-way reference keeps
//! libosmium's undefined location, written as `2147483647`, where pbfhogg
//! refuses. DEVIATIONS.md lists where the two tools differ.
//!
//! So the comparison is only sound on a fixture where those defects cannot
//! decide any output value, and [`assert_sound`] checks each rule on the
//! fixture data before either tool runs:
//!
//! - Each element appears once in the change file: several versions of a
//!   node resolve to the smallest location, not the latest, and a superseded
//!   way version still feeds the set of base nodes looked up.
//! - The base has nodes followed by ways, in strictly increasing ID order:
//!   without a node-to-way transition the index is never sorted.
//! - OSC node IDs strictly increase in file order, and every base node an
//!   OSC way references but the OSC does not carry has a higher ID than every
//!   OSC node. Then the vector starts sorted and each base-pass append adds a
//!   larger key, so every `lower_bound` call meets its precondition, a node
//!   the OSC carries is found rather than duplicated with its base location,
//!   and no key ever has two entries.
//! - A tripwire on that rule rather than a further restriction: every node
//!   the OSC carries and an OSC way references keeps its coordinate or moves
//!   to a larger `(lon, lat)`, the order libosmium's `Location` sorts in. Were
//!   a lookup ever missed and the base location appended beside the OSC's,
//!   the sort would pick the smaller, base one and the comparison would fail
//!   instead of passing by the tie-break.
//! - Every OSC-way reference resolves to a base node or a node the OSC
//!   creates or modifies, never to a deleted one: no unresolved references.
//! - No surviving way references a deleted node, and deletes carry no
//!   coordinates: a delete under a live way is a documented divergence.
//! - All IDs are positive: the index keys on the absolute ID.
//! - The base's inline coordinates are its node records, by construction:
//!   osmium overwrites an inline coordinate with the base record for nodes
//!   OSC ways reference, pbfhogg never repairs one.
//!
//! Left out, each a documented divergence or an osmium defect rather than a
//! case both tools define: a deleted node under a surviving way, several
//! versions of one node, OSC-way-referenced nodes in an index layout that
//! breaks the sort, ways without coordinate arrays, unresolved references.
//!
//! Both outputs are compared, decoded, to an expected state computed from
//! the fixture data (every surviving way's inline coordinates are its refs'
//! final node coordinates), not to each other's bytes: headers and encodings
//! differ. Metadata is not compared (the base carries none).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::cli_apply_changes::write_osc;
use crate::invoker::CliInvoker;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId, MemberType};

/// `(lat, lon)` in decimicrodegrees.
type Coord = (i32, i32);
type Tags = &'static [(&'static str, &'static str)];
type OwnedTags = Vec<(String, String)>;

struct BaseWay {
    id: i64,
    refs: &'static [i64],
    tags: Tags,
}

struct BaseRelation {
    id: i64,
    /// `(kind, id, role)`, kind `"node"`, `"way"` or `"relation"`.
    members: &'static [(&'static str, i64, &'static str)],
    tags: Tags,
}

struct Base {
    nodes: &'static [(i64, Coord, Tags)],
    /// One way blob per entry, so each entry's ID range is its blob's.
    way_blobs: Vec<Vec<BaseWay>>,
    relations: Vec<BaseRelation>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Create,
    Modify,
    Delete,
}

struct OscNode {
    action: Action,
    id: i64,
    /// `None` exactly for deletes.
    at: Option<Coord>,
    tags: Tags,
}

struct OscWay {
    action: Action,
    id: i64,
    refs: &'static [i64],
    tags: Tags,
}

/// A change file, written nodes first, each element in its own section, so
/// file order is list order.
struct Osc {
    nodes: Vec<OscNode>,
    ways: Vec<OscWay>,
}

/// What a case must exercise, checked against the written base.
struct Case {
    name: &'static str,
    osc: Osc,
    /// Untouched base ways whose inline coordinates must change: the
    /// pre-fix pbfhogg wrote them unchanged, so the comparison would fail.
    refreshed_untouched: &'static [i64],
    /// Indices into `Base::way_blobs` whose ID range must miss every OSC way.
    disjoint_blobs: &'static [usize],
    /// Indices into `Base::way_blobs` whose ID range must hold an OSC way.
    overlapped_blobs: &'static [usize],
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

const NODE_10: Coord = (550_000_000, 100_000_000);
const NODE_20: Coord = (551_000_000, 110_000_000);
const NODE_30: Coord = (552_000_000, 120_000_000);
/// Node 10 moves east: OSC way 2010 references it, so the tie-break tripwire
/// in [`assert_index_stays_sorted`] requires a larger `(lon, lat)`.
const NODE_10_MOVED: Coord = (551_234_567, 107_654_321);
const NODE_20_MOVED: Coord = (549_876_543, 108_765_432);
const NODE_40_CREATED: Coord = (553_000_000, 130_000_000);

const BASE_NODES: &[(i64, Coord, Tags)] = &[
    (10, NODE_10, &[]),
    (20, NODE_20, &[]),
    (30, NODE_30, &[("amenity", "bench")]),
    (50, (554_000_000, 140_000_000), &[]),
    (100, (555_000_000, 150_000_000), &[]),
    (110, (556_000_000, 160_000_000), &[]),
    (120, (557_000_000, 170_000_000), &[]),
];

fn base() -> Base {
    let way = |id, refs, tags| BaseWay { id, refs, tags };
    Base {
        nodes: BASE_NODES,
        way_blobs: vec![
            // Blob 0: its range misses every OSC way (case a).
            vec![
                way(1000, &[120, 10], &[("highway", "path")]),
                way(1001, &[20, 30, 120], &[("highway", "track")]),
            ],
            // Blob 1: the OSC modifies 2010 and deletes 2020 (case b).
            vec![
                way(2000, &[20, 120], &[("highway", "service")]),
                way(2010, &[10, 100, 120], &[("highway", "residential")]),
                way(2020, &[110, 120], &[("barrier", "fence")]),
            ],
        ],
        relations: vec![BaseRelation {
            id: 5000,
            members: &[("way", 1000, "outer"), ("node", 120, "label")],
            tags: &[("type", "multipolygon")],
        }],
    }
}

fn cases() -> Vec<Case> {
    let node = |action, id, at, tags| OscNode {
        action,
        id,
        at,
        tags,
    };
    let way = |action, id, refs, tags| OscWay {
        action,
        id,
        refs,
        tags,
    };
    vec![
        Case {
            name: "combined",
            osc: Osc {
                nodes: vec![
                    // Moved under untouched ways 1000 (blob 0) and modified
                    // way 2010 (case c).
                    node(Action::Modify, 10, Some(NODE_10_MOVED), &[]),
                    // Moved under untouched ways 1001 (blob 0, case a) and
                    // 2000 (blob 1, case b).
                    node(Action::Modify, 20, Some(NODE_20_MOVED), &[]),
                    // Tag-only edit under untouched way 1001 (case d).
                    node(Action::Modify, 30, Some(NODE_30), &[("name", "x")]),
                    node(Action::Create, 40, Some(NODE_40_CREATED), &[]),
                    // Referenced by no way.
                    node(Action::Delete, 50, None, &[]),
                ],
                ways: vec![
                    // Case c: a tag edit to a way already holding node 10;
                    // osmium looks up base nodes 100 and 120.
                    way(
                        Action::Modify,
                        2010,
                        &[10, 100, 120],
                        &[("highway", "unclassified")],
                    ),
                    way(Action::Delete, 2020, &[], &[]),
                    // Case e: a created node and base node 110.
                    way(Action::Create, 3000, &[40, 110], &[("highway", "footway")]),
                ],
            },
            refreshed_untouched: &[1000, 1001, 2000],
            disjoint_blobs: &[0],
            overlapped_blobs: &[1],
        },
        Case {
            // The routine vertex drag: no OSC way at all, so osmium looks up
            // no base node and every way blob's range misses the diff.
            name: "node-only",
            osc: Osc {
                nodes: vec![
                    node(Action::Modify, 10, Some(NODE_10_MOVED), &[]),
                    node(Action::Modify, 20, Some(NODE_20_MOVED), &[]),
                ],
                ways: vec![],
            },
            refreshed_untouched: &[1000, 1001, 2000, 2010],
            disjoint_blobs: &[0, 1],
            overlapped_blobs: &[],
        },
    ]
}

// ---------------------------------------------------------------------------
// Soundness rules (see the module doc for why each exists)
// ---------------------------------------------------------------------------

fn strictly_increasing(ids: &[i64]) -> bool {
    ids.windows(2).all(|w| w[0] < w[1])
}

/// A valid WGS84 coordinate: an invalid one is a defined location in
/// osmium's index but refused or clamped elsewhere.
fn valid((lat, lon): Coord) -> bool {
    (-900_000_000..=900_000_000).contains(&lat) && (-1_800_000_000..=1_800_000_000).contains(&lon)
}

fn base_node_ids(base: &Base) -> Vec<i64> {
    base.nodes.iter().map(|n| n.0).collect()
}

fn base_way_ids(base: &Base) -> Vec<i64> {
    base.way_blobs.iter().flatten().map(|w| w.id).collect()
}

fn osc_node_ids(osc: &Osc, pick: impl Fn(Action) -> bool) -> BTreeSet<i64> {
    osc.nodes
        .iter()
        .filter(|n| pick(n.action))
        .map(|n| n.id)
        .collect()
}

fn assert_sound(base: &Base, case: &Case) {
    assert_base_sound(base, case.name);
    assert_osc_sound(base, case);
    assert_refs_resolve(base, case);
    assert_index_stays_sorted(base, case);
}

/// Base: nodes then ways, IDs strictly increasing and positive, every way
/// ref a base node (so its inline coordinates are the node records).
fn assert_base_sound(base: &Base, name: &str) {
    let base_nodes = base_node_ids(base);
    let base_ways = base_way_ids(base);
    let base_relations: Vec<i64> = base.relations.iter().map(|r| r.id).collect();
    assert!(
        !base_nodes.is_empty() && !base_ways.is_empty(),
        "{name}: base needs nodes and ways"
    );
    for ids in [&base_nodes, &base_ways, &base_relations] {
        assert!(
            strictly_increasing(ids),
            "{name}: base IDs must strictly increase"
        );
        assert!(
            ids.iter().all(|&id| id > 0),
            "{name}: base IDs must be positive"
        );
    }
    for &(id, at, _) in base.nodes {
        assert!(valid(at), "{name}: base node {id} coordinates out of range");
    }
    for w in base.way_blobs.iter().flatten() {
        assert!(
            w.refs.iter().all(|r| base_nodes.contains(r)),
            "{name}: base way {} needs every node record for honest coordinates",
            w.id
        );
    }
}

/// OSC: one entry per element (strictly increasing node IDs imply it for
/// nodes), positive IDs, actions consistent with the base, coordinates
/// exactly on creates and modifies, refs exactly on live ways.
fn assert_osc_sound(base: &Base, case: &Case) {
    let name = case.name;
    let osc = &case.osc;
    let base_nodes = base_node_ids(base);
    let base_ways = base_way_ids(base);
    let osc_nodes: Vec<i64> = osc.nodes.iter().map(|n| n.id).collect();
    let osc_ways: Vec<i64> = osc.ways.iter().map(|w| w.id).collect();
    assert!(
        strictly_increasing(&osc_nodes),
        "{name}: OSC node IDs must strictly increase in file order"
    );
    let mut seen = BTreeSet::new();
    assert!(
        osc_ways.iter().all(|id| seen.insert(*id)),
        "{name}: one version per OSC way"
    );
    assert!(
        osc_nodes.iter().chain(&osc_ways).all(|&id| id > 0),
        "{name}: OSC IDs must be positive"
    );
    for n in &osc.nodes {
        let present = base_nodes.contains(&n.id);
        assert_eq!(
            present,
            n.action != Action::Create,
            "{name}: node {} action vs base",
            n.id
        );
        assert_eq!(
            n.at.is_some(),
            n.action != Action::Delete,
            "{name}: node {} coordinates",
            n.id
        );
        if let Some(at) = n.at {
            assert!(valid(at), "{name}: node {} coordinates out of range", n.id);
        }
    }
    for w in &osc.ways {
        let present = base_ways.contains(&w.id);
        assert_eq!(
            present,
            w.action != Action::Create,
            "{name}: way {} action vs base",
            w.id
        );
        assert_eq!(
            w.refs.is_empty(),
            w.action == Action::Delete,
            "{name}: way {} refs",
            w.id
        );
    }
}

/// Every OSC-way reference resolves, never to a deleted node, and nothing
/// that survives references a deleted node.
fn assert_refs_resolve(base: &Base, case: &Case) {
    let name = case.name;
    let osc = &case.osc;
    let base_nodes = base_node_ids(base);
    let deleted = osc_node_ids(osc, |a| a == Action::Delete);
    let carried_live = osc_node_ids(osc, |a| a != Action::Delete);
    for w in &osc.ways {
        for r in w.refs {
            assert!(*r > 0, "{name}: way {} ref {r} must be positive", w.id);
            assert!(
                carried_live.contains(r) || (base_nodes.contains(r) && !deleted.contains(r)),
                "{name}: way {} ref {r} must resolve",
                w.id
            );
        }
    }
    let touched: BTreeSet<i64> = osc.ways.iter().map(|w| w.id).collect();
    for w in base
        .way_blobs
        .iter()
        .flatten()
        .filter(|w| !touched.contains(&w.id))
    {
        assert!(
            w.refs.iter().all(|r| !deleted.contains(r)),
            "{name}: surviving way {} references a deleted node",
            w.id
        );
    }
    for r in &base.relations {
        assert!(
            r.members
                .iter()
                .all(|m| m.0 != "node" || !deleted.contains(&m.1)),
            "{name}: relation {} names a deleted node",
            r.id
        );
    }
}

/// osmium's location index stays sorted with unique keys through its base
/// node pass: OSC node IDs strictly increase (checked in
/// [`assert_osc_sound`]) and every base node an OSC way references but the
/// OSC does not carry lies above every OSC node, deletes included.
fn assert_index_stays_sorted(base: &Base, case: &Case) {
    let name = case.name;
    let osc = &case.osc;
    let base_nodes = base_node_ids(base);
    let osc_nodes = osc_node_ids(osc, |_| true);
    let max_osc_node = osc_nodes.last().copied().unwrap_or(0);
    for w in &osc.ways {
        for r in w.refs {
            if base_nodes.contains(r) && !osc_nodes.contains(r) {
                assert!(
                    *r > max_osc_node,
                    "{name}: base node {r}, looked up for OSC way {}, must have an ID above \
                     every OSC node ({max_osc_node})",
                    w.id
                );
            }
        }
    }
    // Tripwire: a missed lookup would surface as a failure, not be masked by
    // the smallest-location tie-break (libosmium orders by lon, then lat).
    for n in &osc.nodes {
        let (Some((lat, lon)), true) = (n.at, base_nodes.contains(&n.id)) else {
            continue;
        };
        if osc.ways.iter().any(|w| w.refs.contains(&n.id)) {
            let (base_lat, base_lon) = base_coord(base, n.id);
            assert!(
                (lon, lat) >= (base_lon, base_lat),
                "{name}: node {}, referenced by an OSC way, must keep its coordinate or \
                 move to a larger (lon, lat)",
                n.id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

fn owned(tags: Tags) -> OwnedTags {
    let mut v: OwnedTags = tags
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();
    v.sort();
    v
}

fn base_coord(base: &Base, id: i64) -> Coord {
    base.nodes
        .iter()
        .find(|n| n.0 == id)
        .unwrap_or_else(|| panic!("base node {id}"))
        .1
}

fn member_id(kind: &str, id: i64) -> MemberId {
    match kind {
        "node" => MemberId::Node(id),
        "way" => MemberId::Way(id),
        "relation" => MemberId::Relation(id),
        other => panic!("member kind {other}"),
    }
}

/// A sorted, indexed LocationsOnWays base whose inline coordinates are its
/// node records.
fn write_base(path: &Path, base: &Base) {
    let file = std::fs::File::create(path).expect("create base");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    for &(id, (lat, lon), tags) in base.nodes {
        bb.add_node(id, lat, lon, tags.iter().copied(), None);
    }
    writer
        .write_primitive_block(bb.take().expect("take").expect("node block"))
        .expect("node blob");

    for blob in &base.way_blobs {
        let mut bb = BlockBuilder::new();
        for w in blob {
            let coords: Vec<Coord> = w.refs.iter().map(|&r| base_coord(base, r)).collect();
            bb.add_way_with_locations(w.id, w.tags.iter().copied(), w.refs, &coords, None);
        }
        writer
            .write_primitive_block(bb.take().expect("take").expect("way block"))
            .expect("way blob");
    }

    let mut bb = BlockBuilder::new();
    for r in &base.relations {
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|&(kind, id, role)| MemberData {
                id: member_id(kind, id),
                role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("relation blob");
    }
    writer.flush().expect("flush");
}

fn degrees(v: i32) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    format!("{sign}{}.{:07}", a / 10_000_000, a % 10_000_000)
}

fn xml_tags(tags: Tags) -> String {
    tags.iter()
        .map(|(k, v)| format!("<tag k=\"{k}\" v=\"{v}\"/>"))
        .collect()
}

fn section(action: Action, body: &str) -> String {
    let tag = match action {
        Action::Create => "create",
        Action::Modify => "modify",
        Action::Delete => "delete",
    };
    format!("  <{tag}>{body}</{tag}>\n")
}

fn version(action: Action) -> u32 {
    if action == Action::Create { 1 } else { 2 }
}

fn osc_xml(osc: &Osc) -> String {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n");
    for n in &osc.nodes {
        let v = version(n.action);
        let body = match n.at {
            Some((lat, lon)) => format!(
                "<node id=\"{}\" version=\"{v}\" lat=\"{}\" lon=\"{}\">{}</node>",
                n.id,
                degrees(lat),
                degrees(lon),
                xml_tags(n.tags)
            ),
            None => format!("<node id=\"{}\" version=\"{v}\"/>", n.id),
        };
        xml.push_str(&section(n.action, &body));
    }
    for w in &osc.ways {
        let refs: String = w
            .refs
            .iter()
            .map(|r| format!("<nd ref=\"{r}\"/>"))
            .collect();
        let body = format!(
            "<way id=\"{}\" version=\"{}\">{refs}{}</way>",
            w.id,
            version(w.action),
            xml_tags(w.tags)
        );
        xml.push_str(&section(w.action, &body));
    }
    xml.push_str("</osmChange>\n");
    xml
}

// ---------------------------------------------------------------------------
// Decoded state
// ---------------------------------------------------------------------------

/// Decoded content keyed by ID: nodes `(coord, tags)`, ways `(refs, tags,
/// inline coords)`, relations `(members, tags)`. Tags sorted; ref and
/// member order kept.
#[derive(Debug, Default, PartialEq, Eq)]
#[allow(clippy::type_complexity)]
struct State {
    nodes: BTreeMap<i64, (Coord, OwnedTags)>,
    ways: BTreeMap<i64, (Vec<i64>, OwnedTags, Vec<Coord>)>,
    relations: BTreeMap<i64, (Vec<(String, i64, String)>, OwnedTags)>,
}

fn sorted_tags<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>) -> OwnedTags {
    let mut v: OwnedTags = tags.map(|(k, v)| (k.to_string(), v.to_string())).collect();
    v.sort();
    v
}

fn decode(path: &Path) -> State {
    let mut state = State::default();
    for blob in BlobReader::from_path(path).expect("open pbf") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        for element in block.elements() {
            let dup = match element {
                Element::DenseNode(n) => state
                    .nodes
                    .insert(
                        n.id(),
                        (
                            (n.decimicro_lat(), n.decimicro_lon()),
                            sorted_tags(n.tags()),
                        ),
                    )
                    .map(|_| n.id()),
                Element::Node(n) => state
                    .nodes
                    .insert(
                        n.id(),
                        (
                            (n.decimicro_lat(), n.decimicro_lon()),
                            sorted_tags(n.tags()),
                        ),
                    )
                    .map(|_| n.id()),
                Element::Way(w) => {
                    let locs = w
                        .node_locations()
                        .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
                        .collect();
                    state
                        .ways
                        .insert(w.id(), (w.refs().collect(), sorted_tags(w.tags()), locs))
                        .map(|_| w.id())
                }
                Element::Relation(r) => {
                    let members = r
                        .members()
                        .map(|m| {
                            let kind = match m.id.member_type() {
                                MemberType::Node => "node",
                                MemberType::Way => "way",
                                MemberType::Relation => "relation",
                                _ => "unknown",
                            };
                            (
                                kind.to_string(),
                                m.id.id(),
                                m.role().expect("role").to_string(),
                            )
                        })
                        .collect();
                    state
                        .relations
                        .insert(r.id(), (members, sorted_tags(r.tags())))
                        .map(|_| r.id())
                }
                _ => None,
            };
            assert!(
                dup.is_none(),
                "{}: element {dup:?} appears twice",
                path.display()
            );
        }
    }
    state
}

/// The base with the change applied, every surviving way's inline
/// coordinates taken from the final node table.
fn expected(base: &Base, osc: &Osc) -> State {
    let mut nodes: BTreeMap<i64, (Coord, OwnedTags)> = base
        .nodes
        .iter()
        .map(|&(id, at, tags)| (id, (at, owned(tags))))
        .collect();
    for n in &osc.nodes {
        match n.at {
            Some(at) => {
                nodes.insert(n.id, (at, owned(n.tags)));
            }
            None => {
                nodes.remove(&n.id);
            }
        }
    }
    let mut ways: BTreeMap<i64, (Vec<i64>, OwnedTags)> = base
        .way_blobs
        .iter()
        .flatten()
        .map(|w| (w.id, (w.refs.to_vec(), owned(w.tags))))
        .collect();
    for w in &osc.ways {
        if w.action == Action::Delete {
            ways.remove(&w.id);
        } else {
            ways.insert(w.id, (w.refs.to_vec(), owned(w.tags)));
        }
    }
    let ways = ways
        .into_iter()
        .map(|(id, (refs, tags))| {
            let locs = refs
                .iter()
                .map(|r| {
                    nodes
                        .get(r)
                        .unwrap_or_else(|| panic!("way {id} ref {r} does not resolve"))
                        .0
                })
                .collect();
            (id, (refs, tags, locs))
        })
        .collect();
    let relations = base
        .relations
        .iter()
        .map(|r| {
            let members = r
                .members
                .iter()
                .map(|&(kind, id, role)| (kind.to_string(), id, role.to_string()))
                .collect();
            (r.id, (members, owned(r.tags)))
        })
        .collect();
    State {
        nodes,
        ways,
        relations,
    }
}

/// The written base holds exactly the fixture's way blobs, so the blob
/// ranges the cases rely on exist, and each case's disjoint and overlapped
/// blobs relate to its OSC ways as the case claims.
fn assert_layout(path: &Path, base: &Base, case: &Case) {
    let mut written = Vec::new();
    for blob in BlobReader::from_path(path).expect("open base") {
        let BlobDecode::OsmData(block) = blob.expect("blob").decode().expect("decode") else {
            continue;
        };
        let ids: Vec<i64> = block
            .elements()
            .filter_map(|e| {
                if let Element::Way(w) = e {
                    Some(w.id())
                } else {
                    None
                }
            })
            .collect();
        if !ids.is_empty() {
            written.push(ids);
        }
    }
    let fixture: Vec<Vec<i64>> = base
        .way_blobs
        .iter()
        .map(|b| b.iter().map(|w| w.id).collect())
        .collect();
    assert_eq!(written, fixture, "way blob layout");

    let name = case.name;
    // Every blob is one or the other, so pbfhogg's searched-blob count is
    // exactly the disjoint ones (no range-overlap false positive exists).
    for i in 0..fixture.len() {
        assert!(
            case.disjoint_blobs.contains(&i) != case.overlapped_blobs.contains(&i),
            "{name}: blob {i} must be exactly one of disjoint and overlapped"
        );
    }
    let in_range =
        |i: usize, id: i64| (fixture[i][0]..=fixture[i][fixture[i].len() - 1]).contains(&id);
    let refreshed_in = |i: usize| {
        case.refreshed_untouched
            .iter()
            .any(|id| fixture[i].contains(id))
    };
    for &i in case.disjoint_blobs {
        assert!(
            !case.osc.ways.iter().any(|w| in_range(i, w.id)),
            "{name}: blob {i}'s range must miss every OSC way"
        );
        assert!(
            refreshed_in(i),
            "{name}: disjoint blob {i} must hold a refreshed untouched way"
        );
    }
    for &i in case.overlapped_blobs {
        assert!(
            case.osc.ways.iter().any(|w| fixture[i].contains(&w.id)),
            "{name}: blob {i} must hold a way the OSC changes"
        );
        assert!(
            refreshed_in(i),
            "{name}: overlapped blob {i} must hold a refreshed untouched way"
        );
    }
}

/// The refresh summary pbfhogg must print for a case, derived from the
/// fixture: every untouched way whose coordinates change is refreshed, each
/// disjoint blob is searched and rewritten only for the refresh, and the
/// overlapped blobs, rewritten for the merge, are not counted as searched.
fn expected_summary(case: &Case, base_state: &State, want: &State) -> String {
    let touched: BTreeSet<i64> = case.osc.ways.iter().map(|w| w.id).collect();
    let mut ways = BTreeSet::new();
    let mut refs = 0;
    for (id, (_, _, locs)) in &want.ways {
        let Some((_, _, before)) = base_state.ways.get(id) else {
            continue;
        };
        if touched.contains(id) {
            continue;
        }
        let changed = locs.iter().zip(before).filter(|(a, b)| a != b).count();
        if changed > 0 {
            ways.insert(*id);
            refs += changed;
        }
    }
    let listed: BTreeSet<i64> = case.refreshed_untouched.iter().copied().collect();
    assert_eq!(
        ways, listed,
        "{}: refreshed_untouched must list every refreshed way",
        case.name
    );
    let blobs = case.disjoint_blobs.len();
    format!(
        "Inline locations refreshed: {refs} refs in {} ways, {blobs} way blobs scanned, \
         {blobs} rewritten only for this, 0 refs to deleted nodes kept",
        ways.len()
    )
}

fn assert_locations_check_passes(path: &Path) {
    let out = CliInvoker::new()
        .arg("check")
        .arg("--locations")
        .arg(path)
        .run();
    assert!(
        out.status.success(),
        "check --locations {} failed\nstdout:\n{}\nstderr:\n{}",
        path.display(),
        out.stdout_str(),
        out.stderr_str()
    );
}

fn assert_state(tool: &str, got: &State, want: &State) {
    assert_eq!(got.nodes, want.nodes, "{tool}: nodes");
    for (id, way) in &want.ways {
        assert_eq!(
            got.ways.get(id),
            Some(way),
            "{tool}: way {id} (refs, tags, inline coords)"
        );
    }
    assert_eq!(got.ways, want.ways, "{tool}: way set");
    assert_eq!(got.relations, want.relations, "{tool}: relations");
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "external"]
fn apply_changes_locations_on_ways_agrees_with_osmium() {
    // Only a missing osmium skips; any other spawn failure is a failure.
    let version = match std::process::Command::new("osmium")
        .arg("--version")
        .output()
    {
        Ok(out) => {
            assert!(out.status.success(), "osmium --version failed: {out:?}");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .take(2)
                .collect::<Vec<_>>()
                .join(", ")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::common::skip("osmium not found in PATH");
            return;
        }
        Err(e) => panic!("spawning osmium --version: {e}"),
    };
    let dir = crate::common::test_scratch();
    let base = base();
    let base_path = dir.path().join("base.osm.pbf");
    write_base(&base_path, &base);
    let base_state = decode(&base_path);

    for case in cases() {
        let name = case.name;
        assert_sound(&base, &case);
        assert_layout(&base_path, &base, &case);
        let want = expected(&base, &case.osc);
        // Also proves the case discriminates: the listed untouched ways must
        // change, and the pre-fix pbfhogg left them as they were.
        let summary = expected_summary(&case, &base_state, &want);
        assert!(
            !case.refreshed_untouched.is_empty(),
            "{name}: nothing to refresh"
        );

        let osc_path = dir.path().join(format!("{name}.osc.gz"));
        write_osc(&osc_path, &osc_xml(&case.osc));

        let hogg_out = dir.path().join(format!("{name}-pbfhogg.osm.pbf"));
        let out = CliInvoker::new()
            .arg("apply-changes")
            .arg(&base_path)
            .arg(&osc_path)
            .arg("-o")
            .arg(&hogg_out)
            .arg("--locations-on-ways")
            .run();
        assert!(
            out.status.success(),
            "{name}: pbfhogg failed; stderr:\n{}",
            out.stderr_str()
        );
        out.assert_stderr_contains(&summary);

        let osmium_out = dir.path().join(format!("{name}-osmium.osm.pbf"));
        let status = std::process::Command::new("osmium")
            .arg("apply-changes")
            .arg(&base_path)
            .arg(&osc_path)
            .arg("--locations-on-ways")
            .arg("-o")
            .arg(&osmium_out)
            .arg("--overwrite")
            .status()
            .expect("run osmium");
        assert!(
            status.success(),
            "{name}: osmium apply-changes failed ({version})"
        );

        assert_state(
            &format!("{name}: osmium ({version})"),
            &decode(&osmium_out),
            &want,
        );
        assert_state(&format!("{name}: pbfhogg"), &decode(&hogg_out), &want);
        assert_locations_check_passes(&hogg_out);
        assert_locations_check_passes(&osmium_out);
    }
}
