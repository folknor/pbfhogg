//! check --ids against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
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
//! covered in `cli/tests/cli_input_contract.rs`; the caller-pin tests here
//! cover the library side of it for all three checks.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::check::CheckOptions;
use pbfhogg::check::verify_ids::{VerifyIdsOptions, verify_ids};
use pbfhogg::read::input::{InputSource, PinnedInput};

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
        check: CheckOptions {
            max_errors: 0,
            direct_io: route.direct,
        },
        full: route.full,
        type_filter: route.ways_only.then_some(pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: false,
        }),
    };
    Ok(verify_ids(input, &opts, host)?.to_json_value("in"))
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
        "passed": true,
        "total_violations": 0,
        "violations": [],
    })
}

/// What [`reference`] found.
enum Reference {
    Report(serde_json::Value),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's report, with its open count checked: armed past the
/// last open, the replacement never fires and only counts. Skipped when the
/// run failed and `skippable` accepts its error; any other failure panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), OPENS + 1);
    let report = match run(route, input, &host) {
        Ok(report) => report,
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
    assert_eq!(report, expected(route), "{route:?}: the unreplaced report");
    Reference::Report(report)
}

/// Run `route` with the replacement armed after each of its opens in turn,
/// in both reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &serde_json::Value) {
    let replacement = dir.join("replacement.osm.pbf");
    for after in 1..=OPENS {
        for force_path in [true, false] {
            write_input(input, route.unindexed);
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
            let before = crate::common::snapshot_dir(dir);
            let result = run(route, input, &host);

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
                let got = result.unwrap_or_else(|e| {
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
                assert_eq!(
                    &got, expected,
                    "{cell}: the report must come only from the pinned input"
                );
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
                assert_eq!(report.refused, 1, "{cell}: {report:?}");
                assert_eq!(report.verified, after, "{cell}: {report:?}");
                assert_eq!(report.attempts, after + 1, "{cell}: {report:?}");
                assert_eq!(report.attempts_after_fired, 1, "{cell}: {report:?}");
                // The replacement was renamed over the input, so it is the
                // one path the run itself did not account for.
                let mut after_run = crate::common::snapshot_dir(dir);
                after_run.insert(PathBuf::from("replacement.osm.pbf"));
                crate::common::assert_scratch_unchanged(&before, &after_run);
            }
        }
    }
}

fn check_route(route: Route) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route.unindexed);
    let Reference::Report(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
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

/// On an input without indexdata every blob is scheduled for all three
/// kinds; the probe and the walk still open once each.
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

/// The probe opens `O_DIRECT` when asked to, through the pin's own direct
/// open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for full in [false, true] {
        let route = Route {
            full,
            direct: true,
            ..Route::PLAIN
        };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        write_input(&input, false);
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        let Reference::Report(expected) =
            reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
        else {
            return;
        };
        check_replacements(route, dir.path(), &input, &expected);
    }
}

/// Each check run through a caller's pin, the input replaced between the
/// pin and the call: the check reads the file the caller pinned (procfs) or
/// refuses its first open (path reopen). A check that pinned or opened the
/// path afresh would read the replacement instead. This is what lets the
/// CLI compose the three checks on one pin.
#[test]
fn every_check_uses_a_caller_pin_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let opts = CheckOptions {
        max_errors: 0,
        direct_io: false,
    };
    let refs_opts = pbfhogg::check::refs::CheckRefsOptions {
        check: opts,
        check_relations: false,
        show_ids: false,
    };
    for force_path in [true, false] {
        for check in ["ids", "index", "refs"] {
            write_input(&input, false);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &input).expect("replace the input");
            // `(node count, data blob count)` where the check reports it.
            let result: pbfhogg::Result<(u64, Option<u64>)> = match check {
                "ids" => run(Route::PLAIN, &pin, &host)
                    .map(|r| (r["counts"]["nodes"].as_u64().expect("node count"), None)),
                "index" => pbfhogg::check::index::check_index(&pin, &opts, &host)
                    .map(|r| (0, Some(r.blobs))),
                _ => pbfhogg::check::refs::check_refs(&pin, &refs_opts, &host)
                    .map(|r| (r.node_count, None)),
            };
            let cell = format!("{check}, procfs {procfs}");
            if procfs {
                let (nodes, blobs) = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                match check {
                    // Four node blobs and one way blob in the pinned file,
                    // six and one in the replacement.
                    "index" => assert_eq!(blobs, Some(5), "{cell}"),
                    _ => assert_eq!(nodes, 100, "{cell}: must read the pinned file"),
                }
            } else if cfg!(unix) {
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
            }
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
