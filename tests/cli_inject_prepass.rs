//! CLI-driven integration tests for the injected-prepass producer
//! (`add-locations-to-ways --inject-prepass`).
//!
//! Covers the two private wire extensions this project carries: the
//! `BlobHeader` field-5 WayMembers-v1 payload (`Blob::way_members` /
//! `Blob::way_member_count`) and the Way field-20 SharedNodePins-v1 bitmap
//! (`Way::shared_node_pins`), plus the two header feature strings
//! (`HeaderBlock::has_way_members_v1` / `has_shared_node_pins_v1`).
//!
//! The gate commands for landings 2-4 name this file
//! (`brokkr test cli_inject_prepass <name>`); the tests live here rather than
//! in `cli_add_locations_to_ways.rs` so those names resolve. Fixtures are
//! built with the stable-allowlist writers (`common::write_indexed_pbf`) and
//! the producer runs through `CliInvoker`; output is read back with
//! `BlobReader` + `set_parse_waymembers(true)`.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use common::cli::CliInvoker;
use common::{
    TestMember, TestMeta, TestNode, TestRelation, TestWay, read_header, write_indexed_pbf,
};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn node(id: i64) -> TestNode {
    let k = i32::try_from(id).expect("small id");
    TestNode {
        id,
        lat: 550_000_000 + k * 1_000,
        lon: 120_000_000 + k * 1_000,
        tags: vec![],
        meta: None,
    }
}

fn way(id: i64, refs: Vec<i64>) -> TestWay {
    TestWay {
        id,
        refs,
        tags: vec![],
        meta: None,
    }
}

fn mp_relation(id: i64, way_id: i64) -> TestRelation {
    TestRelation {
        id,
        members: vec![TestMember {
            id: MemberId::Way(way_id),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }
}

fn run_inject(input: &Path, output: &Path, backend: &str) {
    let out = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--index-type")
        .arg(backend)
        .arg("--inject-prepass")
        .run();
    assert!(
        out.status.success(),
        "{backend} inject failed; stderr:\n{}",
        out.stderr_str()
    );
}

// ---------------------------------------------------------------------------
// Per-blob / per-way readback
// ---------------------------------------------------------------------------

type WayPins = Vec<(i64, Option<Vec<u8>>)>;
type BlobMembers = Vec<(Vec<u8>, u32)>;

/// Read an enriched output into its per-way pin bitmaps (sorted by way id) and
/// per-way-blob field-5 (bitmap, encoded way count).
fn read_inject_artifacts(path: &Path) -> (WayPins, BlobMembers) {
    let mut reader = BlobReader::from_path(path).expect("open output");
    reader.set_parse_waymembers(true);
    let mut ways: WayPins = Vec::new();
    let mut members: BlobMembers = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let Some(wm) = blob.way_members() {
            members.push((wm.to_vec(), blob.way_member_count().expect("count")));
        }
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    ways.push((w.id(), w.shared_node_pins().map(<[u8]>::to_vec)));
                }
            }
        }
    }
    ways.sort_by_key(|(id, _)| *id);
    (ways, members)
}

fn pins_for(ways: &WayPins, id: i64) -> Option<Vec<u8>> {
    ways.iter()
        .find(|(w, _)| *w == id)
        .and_then(|(_, p)| p.clone())
}

// ---------------------------------------------------------------------------
// Feature-flag smoke tests (sparse + external)
// ---------------------------------------------------------------------------

/// The opt-in producer attaches a field-5 payload to every way blob and emits
/// a field-20 bitmap only when a way has resolved shared references.
#[test]
fn inject_prepass_sparse_emits_feature_flags_and_bitmaps() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let nodes: Vec<TestNode> = (1..=4).map(node).collect();
    let ways = vec![way(10, vec![1, 2, 3]), way(11, vec![2, 4])];
    let relations = vec![mp_relation(20, 10)];
    write_indexed_pbf(&input, &nodes, &ways, &relations);
    run_inject(&input, &output, "sparse");

    let header = read_header(&output);
    assert!(header.has_way_members_v1());
    assert!(header.has_shared_node_pins_v1());

    let (ways_out, members) = read_inject_artifacts(&output);
    // Node 2 is shared: way 10 position 1, way 11 position 0.
    assert_eq!(pins_for(&ways_out, 10), Some(vec![0b0000_0010]));
    assert_eq!(pins_for(&ways_out, 11), Some(vec![0b0000_0001]));
    // One way blob, both ways in it; way 10 is the sole relation member.
    assert_eq!(members, vec![(vec![0b0000_0001], 2)]);
}

