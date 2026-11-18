//! The shared mixed-sign fixture graph and its relabelled positive twin.
//!
//! [`mixed_sign_graph`] is one small, referentially complete OSM graph whose
//! IDs span zero in every kind, with edges across zero in both directions:
//! negative ways over positive nodes and positive ways over negative ones,
//! relations whose node, way and relation members cross zero both ways,
//! magnitude twins (`n7` and `n-7`, `w7` and `w-7`, `r7` and `r-7`) that an
//! abs-folding implementation would confuse, tags that tags-filter can select
//! on both signs, coordinates inside and outside [`MIXED_SIGN_BBOX`], and a
//! named administrative boundary with a closed outer ring plus a label member
//! outside the box.
//!
//! [`MixedSignGraph::relabelled_twin`] maps every kind's IDs (elements and
//! the IDs referenced as that kind) onto `1..=n` by canonical rank, so the
//! twin holds the same elements in the same order, the same references and
//! the same blob cuts, with every ID positive. A command run on both,
//! with its output mapped through the [`Relabel`], answers the same where it
//! treats a negative ID like any other; an arithmetic shift would not do,
//! since it can change canonical order or overflow.
//!
//! Written by [`MixedSignGraph::write`] with [`MIXED_SIGN_BLOCK`] elements
//! per block, the node blobs are `[0, -1, -2]` (zero beside negatives),
//! `[-5, -7, -9]` (negative only), `[-11, 1, 2]` (straddling zero after a
//! negative-only blob), then positive blobs; ways and relations each have a
//! blob straddling zero too.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};

use super::{
    FixtureFraming, TestMember, TestNode, TestRelation, TestWay, encode_fixture_blocks,
    frame_fixture_blocks,
};

/// Elements per block of a kind in [`MixedSignGraph::write`].
pub const MIXED_SIGN_BLOCK: usize = 3;

/// `(min_lat, min_lon, max_lat, max_lon)` in decimicrodegrees: holds every
/// node of the graph but `-11` and `11`.
pub const MIXED_SIGN_BBOX: (i32, i32, i32, i32) =
    (549_000_000, 119_000_000, 554_000_000, 124_000_000);

/// The graph: elements of each kind in canonical order.
pub struct MixedSignGraph {
    pub nodes: Vec<TestNode<'static>>,
    pub ways: Vec<TestWay<'static>>,
    pub relations: Vec<TestRelation<'static>>,
}

/// Canonical OSM order (`0`, then `-1, -2, ...`, then `1, 2, ...`) as a sort
/// key.
pub fn canonical_key(id: i64) -> (u8, u64) {
    match id {
        0 => (0, 0),
        i if i < 0 => (1, i.unsigned_abs()),
        i => (2, i.unsigned_abs()),
    }
}

fn node(id: i64, lat: i32, lon: i32, tags: Vec<(&'static str, &'static str)>) -> TestNode<'static> {
    TestNode {
        id,
        lat,
        lon,
        tags,
        meta: None,
    }
}

fn way(id: i64, refs: &[i64], tags: Vec<(&'static str, &'static str)>) -> TestWay<'static> {
    TestWay {
        id,
        refs: refs.to_vec(),
        tags,
        meta: None,
    }
}

fn relation(
    id: i64,
    members: Vec<(MemberId, &'static str)>,
    tags: Vec<(&'static str, &'static str)>,
) -> TestRelation<'static> {
    TestRelation {
        id,
        members: members
            .into_iter()
            .map(|(id, role)| TestMember { id, role })
            .collect(),
        tags,
        meta: None,
    }
}

/// The mixed-sign fixture graph (see the module docs).
pub fn mixed_sign_graph() -> MixedSignGraph {
    let nodes = vec![
        node(0, 550_000_000, 120_000_000, vec![]),
        node(-1, 550_100_000, 120_100_000, vec![("amenity", "bench")]),
        node(-2, 550_200_000, 120_000_000, vec![]),
        // The boundary ring: 5 (NW), 7 (NE), -7 (SE), -5 (SW).
        node(-5, 551_000_000, 121_000_000, vec![]),
        node(-7, 551_000_000, 123_000_000, vec![("amenity", "bench")]),
        node(-9, 550_500_000, 120_500_000, vec![]),
        node(-11, 565_000_000, 135_000_000, vec![]),
        node(1, 550_100_000, 120_200_000, vec![("amenity", "bench")]),
        node(2, 550_200_000, 120_300_000, vec![]),
        node(5, 553_000_000, 121_000_000, vec![]),
        node(7, 553_000_000, 123_000_000, vec![]),
        node(9, 550_600_000, 120_600_000, vec![]),
        node(11, 566_000_000, 136_000_000, vec![("place", "town")]),
    ];
    let ways = vec![
        way(-1, &[-1, -2, -5], vec![("highway", "residential")]),
        way(-2, &[1, 2], vec![("highway", "residential")]),
        way(-7, &[-7, 7], vec![("highway", "primary")]),
        way(-9, &[0, -9, 9], vec![]),
        way(1, &[-1, 1], vec![("highway", "residential")]),
        way(2, &[-11, 11], vec![("building", "yes")]),
        way(7, &[5, 7, -7, -5, 5], vec![]),
        way(9, &[2, 9, -2], vec![("highway", "service")]),
    ];
    let relations = vec![
        relation(
            -1,
            vec![
                (MemberId::Node(1), "stop"),
                (MemberId::Way(-2), ""),
                (MemberId::Relation(1), ""),
            ],
            vec![("type", "route"), ("route", "bus")],
        ),
        relation(
            -7,
            vec![(MemberId::Node(-7), "stop"), (MemberId::Way(7), "")],
            vec![("type", "route"), ("route", "bus")],
        ),
        relation(
            1,
            vec![
                (MemberId::Way(7), "outer"),
                (MemberId::Node(11), "label"),
                (MemberId::Relation(-1), "subarea"),
            ],
            vec![
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "4"),
                ("name", "Mixed"),
            ],
        ),
        relation(
            7,
            vec![(MemberId::Relation(-7), ""), (MemberId::Way(-1), "")],
            vec![("type", "route"), ("route", "road")],
        ),
        relation(
            9,
            vec![
                (MemberId::Relation(7), ""),
                (MemberId::Node(0), ""),
                (MemberId::Node(-1), ""),
            ],
            vec![("type", "site")],
        ),
    ];
    MixedSignGraph {
        nodes,
        ways,
        relations,
    }
}

