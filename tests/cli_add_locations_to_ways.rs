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

use crate::common::cli::{CliInvoker, CliOutput};
use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, generate_nodes,
    generate_ways, write_indexed_pbf, write_multi_block_test_pbf, write_test_pbf,
    write_test_pbf_non_indexed,
};
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
/// [--keep-untagged-nodes] [--index-type T] [--direct-io] --force`.
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
    if let Some(name) = backend.flag() {
        cli = cli.arg("--index-type").arg(name);
    }
    if direct_io {
        cli = cli.arg("--direct-io");
    }
    cli.arg("--force").run()
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
    if let Some(name) = backend.flag() {
        cli = cli.arg("--index-type").arg(name);
    }
    cli.arg("--force").run()
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
/// The sparse backend refuses because its slot encoding cannot represent a
/// latitude past 90 degrees without decoding as some other place. That is a
/// representational necessity, but the refusal is a command-level contract:
/// `--index-type` is a performance knob, and a knob that flips a run between
/// hard error and silent pass-through is a semantic switch. The external
/// backend can represent the value, so its guard exists purely to agree.
#[test]
fn out_of_domain_node_coordinates_are_refused_by_both_backends() {
    let nodes = vec![TestNode {
        id: 1,
        lat: 1_000_000_000, // 100 degrees north: a fine longitude, no latitude
        lon: 120_000_000,
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

/// Scale-aware auto routing: sorted + indexed no
/// longer implies external. A small input's estimated node store sits
/// far below any real host's page-cache budget, so auto must route it
/// to sparse and say why on stderr. Reads `/proc/meminfo`, hence
/// Linux-gated: elsewhere the budget probe returns unavailable and
/// auto deliberately falls back to external.
#[test]
#[cfg(target_os = "linux")]
fn auto_routes_small_sorted_indexed_input_to_sparse() {
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
        stderr.contains("auto-selected --index-type sparse"),
        "small sorted+indexed input must route to sparse; stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("store estimate") && stderr.contains("page-cache budget"),
        "routing must go through the scale estimate, not the \
         eligibility fallback; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("hint: this sorted indexed PBF is eligible"),
        "auto-resolved sparse must not be second-guessed by the \
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
        stderr.contains("auto-selected --index-type sparse (sorted="),
        "non-indexed input must route sparse via eligibility; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("store estimate"),
        "no scale estimate should run when external is ineligible; \
         stderr:\n{stderr}",
    );
}

/// Regression: `--index-type external --force` against a non-indexed
/// PBF must reject the combination up front with a clear migration
/// hint, not accept `--force` and fail much later with an opaque
/// "OsmData blob missing indexdata" error. External join depends on
/// per-blob indexdata to compute rank-based bucket ranges, so
/// `--force` cannot meaningfully apply to this path.
#[test]
fn altw_external_rejects_force_on_non_indexed() {
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
        "expected external-join to reject --force on non-indexed PBF, but it succeeded; stdout:\n{}\nstderr:\n{}",
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
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
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
/// error that points at `--index-type sparse` - and that pointer must be
/// true, so the same file has to succeed under sparse.
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
        stderr.contains("--index-type sparse") && !stderr.contains("panicked"),
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