#[test]
fn inject_prepass_external_emits_feature_flags() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let nodes: Vec<TestNode> = (1..=3).map(node).collect();
    let ways = vec![way(10, vec![1, 2, 3])];
    write_indexed_pbf(&input, &nodes, &ways, &[]);
    run_inject(&input, &output, "external");

    let header = read_header(&output);
    assert!(header.has_way_members_v1());
    assert!(header.has_shared_node_pins_v1());

    let (_ways, members) = read_inject_artifacts(&output);
    assert_eq!(
        members.len(),
        1,
        "the single way blob answers way_members()"
    );
}

// ---------------------------------------------------------------------------
// Backend parity (tier 2): sparse == external, byte for byte
// ---------------------------------------------------------------------------

/// The sparse and external backends must produce byte-identical WayMembers-v1
/// and SharedNodePins-v1 artifacts. The fixture exercises two regressions the
/// per-backend smoke tests miss: a shared node (2) whose two occurrences fall
/// at the tail of a stage-2 run (external pin accounting), and a way with more
/// than eight refs whose only pin sits early, forcing the fixed-width
/// `ceil(ref_count/8)` field-20 bitmap (sparse otherwise truncates it).
#[test]
fn backend_parity_inject_prepass() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let nodes: Vec<TestNode> = (1..=12).map(node).collect();
    let ways = vec![
        way(10, vec![1, 2, 3]),
        way(11, vec![2, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
    ];
    let relations = vec![mp_relation(20, 10)];
    write_indexed_pbf(&input, &nodes, &ways, &relations);

    let sparse_out = dir.path().join("sparse.osm.pbf");
    let external_out = dir.path().join("external.osm.pbf");
    run_inject(&input, &sparse_out, "sparse");
    run_inject(&input, &external_out, "external");

    let sparse = read_inject_artifacts(&sparse_out);
    let external = read_inject_artifacts(&external_out);

    // Node 2 is shared: way 10 position 1, way 11 position 0.
    assert_eq!(pins_for(&sparse.0, 10), Some(vec![0b0000_0010]));
    assert_eq!(pins_for(&sparse.0, 11), Some(vec![0b0000_0001, 0]));

    assert_eq!(
        sparse.0, external.0,
        "shared_node_pins differ between backends"
    );
    assert_eq!(sparse.1, external.1, "way_members differ between backends");
}

// ---------------------------------------------------------------------------
// Oracle roundtrip (Brick 2 contract gate)
// ---------------------------------------------------------------------------

/// Independent, in-test reimplementation of the ratified contract (D9: pin =
/// shared AND resolved; D2 closure ref mirrors bit 0; D10 within-way repeats
/// count). Recomputes the expected field-20 bitmap for every way straight
/// from the raw fixture, with zero shared code with the producer.
fn expected_pins(nodes: &[TestNode], ways: &[TestWay]) -> HashMap<i64, Option<Vec<u8>>> {
    let present: HashSet<i64> = nodes.iter().map(|n| n.id).collect();

    // Shared-ness counts every non-closure ref occurrence (within-way repeats
    // included); a closed ring (len >= 4, first == last) drops its trailing
    // duplicate before counting.
    let mut count: HashMap<i64, u32> = HashMap::new();
    for w in ways {
        for &id in trimmed_refs(&w.refs) {
            if id >= 0 {
                *count.entry(id).or_default() += 1;
            }
        }
    }

    let mut out = HashMap::new();
    for w in ways {
        let mut bits = vec![0u8; w.refs.len().div_ceil(8)];
        let mut any = false;
        for (i, &id) in w.refs.iter().enumerate() {
            let shared = id >= 0 && count.get(&id).copied().unwrap_or(0) >= 2;
            let resolved = present.contains(&id);
            if shared && resolved {
                bits[i / 8] |= 1 << (i % 8);
                any = true;
            }
        }
        out.insert(w.id, if any { Some(bits) } else { None });
    }
    out
}

fn trimmed_refs(refs: &[i64]) -> &[i64] {
    if refs.len() >= 4 && refs.first() == refs.last() {
        &refs[..refs.len() - 1]
    } else {
        refs
    }
}

/// A way is a member iff it is a `Way` member (id >= 0) of a `multipolygon` or
/// `boundary` relation. Returned in fixture file order.
fn expected_members(ways: &[TestWay], relations: &[TestRelation]) -> Vec<bool> {
    let member_ids: HashSet<i64> = relations
        .iter()
        .filter(|r| {
            r.tags
                .iter()
                .any(|(k, v)| *k == "type" && matches!(*v, "multipolygon" | "boundary"))
        })
        .flat_map(|r| r.members.iter())
        .filter_map(|m| match m.id {
            MemberId::Way(id) if id >= 0 => Some(id),
            _ => None,
        })
        .collect();
    ways.iter().map(|w| member_ids.contains(&w.id)).collect()
}

fn pack_lsb(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// End-to-end oracle: build a fixture with rich topology - junctions,
/// within-way repeats, a closed ring sharing an edge with a road, mp / boundary
/// / other relations, and refs to an absent-but-shared node - enrich it with
/// both backends, read the field-5 and field-20 metadata back, and compare
/// bit-for-bit against an independent recomputation of the ratified semantics.
/// Also exercises `way_member_count()` against the blob's real decoded Way
/// count (the equal case; the within-byte gap is the `blob.rs` unit test).
#[test]
fn inject_prepass_oracle_roundtrip() {
    // Present nodes 1..=10; node 99 is referenced but absent.
    let nodes: Vec<TestNode> = (1..=10).map(node).collect();
    let ways = vec![
        way(10, vec![1, 2, 3, 4]), // road; shares 2,3 with the ring
        way(11, vec![3, 5]),       // junction at node 3
        way(12, vec![2, 3, 6, 2]), // closed ring sharing edge 2-3 with road 10
        way(13, vec![7, 8, 7]),    // within-way repeat of node 7 (not a ring)
        way(14, vec![9, 99]),      // 99 is shared-but-absent
        way(15, vec![99, 10]),     // second occurrence of absent node 99
    ];
    let relations = vec![
        mp_relation(20, 10),
        TestRelation {
            id: 21,
            members: vec![TestMember {
                id: MemberId::Way(12),
                role: "outer",
            }],
            tags: vec![("type", "boundary")],
            meta: None,
        },
        TestRelation {
            id: 22,
            members: vec![TestMember {
                id: MemberId::Way(11),
                role: "",
            }],
            tags: vec![("type", "route")], // not mp/boundary: no membership
            meta: None,
        },
        TestRelation {
            id: 23,
            members: vec![
                TestMember {
                    id: MemberId::Node(2),
                    role: "",
                },
                TestMember {
                    id: MemberId::Way(99),
                    role: "",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        },
    ];

    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    write_indexed_pbf(&input, &nodes, &ways, &relations);

    let want_pins = expected_pins(&nodes, &ways);
    let want_members = pack_lsb(&expected_members(&ways, &relations));

    for backend in ["sparse", "external"] {
        let output = dir.path().join(format!("{backend}.osm.pbf"));
        run_inject(&input, &output, backend);

        let header = read_header(&output);
        assert!(header.has_way_members_v1(), "{backend}: no WayMembers-v1");
        assert!(
            header.has_shared_node_pins_v1(),
            "{backend}: no SharedNodePins-v1"
        );

        let (ways_out, members) = read_inject_artifacts(&output);
        assert_eq!(
            ways_out.len(),
            ways.len(),
            "{backend}: way count round-trips"
        );
        for (id, pins) in &ways_out {
            assert_eq!(
                pins,
                want_pins.get(id).expect("known way"),
                "{backend}: pins mismatch for way {id}"
            );
        }

        // All ways land in one blob; its field-5 mirrors the oracle membership
        // bitmap, and the encoded count equals the real decoded Way count.
        assert_eq!(members.len(), 1, "{backend}: single way blob");
        let (bitmap, count) = &members[0];
        assert_eq!(bitmap, &want_members, "{backend}: field-5 membership");
        assert_eq!(
            *count as usize,
            ways.len(),
            "{backend}: way_member_count == decoded way count"
        );
    }
}

// ---------------------------------------------------------------------------
// Flag hygiene: rewriting commands must drop the enrichment feature strings
// ---------------------------------------------------------------------------

fn assert_flags_dropped(label: &str, path: &Path) {
    let h = read_header(path);
    assert!(
        !h.has_way_members_v1(),
        "{label}: WayMembers-v1 leaked into rewritten output"
    );
    assert!(
        !h.has_shared_node_pins_v1(),
        "{label}: SharedNodePins-v1 leaked into rewritten output"
    );
}

fn assert_cmd_ok(label: &str, cli: CliInvoker) {
    let out = cli.run();
    assert!(
        out.status.success(),
        "{label} failed; stderr:\n{}",
        out.stderr_str()
    );
}

/// Enrichment flags are load-bearing only while every command that rewrites
/// way payloads or way-blob headers drops them: `HeaderBuilder::from_header`
/// deliberately does not copy `optional_features`, so a rewritten file must
/// never claim WayMembers-v1 / SharedNodePins-v1 it no longer maintains. This
/// sweep drives the complete set of `warn_locations_on_ways_loss` callers that
/// produce a PBF and asserts the output header carries neither string, pinning
/// the accidental safety so a future header-preserving change fails here rather
/// than shipping a malformed enrichment.
///
/// Exempt (noted, not silently skipped): `apply-changes` and `tags-filter
/// --input-kind osc` need an OSC change stream, and `multi-extract` writes to a
/// config-driven directory rather than a single `-o` output - all three funnel
/// through the same `build_output_header` chokepoint the driven commands
/// already cover.
#[test]
#[allow(clippy::too_many_lines)]
fn rewriting_commands_drop_enrichment_flags() {
    let dir = TempDir::new().expect("tempdir");
    let raw = dir.path().join("raw.osm.pbf");
    let enriched = dir.path().join("enriched.osm.pbf");

    let nodes: Vec<TestNode> = (1..=5)
        .map(|id| TestNode {
            tags: vec![("name", "n")],
            meta: Some(TestMeta::default()),
            ..node(id)
        })
        .collect();
    let ways = vec![
        TestWay {
            tags: vec![("highway", "primary")],
            meta: Some(TestMeta::default()),
            ..way(10, vec![1, 2, 3])
        },
        TestWay {
            tags: vec![("highway", "service")],
            meta: Some(TestMeta::default()),
            ..way(11, vec![3, 4, 5])
        },
    ];
    let relations = vec![mp_relation(20, 10)];
    write_indexed_pbf(&raw, &nodes, &ways, &relations);

    // Keep untagged nodes so downstream commands see a complete node+way graph.
    let out = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(&raw)
        .arg("-o")
        .arg(&enriched)
        .arg("--index-type")
        .arg("external")
        .arg("--keep-untagged-nodes")
        .arg("--inject-prepass")
        .run();
    assert!(
        out.status.success(),
        "enrich failed; stderr:\n{}",
        out.stderr_str()
    );
    let enriched_header = read_header(&enriched);
    assert!(
        enriched_header.has_way_members_v1() && enriched_header.has_shared_node_pins_v1(),
        "fixture precondition: enriched input must declare both feature strings"
    );

    let o = |name: &str| dir.path().join(format!("out_{name}.osm.pbf"));

    // sort
    assert_cmd_ok(
        "sort",
        CliInvoker::new()
            .arg("sort")
            .arg(&enriched)
            .arg("-o")
            .arg(o("sort")),
    );
    assert_flags_dropped("sort", &o("sort"));

    // renumber
    assert_cmd_ok(
        "renumber",
        CliInvoker::new()
            .arg("renumber")
            .arg(&enriched)
            .arg("-o")
            .arg(o("renumber")),
    );
    assert_flags_dropped("renumber", &o("renumber"));

    // repack
    assert_cmd_ok(
        "repack",
        CliInvoker::new()
            .arg("repack")
            .arg(&enriched)
            .arg("-o")
            .arg(o("repack")),
    );
    assert_flags_dropped("repack", &o("repack"));

    // cat (single-input rewrite)
    assert_cmd_ok(
        "cat",
        CliInvoker::new()
            .arg("cat")
            .arg(&enriched)
            .arg("-o")
            .arg(o("cat")),
    );
    assert_flags_dropped("cat", &o("cat"));

    // cat --dedupe (two sorted inputs)
    assert_cmd_ok(
        "cat-dedupe",
        CliInvoker::new()
            .arg("cat")
            .arg(&enriched)
            .arg(&enriched)
            .arg("--dedupe")
            .arg("-o")
            .arg(o("cat_dedupe")),
    );
    assert_flags_dropped("cat-dedupe", &o("cat_dedupe"));

    // getid
    assert_cmd_ok(
        "getid",
        CliInvoker::new()
            .arg("getid")
            .arg(&enriched)
            .arg("w10")
            .arg("-o")
            .arg(o("getid")),
    );
    assert_flags_dropped("getid", &o("getid"));

    // getparents
    assert_cmd_ok(
        "getparents",
        CliInvoker::new()
            .arg("getparents")
            .arg(&enriched)
            .arg("n3")
            .arg("-o")
            .arg(o("getparents")),
    );
    assert_flags_dropped("getparents", &o("getparents"));

    // tags-filter (PBF path)
    assert_cmd_ok(
        "tags-filter",
        CliInvoker::new()
            .arg("tags-filter")
            .arg(&enriched)
            .arg("highway")
            .arg("-o")
            .arg(o("tags_filter")),
    );
    assert_flags_dropped("tags-filter", &o("tags_filter"));

    // time-filter (a far-future cutoff keeps everything)
    assert_cmd_ok(
        "time-filter",
        CliInvoker::new()
            .arg("time-filter")
            .arg(&enriched)
            .arg("4102444800")
            .arg("-o")
            .arg(o("time_filter")),
    );
    assert_flags_dropped("time-filter", &o("time_filter"));

    // degrade (re-encode; strip inline locations)
    assert_cmd_ok(
        "degrade",
        CliInvoker::new()
            .arg("degrade")
            .arg(&enriched)
            .arg("--strip-locations")
            .arg("-o")
            .arg(o("degrade")),
    );
    assert_flags_dropped("degrade", &o("degrade"));

    // extract: complete (default), simple (-s), smart (--smart)
    let bbox = "11,54,13,56";
    assert_cmd_ok(
        "extract-complete",
        CliInvoker::new()
            .arg("extract")
            .arg(&enriched)
            .arg("-b")
            .arg(bbox)
            .arg("-o")
            .arg(o("extract_complete")),
    );
    assert_flags_dropped("extract-complete", &o("extract_complete"));

    assert_cmd_ok(
        "extract-simple",
        CliInvoker::new()
            .arg("extract")
            .arg(&enriched)
            .arg("-s")
            .arg("-b")
            .arg(bbox)
            .arg("-o")
            .arg(o("extract_simple")),
    );
    assert_flags_dropped("extract-simple", &o("extract_simple"));

    assert_cmd_ok(
        "extract-smart",
        CliInvoker::new()
            .arg("extract")
            .arg(&enriched)
            .arg("--smart")
            .arg("-b")
            .arg(bbox)
            .arg("-o")
            .arg(o("extract_smart")),
    );
    assert_flags_dropped("extract-smart", &o("extract_smart"));
}
