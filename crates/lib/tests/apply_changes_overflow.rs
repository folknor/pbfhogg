//! apply-changes output blocks that outgrow the PBF blob limit although
//! everything written into them fits on its own: a rewritten block keeps
//! its source block's whole string table and adds the OSC elements'
//! strings, and blocks are cut by element count, so a large value modified
//! to another large value, or two large creates side by side, would make
//! one block past the 32 MiB payload limit. The merge must cut such output
//! into blocks that fit and lose nothing, on the worker's rewrite (modified
//! and inline-created elements) and on the drain's create runs alike.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::Path;

use crate::common::{
    TestNode, generate_nodes, generate_ways, node_ids_id_only, read_all_elements_id_only,
    write_multi_block_test_pbf,
};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Each value alone fits a block with room to spare; two do not.
const LARGE: usize = 17 << 20;

fn value(fill: char) -> String {
    std::iter::repeat_n(fill, LARGE).collect()
}

/// A plain-XML OSC: `create` and `modify` node bodies, each `(id, value)`,
/// a value of `None` writing a tagless node.
fn write_osc(path: &Path, create: &[(i64, Option<&str>)], modify: &[(i64, Option<&str>)]) {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n");
    for (action, nodes) in [("create", create), ("modify", modify)] {
        if nodes.is_empty() {
            continue;
        }
        writeln!(xml, "<{action}>").unwrap();
        for &(id, v) in nodes {
            write!(
                xml,
                "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"2\">"
            )
            .unwrap();
            if let Some(v) = v {
                write!(xml, "<tag k=\"note\" v=\"{v}\"/>").unwrap();
            }
            xml.push_str("</node>\n");
        }
        writeln!(xml, "</{action}>").unwrap();
    }
    xml.push_str("</osmChange>\n");
    std::fs::write(path, xml).unwrap();
}

/// Merge `osc` into `base`, uncompressed (zlib over tens of MiB in a debug
/// build would be most of the run).
fn merge(base: &Path, osc: &Path, output: &Path) -> pbfhogg::apply_changes::MergeStats {
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: pbfhogg::write::WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..pbfhogg::write::WriteOptions::default()
        },
        locations_on_ways: None,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    pbfhogg::apply_changes::merge(
        base,
        osc,
        crate::common::out(output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()))
}

/// The `note` value of every node that has one, by id, in file order.
fn node_notes(path: &Path) -> Vec<(i64, String)> {
    let mut notes = Vec::new();
    for blob in BlobReader::from_path(path).unwrap() {
        if let BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
            for element in block.elements() {
                let (id, tags): (i64, Vec<(String, String)>) = match &element {
                    Element::DenseNode(n) => (
                        n.id(),
                        n.tags()
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                            .collect(),
                    ),
                    Element::Node(n) => (
                        n.id(),
                        n.tags()
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                            .collect(),
                    ),
                    _ => continue,
                };
                for (k, v) in tags {
                    if k == "note" {
                        notes.push((id, v));
                    }
                }
            }
        }
    }
    notes
}

/// Base: nodes 1..=50 in one blob (minus `skip`), `note` values on the
/// listed nodes, and one way blob.
fn write_base(path: &Path, notes: &[(i64, &str)], skip: &[i64]) {
    let mut nodes: Vec<TestNode<'_>> = generate_nodes(50, 1)
        .into_iter()
        .filter(|n| !skip.contains(&n.id))
        .collect();
    for node in &mut nodes {
        if let Some(&(_, v)) = notes.iter().find(|(id, _)| *id == node.id) {
            node.tags = vec![("note", v)];
        }
    }
    write_multi_block_test_pbf(path, &nodes, &generate_ways(10, 1, 2, 1), &[], 50);
}

