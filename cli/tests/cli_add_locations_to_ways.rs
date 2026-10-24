//! CLI-driven integration tests for `pbfhogg add-locations-to-ways`.
//!
//! Replaces the removed library-API `add_locations_to_ways.rs` test. Fixture
//! PBFs are built with the stable-allowlist writers (including
//! `common::write_indexed_pbf`, which wraps `PbfWriter::to_path` for
//! indexdata-bearing inputs); `add-locations-to-ways` runs through
//! `CliInvoker`; output
//! is verified by reading the resulting PBF with `BlobReader` +
//! `Element` (allowlist). No imports from `pbfhogg::altw::*` - a
//! rewrite of `src/commands/altw/` cannot break these tests by type
//! changes alone.
//!
//! ALTW is the motivating example for the CLI-decoupled test layout
//! (`reference/testing.md` > "Test placement"): the sparse / external /
//! auto index backends are internal types that change shape during
//! ongoing external-join optimization work. This file's only knob
//! into "which backend" is the `--index-type` CLI flag, so backend renames or
//! splits don't ripple in.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, generate_nodes,
    generate_ways, write_indexed_pbf, write_multi_block_test_pbf, write_test_pbf,
    write_test_pbf_non_indexed,
};
use crate::invoker::{CliInvoker, CliOutput};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_nodes() -> Vec<TestNode> {
    vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged_node")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 552_000_000,
            lon: 122_000_000,
            tags: vec![("amenity", "cafe")],
            meta: None,
        },
    ]
}

fn test_ways() -> Vec<TestWay> {
    vec![TestWay {
        id: 10,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
        meta: None,
    }]
}

#[derive(Clone, Copy)]
enum IndexBackend {
    Default, // sparse (CLI default)
    Sparse,
    External,
    Auto,
}

impl IndexBackend {
    fn flag(self) -> Option<&'static str> {
        match self {
            IndexBackend::Default => None,
            IndexBackend::Sparse => Some("sparse"),
            IndexBackend::External => Some("external"),
            IndexBackend::Auto => Some("auto"),
        }
    }
}

/// Invoke `pbfhogg add-locations-to-ways <input> -o <output>
/// [--keep-untagged-nodes] [--index-type T] [--direct-io] [--force]`.
/// Returns the captured output.
fn run_altw(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    backend: IndexBackend,
    direct_io: bool,
) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(input)
        .arg("-o")
        .arg(output);
    if keep_untagged_nodes {
        cli = cli.arg("--keep-untagged-nodes");
    }
    cli = backend_args(cli, backend);
    if direct_io {
        cli = cli.arg("--direct-io");
    }
    cli.run()
}

/// `--index-type T`, plus `--force` on the backends that take it: external
/// requires indexed input whatever `--force` says, so it refuses the flag.
fn backend_args(mut cli: CliInvoker, backend: IndexBackend) -> CliInvoker {
    if let Some(name) = backend.flag() {
        cli = cli.arg("--index-type").arg(name);
    }
    if backend.flag() == Some("external") {
        cli
    } else {
        cli.arg("--force")
    }
}

/// Same as [`run_altw`] but adds `--ignore-missing-nodes`, which is what a
/// caller must now pass to accept `(0, 0)` placeholders for unresolvable
/// way refs. Without it the run fails.
fn run_altw_ignoring_missing(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    backend: IndexBackend,
) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--ignore-missing-nodes");
    if keep_untagged_nodes {
        cli = cli.arg("--keep-untagged-nodes");
    }
    backend_args(cli, backend).run()
}

/// Assert a run refused because of unresolvable node references, on that
/// guard's own wording rather than on "it failed somehow".
fn assert_refused_for_missing_nodes(out: &CliOutput, context: &str) {
    assert!(
        !out.status.success(),
        "{context}: unresolvable way node refs must fail the run; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str()
            .contains("could not be resolved to a node location"),
        "{context}: expected the missing-locations guard specifically; stderr:\n{}",
        out.stderr_str(),
    );
}

