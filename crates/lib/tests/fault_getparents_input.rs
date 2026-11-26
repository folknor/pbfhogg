//! getparents against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
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

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::path::Path;

use pbfhogg::getid::parse_ids;
use pbfhogg::getparents::{GetparentsOptions, GetparentsStats, getparents_with_min_blobs};
use pbfhogg::read::input::InputSource;
use pbfhogg::write::WriteOptions;
use pbfhogg::{Element, MemberId};

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, write_multi_block_non_indexed_pbf,
    write_multi_block_test_pbf,
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

    /// The transport of each open on a direct run, following the
    /// getparents row of `crates/cli/src/io_reach.rs`: the dispatch probe
    /// direct, the
    /// estimate walk buffered, then the full scan's stream direct or the
    /// walker's scheduled preads buffered.
    fn transports(self) -> &'static str {
        match (self.unindexed, self.taken()) {
            (false, Arm::FullScan) => "DBD",
            (false, Arm::Walker) => "DBB",
            // No estimate walk: the probe, then the walker.
            (true, _) => "DB",
        }
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

fn relation(id: i64, members: Vec<MemberId>) -> TestRelation<'static> {
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
    let mut high_water = 0;
    host.invocation.for_each_counter(|name, value| {
        if name == "pipeline_reorder_high_water" {
            high_water = value;
        }
    });
    high_water > 0
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

/// Check `route` in every read mode through [`pinned_cell::check`]: the
/// reference with its exact transports and arm, then the replacement armed
/// after each open in turn, in both reopen modes. `expected_opens` is
/// fixed, so an open that stopped deriving from the pin (and so stopped
/// counting) cannot shrink the arming range unnoticed.
fn check_route(route: Route, expected_opens: u64) {
    assert_eq!(route.opens(), expected_opens, "{route:?}");
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_input(&input, route);
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            route.opens(),
            route.transports(),
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| {
                let stats = run(route, &input, &output, host)?;
                assert_eq!(
                    ran_full_scan(host),
                    route.taken() == Arm::FullScan,
                    "{route:?}: the run took the other arm"
                );
                Ok(stats)
            },
            |stats| {
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
                pinned_cell::take(&output)
            },
            |_, _| {
                write_input(&input, route);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, &output, step.host),
            |expected, (), step, judged| {
                let full_scan = ran_full_scan(step.host);
                match judged {
                    Judged::Unaffected(_) => {
                        assert_eq!(full_scan, route.taken() == Arm::FullScan, "{}", step.what);
                        assert_eq!(
                            &pinned_cell::take(&output),
                            expected,
                            "{}: the output must hold only what the pinned input yields",
                            step.what
                        );
                    }
                    Judged::Refused => {
                        // Refused at an open before the scan's: no pipeline ran.
                        assert!(!full_scan, "{}", step.what);
                        assert!(
                            !output.exists(),
                            "{}: a refused run must publish nothing",
                            step.what
                        );
                    }
                }
            },
        );
    }
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

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (stable reopen name) or the first
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
        let unreplaced = dir.path().join("unreplaced.osm.pbf");
        run(route, &input, &unreplaced, &crate::common::test_host())
            .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
        assert_eq!(elements(&unreplaced), EXPECTED, "{route:?}");
        let expected = pinned_cell::take(&unreplaced);
        for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
            write_input(&input, route);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            scenario.run(&input, &host, |pin, scenario| {
                std::fs::rename(&replacement, &input).expect("replace the input");
                let output = dir.path().join("out.osm.pbf");
                let result = if public_default {
                    pbfhogg::getparents::getparents(
                        pin,
                        crate::common::out(&output),
                        &query(),
                        &GetparentsOptions { add_self: true },
                        &WriteOptions::default(),
                        &pbfhogg::HeaderOverrides::default(),
                        &host,
                    )
                } else {
                    run(route, pin, &output, &host)
                };
                let cell = format!("{route:?}, getparents {public_default}, scenario {scenario:?}");
                match scenario {
                    reopen_cell::Scenario::Pinned => {
                        result.unwrap_or_else(|e| panic!("{cell}: {e}"));
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
                        assert!(!output.exists(), "{cell}: published an output");
                    }
                }
            });
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
