//! CLI-driven integration tests for `pbfhogg apply-changes` (merge).
//!
//! Replaces the removed library-API `merge.rs` test. Fixture PBFs are written via the
//! stable-allowlist writers (`write_test_pbf_sorted`,
//! `write_multi_block_test_pbf`, plus inline `BlockBuilder` helpers
//! for tests that need explicit metadata / multi-block layouts /
//! LocationsOnWays); `apply-changes` runs through `CliInvoker`;
//! output is verified by reading the resulting PBF with the
//! stable-allowlist readers. No imports from
//! `pbfhogg::apply_changes::*` - a rewrite of
//! `src/commands/apply_changes/` cannot break these tests by type
//! changes alone.
//!
//! Apply-changes is a high-traffic rewrite surface. The
//! descriptor-first streaming rewrite (landed 2026-04) moved large
//! parts of the internal API around; the CLI surface (`pbfhogg apply-changes
//! <base> <changes> -o <output> [--locations-on-ways] [-j N]
//! [--direct-io] [--io-uring] [--force]`) is the only thing this file
//! pins.
//!
//! Per-test stats assertions on `MergeStats` counters survive via
//! stderr substring matching against `MergeStats::write_summary`.
//! Where a stats assertion is redundant with an element-set
//! assertion it is dropped.
//!
//! Cross-validation against osmium on real data is `brokkr verify
//! merge`, not a test here.

#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, node_ids_with_coords as node_ids,
    read_all_elements_with_coords as read_all_elements, relation_ids_with_coords as relation_ids,
    way_ids_with_coords as way_ids, write_test_pbf_sorted,
};
use crate::invoker::{CliInvoker, CliOutput};
use flate2::write::GzEncoder;
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

pub(crate) const CRUD_OSC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="4" lat="55.0" lon="12.0" version="1">
      <tag k="name" v="four"/>
    </node>
  </create>
  <modify>
    <node id="2" lat="35.0" lon="45.0" version="2">
      <tag k="name" v="two-modified"/>
    </node>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
  <delete>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#;

pub(crate) fn write_crud_base(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "one")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 300_000_000,
                lon: 400_000_000,
                tags: vec![("name", "two")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 500_000_000,
                lon: 600_000_000,
                tags: vec![("name", "three")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "road")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );
}

#[derive(Default, Clone, Copy)]
struct ApplyOpts {
    locations_on_ways: bool,
    ignore_missing_nodes: bool,
    jobs: Option<usize>,
    direct_io: bool,
    io_uring: bool,
}

fn run_apply_changes(base: &Path, osc: &Path, output: &Path, opts: ApplyOpts) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("apply-changes")
        .arg(base)
        .arg(osc)
        .arg("-o")
        .arg(output)
        // Several tests run more than one apply against one output path;
        // the replacement itself is not what they test.
        .arg("--overwrite");
    if opts.locations_on_ways {
        cli = cli.arg("--locations-on-ways");
    }
    if opts.ignore_missing_nodes {
        cli = cli.arg("--ignore-missing-nodes");
    }
    if let Some(j) = opts.jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    if opts.direct_io {
        cli = cli.arg("--direct-io");
    }
    if opts.io_uring {
        cli = cli.arg("--io-uring");
    }
    // `--locations-on-ways` needs an indexed base whatever `--force` says,
    // so it refuses the flag.
    if !opts.locations_on_ways {
        cli = cli.arg("--force");
    }
    cli.run()
}

fn run_apply_ok(base: &Path, osc: &Path, output: &Path) -> CliOutput {
    let out = run_apply_changes(base, osc, output, ApplyOpts::default());
    assert!(
        out.status.success(),
        "apply-changes failed; stderr:\n{}",
        out.stderr_str(),
    );
    out
}

// ---------------------------------------------------------------------------
// Basic CRUD
// ---------------------------------------------------------------------------

#[test]
fn merge_basic_create_modify_delete() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_crud_base(&base);
    write_osc(&osc, CRUD_OSC);

    let out = run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 4]);
    assert_eq!(c.nodes[0].1, 100_000_000);
    assert_eq!(c.nodes[0].2, 200_000_000);
    assert_eq!(c.nodes[0].3, vec![("name".to_string(), "one".to_string())]);
    assert_eq!(c.nodes[1].1, 350_000_000);
    assert_eq!(c.nodes[1].2, 450_000_000);
    assert_eq!(
        c.nodes[1].3,
        vec![("name".to_string(), "two-modified".to_string())]
    );
    assert_eq!(c.nodes[2].1, 550_000_000);
    assert_eq!(c.nodes[2].2, 120_000_000);
    assert_eq!(c.nodes[2].3, vec![("name".to_string(), "four".to_string())]);

    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(c.ways[0].1, vec![1, 2]);
    assert_eq!(
        c.ways[0].2,
        vec![("highway".to_string(), "primary".to_string())]
    );

    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(
        c.relations[0].2,
        vec![("type".to_string(), "multipolygon".to_string())]
    );

    assert!(
        out.stderr_str().contains("Deleted: 1"),
        "stats line missing 'Deleted: 1'; stderr:\n{}",
        out.stderr_str(),
    );
}

#[test]
fn merge_create_between_existing_ids() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 10,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 30,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="15" lat="1.0" lon="2.0" version="1"/>
    <node id="25" lat="3.0" lon="4.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    let ids = node_ids(&c);
    assert_eq!(ids.len(), 5);
    assert!(ids.contains(&10));
    assert!(ids.contains(&15));
    assert!(ids.contains(&20));
    assert!(ids.contains(&25));
    assert!(ids.contains(&30));
}

#[test]
fn merge_create_beyond_max_id() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="10.0" lon="20.0" version="1">
      <tag k="name" v="far"/>
    </node>
    <node id="200" lat="30.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2, 3, 100, 200]);
    assert_eq!(c.nodes[3].3, vec![("name".to_string(), "far".to_string())]);
}

