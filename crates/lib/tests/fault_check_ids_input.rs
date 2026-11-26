//! check --ids against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
//! `InputReplaced`. Each route is checked with the replacement armed after
//! each of its opens in turn, the last included. Both opens are taken on the
//! calling thread, one at a time, so the counts are exact.
//!
//! The opens, in order, on every route: the header and indexdata probe, then
//! the schedule walk, whose descriptor the classify pool preads. Streaming
//! and `--full`, an unindexed input, a type filter and direct reads change
//! neither. The `--full` allocation bound reads the pin's size; no report
//! can show where that size came from, so that part rests on the code.
//!
//! The CLI's composition of `--ids`, `--index` and `--refs` on one pin is
//! covered in `crates/cli/tests/cli_input_contract.rs`; the caller-pin tests here
//! cover the library side of it for all three checks.

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::path::Path;

use pbfhogg::check::verify_ids::{VerifyIdsOptions, verify_ids};
use pbfhogg::read::input::InputSource;

use crate::common::{
    TestWay, generate_nodes, write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

/// Every route opens the input twice after the pin.
const OPENS: u64 = 2;

#[derive(Clone, Copy, Debug)]
struct Route {
    full: bool,
    /// The input carries no indexdata (check needs no `--force` for it).
    unindexed: bool,
    /// Ways only.
    ways_only: bool,
    direct: bool,
}

impl Route {
    const PLAIN: Self = Self {
        full: false,
        unindexed: false,
        ways_only: false,
        direct: false,
    };
}

/// Nodes 1..=100, 25 per blob, and five ways: every ID unique and in
/// order, so the report is the same whatever the worker timing.
fn write_input(path: &Path, unindexed: bool) {
    write_fixture(path, unindexed, 100, 5);
}

/// More nodes and ways, and indexed: a run that read any of it would count
/// differently (and an unindexed route would see its indexdata).
fn write_replacement(path: &Path) {
    write_fixture(path, false, 130, 9);
}

fn write_fixture(path: &Path, unindexed: bool, node_count: usize, way_count: i64) {
    let nodes = generate_nodes(node_count, 1);
    let ways: Vec<TestWay> = (0..way_count)
        .map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![k + 1, k + 2, k + 3],
            tags: vec![("highway", "primary")],
            meta: None,
        })
        .collect();
    if unindexed {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &[], 25);
    } else {
        write_multi_block_test_pbf(path, &nodes, &ways, &[], 25);
    }
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<serde_json::Value> {
    let opts = VerifyIdsOptions {
        max_errors: 0,
        direct_io: route.direct,
        full: route.full,
        type_filter: route.ways_only.then_some(pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: false,
        }),
    };
    Ok(match verify_ids(input, &opts, host)? {
        pbfhogg::check::CheckOutcome::Complete(report) => report.to_json_value("in"),
        pbfhogg::check::CheckOutcome::Stopped(stopped) => stopped.to_json_value("in"),
    })
}

/// What the unreplaced run of `route` reports, from the fixture's
/// construction.
fn expected(route: Route) -> serde_json::Value {
    let (nodes, ways) = if route.ways_only { (0, 5) } else { (100, 5) };
    serde_json::json!({
        "file": "in",
        "header_sorted": true,
        "indexed": !route.unindexed,
        "counts": { "nodes": nodes, "ways": ways, "relations": 0 },
        "has_negative_ids": false,
        "passed": true,
        "total_violations": 0,
        "violations": [],
    })
}

/// Check `route` in every read mode through [`pinned_cell::check`]: the
/// reference with its exact transports (the probe direct on a direct run,
/// the walk buffered, following the check row of
/// `crates/cli/src/io_reach.rs`), then the replacement armed after each
/// open in turn, in both reopen modes.
fn check_route(route: Route) {
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        write_input(&input, route.unindexed);
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            OPENS,
            "DB",
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| run(route, &input, host),
            |report| {
                assert_eq!(report, expected(route), "{route:?}: the unreplaced report");
                report
            },
            |_, _| {
                write_input(&input, route.unindexed);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, step.host),
            |expected, (), step, judged| {
                if let Judged::Unaffected(got) = judged {
                    assert_eq!(
                        &got, expected,
                        "{}: the report must come only from the pinned input",
                        step.what
                    );
                }
            },
        );
    }
}

#[test]
fn streaming_and_full_read_only_the_pinned_input() {
    for full in [false, true] {
        check_route(Route {
            full,
            ..Route::PLAIN
        });
    }
}

/// On an input without indexdata every blob is read in one pass for every
/// kind; the probe and the walk still open once each.
#[test]
fn an_unindexed_input_reads_only_the_pinned_input() {
    for full in [false, true] {
        check_route(Route {
            full,
            unindexed: true,
            ..Route::PLAIN
        });
    }
}

#[test]
fn a_type_filter_reads_only_the_pinned_input() {
    for full in [false, true] {
        check_route(Route {
            full,
            ways_only: true,
            ..Route::PLAIN
        });
    }
}

/// Each check run through a caller's pin, the input replaced between the
/// pin and the call: the check reads the file the caller pinned (stable reopen name) or
/// refuses its first open (path reopen). A check that pinned or opened the
/// path afresh would read the replacement instead. This is what lets the
/// CLI compose the three checks on one pin.
#[test]
fn every_check_uses_a_caller_pin_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let opts = pbfhogg::check::index::CheckIndexOptions {
        max_errors: 0,
        direct_io: false,
    };
    let refs_opts = pbfhogg::check::refs::CheckRefsOptions {
        direct_io: false,
        check_relations: false,
        show_ids: None,
    };
    for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
        for check in ["ids", "index", "refs"] {
            write_input(&input, false);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            scenario.run(&input, &host, |pin, scenario| {
                std::fs::rename(&replacement, &input).expect("replace the input");
                // `(node count, data blob count)` where the check reports it.
                let result: pbfhogg::Result<(u64, Option<u64>)> = match check {
                    "ids" => run(Route::PLAIN, pin, &host)
                        .map(|r| (r["counts"]["nodes"].as_u64().expect("node count"), None)),
                    "index" => pbfhogg::check::index::check_index(pin, &opts, &host)
                        .map(|r| (0, Some(r.blobs))),
                    _ => pbfhogg::check::refs::check_refs(pin, &refs_opts, &host)
                        .map(|r| (r.complete().expect("complete").node_count, None)),
                };
                let cell = format!("{check}, scenario {scenario:?}");
                match scenario {
                    reopen_cell::Scenario::Pinned => {
                        let (nodes, blobs) = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                        match check {
                            // Four node blobs and one way blob in the pinned file,
                            // six and one in the replacement.
                            "index" => assert_eq!(blobs, Some(5), "{cell}"),
                            _ => assert_eq!(nodes, 100, "{cell}: must read the pinned file"),
                        }
                    }
                    reopen_cell::Scenario::Path => {
                        let err = result
                            .err()
                            .unwrap_or_else(|| panic!("{cell}: must refuse"));
                        assert!(
                            matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                            "{cell}: {err:?}"
                        );
                    }
                }
            });
        }
    }
}

/// The input is pinned, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    for full in [false, true] {
        let route = Route {
            full,
            ..Route::PLAIN
        };
        let err = run(route, &fifo, &crate::common::test_host())
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
    }
}