/// A referenced node whose coordinates are outside the geographic domain
/// must be refused by BOTH backends, on the same guard.
///
/// Backend selection is a performance choice: all four geographic limits
/// must have the same refusal behavior for the same referenced node.
#[test]
fn out_of_domain_node_coordinates_are_refused_by_both_backends() {
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    for (lat, lon) in [
        (900_000_001, 0),
        (-900_000_001, 0),
        (0, 1_800_000_001),
        (0, -1_800_000_001),
    ] {
        for backend in [IndexBackend::Sparse, IndexBackend::External] {
            let nodes = vec![TestNode {
                id: 1,
                lat,
                lon,
                tags: vec![],
                meta: None,
            }];
            let dir = TempDir::new().expect("tempdir");
            let input = dir.path().join("input.osm.pbf");
            let output = dir.path().join("output.osm.pbf");
            write_indexed_pbf(&input, &nodes, &ways, &[]);

            let out = run_altw(&input, &output, true, backend, false);
            assert!(
                !out.status.success(),
                "an out-of-domain referenced node must fail the run; stderr:\n{}",
                out.stderr_str(),
            );
            assert!(
                out.stderr_str().contains("outside the geographic domain"),
                "expected the domain guard specifically; stderr:\n{}",
                out.stderr_str(),
            );
            assert!(
                !output.exists(),
                "a refused run must publish no output file"
            );
        }
    }
}

/// Convenience: run with default index, assert success.
fn run_altw_ok(input: &Path, output: &Path, keep_untagged_nodes: bool) -> CliOutput {
    let out = run_altw(
        input,
        output,
        keep_untagged_nodes,
        IndexBackend::Default,
        false,
    );
    assert!(
        out.status.success(),
        "add-locations-to-ways failed; stderr:\n{}",
        out.stderr_str(),
    );
    out
}

/// Read every way in the output and return their node-location lists.
fn read_way_locations(path: &Path) -> Vec<(i64, Vec<(i32, i32)>)> {
    let reader = BlobReader::from_path(path).expect("open output");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    out.push((w.id(), locs));
                }
            }
        }
    }
    out
}

fn read_node_ids(path: &Path) -> Vec<i64> {
    let reader = BlobReader::from_path(path).expect("open output");
    let mut ids = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => ids.push(dn.id()),
                    Element::Node(n) => ids.push(n.id()),
                    _ => {}
                }
            }
        }
    }
    ids
}

fn read_relation_ids(path: &Path) -> Vec<i64> {
    let reader = BlobReader::from_path(path).expect("open output");
    let mut ids = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Relation(r) = element {
                    ids.push(r.id());
                }
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Basic tests (default / sparse backend, non-indexed input)
// ---------------------------------------------------------------------------

#[test]
fn basic_locations_added_to_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    run_altw_ok(&input, &output, true);

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (551_000_000, 121_000_000),
                (552_000_000, 122_000_000),
            ]
        )],
    );
}

#[test]
fn header_has_locations_on_ways_feature() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    run_altw_ok(&input, &output, true);

    let reader = BlobReader::from_path(&output).expect("open output");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmHeader(header) = blob.decode().expect("decode") {
            let features: Vec<&str> = header
                .optional_features()
                .iter()
                .map(String::as_str)
                .collect();
            assert!(
                features.contains(&"LocationsOnWays"),
                "LocationsOnWays not in optional features: {features:?}"
            );
            return;
        }
    }
    panic!("no header found in output");
}

#[test]
fn drop_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw_ok(&input, &output, false);

    // Stats line: pin the read/written/dropped counters (the test's
    // entire point is "untagged nodes are dropped").
    assert!(
        out.stderr_str()
            .contains("3 nodes read, 2 written, 1 dropped"),
        "stats counters mismatch; stderr:\n{}",
        out.stderr_str(),
    );

    // Node 2 has no tags -> dropped.
    assert_eq!(read_node_ids(&output), vec![1, 3]);
}

#[test]
fn keep_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw_ok(&input, &output, true);

    assert!(
        out.stderr_str()
            .contains("3 nodes read, 3 written, 0 dropped"),
        "stats counters mismatch; stderr:\n{}",
        out.stderr_str(),
    );
    assert_eq!(read_node_ids(&output), vec![1, 2, 3]);
}

