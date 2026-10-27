//! getparents against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn, the last included.
//! Every open is taken on the calling thread, one at a time, so the counts
//! are exact.
//!
//! The opens, in order (see `Route::opens`): the indexdata probe, the arm
//! estimate walk (indexed input only; an unindexed one always takes the
//! walker), then the header walker (whose descriptor serves the classify
//! pool) or the full-scan arm's streaming reader.
//!
//! The arm is chosen through the public `getparents_with_min_blobs` (a
//! threshold of one picks the full scan on indexed input, `u64::MAX` the
//! walker), and each run checks the arm it took on its own token: only the
//! full scan runs the fused read pipeline, which records its reorder high
//! water there, so it is positive after a full-scan run that decoded
//! anything and zero after a walker run or a run refused before the scan.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::getid::parse_ids;
use pbfhogg::getparents::{GetparentsOptions, GetparentsStats, getparents_with_min_blobs};
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::write::WriteOptions;
use pbfhogg::{Element, MemberId};

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, snapshot_dir,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    Walker,
    FullScan,
}

#[derive(Clone, Copy, Debug)]
struct Route {
    /// The arm asked for; an unindexed input takes the walker regardless.
    arm: Arm,
    /// The input carries no indexdata.
    unindexed: bool,
    direct: bool,
}

impl Route {
    const WALKER: Self = Self {
        arm: Arm::Walker,
        unindexed: false,
        direct: false,
    };

    const FULL_SCAN: Self = Self {
        arm: Arm::FullScan,
        ..Self::WALKER
    };

    fn opens(self) -> u64 {
        2 + u64::from(!self.unindexed)
    }

    fn min_blobs(self) -> u64 {
        match self.arm {
            Arm::Walker => u64::MAX,
            Arm::FullScan => 1,
        }
    }

    /// The arm the run takes.
    fn taken(self) -> Arm {
        if self.unindexed {
            Arm::Walker
        } else {
            self.arm
        }
    }
}

/// The parents and selves the query yields on the original input, in file
/// order.
const EXPECTED: &[(char, i64)] = &[
    ('n', 3),
    ('w', 1_000),
    ('w', 1_001),
    ('w', 1_002),
    ('r', 2_000),
    ('r', 2_001),
];

fn relation(id: i64, members: Vec<MemberId>) -> TestRelation {
    TestRelation {
        id,
        members: members
            .into_iter()
            .map(|id| TestMember { id, role: "" })
            .collect(),
        tags: vec![("type", "route")],
        meta: None,
    }
}

/// Nodes 1..=100, 25 per blob; five ways on nodes 1..=7; relation 2000 on
/// node 3 and way 1001, relation 2001 on relation 2000.
fn write_input(path: &Path, route: Route) {
    let nodes = generate_nodes(100, 1);
    let ways: Vec<TestWay> = (0..5_i64)
        .map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![k + 1, k + 2, k + 3],
            tags: vec![("highway", "primary")],
            meta: None,
        })
        .collect();
    let relations = [
        relation(2_000, vec![MemberId::Node(3), MemberId::Way(1_001)]),
        relation(2_001, vec![MemberId::Relation(2_000)]),
    ];
    if route.unindexed {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 25);
    } else {
        write_multi_block_test_pbf(path, &nodes, &ways, &relations, 25);
    }
}

/// A valid PBF holding the same IDs, every one of them changed: the nodes
/// moved, the ways on other nodes, the relations on other members. A run
/// that read any of it writes other bytes (node 3, way 1001 and relation
/// 2000 are selves, so they are written from whichever file was read).
fn write_replacement(path: &Path) {
    let mut nodes = generate_nodes(120, 1);
    for node in &mut nodes {
        node.lat += 7;
    }
    let ways: Vec<TestWay> = (0..6_i64)
        .map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![50 + k, 51 + k],
            tags: vec![("highway", "service")],
            meta: None,
        })
        .collect();
    let relations = [
        relation(2_000, vec![MemberId::Node(60)]),
        relation(2_001, vec![MemberId::Way(1_004)]),
    ];
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, 7);
}

/// A node, a way and a relation, each with parents in the original input
/// and each written itself (`add_self`).
fn query() -> pbfhogg::getid::ElementIds {
    parse_ids(&["n3".to_string(), "w1001".to_string(), "r2000".to_string()]).expect("parse ids")
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<GetparentsStats> {
    let write = WriteOptions {
        direct_reads: route.direct,
        ..WriteOptions::default()
    };
    getparents_with_min_blobs(
        input,
        crate::common::out(output),
        &query(),
        &GetparentsOptions { add_self: true },
        &write,
        &pbfhogg::HeaderOverrides::default(),
        route.min_blobs(),
        host,
    )
}

/// Whether the run on `host` ran the full-scan arm's pipeline. Read before
/// anything else uses the token.
fn ran_full_scan(host: &pbfhogg::host::Host) -> bool {
    host.invocation
        .faults()
        .reorder_filled_high_water
        .load(Ordering::Relaxed)
        > 0
}

/// The output's elements as (kind, id), in file order.
fn elements(path: &Path) -> Vec<(char, i64)> {
    let mut out = Vec::new();
    pbfhogg::ElementReader::open(path, false)
        .expect("open output")
        .for_each(|element| match element {
            Element::DenseNode(node) => out.push(('n', node.id())),
            Element::Node(node) => out.push(('n', node.id())),
            Element::Way(way) => out.push(('w', way.id())),
            Element::Relation(relation) => out.push(('r', relation.id())),
            // No other kind exists today; one that did would fail the
            // comparison.
            _ => out.push(('?', 0)),
        })
        .expect("read output");
    out
}

/// What [`reference`] found.
enum Reference {
    /// The output's bytes.
    Output(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's output, with its open count and arm checked: armed
/// past the last open, the replacement never fires and only counts. Skipped
/// when the run failed and `skippable` accepts its error; any other failure
/// panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let output = dir.join("reference.osm.pbf");
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), opens + 1);
    let stats = match run(route, input, &output, &host) {
        Ok(stats) => stats,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    };
    assert_eq!(
        ran_full_scan(&host),
        route.taken() == Arm::FullScan,
        "{route:?}: the run took the other arm"
    );
    let counted = host
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_eq!(elements(&output), EXPECTED, "{route:?}");
    assert_eq!(
        (
            stats.nodes_written,
            stats.ways_written,
            stats.relations_written
        ),
        (1, 3, 2),
        "{route:?}"
    );
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");
    Reference::Output(bytes)
}