/// The reported case: one node's large value modified to another large
/// value. The source block and the new element each fit; the rewritten
/// block holding the source table and the new value does not.
#[test]
fn a_large_value_modified_to_another_large_value_applies() {
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let (old, new) = (value('a'), value('b'));
    write_base(&base, &[(25, &old)], &[]);
    write_osc(&osc, &[], &[(25, Some(&new))]);
    let stats = merge(&base, &osc, &output);

    assert_eq!(node_notes(&output), vec![(25, new)]);
    let c = read_all_elements_id_only(&output);
    assert_eq!(node_ids_id_only(&c), (1..=50).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, 1);
    assert_eq!(stats.base_nodes, 49);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// A surviving node keeps a large value while another node of its block is
/// modified to a second large value: the survivors' blocks must carry the
/// source table and the modified node must go into a block without it.
#[test]
fn a_large_value_beside_a_surviving_large_value_applies() {
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let (kept, new) = (value('k'), value('n'));
    write_base(&base, &[(10, &kept)], &[]);
    write_osc(&osc, &[], &[(20, Some(&new))]);
    merge(&base, &osc, &output);

    assert_eq!(node_notes(&output), vec![(10, kept), (20, new)]);
    let c = read_all_elements_id_only(&output);
    assert_eq!(node_ids_id_only(&c), (1..=50).collect::<Vec<i64>>());
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// A large string the diff removes no longer rides along with every block
/// of survivors. Node 1's 28 MiB value is modified away while survivors
/// alternate with 5 MiB values: re-cut blocks that each carried the source
/// table would repeat the dead value in every block; blocks carrying only
/// the strings they use come to about the size of the values.
#[test]
fn a_removed_large_value_is_not_repeated_across_recovered_blocks() {
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let dead: String = std::iter::repeat_n('d', 28 << 20).collect();
    write_base(&base, &[(1, &dead)], &[]);
    let values: Vec<(i64, String)> = (0..12)
        .map(|i| {
            let fill = char::from(b'a' + u8::try_from(i).unwrap());
            (3 + 2 * i, std::iter::repeat_n(fill, 5 << 20).collect())
        })
        .collect();
    let mut modify: Vec<(i64, Option<&str>)> = vec![(1, None)];
    modify.extend(values.iter().map(|(id, v)| (*id, Some(v.as_str()))));
    write_osc(&osc, &[], &modify);
    let stats = merge(&base, &osc, &output);

    assert_eq!(node_notes(&output), values);
    let c = read_all_elements_id_only(&output);
    assert_eq!(node_ids_id_only(&c), (1..=50).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, 13);
    assert_eq!(stats.base_nodes, 37);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// One large value shared by many creates that cannot share a block: each
/// create alone fits (the shared value plus its own small one), any two do
/// not, so every create is a block of its own and every block repeats the
/// shared value. The blob's output is several times the 256 MiB the re-cut
/// was once allowed to hold for one blob, and far past what a worker holds
/// before it streams; it is written whole and in order.
#[test]
fn a_large_value_shared_by_many_creates_fans_out() {
    const CREATES: i64 = 9;
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let shared: String = std::iter::repeat_n('s', 29 << 20).collect();
    let ids: Vec<i64> = (20..20 + CREATES).collect();
    write_base(&base, &[], &ids);
    let own: Vec<String> = ids
        .iter()
        .map(|&id| {
            let fill = char::from(b'a' + u8::try_from(id - 20).unwrap());
            std::iter::repeat_n(fill, 2 << 20).collect()
        })
        .collect();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<create>\n",
    );
    for (&id, v) in ids.iter().zip(&own) {
        writeln!(
            xml,
            "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\">\
             <tag k=\"note\" v=\"{shared}\"/><tag k=\"own\" v=\"{v}\"/></node>"
        )
        .unwrap();
    }
    xml.push_str("</create>\n</osmChange>\n");
    std::fs::write(&osc, xml).unwrap();
    let stats = merge(&base, &osc, &output);

    let notes = node_notes(&output);
    assert_eq!(notes.len(), ids.len());
    for ((id, note), &want) in notes.iter().zip(&ids) {
        assert_eq!(*id, want);
        assert!(*note == shared, "node {id} keeps the shared value");
    }
    let c = read_all_elements_id_only(&output);
    assert_eq!(node_ids_id_only(&c), (1..=50).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, u64::try_from(CREATES).unwrap());
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// A surviving way whose refreshed coordinates grew it is written with
/// only its own strings, not beside a large one the diff deleted. The way
/// blob holds way 1, tagged with a value just short of the payload limit,
/// and way 2, whose 20000 nodes all sit at one point (one-byte deltas). The
/// diff deletes way 1 and scatters way 2's nodes, so its location deltas
/// grow by about 160 KB: beside the source table it no longer fits a block,
/// alone it is a fraction of one.
#[test]
fn a_grown_survivor_beside_a_deleted_large_value_applies() {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    const NODES: i64 = 20_000;
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let refs: Vec<i64> = (1..=NODES).collect();
    {
        let file = std::fs::File::create(&base).unwrap();
        let mut writer = PbfWriter::new(file, Compression::None);
        let header = HeaderBuilder::new()
            .sorted()
            .optional_feature("LocationsOnWays")
            .build()
            .unwrap();
        writer.write_header(&header).unwrap();
        let mut bb = BlockBuilder::new();
        for chunk in refs.chunks(8000) {
            for &id in chunk {
                bb.add_node(id, 0, 0, std::iter::empty(), None);
            }
            writer
                .write_primitive_block(bb.take().unwrap().unwrap())
                .unwrap();
        }
        let dead: String = std::iter::repeat_n('d', (32 << 20) - 100_000).collect();
        bb.add_way_with_locations(1, [("note", dead.as_str())], &[1, 2], &[(0, 0); 2], None);
        let zeros = vec![(0, 0); refs.len()];
        bb.add_way_with_locations(2, [("highway", "road")], &refs, &zeros, None);
        writer
            .write_primitive_block(bb.take().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();
    }
    // Node i moves to an alternating far corner.
    let corner = |id: i64| if id % 2 == 0 { (80, 170) } else { (-80, -170) };
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<modify>\n",
    );
    for &id in &refs {
        let (lat, lon) = corner(id);
        writeln!(
            xml,
            "<node id=\"{id}\" lat=\"{lat}.0\" lon=\"{lon}.0\" version=\"2\"/>"
        )
        .unwrap();
    }
    xml.push_str("</modify>\n<delete>\n<way id=\"1\" version=\"2\"/>\n</delete>\n</osmChange>\n");
    std::fs::write(&osc, xml).unwrap();

    let opts = pbfhogg::apply_changes::MergeOptions {
        write: pbfhogg::write::WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..pbfhogg::write::WriteOptions::default()
        },
        locations_on_ways: Some(pbfhogg::apply_changes::LocationsOnWays::default()),
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    pbfhogg::apply_changes::merge(
        &base,
        &osc,
        crate::common::out(&output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .unwrap_or_else(|e| panic!("merge: {}", e.report()));

    let mut ways = Vec::new();
    for blob in BlobReader::from_path(&output).unwrap() {
        if let BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locations: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|l| (l.decimicro_lat().unwrap(), l.decimicro_lon().unwrap()))
                        .collect();
                    ways.push((w.id(), w.refs().collect::<Vec<i64>>(), locations));
                }
            }
        }
    }
    let expected: Vec<(i32, i32)> = refs
        .iter()
        .map(|&id| {
            let (lat, lon) = corner(id);
            (lat * 10_000_000, lon * 10_000_000)
        })
        .collect();
    assert_eq!(ways.len(), 1, "way 1 is deleted");
    assert_eq!(ways[0].0, 2);
    assert_eq!(ways[0].1, refs);
    assert!(ways[0].2 == expected, "way 2 carries the moved coordinates");
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// Two large creates inside the base blob's id range are written by the
/// worker's rewrite, side by side.
#[test]
fn two_large_inline_creates_apply() {
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let (first, second) = (value('p'), value('q'));
    write_base(&base, &[], &[20, 21]);
    write_osc(&osc, &[(20, Some(&first)), (21, Some(&second))], &[]);
    let stats = merge(&base, &osc, &output);

    assert_eq!(node_notes(&output), vec![(20, first), (21, second)]);
    let c = read_all_elements_id_only(&output);
    assert_eq!(node_ids_id_only(&c), (1..=50).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, 2);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// Two large creates past the last base node blob are one create run of
/// the drain, between small creates on either side.
#[test]
fn two_large_creates_in_one_drain_run_apply() {
    let dir = crate::common::test_scratch();
    let (base, osc, output) = (
        dir.path().join("base.osm.pbf"),
        dir.path().join("diff.osc"),
        dir.path().join("out.osm.pbf"),
    );
    let (first, second) = (value('x'), value('y'));
    write_base(&base, &[], &[]);
    write_osc(
        &osc,
        &[
            (100, None),
            (101, Some(&first)),
            (102, Some(&second)),
            (103, None),
        ],
        &[],
    );
    let stats = merge(&base, &osc, &output);

    assert_eq!(node_notes(&output), vec![(101, first), (102, second)]);
    let c = read_all_elements_id_only(&output);
    let mut expected: Vec<i64> = (1..=50).collect();
    expected.extend(100..=103);
    assert_eq!(node_ids_id_only(&c), expected);
    assert_eq!(stats.diff_nodes, 4);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}
