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

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
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
                // Untagged, so add-locations-to-ways keeps it only as a
                // relation member.
                (MemberId::Node(-2), ""),
            ],
            vec![("type", "site")],
        ),
        // A negative way of positive refs as a multipolygon member, the
        // membership the prepass marks, and a member node outside the box
        // that only a completing strategy reaches.
        relation(
            11,
            vec![(MemberId::Way(-2), "outer"), (MemberId::Node(-11), "label")],
            vec![("type", "multipolygon"), ("landuse", "grass")],
        ),
    ];
    MixedSignGraph {
        nodes,
        ways,
        relations,
    }
}

/// The per-kind ID bijections from a graph onto its twin.
#[derive(Clone, Debug)]
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
        (self.relabelled(&relabel), relabel)
    }

    /// This graph under a bijection made for another graph whose domain
    /// holds this one's (a variant of [`mixed_sign_graph`] under the
    /// mixed graph's [`Relabel`], so every variant's twin shares one
    /// bijection). Panics on an ID outside the bijection's domain.
    pub fn relabelled(&self, relabel: &Relabel) -> MixedSignGraph {
        MixedSignGraph {
            nodes: self
                .nodes
                .iter()
                .map(|n| TestNode {
                    id: relabel.node(n.id),
                    ..n.clone()
                })
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
        }
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

    /// Write the graph under `layout`, [`MIXED_SIGN_BLOCK`] elements per
    /// block. An unsorted layout reverses every kind's elements, so blobs
    /// and the elements inside them both descend, under a header that
    /// claims no order; a locations layout embeds each way ref's
    /// coordinates from the graph's own node records (a ref the graph has
    /// no record of panics) under a `LocationsOnWays` header. Each element
    /// carries the metadata it holds (the graph's own hold none).
    #[expect(
        clippy::too_many_lines,
        reason = "one encoding loop per kind, then the framing, read top to bottom"
    )]
    pub fn write_layout(&self, path: &Path, layout: Layout) {
        let order = |n: usize| -> Vec<usize> {
            if layout.sorted {
                (0..n).collect()
            } else {
                (0..n).rev().collect()
            }
        };
        let coords: BTreeMap<i64, (i32, i32)> =
            self.nodes.iter().map(|n| (n.id, (n.lat, n.lon))).collect();
        let meta = |m: &Option<super::TestMeta>| {
            m.map(|m| Metadata {
                version: m.version,
                timestamp: m.timestamp,
                changeset: m.changeset,
                uid: m.uid,
                user: m.user,
                visible: m.visible,
            })
        };
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut bb = BlockBuilder::new();
        let mut in_block = 0;
        let mut flush = |bb: &mut BlockBuilder, in_block: &mut usize, force: bool| {
            if (force || *in_block == MIXED_SIGN_BLOCK)
                && let Some(bytes) = bb.take().expect("take block")
            {
                blocks.push(bytes.to_vec());
                *in_block = 0;
            }
        };
        for i in order(self.nodes.len()) {
            let n = &self.nodes[i];
            bb.add_node(
                n.id,
                n.lat,
                n.lon,
                n.tags.iter().copied(),
                meta(&n.meta).as_ref(),
            );
            in_block += 1;
            flush(&mut bb, &mut in_block, false);
        }
        flush(&mut bb, &mut in_block, true);
        for i in order(self.ways.len()) {
            let w = &self.ways[i];
            if layout.locations {
                let locations: Vec<(i32, i32)> = w
                    .refs
                    .iter()
                    .map(|r| {
                        *coords
                            .get(r)
                            .unwrap_or_else(|| panic!("way {} ref {r}", w.id))
                    })
                    .collect();
                bb.add_way_with_locations(
                    w.id,
                    w.tags.iter().copied(),
                    &w.refs,
                    &locations,
                    meta(&w.meta).as_ref(),
                );
            } else {
                bb.add_way(
                    w.id,
                    w.tags.iter().copied(),
                    &w.refs,
                    meta(&w.meta).as_ref(),
                );
            }
            in_block += 1;
            flush(&mut bb, &mut in_block, false);
        }
        flush(&mut bb, &mut in_block, true);
        for i in order(self.relations.len()) {
            let r = &self.relations[i];
            let members: Vec<MemberData<'_>> = r
                .members
                .iter()
                .map(|m| MemberData {
                    id: m.id,
                    role: m.role,
                })
                .collect();
            bb.add_relation(
                r.id,
                r.tags.iter().copied(),
                &members,
                meta(&r.meta).as_ref(),
            );
            in_block += 1;
            flush(&mut bb, &mut in_block, false);
        }
        flush(&mut bb, &mut in_block, true);

        let mut header = HeaderBuilder::new();
        if layout.sorted {
            header = header.sorted();
        }
        if layout.locations {
            header = header.optional_feature("LocationsOnWays");
        }
        let header = header.build().expect("build header");
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
        writer.write_header(&header).expect("write header");
        for bytes in &blocks {
            if layout.indexdata {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
        writer.flush().expect("flush fixture");
    }

    /// The node with `id`.
    pub fn node(&self, id: i64) -> &TestNode<'static> {
        self.nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("no node {id}"))
    }

    /// The way with `id`.
    pub fn way(&self, id: i64) -> &TestWay<'static> {
        self.ways
            .iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("no way {id}"))
    }

    /// The relation with `id`.
    pub fn relation(&self, id: i64) -> &TestRelation<'static> {
        self.relations
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no relation {id}"))
    }
}