/// Run `route` with the replacement armed after each of its opens in turn,
/// in both reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[u8]) {
    let opens = route.opens();
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    for after in 1..=opens {
        for force_path in [true, false] {
            write_input(input, route);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = PinnedInput::pin(input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.invocation
                .faults()
                .arm_input_replacement(input, &replacement, after);
            let before = snapshot_dir(dir);
            let result = run(route, input, &output, &host);
            let full_scan = ran_full_scan(&host);

            let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
            let report = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(
                report.fired,
                "{cell}: the replacement must fire: {report:?}"
            );

            // After the last open nothing opens again, so the run reads only
            // what it already holds, in either mode.
            if procfs || after == opens {
                result.unwrap_or_else(|e| {
                    panic!("{cell}: the run is unaffected by the replacement: {e}")
                });
                assert_eq!(full_scan, route.taken() == Arm::FullScan, "{cell}");
                assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                assert_eq!(report.verified, opens, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    opens - after,
                    "{cell}: {report:?}"
                );
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
                // Refused at an open before the scan's: no pipeline ran.
                assert!(!full_scan, "{cell}");
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

fn check_route(route: Route, expected_opens: u64) {
    // Fixed counts, so an open that stopped deriving from the pin (and so
    // stopped counting) cannot shrink the arming range unnoticed.
    assert_eq!(route.opens(), expected_opens, "{route:?}");
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Output(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
}

#[test]
fn walker_reads_only_the_pinned_input() {
    check_route(Route::WALKER, 3);
    // An input without indexdata: no arm estimate, and the walker even
    // where the full scan was asked for.
    for arm in [Arm::Walker, Arm::FullScan] {
        check_route(
            Route {
                arm,
                unindexed: true,
                ..Route::WALKER
            },
            2,
        );
    }
}

#[test]
fn full_scan_reads_only_the_pinned_input() {
    check_route(Route::FULL_SCAN, 3);
}

/// The probe and the full scan's reader open `O_DIRECT` when asked to,
/// through the pin's own direct open (the header walks stay buffered).
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for route in [Route::WALKER, Route::FULL_SCAN] {
        let route = Route {
            direct: true,
            ..route
        };
        assert_eq!(route.opens(), 3, "{route:?}");
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        write_input(&input, route);
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        let Reference::Output(expected) =
            reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
        else {
            return;
        };
        check_replacements(route, dir.path(), &input, &expected);
    }
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (procfs) or the first
/// open is refused (path reopen). An entry point that pinned the path
/// afresh would read the replacement instead. Both public entries.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    // `getparents` takes the default threshold: the walker on a fixture.
    for (route, public_default) in [
        (Route::WALKER, false),
        (Route::FULL_SCAN, false),
        (Route::WALKER, true),
    ] {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        write_input(&input, route);
        let Reference::Output(expected) = reference(route, &input, dir.path(), |_| false) else {
            unreachable!("no failure is skippable here");
        };
        for force_path in [true, false] {
            write_input(&input, route);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &input).expect("replace the input");
            let output = dir.path().join("out.osm.pbf");
            let result = if public_default {
                pbfhogg::getparents::getparents(
                    &pin,
                    crate::common::out(&output),
                    &query(),
                    &GetparentsOptions { add_self: true },
                    &WriteOptions::default(),
                    &pbfhogg::HeaderOverrides::default(),
                    &host,
                )
            } else {
                run(route, &pin, &output, &host)
            };
            let cell = format!("{route:?}, getparents {public_default}, procfs {procfs}");
            if procfs {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
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
                assert!(!output.exists(), "{cell}: published an output");
            }
        }
    }
}

/// Every route pins, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_on_both_arms() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    for route in [Route::WALKER, Route::FULL_SCAN] {
        let output = dir.path().join("out.osm.pbf");
        let err = run(route, &fifo, &output, &crate::common::test_host())
            .err()
            .unwrap_or_else(|| panic!("{route:?}: a FIFO must be refused"));
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
            "{route:?}: {err:?}"
        );
        assert!(
            err.to_string().contains("not a regular file"),
            "{route:?}: {err}"
        );
        assert!(
            !output.exists(),
            "{route:?}: a refused run publishes nothing"
        );
    }
}
