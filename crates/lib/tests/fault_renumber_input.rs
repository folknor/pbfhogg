//! renumber against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. The replacement is armed after
//! each open in turn, the last included. Every open is taken on the calling
//! thread before any worker starts, so the counts are exact.
//!
//! The opens, in order: the header read, the one description every data
//! phase preads (pass 1, stage 2d, R1 and R2d), then the schedule walk's own
//! description; all three `O_DIRECT` under direct reads.
//!
//! Renumbering rewrites every ID, so a replacement that differed only in
//! IDs would renumber to the same output; this one differs in coordinates,
//! tags and counts, and the unreplaced output is checked by content before
//! it serves as the byte reference.

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::path::Path;

use pbfhogg::MemberId;
use pbfhogg::read::input::InputSource;
use pbfhogg::renumber::{RenumberOptions, RenumberStats, renumber_external};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, generate_nodes, generate_ways, read_normalized,
    write_multi_block_test_pbf,
};

/// Every run opens the input this many times, on either route.
const OPENS: u64 = 3;

/// Nodes 10..=49 (node 25 tagged), ways 100..=104 over three nodes each,
/// relation 500 (a node and a way) and relation 600 (relation 500 and a
/// node), ten elements per block: four node blobs, so every data phase runs
/// and pass 1 has several blobs to share out.
fn write_input(path: &Path) {
    let mut nodes = generate_nodes(40, 10);
    for node in &mut nodes {
        if node.id == 25 {
            node.tags = vec![("name", "pinned")];
        }
    }
    let ways = generate_ways(5, 100, 3, 10);
    let relations = vec![
        TestRelation {
            id: 500,
            members: vec![
                TestMember {
                    id: MemberId::Node(10),
                    role: "stop",
                },
                TestMember {
                    id: MemberId::Way(102),
                    role: "outer",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        },
        TestRelation {
            id: 600,
            members: vec![
                TestMember {
                    id: MemberId::Relation(500),
                    role: "subarea",
                },
                TestMember {
                    id: MemberId::Node(20),
                    role: "label",
                },
            ],
            tags: vec![("type", "boundary")],
            meta: None,
        },
    ];
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, 10);
}

/// A valid, sorted, indexed PBF with other coordinates, no tags and other
/// counts: a run that read any of it would write different bytes, or fail.
fn write_replacement(path: &Path) {
    let nodes = generate_nodes(70, 3_001);
    let ways = generate_ways(9, 7_000, 4, 3_001);
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

#[derive(Clone, Copy, Debug)]
struct Route {
    direct: bool,
}

fn opts() -> RenumberOptions {
    RenumberOptions {
        start_node_id: 1,
        start_way_id: 1,
        start_relation_id: 1,
    }
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<RenumberStats> {
    renumber_external(
        input,
        crate::common::out(output),
        &opts(),
        &WriteOptions {
            direct_reads: route.direct,
            ..WriteOptions::default()
        },
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
}

/// The renumbered input, checked element by element against what
/// [`write_input`] wrote.
fn assert_renumbered_input(output: &Path, stats: &RenumberStats, cell: &str) {
    assert_eq!(
        (
            stats.nodes_written,
            stats.ways_written,
            stats.relations_written
        ),
        (40, 5, 2),
        "{cell}"
    );
    let norm = read_normalized(output);
    let originals = generate_nodes(40, 10);
    assert_eq!(norm.nodes.len(), originals.len(), "{cell}");
    for ((node, original), new_id) in norm.nodes.iter().zip(&originals).zip(1_i64..) {
        assert_eq!(node.id, new_id, "{cell}");
        assert_eq!(
            (node.decimicro_lat(), node.decimicro_lon()),
            (original.lat, original.lon),
            "{cell}"
        );
        let named = node.tag("name");
        assert_eq!(named, (original.id == 25).then_some("pinned"), "{cell}");
    }
    assert_eq!(norm.ways.len(), 5, "{cell}");
    for (way, new_id) in norm.ways.iter().zip(1_i64..) {
        assert_eq!(way.id, new_id, "{cell}");
        // Nodes 10, 11 and 12 renumber to 1, 2 and 3.
        assert_eq!(way.refs, [1, 2, 3], "{cell}");
    }
    let members = |r: usize| -> Vec<(String, i64, String)> {
        norm.relations[r]
            .members
            .iter()
            .map(|m| (m.member_type.as_str().to_owned(), m.ref_id, m.role.clone()))
            .collect()
    };
    assert_eq!(norm.relations.len(), 2, "{cell}");
    assert_eq!(norm.relations[0].id, 1, "{cell}");
    assert_eq!(
        members(0),
        [
            ("node".to_string(), 1, "stop".to_string()),
            ("way".to_string(), 3, "outer".to_string()),
        ],
        "{cell}"
    );
    assert_eq!(norm.relations[1].id, 2, "{cell}");
    assert_eq!(
        members(1),
        [
            ("relation".to_string(), 1, "subarea".to_string()),
            ("node".to_string(), 11, "label".to_string()),
        ],
        "{cell}"
    );
}

/// Every route in every read mode through [`pinned_cell::check`]: the
/// reference with its exact transports (every open direct on a direct run:
/// the header read, the shared data description and the walk), then the
/// replacement armed after each open in turn, in both reopen modes.
#[test]
fn every_open_reads_only_the_pinned_input() {
    for &direct in pinned_cell::modes() {
        let route = Route { direct };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_input(&input);
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            OPENS,
            "DDD",
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| run(route, &input, &output, host),
            |stats| {
                assert_renumbered_input(&output, &stats, &format!("{route:?}, unreplaced"));
                pinned_cell::take(&output)
            },
            |_, _| {
                write_input(&input);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, &output, step.host),
            |expected, (), step, judged| match judged {
                Judged::Unaffected(stats) => {
                    assert_renumbered_input(&output, &stats, &step.what);
                    assert_eq!(
                        &pinned_cell::take(&output),
                        expected,
                        "{}: the output must hold only what the pinned input yields",
                        step.what
                    );
                }
                Judged::Refused => assert!(
                    !output.exists(),
                    "{}: a refused run must publish nothing",
                    step.what
                ),
            },
        );
    }
}

/// The output of an unreplaced run of `route` on `input`.
fn unreplaced(route: Route, input: &Path, dir: &Path) -> Vec<u8> {
    let output = dir.join("unreplaced.osm.pbf");
    let stats = run(route, input, &output, &crate::common::test_host())
        .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
    assert_renumbered_input(&output, &stats, &format!("{route:?}, unreplaced"));
    pinned_cell::take(&output)
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (stable reopen name) or the first
/// open is refused (path reopen). An entry point that pinned the path
/// afresh would read the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let route = Route { direct: false };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    write_input(&input);
    let expected = unreplaced(route, &input, dir.path());
    for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
        write_input(&input);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        scenario.run(&input, &host, |pin, scenario| {
            std::fs::rename(&replacement, &input).expect("replace the input");
            let output = dir.path().join("out.osm.pbf");
            let result = run(route, pin, &output, &host);
            let cell = format!("caller pin, scenario {scenario:?}");
            match scenario {
                reopen_cell::Scenario::Pinned => {
                    let stats = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                    assert_renumbered_input(&output, &stats, &cell);
                    assert_eq!(
                        std::fs::read(&output).expect("read output"),
                        expected,
                        "{cell}: the output must come from the pinned file"
                    );
                    std::fs::remove_file(&output).expect("remove output");
                }
                reopen_cell::Scenario::Path => {
                    let err = result
                        .err()
                        .unwrap_or_else(|| panic!("{cell}: must refuse"));
                    assert!(
                        matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                        "{cell}: {err:?}"
                    );
                    assert_eq!(err.source_path(), Some(input.as_path()), "{cell}: {err:?}");
                    assert!(!output.exists(), "{cell}: published an output");
                }
            }
        });
    }
}

/// renumber pins, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");
    let err = run(
        Route { direct: false },
        &fifo,
        &output,
        &crate::common::test_host(),
    )
    .err()
    .unwrap_or_else(|| panic!("a FIFO must be refused"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("not a regular file"), "{err}");
    assert!(!output.exists(), "a refused run publishes nothing");
}