/// Multi-block base PBF where the diff only affects the middle block.
/// Exercises block 1 passthrough, block 2 rewritten, block 3 passthrough.
#[test]
fn merge_multi_block_partial_rewrite() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    {
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        bb.add_node(1, 100_000_000, 100_000_000, [("block", "1")], None);
        bb.add_node(
            2,
            200_000_000,
            200_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        bb.add_node(
            3,
            300_000_000,
            300_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        bb.add_node(10, 100_000_000, 100_000_000, [("name", "old")], None);
        bb.add_node(
            11,
            110_000_000,
            110_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        bb.add_node(
            12,
            120_000_000,
            120_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        bb.add_node(20, 200_000_000, 200_000_000, [("block", "3")], None);
        bb.add_node(
            21,
            210_000_000,
            210_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        bb.add_node(
            22,
            220_000_000,
            220_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        writer.flush().expect("flush");
    }

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="10" lat="89.0" lon="99.0" version="2">
      <tag k="name" v="new"/>
    </node>
  </modify>
  <delete>
    <node id="11" version="2"/>
  </delete>
</osmChange>"#,
    );

    let out = run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(
        c.nodes[0],
        (
            1,
            100_000_000,
            100_000_000,
            vec![("block".to_string(), "1".to_string())]
        )
    );
    assert_eq!(c.nodes[1].0, 2);
    assert_eq!(c.nodes[2].0, 3);
    assert_eq!(c.nodes[3].0, 10);
    // 89 degrees, not 99: a latitude past 90 is not a place, and the OSC
    // parser now refuses it rather than storing it as an ordinary node.
    assert_eq!(c.nodes[3].1, 890_000_000);
    assert_eq!(c.nodes[3].3, vec![("name".to_string(), "new".to_string())]);
    assert_eq!(c.nodes[4].0, 12);
    assert_eq!(
        c.nodes[5],
        (
            20,
            200_000_000,
            200_000_000,
            vec![("block".to_string(), "3".to_string())]
        )
    );
    assert_eq!(c.nodes[6].0, 21);
    assert_eq!(c.nodes[7].0, 22);
    assert_eq!(c.nodes.len(), 8);

    let stderr = out.stderr_str();
    assert!(stderr.contains("Deleted: 1"), "stats: {stderr}");
    // "1 rewritten" appears in the Blobs line; blocks 1 and 3 land
    // in the "passthrough" count.
    assert!(stderr.contains("1 rewritten"), "stats: {stderr}");
}

#[test]
fn merge_nodes_only_diff_ways_passthrough() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![("old", "tag")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2, 3],
                tags: vec![("highway", "path")],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![3, 2, 1],
                tags: vec![("building", "yes")],
                meta: None,
            },
        ],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="11.0" lon="22.0" version="2">
      <tag k="new" v="tag"/>
    </node>
  </modify>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes[1].0, 2);
    assert_eq!(c.nodes[1].1, 110_000_000);
    assert_eq!(c.nodes[1].3, vec![("new".to_string(), "tag".to_string())]);
    assert_eq!(way_ids(&c), vec![10, 20]);
    assert_eq!(c.ways[0].1, vec![1, 2, 3]);
    assert_eq!(
        c.ways[0].2,
        vec![("highway".to_string(), "path".to_string())]
    );
    assert_eq!(c.ways[1].1, vec![3, 2, 1]);
    assert_eq!(
        c.ways[1].2,
        vec![("building".to_string(), "yes".to_string())]
    );
}

#[test]
fn merge_ways_only_diff() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "road")],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![2, 1],
                tags: vec![("name", "delete me")],
                meta: None,
            },
            TestWay {
                id: 30,
                refs: vec![1, 2, 1],
                tags: vec![("building", "yes")],
                meta: None,
            },
        ],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <way id="15" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="new" v="way"/>
    </way>
  </create>
  <delete>
    <way id="20" version="2"/>
  </delete>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(way_ids(&c), vec![10, 15, 30]);
    assert_eq!(
        c.ways[0].2,
        vec![("highway".to_string(), "road".to_string())]
    );
    assert_eq!(c.ways[1].2, vec![("new".to_string(), "way".to_string())]);
    assert_eq!(
        c.ways[2].2,
        vec![("building".to_string(), "yes".to_string())]
    );
}

#[test]
fn merge_relations_only_diff() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember {
                    id: MemberId::Node(1),
                    role: "stop",
                }],
                tags: vec![("type", "route")],
                meta: None,
            },
        ],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="150" version="1">
      <member type="way" ref="10" role="inner"/>
      <tag k="type" v="boundary"/>
    </relation>
  </create>
  <modify>
    <relation id="200" version="2">
      <member type="node" ref="1" role="platform"/>
      <member type="way" ref="10" role=""/>
      <tag k="type" v="public_transport"/>
    </relation>
  </modify>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(relation_ids(&c), vec![100, 150, 200]);
    assert_eq!(
        c.relations[0].2,
        vec![("type".to_string(), "multipolygon".to_string())]
    );
    assert_eq!(
        c.relations[1].1,
        vec![(10, "way".to_string(), "inner".to_string())]
    );
    assert_eq!(
        c.relations[1].2,
        vec![("type".to_string(), "boundary".to_string())]
    );
    assert_eq!(c.relations[2].1.len(), 2);
    assert_eq!(
        c.relations[2].1[0],
        (1, "node".to_string(), "platform".to_string())
    );
    assert_eq!(c.relations[2].1[1], (10, "way".to_string(), String::new()));
    assert_eq!(
        c.relations[2].2,
        vec![("type".to_string(), "public_transport".to_string())]
    );
}

