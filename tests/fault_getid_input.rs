//! getid and removeid against an input renamed over mid-run, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn, the last included.
//! Every open is taken on the calling thread, one at a time, so the counts
//! are exact.
//!
//! The opens, in order (see `Route::opens`):
//!
//! - include: the indexdata probe, the arm estimate walk (indexed input
//!   only; an unindexed one always takes the walker) and the walker. The
//!   streaming full-scan arm needs an estimate no fixture reaches through
//!   the public entry; a unit test in `src/commands/getid` covers it.
//! - `add_referenced`: the probe, pass 1's schedule walk (only with a way
//!   ID requested; its descriptor serves the classify pool) and pass 2's
//!   reader.
//! - removeid: the probe and the walker.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::getid::{GetidOptions, GetidStats, getid, parse_ids, removeid};
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestWay, generate_nodes, generate_ways, snapshot_dir, write_multi_block_non_indexed_pbf,
    write_multi_block_test_pbf, write_test_pbf_impl,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Include,
    AddReferenced,
    Remove,
}

#[derive(Clone, Copy, Debug)]
struct Route {
    mode: Mode,
    /// Whether a way ID is requested (with `add_referenced`, whether pass 1
    /// runs).
    ways: bool,
    /// The input carries no indexdata and the run is forced.
    unindexed: bool,
    direct: bool,
}

impl Route {
    const INCLUDE: Self = Self {
        mode: Mode::Include,
        ways: true,
        unindexed: false,
        direct: false,
    };

    const ADD_REFERENCED: Self = Self {
        mode: Mode::AddReferenced,
        ..Self::INCLUDE
    };

    const REMOVE: Self = Self {
        mode: Mode::Remove,
        ..Self::INCLUDE
    };

    fn opens(self) -> u64 {
        match self.mode {
            Mode::Include => 2 + u64::from(!self.unindexed),
            Mode::AddReferenced => 2 + u64::from(self.ways),
            Mode::Remove => 2,
        }
    }

    fn ids(self) -> Vec<String> {
        let mut ids = vec!["n10".to_string(), "n60".to_string()];
        if self.ways {
            ids.push("w1002".to_string());
        }
        ids
    }
}

/// Nodes 1..=100, 25 per blob, and five ways on nodes 1..=7.
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
    if route.unindexed {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &[], 25);
    } else {
        write_test_pbf_impl(path, &nodes, &ways, &[], true, Some(25));
    }
}

/// A valid PBF holding the same IDs with other coordinates and refs, and
/// other ways besides: a run that read any of it would write different
/// bytes, or fail.
fn write_replacement(path: &Path) {
    let mut nodes = generate_nodes(120, 1);
    for node in &mut nodes {
        node.lat += 7;
    }
    let ways = generate_ways(6, 1_000, 2, 50);
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<GetidStats> {
    let ids = parse_ids(&route.ids())?;
    let write = WriteOptions {
        direct_reads: route.direct,
        force: route.unindexed,
        ..WriteOptions::default()
    };
    let overrides = pbfhogg::HeaderOverrides::default();
    let output = crate::common::out(output);
    match route.mode {
        Mode::Remove => removeid(input, output, &ids, &write, &overrides, host),
        Mode::Include | Mode::AddReferenced => {
            let opts = GetidOptions {
                add_referenced: route.mode == Mode::AddReferenced,
                remove_tags: false,
                report_found: false,
            };
            getid(input, output, &ids, &opts, &write, &overrides, host)
        }
    }
}

/// What [`reference`] found.
enum Reference {
    /// The output's bytes.
    Output(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's output, with its open count checked: armed past the
/// last open, the replacement never fires and only counts. Skipped when the
/// run failed and `skippable` accepts its error; any other failure panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let output = dir.join("reference.osm.pbf");
    let host = crate::common::test_host();
    host.cancel
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
    let counted = host
        .cancel
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    // The run selected something, so an output read from the replacement
    // could not pass for it by being empty.
    assert!(
        stats.nodes_written + stats.ways_written > 0,
        "{route:?}: the reference run wrote nothing"
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
            if procfs || after == opens {
                result.unwrap_or_else(|e| {
                    panic!("{cell}: the run is unaffected by the replacement: {e}")
                });
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
fn include_reads_only_the_pinned_input() {
    check_route(Route::INCLUDE, 3);
    // Forced on an input without indexdata: no arm estimate.
    check_route(
        Route {
            unindexed: true,
            ..Route::INCLUDE
        },
        2,
    );
}

#[test]
fn add_referenced_reads_only_the_pinned_input() {
    check_route(Route::ADD_REFERENCED, 3);
    // No way requested: no pass 1.
    check_route(
        Route {
            ways: false,
            ..Route::ADD_REFERENCED
        },
        2,
    );
}

#[test]
fn removeid_reads_only_the_pinned_input() {
    check_route(Route::REMOVE, 2);
}

/// The probe, the walkers and pass 2's reader open `O_DIRECT` when asked
/// to, through the pin's own direct open.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for (route, expected_opens) in [
        (Route::INCLUDE, 3),
        (Route::ADD_REFERENCED, 3),
        (Route::REMOVE, 2),
    ] {
        let route = Route {
            direct: true,
            ..route
        };
        assert_eq!(route.opens(), expected_opens, "{route:?}");
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
/// afresh would read the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    for route in [Route::INCLUDE, Route::ADD_REFERENCED, Route::REMOVE] {
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
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &input).expect("replace the input");
            let output = dir.path().join("out.osm.pbf");
            let result = run(route, &pin, &output, &host);
            let cell = format!("{route:?}, procfs {procfs}");
            if procfs {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                assert_eq!(
                    std::fs::read(&output).expect("read output"),
                    expected,
                    "{cell}: the output must come from the pinned file"
                );
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

/// Every mode pins, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_in_every_mode() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    for route in [
        Route::INCLUDE,
        Route::ADD_REFERENCED,
        Route {
            ways: false,
            ..Route::ADD_REFERENCED
        },
        Route::REMOVE,
    ] {
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

/// `remove_tags` without `add_referenced` is refused before the input is
/// touched: ahead of a FIFO's refusal and of a missing input's.
#[test]
fn an_invalid_option_is_refused_before_the_input_is_touched() {
    let dir = crate::common::test_scratch();
    let mut inputs = vec![dir.path().join("missing.osm.pbf")];
    #[cfg(unix)]
    inputs.push(crate::common::make_fifo(dir.path(), "in.fifo"));
    let ids = parse_ids(&["n1".to_string()]).expect("parse ids");
    let opts = GetidOptions {
        add_referenced: false,
        remove_tags: true,
        report_found: false,
    };
    for input in inputs {
        let output = dir.path().join("out.osm.pbf");
        let err = getid(
            &input,
            crate::common::out(&output),
            &ids,
            &opts,
            &WriteOptions::default(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .err()
        .unwrap_or_else(|| panic!("{}: must be refused", input.display()));
        assert!(
            err.to_string().contains("remove_tags"),
            "{}: {err}",
            input.display()
        );
        assert!(!output.exists(), "{}", input.display());
    }
}