/// Unresolvable refs are refused by default and filled with `(0, 0)` only
/// when the caller says so.
///
/// The default half is the one that bites: `(0, 0)` is Null Island, a real
/// coordinate off the coast of Ghana, so a placeholder vertex written into a
/// well-formed PBF is indistinguishable from genuine data to every reader.
/// The opt-in half pins that the escape hatch still produces exactly the old
/// geometry, so the guard is a policy change and not a capability loss.
#[test]
fn missing_node_refs_get_zero_coordinates() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = vec![TestNode {
        id: 1,
        lat: 550_000_000,
        lon: 120_000_000,
        tags: vec![],
        meta: None,
    }];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 99], // 99 doesn't exist
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    write_test_pbf(&input, &nodes, &ways, &[]);

    let refused = run_altw(&input, &output, true, IndexBackend::Default, false);
    assert_refused_for_missing_nodes(&refused, "default policy");

    let out = run_altw_ignoring_missing(&input, &output, true, IndexBackend::Default);
    assert!(
        out.status.success(),
        "--ignore-missing-nodes must accept the input; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("1 missing locations"),
        "expected '1 missing locations'; stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(ways, vec![(10, vec![(550_000_000, 120_000_000), (0, 0)])]);
}

#[test]
fn relations_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(10),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);
    run_altw_ok(&input, &output, true);

    assert_eq!(read_relation_ids(&output), vec![100]);
}

// ---------------------------------------------------------------------------
// Passthrough tests (indexed input)
// ---------------------------------------------------------------------------

#[test]
fn passthrough_basic_with_indexdata() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw_ok(&input, &output, true);

    // Indexed input enables passthrough; the optional second stats
    // line ("Blobs: ... passthrough, ... decoded") fires when at least
    // one blob took the raw path.
    assert!(
        out.stderr_str().contains("passthrough"),
        "expected passthrough blobs reported; stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (551_000_000, 121_000_000),
                (552_000_000, 122_000_000),
            ]
        )],
    );
}

#[test]
fn passthrough_relations_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(10),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &relations);
    let out = run_altw_ok(&input, &output, true);

    assert!(
        out.stderr_str().contains("passthrough"),
        "expected passthrough blobs reported; stderr:\n{}",
        out.stderr_str(),
    );

    assert_eq!(read_relation_ids(&output), vec![100]);
}

#[test]
fn passthrough_drop_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw_ok(&input, &output, false);

    assert!(
        out.stderr_str()
            .contains("3 nodes read, 2 written, 1 dropped"),
        "stats counters mismatch; stderr:\n{}",
        out.stderr_str(),
    );
}

#[test]
fn passthrough_keep_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw_ok(&input, &output, true);

    assert!(
        out.stderr_str()
            .contains("3 nodes read, 3 written, 0 dropped"),
        "stats counters mismatch; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("passthrough"),
        "expected passthrough blobs reported; stderr:\n{}",
        out.stderr_str(),
    );
}

#[test]
fn drop_untagged_keeps_relation_member_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "service")],
        meta: None,
    }];
    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Node(2),
            role: "label",
        }],
        tags: vec![("type", "site")],
        meta: None,
    }];

    write_test_pbf(&input, &nodes, &ways, &relations);
    run_altw_ok(&input, &output, false);

    let ids = read_node_ids(&output);
    assert!(ids.contains(&1));
    assert!(
        ids.contains(&2),
        "untagged relation-member node was dropped"
    );
}

#[test]
fn passthrough_drop_untagged_keeps_relation_member_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "service")],
        meta: None,
    }];
    let relations = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Node(2),
            role: "label",
        }],
        tags: vec![("type", "site")],
        meta: None,
    }];

    write_indexed_pbf(&input, &nodes, &ways, &relations);
    run_altw_ok(&input, &output, false);

    let ids = read_node_ids(&output);
    assert!(ids.contains(&1));
    assert!(
        ids.contains(&2),
        "untagged relation-member node was dropped"
    );
}

// ---------------------------------------------------------------------------
// Sparse and external backends
// ---------------------------------------------------------------------------