#[test]
fn merge_all_types() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("delete", "me")],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![2, 1],
                tags: vec![("keep", "me")],
                meta: None,
            },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                tags: vec![("old", "tags")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![],
                tags: vec![("type", "site")],
                meta: None,
            },
        ],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="3" lat="1.0" lon="2.0" version="1">
      <tag k="new" v="node"/>
    </node>
  </create>
  <delete>
    <way id="10" version="2"/>
  </delete>
  <modify>
    <relation id="200" version="2">
      <member type="node" ref="3" role="label"/>
      <tag k="type" v="site"/>
      <tag k="name" v="updated"/>
    </relation>
  </modify>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![20]);
    assert_eq!(relation_ids(&c), vec![100, 200]);
    assert_eq!(c.nodes[2].3, vec![("new".to_string(), "node".to_string())]);
    assert_eq!(c.ways[0].2, vec![("keep".to_string(), "me".to_string())]);
    assert_eq!(
        c.relations[0].2,
        vec![("old".to_string(), "tags".to_string())]
    );
    assert_eq!(
        c.relations[1].1,
        vec![(3, "node".to_string(), "label".to_string())]
    );
    assert!(
        c.relations[1]
            .2
            .contains(&("name".to_string(), "updated".to_string()))
    );
}

/// Diff deletes every element in a block. The rewrite should produce
/// no output for that block.
#[test]
fn merge_delete_entire_block() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    {
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        bb.add_node(
            1,
            100_000_000,
            100_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        bb.add_node(
            2,
            200_000_000,
            200_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        bb.add_node(
            3,
            300_000_000,
            300_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        bb.add_node(10, 100_000_000, 100_000_000, [("survivor", "yes")], None);
        bb.add_node(
            11,
            110_000_000,
            110_000_000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        bb.add_way(100, [("highway", "path")], &[10, 11], None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        writer.flush().expect("flush");
    }

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="1" version="2"/>
    <node id="2" version="2"/>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#,
    );

    let out = run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![10, 11]);
    assert_eq!(
        c.nodes[0].3,
        vec![("survivor".to_string(), "yes".to_string())]
    );
    assert_eq!(way_ids(&c), vec![100]);
    assert_eq!(c.ways[0].1, vec![10, 11]);
    assert!(
        out.stderr_str().contains("Deleted: 3"),
        "stats: {}",
        out.stderr_str(),
    );
}

#[test]
fn merge_stats_accuracy() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 0,
                lon: 0,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![],
            meta: None,
        }],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="4" lat="1.0" lon="2.0" version="1"/>
  </create>
  <modify>
    <node id="2" lat="5.0" lon="6.0" version="2"/>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="4"/>
    </way>
  </modify>
  <delete>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#,
    );

    let out = run_apply_ok(&base, &osc, &output);
    let stderr = out.stderr_str();
    // Format: "  Base: {nodes} nodes, {ways} ways, {relations} relations"
    //         "  Diff: {nodes} nodes, {ways} ways, {relations} relations"
    //         "  Deleted: {N}"
    assert!(
        stderr.contains("Base: 1 nodes, 0 ways, 1 relations"),
        "stats: {stderr}"
    );
    assert!(
        stderr.contains("Diff: 2 nodes, 1 ways, 0 relations"),
        "stats: {stderr}"
    );
    assert!(stderr.contains("Deleted: 1"), "stats: {stderr}");
}

/// Metadata (version/timestamp/changeset/uid/user) from base PBF nodes
/// survives a merge, and OSC replacements carry the metadata attributes
/// from the OSC element (node 2's `version="4"` below).
///
/// # A known collapse this test PINS but does not endorse
///
/// The `uid == 0` assertion below records what the code does, not what
/// it should do. The OSC element carries no `uid` at all, and the
/// written PBF records `uid = 0` - a real user id, indistinguishable
/// from a genuine user 0. This is the same absent-versus-zero collapse
/// that has been fixed for coordinates, for `push_info_attrs`, and for
/// OSC `version`; it survives here because `osc::compact::ArenaMeta`
/// and `block_builder::Metadata` have no per-field presence bit, so
/// "attribute absent" and "attribute is zero" share one representation
/// all the way through the PBF write path.
///
/// Closing it means per-field presence in both types - a change across
/// the PBF write path, not the change stream - so it is filed rather
/// than done here. Do NOT read this assertion as the intended contract:
/// when the representation gains presence bits, this expectation must
/// change to "no uid field at all", and the assertion message is worded
/// so that a future reader is not misled into preserving the bug.
#[test]
fn merge_metadata_preservation() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    {
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        bb.add_node(
            1,
            100_000_000,
            200_000_000,
            [("name", "one")],
            Some(&block_builder::Metadata {
                version: 5,
                timestamp: 1_700_000_000,
                changeset: 12345,
                uid: 42,
                user: "mapper",
                visible: true,
            }),
        );
        bb.add_node(
            2,
            300_000_000,
            400_000_000,
            [("name", "two")],
            Some(&block_builder::Metadata {
                version: 3,
                timestamp: 1_600_000_000,
                changeset: 67890,
                uid: 7,
                user: "editor",
                visible: true,
            }),
        );
        bb.add_node(
            3,
            500_000_000,
            600_000_000,
            [("name", "three")],
            Some(&block_builder::Metadata {
                version: 1,
                timestamp: 1_500_000_000,
                changeset: 11111,
                uid: 99,
                user: "creator",
                visible: true,
            }),
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");
        writer.flush().expect("flush");
    }

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="35.0" lon="45.0" version="4">
      <tag k="name" v="two-modified"/>
    </node>
  </modify>
</osmChange>"#,
    );

    run_apply_ok(&base, &osc, &output);

    let reader = BlobReader::from_path(&output).expect("open pbf");
    let mut node_meta: Vec<(i64, Option<(i32, i32)>)> = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::DenseNode(dn) = element {
                    let meta = dn.info().map(|info| (info.version(), info.uid()));
                    node_meta.push((dn.id(), meta));
                }
            }
        }
    }

    assert_eq!(node_meta.len(), 3);
    assert_eq!(node_meta[0].0, 1);
    let (version, uid) = node_meta[0].1.expect("node 1 meta");
    assert_eq!(version, 5);
    assert_eq!(uid, 42);
    assert_eq!(node_meta[1].0, 2);
    let (version, uid) = node_meta[1].1.expect("node 2 meta");
    assert_eq!(version, 4, "OSC-sourced version must be carried");
    assert_eq!(
        uid, 0,
        "KNOWN COLLAPSE, not a contract: an absent uid is recorded as uid 0 because \
         ArenaMeta and block_builder::Metadata have no presence bit for it. See this \
         test's doc comment - when that is fixed, this must become 'no uid field'."
    );
    assert_eq!(node_meta[2].0, 3);
    let (version, uid) = node_meta[2].1.expect("node 3 meta");
    assert_eq!(version, 1);
    assert_eq!(uid, 99);
}