/// How [`MixedSignGraph::write_layout`] writes a graph.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// Every data blob carries `indexdata` and `tagdata`, or none does.
    pub indexdata: bool,
    /// Canonical order under a sorted header, or every kind reversed under
    /// a header claiming no order.
    pub sorted: bool,
    /// Ways carry their nodes' coordinates under `LocationsOnWays`.
    pub locations: bool,
}

impl Layout {
    /// Sorted, without locations, indexed or not.
    pub const fn plain(indexdata: bool) -> Self {
        Self {
            indexdata,
            sorted: true,
            locations: false,
        }
    }
}

/// [`mixed_sign_graph`] after one change of each action: node `-7` moved
/// (inside every region it was in), way `-9` deleted, relation `-7`
/// retagged. Its IDs are the mixed graph's, so the mixed graph's
/// [`Relabel`] maps it.
pub fn changed_graph() -> MixedSignGraph {
    let mut g = mixed_sign_graph();
    for n in &mut g.nodes {
        if n.id == -7 {
            n.lat += 100_000;
        }
    }
    g.ways.retain(|w| w.id != -9);
    for r in &mut g.relations {
        if r.id == -7 {
            r.tags = vec![("type", "route"), ("route", "tram")];
        }
    }
    g
}

/// A small graph whose references to `n-9` (from `w9`) and `n-1` (from
/// `r9`) dangle while their magnitude twins `n9` and `n1` are present, and
/// which holds no negative element: a negative reference is the only
/// negative ID in it. Its IDs are the mixed graph's, so the mixed graph's
/// [`Relabel`] maps it.
pub fn dangling_graph() -> MixedSignGraph {
    MixedSignGraph {
        nodes: vec![
            node(1, 550_100_000, 120_200_000, vec![]),
            node(9, 550_600_000, 120_600_000, vec![]),
        ],
        ways: vec![way(9, &[1, -9, 9], vec![("highway", "service")])],
        relations: vec![relation(
            9,
            vec![(MemberId::Node(-1), ""), (MemberId::Way(9), "")],
            vec![("type", "site")],
        )],
    }
}

/// An OSC moving `base` to `changed`: a create for every element `base`
/// lacks, a modify for every element whose content differs, a delete for
/// every element `changed` lacks, each at version 2.
pub fn osc_between(base: &MixedSignGraph, changed: &MixedSignGraph) -> String {
    let fresh = MixedSignGraph {
        nodes: changed
            .nodes
            .iter()
            .filter(|c| !base.nodes.iter().any(|n| n.id == c.id))
            .cloned()
            .collect(),
        ways: changed
            .ways
            .iter()
            .filter(|c| !base.ways.iter().any(|w| w.id == c.id))
            .cloned()
            .collect(),
        relations: changed
            .relations
            .iter()
            .filter(|c| !base.relations.iter().any(|r| r.id == c.id))
            .cloned()
            .collect(),
    };
    // A create is written as a modify of an element with no prior content.
    let (create, _) = modify_and_delete(&fresh.blank(), &fresh);
    let (modify, delete) = modify_and_delete(base, changed);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n  \
         <create>\n{create}  </create>\n  <modify>\n{modify}  </modify>\n  \
         <delete>\n{delete}  </delete>\n</osmChange>\n"
    )
}