#[test]
fn basic_locations_added_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw(&input, &output, true, IndexBackend::Sparse, false);
    assert!(
        out.status.success(),
        "sparse backend failed; stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (551_000_000, 121_000_000),
                (552_000_000, 122_000_000),
            ]
        )],
    );
}

#[test]
fn passthrough_basic_with_indexdata_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &[]);
    let out = run_altw(&input, &output, true, IndexBackend::Sparse, false);
    assert!(
        out.status.success(),
        "sparse + indexed failed; stderr:\n{}",
        out.stderr_str(),
    );

    assert!(
        out.stderr_str().contains("passthrough"),
        "expected passthrough blobs reported; stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (551_000_000, 121_000_000),
                (552_000_000, 122_000_000),
            ]
        )],
    );
}

#[test]
fn missing_node_refs_get_zero_coordinates_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 999, 3], // 999 doesn't exist
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    write_test_pbf(&input, &test_nodes(), &ways, &[]);

    let refused = run_altw(&input, &output, true, IndexBackend::Sparse, false);
    assert_refused_for_missing_nodes(&refused, "sparse");

    let out = run_altw_ignoring_missing(&input, &output, true, IndexBackend::Sparse);
    assert!(
        out.status.success(),
        "sparse missing-refs failed; stderr:\n{}",
        out.stderr_str(),
    );

    assert!(
        out.stderr_str().contains("1 missing locations"),
        "expected '1 missing locations'; stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (0, 0),
                (552_000_000, 122_000_000)
            ]
        )],
    );
}

#[test]
#[allow(clippy::cast_possible_wrap)]
fn backend_parity_sparse_external_auto() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_sparse = dir.path().join("out_sparse.osm.pbf");
    let out_external = dir.path().join("out_external.osm.pbf");
    let out_auto = dir.path().join("out_auto.osm.pbf");

    let mut nodes = generate_nodes(18, 1);
    for idx in [0_usize, 3, 6, 9, 12, 15] {
        nodes[idx].tags = vec![("name", "kept")];
    }

    let mut ways = generate_ways(5, 1_000, 3, 1);
    for (i, way) in ways.iter_mut().enumerate() {
        let start = 1 + i as i64 * 3;
        way.refs = vec![start, start + 1, start + 2];
        way.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("highway", "service")]
        };
    }

    let relations = vec![TestRelation {
        id: 300,
        members: vec![
            TestMember {
                id: MemberId::Way(1_000),
                role: "outer",
            },
            TestMember {
                id: MemberId::Node(18),
                role: "label",
            },
        ],
        tags: vec![("type", "site")],
        meta: None,
    }];

    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 5);

    for (output, backend, label) in [
        (&out_sparse, IndexBackend::Sparse, "sparse"),
        (&out_external, IndexBackend::External, "external"),
        (&out_auto, IndexBackend::Auto, "auto"),
    ] {
        let out = run_altw(&input, output, false, backend, false);
        assert!(
            out.status.success(),
            "{label} backend failed; stderr:\n{}",
            out.stderr_str(),
        );
    }

    assert_elements_equivalent(&out_sparse, &out_external);
    assert_elements_equivalent(&out_external, &out_auto);
}

#[test]
fn zero_ref_ways_survive_sparse_and_external_assembly() {
    // Mixed blobs exercise refcount alignment on both sides of a nonempty
    // way; all-empty blobs exercise the router's pre-populated Empty slot.
    for all_empty in [false, true] {
        let dir = TempDir::new().unwrap();
        let input = dir.path().join("input.osm.pbf");
        let sparse = dir.path().join("sparse.osm.pbf");
        let external = dir.path().join("external.osm.pbf");
        let mut ways = generate_ways(3, 10, 1, 1);
        ways[0].refs.clear();
        ways[1].refs = if all_empty { vec![] } else { vec![1, 2, 3] };
        ways[2].refs.clear();
        write_indexed_pbf(&input, &test_nodes(), &ways, &[]);
        for (output, backend) in [
            (&sparse, IndexBackend::Sparse),
            (&external, IndexBackend::External),
        ] {
            let out = run_altw(&input, output, true, backend, false);
            assert!(
                out.status.success(),
                "all_empty={all_empty} output={}: {}",
                output.display(),
                out.stderr_str().replace('\n', " | ")
            );
        }
        assert_elements_equivalent(&sparse, &external);
        let locations = read_way_locations(&external);
        assert_eq!(locations.len(), 3);
        assert_eq!(locations[0], (10, vec![]));
        assert_eq!(locations[2], (12, vec![]));
        assert_eq!(locations[1].0, 11);
        assert_eq!(locations[1].1.len(), if all_empty { 0 } else { 3 });
    }
}