// ---------------------------------------------------------------------------
// LocationsOnWays
// ---------------------------------------------------------------------------

/// Helper: write a PBF with LocationsOnWays header feature and sorted
/// flag. Ways are written with inline node coordinates via
/// `add_way_with_locations`.
#[allow(clippy::type_complexity)]
fn write_test_pbf_with_locations(
    path: &Path,
    nodes: &[TestNode],
    ways: &[(i64, Vec<i64>, Vec<(i32, i32)>, Vec<(&str, &str)>)],
    relations: &[TestRelation],
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for &(id, ref refs, ref locations, ref tags) in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let tag_refs: Vec<(&str, &str)> = tags.iter().map(|&(k, v)| (k, v)).collect();
        bb.add_way_with_locations(id, tag_refs.iter().copied(), refs, locations, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let members: Vec<block_builder::MemberData<'_>> = r
            .members
            .iter()
            .map(|m| block_builder::MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

fn read_way_locations(path: &Path) -> Vec<(i64, Vec<(i32, i32)>)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut result = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    result.push((w.id(), locs));
                }
            }
        }
    }
    result
}

#[test]
fn merge_locations_on_ways_basic() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_with_locations(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 300_000_000,
                lon: 400_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 500_000_000,
                lon: 600_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            (
                10,
                vec![1, 2],
                vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)],
                vec![("highway", "road")],
            ),
            (
                20,
                vec![2, 3],
                vec![(300_000_000, 400_000_000), (500_000_000, 600_000_000)],
                vec![("highway", "path")],
            ),
        ],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="3"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
  <create>
    <way id="30" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <nd ref="3"/>
      <tag k="highway" v="footway"/>
    </way>
  </create>
</osmChange>"#,
    );

    let out = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert!(
        out.status.success(),
        "apply-changes --locations-on-ways failed; stderr:\n{}",
        out.stderr_str(),
    );

    let stderr = out.stderr_str();
    assert!(stderr.contains("nodes needed"), "loc stats: {stderr}");
    assert!(stderr.contains("0 missing"), "loc stats: {stderr}");
    // One node blob and one (rewritten) way blob: only the node blob is
    // scanned for coordinates. This counter used to report every blob a
    // worker touched, of any kind.
    assert!(
        stderr.contains(" 1 node blobs scanned"),
        "loc stats: {stderr}"
    );

    let header = crate::common::read_header(&output);
    assert!(
        header.has_locations_on_ways(),
        "output must have LocationsOnWays"
    );

    let locs = read_way_locations(&output);
    assert_eq!(locs.len(), 3);
    let way10 = locs.iter().find(|(id, _)| *id == 10).expect("way 10");
    assert_eq!(
        way10.1,
        vec![(100_000_000, 200_000_000), (500_000_000, 600_000_000)]
    );
    let way20 = locs.iter().find(|(id, _)| *id == 20).expect("way 20");
    assert_eq!(
        way20.1,
        vec![(300_000_000, 400_000_000), (500_000_000, 600_000_000)]
    );
    let way30 = locs.iter().find(|(id, _)| *id == 30).expect("way 30");
    assert_eq!(
        way30.1,
        vec![
            (100_000_000, 200_000_000),
            (300_000_000, 400_000_000),
            (500_000_000, 600_000_000)
        ]
    );
}

#[test]
fn merge_locations_on_ways_osc_node_coords() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_with_locations(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 300_000_000,
                lon: 400_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[(
            10,
            vec![1, 2],
            vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)],
            vec![],
        )],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="55.0" lon="12.0" version="2"/>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
</osmChange>"#,
    );

    let out = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert!(out.status.success(), "stderr:\n{}", out.stderr_str());

    let locs = read_way_locations(&output);
    let way10 = locs.iter().find(|(id, _)| *id == 10).expect("way 10");
    assert_eq!(
        way10.1,
        vec![(100_000_000, 200_000_000), (550_000_000, 120_000_000)]
    );
}

#[test]
fn merge_locations_on_ways_rejects_missing_node_unless_explicitly_ignored() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_with_locations(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[(1, vec![1], vec![(100_000_000, 200_000_000)], vec![])],
        &[],
    );
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6"><create><way id="10" version="1"><nd ref="999"/></way></create></osmChange>"#,
    );

    let rejected = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert_eq!(rejected.status.code(), Some(2));
    rejected.assert_stderr_contains("1 way node references could not be resolved");
    assert!(
        !output.exists(),
        "guarded output must discard the failed merge"
    );

    let accepted = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ignore_missing_nodes: true,
            ..ApplyOpts::default()
        },
    );
    assert!(
        accepted.status.success(),
        "stderr:\n{}",
        accepted.stderr_str()
    );
    let locations = read_way_locations(&output);
    let way = locations.iter().find(|(id, _)| *id == 10).expect("way 10");
    assert_eq!(way.1, vec![(0, 0)]);
}

