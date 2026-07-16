//! CLI-driven integration tests for `pbfhogg add-locations-to-ways`.
//!
//! Replaces the library-API `tests/add_locations_to_ways.rs`. Fixture
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
//! the join rewrite documented in `notes/altw-external.md`. This
//! file's only knob into "which
//! backend" is the `--index-type` CLI flag, so backend renames or
//! splits don't ripple in.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::{CliInvoker, CliOutput};
use common::{
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
    let out = run_altw_ok(&input, &output, true);

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

    let out = run_altw(&input, &output, true, IndexBackend::Sparse, false);
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

/// Scale-aware auto routing (notes/altw.md P1): sorted + indexed no
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

/// B1 - Null Island sentinel collision (CORRECTNESS.md "Null Island
/// ambiguity"). Every coordinate index uses `(0, 0)` as the
/// "absent" sentinel - so a real node at exactly Null Island is
/// indistinguishable from a missing reference. The four sites
/// (`DenseMmapIndex::get`, `SparseArrayIndex::get_at_offset`, ALTW
/// external stage 2 `is_resolved`, geocode Pass 2) all share this
/// behavior. CORRECTNESS.md documents the limitation as accepted;
/// fixing it requires a separate occupancy bitmap (~550 MB at planet
/// scale).
///
/// This test pins the documented status quo: a way referencing a
/// node at exactly `(lat: 0, lon: 0)` produces a "1 missing
/// locations" report, even though the node is present and at its
/// stored coordinates. If a future change introduces an occupancy
/// bitmap (or any other fix), this test fails - prompting a
/// deliberate update to both the test AND the CORRECTNESS entry.
#[test]
fn null_island_real_node_treated_as_missing() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

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
    write_test_pbf(&input, &nodes, &ways, &[]);

    let out = run_altw_ok(&input, &output, true);
    assert!(
        out.stderr_str().contains("1 missing locations"),
        "Null Island sentinel collision: a real node at (0, 0) must \
         be reported as missing per CORRECTNESS.md 'Null Island \
         ambiguity'. If this assertion fails because altw now \
         distinguishes real (0,0) from missing, update the \
         CORRECTNESS.md entry too. stderr:\n{}",
        out.stderr_str(),
    );
}

/// B2 - missing-node tolerance for `--index-type external`.
/// DEVIATIONS.md says missing nodes are tolerated by default, with
/// `(0, 0)` substituted. Pre-batch tests pinned this for sparse
/// (`missing_node_refs_get_zero_coordinates_sparse`); the external
/// variant was unpinned. Same fixture, same expected output - this
/// test makes the contract uniform across both backends.
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

    let out = run_altw(&input, &output, true, IndexBackend::External, false);
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