/// Auto on a sorted + indexed input runs and names the backend it chose.
/// Which backend depends on the runner's memory, so that choice is pinned
/// by `host_contract::fixed_memory_routes_auto`, which supplies the memory.
#[test]
fn auto_on_sorted_indexed_input_reports_its_choice() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_indexed_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let out = run_altw(&input, &output, false, IndexBackend::Auto, false);
    assert!(
        out.status.success(),
        "auto backend failed; stderr:\n{}",
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("auto-selected index type"),
        "auto must report its choice; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("Note: this sorted indexed PBF is eligible"),
        "an auto-resolved backend must not be second-guessed by the \
         explicit-sparse hint; stderr:\n{stderr}",
    );
}

/// Inputs external cannot handle (no indexdata) skip the scale estimate
/// entirely and route to sparse via the eligibility check.
#[test]
fn auto_routes_non_indexed_input_to_sparse_without_estimate() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let out = run_altw(&input, &output, false, IndexBackend::Auto, false);
    assert!(
        out.status.success(),
        "auto backend failed; stderr:\n{}",
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("auto-selected index type sparse (sorted="),
        "non-indexed input must route sparse via eligibility; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("store estimate"),
        "no scale estimate should run when external is ineligible; \
         stderr:\n{stderr}",
    );
}

/// `--index-type external --force` can never take effect: external join
/// depends on per-blob indexdata to compute rank-based bucket ranges, so an
/// unindexed input is refused whatever `--force` says and on an indexed one
/// `--force` does nothing. The combination is refused as a usage error,
/// before any input is opened.
#[test]
fn altw_external_refuses_force_before_any_input() {
    let dir = TempDir::new().expect("tempdir");
    let out = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(dir.path().join("no-such-input.osm.pbf"))
        .arg("-o")
        .arg(dir.path().join("out.osm.pbf"))
        .args(["--index-type", "external", "--force"])
        .run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("--force")
            && stderr.contains("external")
            && !stderr.contains("no-such-input"),
        "expected the argument refusal, not an input error; stderr:\n{stderr}",
    );
}

/// Without `--force`, an unindexed input on the external path is refused by
/// the indexdata gate, which points at the indexed-generation workflow.
#[test]
fn altw_external_refuses_non_indexed_input() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_non_indexed(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 550_000_000,
                lon: 120_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 551_000_000,
                lon: 121_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 552_000_000,
                lon: 122_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "residential")],
            meta: None,
        }],
        &[],
    );

    let out = run_altw(&input, &output, true, IndexBackend::External, false);
    assert!(
        !out.status.success(),
        "expected external-join to refuse a non-indexed PBF, but it succeeded; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );

    let stderr = out.stderr_str();
    assert!(
        stderr.contains("external") && stderr.contains("indexed"),
        "expected setup-time rejection mentioning external + indexed, got stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("pbfhogg cat"),
        "error should point at the indexed-generation workflow, got stderr:\n{stderr}",
    );
}

/// `--index-type dense` was removed (commit `b70dd8c`) after the
/// rank-indexed flat sparse layout dominated dense at every measured
/// scale. The parser must surface a clear migration hint pointing at
/// `sparse` rather than a generic "unknown index type" error, so users
/// upgrading from older releases see *why* their flag stopped working
/// and what to switch to.
#[test]
fn altw_dense_index_type_rejected_with_migration_hint() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let out = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--index-type")
        .arg("dense")
        .arg("--force")
        .run();

    assert!(
        !out.status.success(),
        "expected --index-type dense to be rejected, but it succeeded; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );

    let stderr = out.stderr_str();
    assert!(
        stderr.contains("dense") && stderr.contains("sparse"),
        "expected migration hint mentioning both 'dense' (the removed type) and 'sparse' (the replacement), got stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("removed"),
        "expected error to state dense was removed, got stderr:\n{stderr}",
    );
}