/// The per-kind ID bijections from a graph onto its twin.
pub struct Relabel {
    pub nodes: BTreeMap<i64, i64>,
    pub ways: BTreeMap<i64, i64>,
    pub relations: BTreeMap<i64, i64>,
}

fn rank_map(ids: BTreeSet<i64>) -> BTreeMap<i64, i64> {
    let mut sorted: Vec<i64> = ids.into_iter().collect();
    sorted.sort_by_key(|&id| canonical_key(id));
    sorted.into_iter().zip(1_i64..).collect()
}

fn mapped(map: &BTreeMap<i64, i64>, id: i64, kind: &str) -> i64 {
    *map.get(&id)
        .unwrap_or_else(|| panic!("{kind} {id} is not in the relabelled domain"))
}

impl Relabel {
    pub fn node(&self, id: i64) -> i64 {
        mapped(&self.nodes, id, "node")
    }

    pub fn way(&self, id: i64) -> i64 {
        mapped(&self.ways, id, "way")
    }

    pub fn relation(&self, id: i64) -> i64 {
        mapped(&self.relations, id, "relation")
    }

    /// A member under the bijection of its kind; an unknown member type is
    /// opaque and kept.
    pub fn member(&self, id: MemberId) -> MemberId {
        match id {
            MemberId::Node(id) => MemberId::Node(self.node(id)),
            MemberId::Way(id) => MemberId::Way(self.way(id)),
            MemberId::Relation(id) => MemberId::Relation(self.relation(id)),
            other => other,
        }
    }
}

impl MixedSignGraph {
    /// Every kind's ID domain: its elements and the IDs referenced as it.
    fn domains(&self) -> [BTreeSet<i64>; 3] {
        let mut nodes: BTreeSet<i64> = self.nodes.iter().map(|n| n.id).collect();
        let mut ways: BTreeSet<i64> = self.ways.iter().map(|w| w.id).collect();
        let mut relations: BTreeSet<i64> = self.relations.iter().map(|r| r.id).collect();
        for w in &self.ways {
            nodes.extend(&w.refs);
        }
        for r in &self.relations {
            for m in &r.members {
                match m.id {
                    MemberId::Node(id) => {
                        nodes.insert(id);
                    }
                    MemberId::Way(id) => {
                        ways.insert(id);
                    }
                    MemberId::Relation(id) => {
                        relations.insert(id);
                    }
                    _ => {}
                }
            }
        }
        [nodes, ways, relations]
    }

    /// The relabelled positive twin and the bijection that made it (see the
    /// module docs).
    pub fn relabelled_twin(&self) -> (MixedSignGraph, Relabel) {
        let [nodes, ways, relations] = self.domains();
        let relabel = Relabel {
            nodes: rank_map(nodes),
            ways: rank_map(ways),
            relations: rank_map(relations),
        };
        let twin = MixedSignGraph {
            nodes: self
                .nodes
                .iter()
                .map(|n| node(relabel.node(n.id), n.lat, n.lon, n.tags.clone()))
                .collect(),
            ways: self
                .ways
                .iter()
                .map(|w| TestWay {
                    id: relabel.way(w.id),
                    refs: w.refs.iter().map(|&r| relabel.node(r)).collect(),
                    tags: w.tags.clone(),
                    meta: w.meta,
                })
                .collect(),
            relations: self
                .relations
                .iter()
                .map(|r| TestRelation {
                    id: relabel.relation(r.id),
                    members: r
                        .members
                        .iter()
                        .map(|m| TestMember {
                            id: relabel.member(m.id),
                            role: m.role,
                        })
                        .collect(),
                    tags: r.tags.clone(),
                    meta: r.meta,
                })
                .collect(),
        };
        (twin, relabel)
    }

    /// Write the graph as a sorted PBF of [`MIXED_SIGN_BLOCK`] elements per
    /// block, every data blob indexed (`indexdata`) or none.
    pub fn write(&self, path: &Path, indexdata: bool) {
        let blocks = encode_fixture_blocks(
            &self.nodes,
            &self.ways,
            &self.relations,
            Some(MIXED_SIGN_BLOCK),
        );
        let framing = if indexdata {
            FixtureFraming::INDEXED
        } else {
            FixtureFraming::NON_INDEXED
        };
        frame_fixture_blocks(path, &blocks, framing);
    }
}

/// The element IDs of every OSMData blob of `path`, in file order.
pub fn blob_ids(path: &Path) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    for blob in BlobReader::from_path(path).expect("open pbf") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        out.push(
            block
                .elements()
                .map(|element| match element {
                    Element::DenseNode(n) => n.id(),
                    Element::Node(n) => n.id(),
                    Element::Way(w) => w.id(),
                    Element::Relation(r) => r.id(),
                    other => panic!("unexpected element {other:?}"),
                })
                .collect(),
        );
    }
    out
}