/// A duplicate node record in the base must not conceal a genuinely
/// missing one.
///
/// The guard above compares "required" against "resolved". Resolution used
/// to be counted per extracted tuple, and the per-worker coordinate map is
/// keyed by id - so a base carrying node 1 twice contributed two to the
/// count while adding one key. Two required ids, one of them absent, and
/// the arithmetic balanced: the guard passed and the missing node silently
/// took the (0,0) fallback that `--ignore-missing-nodes` is supposed to be
/// the only way to get.
///
/// The fixture therefore needs BOTH a duplicated present id and a genuinely
/// absent one; either alone leaves the counting bug invisible.
#[test]
fn merge_locations_on_ways_duplicate_base_records_cannot_mask_a_missing_node() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    let dup = || TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![],
        meta: None,
    };
    write_test_pbf_with_locations(
        &base,
        &[dup(), dup()],
        &[(1, vec![1], vec![(100_000_000, 200_000_000)], vec![])],
        &[],
    );
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6"><create><way id="10" version="1"><nd ref="1"/><nd ref="999"/></way></create></osmChange>"#,
    );

    let rejected = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "the duplicate record must not pay for node 999. stderr:\n{}",
        rejected.stderr_str()
    );
    rejected.assert_stderr_contains("1 way node references could not be resolved");
    assert!(!output.exists(), "failed merge must leave no output");
}

/// Merge with --locations-on-ways requires LocationsOnWays in base PBF.
#[test]
fn merge_locations_on_ways_requires_base_feature() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 0,
            lon: 0,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="2" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#,
    );

    let out = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert!(
        !out.status.success(),
        "should fail without LocationsOnWays in base; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("LocationsOnWays"),
        "error should mention LocationsOnWays; stderr:\n{}",
        out.stderr_str(),
    );
}

fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)]
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn pb_len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
    pb_varint(u64::from((field << 3) | 2), out);
    pb_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

#[allow(clippy::cast_sign_loss)]
fn pb_sint(field: u32, value: i64, out: &mut Vec<u8>) {
    pb_varint(u64::from(field << 3), out);
    pb_varint(((value << 1) ^ (value >> 63)) as u64, out);
}

/// A serialized PrimitiveBlock holding one group of plain (non-dense)
/// `Node` messages, `(id, lat, lon)` in decimicrodegrees (the default
/// granularity of 100 nanodegrees makes the raw value the decimicro value).
/// `BlockBuilder` only writes DenseNodes, so this is built by hand.
fn plain_node_block(nodes: &[(i64, i64, i64)]) -> Vec<u8> {
    let mut group = Vec::new();
    for &(id, lat, lon) in nodes {
        let mut node = Vec::new();
        pb_sint(1, id, &mut node);
        pb_sint(8, lat, &mut node);
        pb_sint(9, lon, &mut node);
        pb_len_delimited(1, &node, &mut group);
    }
    let mut block = Vec::new();
    // String table with the mandatory empty entry 0.
    pb_len_delimited(1, &[0x0A, 0x00], &mut block);
    pb_len_delimited(2, &group, &mut block);
    block
}

/// A sorted LocationsOnWays PBF whose node blob is `node_block` (raw
/// PrimitiveBlock bytes) and whose ways are one BlockBuilder blob, written
/// with or without indexdata.
#[allow(clippy::type_complexity)]
fn write_low_pbf_raw_nodes(
    path: &Path,
    node_block: &[u8],
    ways: &[(i64, Vec<i64>, Vec<(i32, i32)>)],
    index_way_blob: bool,
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    writer.write_primitive_block(node_block).expect("node blob");

    let mut bb = BlockBuilder::new();
    for (id, refs, locations) in ways {
        bb.add_way_with_locations(*id, std::iter::empty(), refs, locations, None);
    }
    let bytes = bb.take().expect("take").expect("way block");
    if index_way_blob {
        writer.write_primitive_block(bytes).expect("way blob");
    } else {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("way blob");
    }
    writer.flush().expect("flush");
}

const LOW_WAY_EDIT_OSC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <way id="10" version="2"><nd ref="1"/><nd ref="3"/></way>
  </modify>
  <create>
    <way id="30" version="1"><nd ref="2"/></way>
  </create>
</osmChange>"#;

fn low_opts() -> ApplyOpts {
    ApplyOpts {
        locations_on_ways: true,
        ..ApplyOpts::default()
    }
}

/// Plain (non-dense) `Node` messages are a legal node encoding. Under
/// --locations-on-ways their coordinates used to be dropped wholesale: the
/// dense-only extractor rejected the blob and the error was swallowed, so
/// the run failed later with a cause-less "N referenced node(s) could not
/// be resolved".
#[test]
fn merge_locations_on_ways_resolves_plain_node_messages() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_low_pbf_raw_nodes(
        &base,
        &plain_node_block(&[
            (1, 100_000_000, 200_000_000),
            (2, 300_000_000, 400_000_000),
            (3, 500_000_000, 600_000_000),
        ]),
        &[(
            10,
            vec![1, 2],
            vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)],
        )],
        true,
    );
    write_osc(&osc, LOW_WAY_EDIT_OSC);

    let out = run_apply_changes(&base, &osc, &output, low_opts());
    assert!(out.status.success(), "stderr:\n{}", out.stderr_str());
    out.assert_stderr_contains("0 missing");

    let locs = read_way_locations(&output);
    let way10 = locs.iter().find(|(id, _)| *id == 10).expect("way 10");
    assert_eq!(
        way10.1,
        vec![(100_000_000, 200_000_000), (500_000_000, 600_000_000)]
    );
    let way30 = locs.iter().find(|(id, _)| *id == 30).expect("way 30");
    assert_eq!(way30.1, vec![(300_000_000, 400_000_000)]);
}

