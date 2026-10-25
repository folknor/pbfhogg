//! renumber against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. The replacement is armed after
//! each open in turn, the last included. Every open is taken on the calling
//! thread before any worker starts, so the counts are exact.
//!
//! The opens, in order: the header read (`O_DIRECT` under direct reads),
//! the one description every data phase preads (pass 1, stage 2d, R1 and
//! R2d), then the schedule walk's own description.
//!
//! Renumbering rewrites every ID, so a replacement that differed only in
//! IDs would renumber to the same output; this one differs in coordinates,
//! tags and counts, and the unreplaced output is checked by content before
//! it serves as the byte reference.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::MemberId;
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::renumber::{RenumberOptions, RenumberStats, renumber_external};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, generate_nodes, generate_ways, read_normalized, snapshot_dir,
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
        assert_eq!((node.lat, node.lon), (original.lat, original.lon), "{cell}");
        let named = node.tags.get("name").map(String::as_str);
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
            .map(|m| (m.member_type.clone(), m.ref_id, m.role.clone()))
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

/// What [`reference`] found.
enum Reference {
    /// The output's bytes.
    Output(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's output, its open count and content checked: armed
/// past the last open, the replacement never fires and only counts. Skipped
/// when the run failed and `skippable` accepts its error; any other failure
/// panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let output = dir.join("reference.osm.pbf");
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), OPENS + 1);
    let stats = match run(route, input, &output, &host) {
        Ok(stats) => stats,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    };
    let counted = host
        .cancel
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_renumbered_input(&output, &stats, &format!("{route:?}, unreplaced"));
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");
    Reference::Output(bytes)
}

/// Run `route` with the replacement armed after each open in turn, in both
/// reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[u8]) {
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    for after in 1..=OPENS {
        for force_path in [true, false] {
            write_input(input);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = PinnedInput::pin(input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.cancel
                .faults()
                .arm_input_replacement(input, &replacement, after);
            let before = snapshot_dir(dir);
            let result = run(route, input, &output, &host);

            let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
            let report = host
                .cancel
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(
                report.fired,
                "{cell}: the replacement must fire: {report:?}"
            );

            // After the last open nothing opens again, so the run reads only
            // what it already holds, in either mode.
            if procfs || after == OPENS {
                let stats = result.unwrap_or_else(|e| {
                    panic!("{cell}: the run is unaffected by the replacement: {e}")
                });
                assert_eq!(report.attempts, OPENS, "{cell}: {report:?}");
                assert_eq!(report.verified, OPENS, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    OPENS - after,
                    "{cell}: {report:?}"
                );
                assert_renumbered_input(&output, &stats, &cell);
                assert_eq!(
                    std::fs::read(&output).expect("read output"),
                    expected,
                    "{cell}: the output must hold only what the pinned input yields"
                );
                std::fs::remove_file(&output).expect("remove output");
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is
                // a Unix guarantee.
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
                assert_eq!(err.source_path(), Some(input), "{cell}: {err:?}");
                assert_eq!(report.refused, 1, "{cell}: {report:?}");
                assert_eq!(report.verified, after, "{cell}: {report:?}");
                assert_eq!(report.attempts, after + 1, "{cell}: {report:?}");
                assert_eq!(report.attempts_after_fired, 1, "{cell}: {report:?}");
                assert!(
                    !output.exists(),
                    "{cell}: a refused run must publish nothing"
                );
                // The replacement was renamed over the input, so it is the
                // one path the run itself did not account for.
                let mut after_run = snapshot_dir(dir);
                after_run.insert(PathBuf::from("replacement.osm.pbf"));
                crate::common::assert_scratch_unchanged(&before, &after_run);
            }
        }
    }
}

#[test]
fn buffered_reads_only_the_pinned_input() {
    let route = Route { direct: false };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input);
    let Reference::Output(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// The header read opens `O_DIRECT` when asked to, through the pin's own
/// direct open; the shared data description and the walk stay buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    let route = Route { direct: true };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input);
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    let Reference::Output(expected) =
        reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
    else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (procfs) or the first
/// open is refused (path reopen). An entry point that pinned the path
/// afresh would read the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let route = Route { direct: false };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    write_input(&input);
    let Reference::Output(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    for force_path in [true, false] {
        write_input(&input);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        host.cancel
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        std::fs::rename(&replacement, &input).expect("replace the input");
        let output = dir.path().join("out.osm.pbf");
        let result = run(route, &pin, &output, &host);
        let cell = format!("caller pin, procfs {procfs}");
        if procfs {
            let stats = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            assert_renumbered_input(&output, &stats, &cell);
            assert_eq!(
                std::fs::read(&output).expect("read output"),
                expected,
                "{cell}: the output must come from the pinned file"
            );
            std::fs::remove_file(&output).expect("remove output");
        } else if cfg!(unix) {
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