// ---------------------------------------------------------------------------
// Platform tier
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
mod platform {
    use super::*;

    #[test]
    fn basic_locations_added_direct_io() {
        let dir = TempDir::new().expect("tempdir");
        let input = dir.path().join("input.osm.pbf");
        let output = dir.path().join("output.osm.pbf");

        write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
        let out = run_altw(&input, &output, true, IndexBackend::Default, true);
        if out.is_o_direct_unsupported() {
            crate::common::skip("O_DIRECT not supported on this filesystem");
            return;
        }
        assert!(
            out.status.success(),
            "add-locations-to-ways --direct-io failed unexpectedly; stderr:\n{}",
            out.stderr_str(),
        );

        let ways = read_way_locations(&output);
        assert_eq!(
            ways,
            vec![(
                10,
                vec![
                    (550_000_000, 120_000_000),
                    (551_000_000, 121_000_000),
                    (552_000_000, 122_000_000),
                ]
            )],
        );
    }

    /// Stage 4 of the external backend owns the output writer, and runs on
    /// its own thread. A platform setup refusal raised there must cross the
    /// stage's thread join as the typed refusal, not flattened into a generic
    /// failure.
    ///
    /// This drives the library with a direct output and buffered reads
    /// (`direct_reads: false`), a combination the CLI cannot express (its
    /// `--direct-io` sets both, so an input-side refusal would fire first on
    /// the main thread and never reach stage 4). With reads buffered, the only
    /// way the run can fail with a refusal is stage 4's output open. A host
    /// whose filesystem accepts O_DIRECT output completes the run instead, so
    /// the test cannot force the refusal; it then passes without exercising
    /// the join, and only a failure is held to the contract.
    #[test]
    fn external_backend_output_refusal_stays_typed_across_stage_join() {
        let dir = TempDir::new().expect("tempdir");
        let input = dir.path().join("input.osm.pbf");
        let output = dir.path().join("output.osm.pbf");

        let nodes = generate_nodes(6, 1);
        let mut ways = generate_ways(1, 1_000, 3, 1);
        ways[0].refs = vec![1, 2, 3];
        write_multi_block_test_pbf(&input, &nodes, &ways, &[], 3);

        let result = pbfhogg::altw::add_locations_to_ways(
            &input,
            crate::common::out(&output),
            &pbfhogg::altw::AltwOptions {
                keep_untagged_nodes: true,
                write: pbfhogg::write::WriteOptions {
                    backend: pbfhogg::write::OutputBackend::Direct,
                    direct_reads: false,
                    ..pbfhogg::write::WriteOptions::default()
                },
                index_type: pbfhogg::altw::IndexType::External,
                inject_prepass: false,
                ignore_missing_nodes: false,
            },
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
            crate::common::scratch_in(dir.path()),
        );
        match result {
            Ok(_) => crate::common::skip("O_DIRECT output accepted on this filesystem"),
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    pbfhogg::ErrorKind::PlatformUnsupported {
                        feature: "direct-io",
                        ..
                    }
                ),
                "an output-side O_DIRECT refusal must stay typed; got: {}",
                error.report(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tier B contract gaps from 2026-04-26 review
// ---------------------------------------------------------------------------

/// B1 - a real node at Null Island is present, not missing.
///
/// The coordinate stores used to spell "absent" as `(0, 0)`, which is a real
/// place in the Gulf of Guinea. That made a genuine node at exactly `0, 0`
/// report as a missing location, and it made a missing reference come out of
/// the writer as if the node were there. Absence is now a flag bit in the
/// sparse slot encoding, and the external backend counts on presence rather
/// than on the coordinate being non-zero.
///
/// Both backends are exercised on the same fixture: they answer the same
/// question and must not disagree about it. The run must also SUCCEED, since
/// the missing-locations guard now fails any run with a nonzero count - so a
/// regression to the old sentinel shows up twice over.
#[test]
fn null_island_real_node_is_not_reported_missing() {
    let nodes = vec![TestNode {
        id: 1,
        lat: 0,
        lon: 0,
        tags: vec![],
        meta: None,
    }];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    for backend in [IndexBackend::Sparse, IndexBackend::External] {
        let dir = TempDir::new().expect("tempdir");
        let input = dir.path().join("input.osm.pbf");
        let output = dir.path().join("output.osm.pbf");
        // External needs indexdata to build its I/O schedule; sparse is
        // happy either way, so the indexed fixture serves both.
        write_indexed_pbf(&input, &nodes, &ways, &[]);

        let out = run_altw(&input, &output, true, backend, false);
        assert!(
            out.status.success(),
            "a node at Null Island is present; the run must not fail. \
             stderr:\n{}",
            out.stderr_str(),
        );
        assert!(
            out.stderr_str().contains("0 missing locations"),
            "a real node at (0, 0) must not be reported missing; stderr:\n{}",
            out.stderr_str(),
        );
        // And the coordinate itself survives, so this is not satisfied by a
        // backend that merely stopped counting.
        assert_eq!(read_way_locations(&output), vec![(10, vec![(0, 0)])]);
    }
}

/// A way ref (and a relation node member) near `i64::MAX` used to
/// make sparse pass 0 and the relation-member scan grow the ID set's chunk
/// directory to ~2^38 entries and abort. It is now an ordinary reference to
/// a node the file does not contain: refused by the missing-locations guard,
/// and written as `(0, 0)` under `--ignore-missing-nodes`.
///
/// `keep_untagged_nodes = false` is what routes the relation through the
/// relation-member scan, so both input-fed sets are exercised.
#[test]
fn way_ref_near_i64_max_is_a_missing_location_not_an_abort() {
    let far = i64::MAX - 1;
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, far, 3],
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    let relations = vec![TestRelation {
        id: 300,
        members: vec![TestMember {
            id: MemberId::Node(far),
            role: "label",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];

    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &ways, &relations);

    let refused = run_altw(&input, &output, false, IndexBackend::Sparse, false);
    assert_refused_for_missing_nodes(&refused, "sparse, ref near i64::MAX");
    assert!(
        !output.exists(),
        "a refused run must publish no output file"
    );

    let out = run_altw_ignoring_missing(&input, &output, false, IndexBackend::Sparse);
    assert!(
        out.status.success(),
        "sparse with a ref near i64::MAX failed; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("1 missing locations"),
        "expected '1 missing locations'; stderr:\n{}",
        out.stderr_str(),
    );
    assert_eq!(
        read_way_locations(&output),
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (0, 0),
                (552_000_000, 122_000_000)
            ]
        )],
    );
}

/// The external backend sizes its id buckets from the node blobs'
/// indexdata `max_id`, and used to assert (panic) when that exceeded what its
/// 31-bit bucket-local ids can encode (2^38 with 256 buckets). It is now an
/// error that points at the sparse index - and that pointer must be true, so
/// the same file has to succeed under sparse.
#[test]
fn node_id_beyond_external_encoding_limit_is_an_error_and_sparse_handles_it() {
    let big = 1_i64 << 39;
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: big,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, big],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    write_indexed_pbf(&input, &nodes, &ways, &[]);