/// A node coordinate that cannot be decoded is a hard error naming the
/// blob, not a silently empty coordinate table. It also must not hang:
/// under --locations-on-ways a node blob that never reaches the drain
/// used to leave the node-to-way barrier unfired forever.
#[test]
fn merge_locations_on_ways_fails_loudly_on_undecodable_node_coordinates() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_low_pbf_raw_nodes(
        &base,
        // Node 4 is referenced by no way: validation must not depend on
        // whether a node is needed.
        &plain_node_block(&[
            (1, 100_000_000, 200_000_000),
            (2, 300_000_000, 400_000_000),
            (3, 500_000_000, 600_000_000),
            (4, i64::from(i32::MAX) + 1, 0),
        ]),
        &[(10, vec![1, 3], vec![(0, 0), (0, 0)])],
        true,
    );
    write_osc(&osc, LOW_WAY_EDIT_OSC);

    let out = run_apply_changes(&base, &osc, &output, low_opts());
    assert!(!out.status.success(), "stderr:\n{}", out.stderr_str());
    out.assert_stderr_contains("apply-changes: base blob");
    // The location is the walker's true blob ordinal (the header is blob 0),
    // not the pipeline's data-blob sequence number.
    out.assert_stderr_contains("base.osm.pbf, blob 1, at byte offset");
    out.assert_stderr_contains("does not fit in an i32");
    assert!(!output.exists(), "failed merge must leave no output");
}

/// A base whose way blob lacks indexdata while its node blob carries it is
/// partially indexed, and the scanner's header walk refuses it at the way
/// blob. Without that refusal the scanner would route the unindexed way blob
/// with the node blobs, ahead of the node-to-way barrier, where the rewrite
/// used to write every way in it without coordinates under a header that
/// still claims LocationsOnWays (the worker now refuses that too, as a
/// backstop this fixture can no longer reach).
#[test]
fn merge_locations_on_ways_refuses_a_partially_indexed_base() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    let mut nodes = BlockBuilder::new();
    nodes.add_node(1, 100_000_000, 200_000_000, std::iter::empty(), None);
    nodes.add_node(2, 300_000_000, 400_000_000, std::iter::empty(), None);
    nodes.add_node(3, 500_000_000, 600_000_000, std::iter::empty(), None);
    let node_block = nodes.take().expect("take").expect("node block").to_vec();

    write_low_pbf_raw_nodes(
        &base,
        &node_block,
        &[
            (
                10,
                vec![1, 2],
                vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)],
            ),
            (
                20,
                vec![2, 3],
                vec![(300_000_000, 400_000_000), (500_000_000, 600_000_000)],
            ),
        ],
        false,
    );
    write_osc(&osc, LOW_WAY_EDIT_OSC);

    let out = run_apply_changes(&base, &osc, &output, low_opts());
    assert_eq!(
        out.status.code(),
        Some(2),
        "a partially indexed base must be refused; stderr:\n{}",
        out.stderr_str()
    );
    out.assert_stderr_contains("the file is partially indexed");
    // Located at the way blob: the header is blob 0, the node blob blob 1.
    out.assert_stderr_contains("base.osm.pbf, blob 2, at byte offset");
    assert!(!output.exists(), "failed merge must leave no output");
}

/// A way referencing a node the same diff deletes has an unresolved
/// reference: the node is gone from the output. Its pre-deletion base
/// coordinates used to satisfy the missing-node guard.
#[test]
fn merge_locations_on_ways_counts_a_deleted_node_reference_as_missing() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_with_locations(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 300_000_000,
                lon: 400_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[(1, vec![1], vec![(100_000_000, 200_000_000)], vec![])],
        &[],
    );
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete><node id="2" version="2"/></delete>
  <create><way id="10" version="1"><nd ref="1"/><nd ref="2"/></way></create>
</osmChange>"#,
    );

    let out = run_apply_changes(&base, &osc, &output, low_opts());
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    out.assert_stderr_contains("1 way node references could not be resolved");
    // The library names no flag; the CLI adds the switch that allows it.
    out.assert_stderr_contains("--ignore-missing-nodes");
    assert!(!output.exists(), "failed merge must leave no output");
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

/// Create elements with IDs that fall in gaps between base blobs.
#[test]
fn merge_gap_creates_between_blobs() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 10,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 30,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 100,
                refs: vec![10, 20],
                tags: vec![("highway", "road")],
                meta: None,
            },
            TestWay {
                id: 200,
                refs: vec![20, 30],
                tags: vec![("highway", "path")],
                meta: None,
            },
        ],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="50.0" lon="10.0" version="1"/>
    <node id="15" lat="51.0" lon="11.0" version="1"/>
    <node id="35" lat="52.0" lon="12.0" version="1"/>
    <way id="50" version="1">
      <nd ref="10"/>
      <nd ref="20"/>
    </way>
    <way id="150" version="1">
      <nd ref="20"/>
      <nd ref="30"/>
    </way>
  </create>
</osmChange>"#,
    );

    let out = run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    let nids = node_ids(&c);
    assert_eq!(nids, vec![5, 10, 15, 20, 30, 35], "node order");
    let wids = way_ids(&c);
    assert_eq!(wids, vec![50, 100, 150, 200], "way order");

    let stderr = out.stderr_str();
    assert!(stderr.contains("Base: 3 nodes, 2 ways"), "stats: {stderr}");
    assert!(stderr.contains("Diff: 3 nodes, 2 ways"), "stats: {stderr}");
}

/// Base has only nodes and relations (no ways). Diff creates ways.
#[test]
fn merge_type_transition_node_to_relation_skipping_ways() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Node(1),
                role: "label",
            }],
            tags: vec![("type", "boundary")],
            meta: None,
        }],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <way id="50" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="residential"/>
    </way>
    <way id="51" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </create>