impl MixedSignGraph {
    /// The same IDs with no content, which every element differs from.
    fn blank(&self) -> MixedSignGraph {
        MixedSignGraph {
            nodes: self
                .nodes
                .iter()
                .map(|n| node(n.id, i32::MIN, i32::MIN, vec![("", "")]))
                .collect(),
            ways: self
                .ways
                .iter()
                .map(|w| way(w.id, &[], vec![("", "")]))
                .collect(),
            relations: self
                .relations
                .iter()
                .map(|r| relation(r.id, Vec::new(), vec![("", "")]))
                .collect(),
        }
    }
}

/// The modify and delete sections moving `base` to `changed`.
#[expect(
    clippy::let_underscore_must_use,
    reason = "writing to a String cannot fail"
)]
fn modify_and_delete(base: &MixedSignGraph, changed: &MixedSignGraph) -> (String, String) {
    use std::fmt::Write as _;
    let coord = |v: i32| format!("{:.7}", f64::from(v) / 1e7);
    let tags = |out: &mut String, tags: &[(&str, &str)]| {
        for (k, v) in tags {
            let _ = writeln!(out, "      <tag k=\"{k}\" v=\"{v}\"/>");
        }
    };
    let mut modify = String::new();
    let mut delete = String::new();
    for n in &base.nodes {
        match changed.nodes.iter().find(|c| c.id == n.id) {
            None => {
                let _ = writeln!(delete, "    <node id=\"{}\" version=\"2\"/>", n.id);
            }
            Some(c) if (c.lat, c.lon, &c.tags) != (n.lat, n.lon, &n.tags) => {
                let _ = writeln!(
                    modify,
                    "    <node id=\"{}\" lat=\"{}\" lon=\"{}\" version=\"2\">",
                    c.id,
                    coord(c.lat),
                    coord(c.lon)
                );
                tags(&mut modify, &c.tags);
                modify.push_str("    </node>\n");
            }
            Some(_) => {}
        }
    }
    for w in &base.ways {
        match changed.ways.iter().find(|c| c.id == w.id) {
            None => {
                let _ = writeln!(delete, "    <way id=\"{}\" version=\"2\"/>", w.id);
            }
            Some(c) if (&c.refs, &c.tags) != (&w.refs, &w.tags) => {
                let _ = writeln!(modify, "    <way id=\"{}\" version=\"2\">", c.id);
                for r in &c.refs {
                    let _ = writeln!(modify, "      <nd ref=\"{r}\"/>");
                }
                tags(&mut modify, &c.tags);
                modify.push_str("    </way>\n");
            }
            Some(_) => {}
        }
    }
    for r in &base.relations {
        let same = |c: &TestRelation| {
            c.tags == r.tags
                && c.members.len() == r.members.len()
                && c.members
                    .iter()
                    .zip(&r.members)
                    .all(|(a, b)| a.id == b.id && a.role == b.role)
        };
        match changed.relations.iter().find(|c| c.id == r.id) {
            None => {
                let _ = writeln!(delete, "    <relation id=\"{}\" version=\"2\"/>", r.id);
            }
            Some(c) if !same(c) => {
                let _ = writeln!(modify, "    <relation id=\"{}\" version=\"2\">", c.id);
                for m in &c.members {
                    let (kind, id) = match m.id {
                        MemberId::Node(id) => ("node", id),
                        MemberId::Way(id) => ("way", id),
                        MemberId::Relation(id) => ("relation", id),
                        other => panic!("unexpected member {other:?}"),
                    };
                    let _ = writeln!(
                        modify,
                        "      <member type=\"{kind}\" ref=\"{id}\" role=\"{}\"/>",
                        m.role
                    );
                }
                tags(&mut modify, &c.tags);
                modify.push_str("    </relation>\n");
            }
            Some(_) => {}
        }
    }
    (modify, delete)
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