    let external_out = dir.path().join("external.osm.pbf");
    let out = run_altw(&input, &external_out, true, IndexBackend::External, false);
    assert!(
        !out.status.success(),
        "external must refuse a node max_id it cannot encode; stderr:\n{}",
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("the sparse index has no such limit") && !stderr.contains("panicked"),
        "expected the encoding-limit error, not a panic; stderr:\n{stderr}",
    );
    assert!(
        !external_out.exists(),
        "a refused run must publish no output file"
    );

    let sparse_out = dir.path().join("sparse.osm.pbf");
    let out = run_altw(&input, &sparse_out, true, IndexBackend::Sparse, false);
    assert!(
        out.status.success(),
        "sparse must handle the node id external cannot; stderr:\n{}",
        out.stderr_str(),
    );
    assert_eq!(
        read_way_locations(&sparse_out),
        vec![(
            10,
            vec![(550_000_000, 120_000_000), (551_000_000, 121_000_000)]
        )],
    );
}

/// B2 - missing-node handling for `--index-type external`.
///
/// Missing refs are refused by default and substituted with `(0, 0)` under
/// `--ignore-missing-nodes`. Pre-batch tests pinned this for sparse
/// (`missing_node_refs_get_zero_coordinates_sparse`); the external variant
/// was unpinned. Same fixture, same expected output - this test makes the
/// contract uniform across both backends, which is exactly where they used
/// to disagree.
#[test]
fn missing_node_refs_get_zero_coordinates_external() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 999, 3], // 999 doesn't exist
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    // External requires indexed input (HeaderWalker fast-path scans
    // BlobHeader.indexdata to drive its I/O schedule). The sparse
    // twin uses `write_test_pbf` because its code path falls back
    // to full-decode without indexdata; external does not.
    write_indexed_pbf(&input, &test_nodes(), &ways, &[]);

    let refused = run_altw(&input, &output, true, IndexBackend::External, false);
    assert_refused_for_missing_nodes(&refused, "external");

    let out = run_altw_ignoring_missing(&input, &output, true, IndexBackend::External);
    assert!(
        out.status.success(),
        "external missing-refs failed; stderr:\n{}",
        out.stderr_str(),
    );
    assert!(
        out.stderr_str().contains("1 missing locations"),
        "expected '1 missing locations' under --index-type external; \
         stderr:\n{}",
        out.stderr_str(),
    );

    let ways = read_way_locations(&output);
    assert_eq!(
        ways,
        vec![(
            10,
            vec![
                (550_000_000, 120_000_000),
                (0, 0),
                (552_000_000, 122_000_000)
            ]
        )],
    );
}