</osmChange>"#,
    );

    let out = run_apply_ok(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(way_ids(&c), vec![50, 51]);
    assert_eq!(relation_ids(&c), vec![100]);

    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Base: 2 nodes, 0 ways, 1 relations"),
        "stats: {stderr}"
    );
    assert!(
        stderr.contains("Diff: 0 nodes, 2 ways, 0 relations"),
        "stats: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

/// The version-precedence gate, and its escape hatch, reaching
/// `apply-changes` through the CLI.
///
/// The gate lives in two places - `CompactDiffOverlay` for this command
/// and the `--simplify` dedupe for `merge-changes` - and only the second
/// half had a CLI test. So `apply-changes --allow-version-regression`
/// existed as a flag with nothing at all asserting that it reached
/// `CompactDiffOverlay::allow_version_regression`: unwiring the call in
/// `rewrite.rs` broke no test. Every arm is pinned here - the refusal,
/// the escape hatch, and a genuine advance - because a gate that is
/// merely inert also passes "the flag parsed".
#[test]
fn merge_refuses_a_version_regression_and_honours_the_escape_hatch() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    // A delete followed by a stale modify: before the versioned
    // tombstone the delete erased the stored version and version 8 was
    // resurrected past a gate meant to stop exactly that.
    let osc = dir.path().join("regress.osc.gz");
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete><node id="1" version="9"/></delete>
  <modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>
</osmChange>"#,
    );
    let out = run_apply_changes(&base, &osc, &output, ApplyOpts::default());
    assert!(
        !out.status.success(),
        "a stale change after a delete must be refused"
    );
    assert!(
        out.stderr_str().contains("is lower than version 9"),
        "error must name the regression; stderr:\n{}",
        out.stderr_str()
    );

    // The escape hatch restores the old positional rule.
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .arg("--overwrite")
        .arg("--allow-version-regression")
        .arg("--force")
        .run();
    assert!(
        out.status.success(),
        "--allow-version-regression must accept it; stderr:\n{}",
        out.stderr_str()
    );

    // And a genuine advance was never in question.
    let forward = dir.path().join("forward.osc.gz");
    write_osc(
        &forward,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete><node id="1" version="9"/></delete>
  <create><node id="1" lat="8.0" lon="8.0" version="10"/></create>
</osmChange>"#,
    );
    run_apply_ok(&base, &forward, &output);
}

/// Two overlapping diffs, concatenated by `merge-changes` in its default
/// full-stream mode and applied in one run: equal versions are admitted
/// and the later change wins. The overlap is the real germany 4705/4706
/// shape - a node modified at version 12, then deleted at version 12 as
/// it leaves the extract - plus a verbatim repeat and an equal version
/// with different content. Each used to fail the whole apply.
#[test]
fn merge_admits_equal_versions_from_overlapping_diffs() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let first = dir.path().join("4705.osc.gz");
    let second = dir.path().join("4706.osc.gz");
    let combined = dir.path().join("combined.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 200_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 300_000_000,
                lon: 300_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_osc(
        &first,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="50.6575433" lon="9.511884" version="12" timestamp="2026-02-24T21:21:17Z"/>
    <node id="2" lat="21.0" lon="21.0" version="3"/>
    <node id="3" lat="31.0" lon="31.0" version="5"/>
  </modify>
</osmChange>"#,
    );
    write_osc(
        &second,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="21.0" lon="21.0" version="3"/>
    <node id="3" lat="32.0" lon="32.0" version="5"/>
  </modify>
  <delete>
    <node id="1" lat="50.6575433" lon="9.511884" version="12" timestamp="2026-02-24T21:21:17Z"/>
  </delete>
</osmChange>"#,
    );
    CliInvoker::new()
        .arg("merge-changes")
        .arg(&first)
        .arg(&second)
        .arg("-o")
        .arg(&combined)
        .assert_success();

    run_apply_ok(&base, &combined, &output);
    let c = read_all_elements(&output);
    assert_eq!(
        node_ids(&c),
        vec![2, 3],
        "the equal-version delete must win"
    );
    assert_eq!(
        c.nodes[0].1, 210_000_000,
        "the verbatim repeat applies once"
    );
    assert_eq!(
        c.nodes[1].1, 320_000_000,
        "an equal version with different content is won by the later change"
    );

    // A lower version after the equal ones is still refused, and the
    // escape hatch still admits it.
    let stale = dir.path().join("stale.osc.gz");
    write_osc(
        &stale,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify><node id="3" lat="33.0" lon="33.0" version="4"/></modify>
</osmChange>"#,
    );
    let combined_stale = dir.path().join("combined-stale.osc.gz");
    CliInvoker::new()
        .arg("merge-changes")
        .arg(&first)
        .arg(&second)
        .arg(&stale)
        .arg("-o")
        .arg(&combined_stale)
        .assert_success();
    let out = run_apply_changes(&base, &combined_stale, &output, ApplyOpts::default());
    assert!(!out.status.success(), "a lower claim must still be refused");
    assert!(
        out.stderr_str().contains("is lower than version 5"),
        "error must name the regression; stderr:\n{}",
        out.stderr_str()
    );
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&combined_stale)
        .arg("-o")
        .arg(&output)
        .arg("--overwrite")
        .arg("--allow-version-regression")
        .arg("--force")
        .run();
    assert!(
        out.status.success(),
        "--allow-version-regression must accept it; stderr:\n{}",
        out.stderr_str()
    );
    let c = read_all_elements(&output);
    assert_eq!(c.nodes[1].1, 330_000_000, "the escape hatch is positional");
}

/// `apply-changes --force --locations-on-ways` can never take effect (an
/// indexed base makes `--force` a no-op, an unindexed one is unsound under
/// `-l`), so the combination is a usage error, before any input is opened.
#[test]
fn merge_refuses_force_with_locations_on_ways_before_any_input() {
    let dir = crate::common::test_scratch();
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(dir.path().join("no-such-base.osm.pbf"))
        .arg(dir.path().join("no-such-diff.osc.gz"))
        .arg("-o")
        .arg(dir.path().join("out.osm.pbf"))
        .args(["--locations-on-ways", "--force"])
        .run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("--force")
            && stderr.contains("--locations-on-ways")
            && !stderr.contains("no-such-base"),
        "expected the argument refusal, not an input error; stderr:\n{stderr}",
    );
}

/// Without `--force`, a non-indexed base under `--locations-on-ways` is
/// refused by the indexdata gate, which points at the indexed-generation
/// workflow.
#[test]
fn merge_refuses_locations_on_ways_on_non_indexed() {
    use crate::common::write_test_pbf_non_indexed;

    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_non_indexed(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 300_000_000,
                lon: 400_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "road")],
            meta: None,
        }],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6"></osmChange>"#,
    );

    let out = run_apply_changes(
        &base,
        &osc,
        &output,
        ApplyOpts {
            locations_on_ways: true,
            ..ApplyOpts::default()
        },
    );
    assert!(
        !out.status.success(),
        "must refuse --locations-on-ways on a non-indexed base; stderr:\n{}",
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("pbfhogg cat"),
        "error should point at the indexed-generation workflow; stderr:\n{stderr}",
    );
}

/// `-j 1` runs, through the node-to-way barrier too, and writes what
/// `-j 2` writes. It used to be refused as an unvalidated configuration;
/// the barrier fault tests (`tests/fault_apply_changes.rs`) now run every
/// ordering at one worker.
#[test]
fn merge_runs_at_one_worker() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");

    let node = |id: i32| TestNode {
        id: i64::from(id),
        lat: 100_000_000 + id,
        lon: 200_000_000 + id,
        tags: vec![],
        meta: None,
    };
    write_test_pbf_with_locations(
        &base,
        &[node(1), node(2), node(3)],
        &[(
            10,
            vec![1, 2],
            vec![(100_000_001, 200_000_001), (100_000_002, 200_000_002)],
            vec![("highway", "road")],
        )],
        &[],
    );
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <way id="10" version="2"><nd ref="1"/><nd ref="3"/></way>
  </modify>
</osmChange>"#,
    );

    let run = |jobs: usize| {
        let output = dir.path().join(format!("out-{jobs}.osm.pbf"));
        let out = run_apply_changes(
            &base,
            &osc,
            &output,
            ApplyOpts {
                jobs: Some(jobs),
                locations_on_ways: true,
                ..ApplyOpts::default()
            },
        );
        assert!(
            out.status.success(),
            "-j {jobs} must run; stderr:\n{}",
            out.stderr_str(),
        );
        std::fs::read(&output).expect("read output")
    };
    let one = run(1);
    assert_eq!(one, run(2), "-j 1 output differs from -j 2");
    let locs = read_way_locations(&dir.path().join("out-1.osm.pbf"));
    assert_eq!(
        locs,
        vec![(
            10,
            vec![(100_000_001, 200_000_001), (100_000_003, 200_000_003)]
        )]
    );
}

// ---------------------------------------------------------------------------
// Platform tier
// ---------------------------------------------------------------------------

#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
mod platform {
    use super::*;

    fn basic_fixture(base: &Path, osc: &Path) {
        write_crud_base(base);
        write_osc(osc, CRUD_OSC);
    }

    fn check_basic_output(output: &Path) {
        let c = read_all_elements(output);
        assert_eq!(node_ids(&c), vec![1, 2, 4]);
        assert_eq!(c.nodes[0].1, 100_000_000);
        assert_eq!(c.nodes[0].2, 200_000_000);
        assert_eq!(c.nodes[1].1, 350_000_000);
        assert_eq!(c.nodes[1].2, 450_000_000);
        assert_eq!(c.nodes[2].1, 550_000_000);
        assert_eq!(c.nodes[2].2, 120_000_000);
        assert_eq!(way_ids(&c), vec![10]);
        assert_eq!(c.ways[0].1, vec![1, 2]);
        assert_eq!(relation_ids(&c), vec![100]);
    }

    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn merge_basic_create_modify_delete_direct_io() {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        basic_fixture(&base, &osc);
        let out = run_apply_changes(
            &base,
            &osc,
            &output,
            ApplyOpts {
                direct_io: true,
                ..ApplyOpts::default()
            },
        );
        if out.is_o_direct_unsupported() {
            crate::common::skip("O_DIRECT not supported on this filesystem");
            return;
        }
        assert!(
            out.status.success(),
            "apply-changes --direct-io failed; stderr:\n{}",
            out.stderr_str(),
        );
        check_basic_output(&output);
        assert!(
            out.stderr_str().contains("Deleted: 1"),
            "stats: {}",
            out.stderr_str(),
        );
    }

    #[cfg(feature = "linux-io-uring")]
    #[test]
    fn merge_basic_create_modify_delete_uring() {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        basic_fixture(&base, &osc);
        let out = run_apply_changes(
            &base,
            &osc,
            &output,
            ApplyOpts {
                io_uring: true,
                ..ApplyOpts::default()
            },
        );
        if out.is_uring_unsupported() {
            crate::common::skip("io_uring not available");
            return;
        }
        assert!(
            out.status.success(),
            "apply-changes --io-uring failed; stderr:\n{}",
            out.stderr_str(),
        );
        check_basic_output(&output);
        assert!(
            out.stderr_str().contains("Deleted: 1"),
            "stats: {}",
            out.stderr_str(),
        );
    }
}