#[test]
fn plain_nodes_supply_coordinates_to_both_backends() {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 128 {
            out.push(u8::try_from(value & 127).expect("seven bits") | 128);
            value >>= 7;
        }
        out.push(u8::try_from(value).expect("seven bits"));
    }
    fn message(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(field * 8 + 2, out);
        varint(u64::try_from(bytes.len()).expect("message length"), out);
        out.extend_from_slice(bytes);
    }
    let dir = crate::common::test_scratch();
    let input = dir.path().join("plain.osm.pbf");
    let mut writer = pbfhogg::writer::PbfWriter::new(
        std::fs::File::create(&input).expect("create input"),
        pbfhogg::writer::Compression::default(),
    );
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    let mut group = Vec::new();
    for (id, lat, lon) in [
        (1_u64, 550_000_000_u64, 120_000_000_u64),
        (2, 551_000_000, 121_000_000),
    ] {
        let mut node = Vec::new();
        for (field, value) in [(1, id), (8, lat), (9, lon)] {
            varint(field * 8, &mut node);
            varint(value * 2, &mut node);
        }
        message(1, &node, &mut group);
    }
    let mut block = Vec::new();
    message(1, &[10, 0], &mut block); // StringTable with empty entry 0
    message(2, &group, &mut block);
    writer
        .write_primitive_block(&block)
        .expect("write plain nodes");
    let mut builder = pbfhogg::block_builder::BlockBuilder::new();
    builder.add_way(10, [("highway", "primary")], &[1, 2], None);
    writer
        .write_primitive_block(builder.take().expect("take").expect("way block"))
        .expect("write way");
    writer.flush().expect("flush");
    drop(writer);
    for backend in [IndexBackend::Sparse, IndexBackend::External] {
        let output = dir.path().join(format!(
            "{}.osm.pbf",
            backend.flag().expect("explicit backend")
        ));
        let result = run_altw(&input, &output, true, backend, false);
        assert!(result.status.success(), "{}", result.stderr_str());
        assert_eq!(
            read_way_locations(&output),
            vec![(
                10,
                vec![(550_000_000, 120_000_000), (551_000_000, 121_000_000)]
            )]
        );
    }
}
