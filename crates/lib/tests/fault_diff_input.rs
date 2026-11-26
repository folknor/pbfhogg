//! `diff` and `diff_stats` against an input renamed over mid-run, through
//! the fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Both inputs are pinned before either is read, and every later open of
//! each is derived from its pin. With an injected stable scratch reopen name
//! a replaced input does not affect the run, and where opens
//! go through the path the run fails with `InputReplaced`. The replacement
//! is armed on each input in turn, after each of its opens, the last
//! included. Every open is taken on the calling thread, one at a time, so
//! the counts are exact: the planner thread streams frames from the cursors
//! the caller opened, and the workers merge in-memory frame bodies.
//!
//! The opens of one input, in order (see `Route::opens`): the front read of
//! the header and the first blob's indexdata; then, when both inputs carry
//! indexdata, the one sequential blob cursor the work-unit planner reads (at
//! any `-j`, mixed-sign inputs and type filters included), or the element
//! stream reader of a route with an unindexed side.
//!
//! `diff` writes to a caller's `Write`, so a run refused at a later open
//! may already have written text; what it owes is the typed refusal.

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::path::{Path, PathBuf};

use pbfhogg::TypeFilter;
use pbfhogg::diff::DiffOptions;
use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{
    TestNode, generate_nodes, generate_nodes_with_negatives, generate_relations, generate_ways,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

#[derive(Clone, Copy, Debug)]
struct Route {
    /// Worker limit (`-j`): 1 or 3 merge workers over the same planner.
    jobs: usize,
    /// `diff_stats` instead of `diff`.
    count: bool,
    /// `-t node`: the kinds after the node phase are excluded.
    nodes_only: bool,
    /// The fixtures hold negative IDs.
    negative: bool,
    /// Which of the two inputs carry no indexdata.
    unindexed: [bool; 2],
    direct: bool,
}

impl Route {
    const SEQUENTIAL: Self = Self {
        jobs: 1,
        count: false,
        nodes_only: false,
        negative: false,
        unindexed: [false, false],
        direct: false,
    };
    const PARALLEL: Self = Self {
        jobs: 3,
        ..Self::SEQUENTIAL
    };

    /// Opens of either input through its pin: the front read, then the
    /// planner's blob cursor (both inputs indexed) or the element stream
    /// reader (an unindexed side).
    fn opens(self) -> u64 {
        2
    }

    fn type_filter(self) -> Option<TypeFilter> {
        self.nodes_only.then_some(TypeFilter {
            nodes: true,
            ways: false,
            relations: false,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// Input `side` of `route`: the new side has four more nodes and every
/// fifth node moved. The replacement moves every node, so a run that read
/// any of it counts other differences.
fn write_input(path: &Path, route: Route, side: Side, replacement: bool) {
    let extra = if side == Side::New { 4 } else { 0 };
    let mut nodes: Vec<TestNode> = if route.negative {
        generate_nodes_with_negatives(10, 30 + extra)
    } else {
        generate_nodes(40 + extra, 1)
    };
    for (i, node) in nodes.iter_mut().enumerate() {
        if side == Side::New && i % 5 == 0 {
            node.lat = node.lat.saturating_add(1_000);
        }
        if replacement {
            node.lat = node.lat.saturating_add(7);
        }
    }
    let ways = generate_ways(5, 1, 3, 1);
    let relations = generate_relations(2, 1, 2, 1);
    let index = usize::from(side == Side::New);
    if route.unindexed[index] {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 10);
    } else {
        write_multi_block_test_pbf(path, &nodes, &ways, &relations, 10);
    }
}

struct Inputs {
    dir: pbfhogg_test_scratch::TestScratch,
    old: PathBuf,
    new: PathBuf,
}

impl Inputs {
    fn path(&self, side: Side) -> &Path {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }
}

fn inputs(route: Route) -> Inputs {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    write_input(&old, route, Side::Old, false);
    write_input(&new, route, Side::New, false);
    Inputs { dir, old, new }
}

/// Run `route`: the text and the counts for `diff`, the counts alone for
/// `diff_stats`.
fn run(
    route: Route,
    old: InputSource<'_>,
    new: InputSource<'_>,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    let parallelism = pbfhogg::Parallelism::from_jobs(route.jobs);
    if route.count {
        let stats = pbfhogg::diff::diff_stats(
            old,
            new,
            route.type_filter(),
            parallelism,
            false,
            route.direct,
            host,
        )?;
        return Ok(format!("{stats:?}"));
    }
    let options = DiffOptions {
        suppress_common: false,
        verbose: false,
        type_filter: route.type_filter(),
        parallelism,
        locations: false,
    };
    let mut text = Vec::new();
    let stats = pbfhogg::diff::diff(old, new, &mut text, &options, route.direct, host)?;
    Ok(format!("{}{stats:?}", String::from_utf8_lossy(&text)))
}

fn run_paths(
    route: Route,
    inputs: &Inputs,
    pinned: Option<(Side, &PinnedInput)>,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    run(
        route,
        match pinned {
            Some((Side::Old, pin)) => InputSource::Pinned(pin),
            _ => InputSource::Path(&inputs.old),
        },
        match pinned {
            Some((Side::New, pin)) => InputSource::Pinned(pin),
            _ => InputSource::Path(&inputs.new),
        },
        host,
    )
}

/// Check `route` in every read mode through [`pinned_cell::check`], with
/// each input as the replacement target in turn: the reference with the
/// target's exact transports (both opens direct on a direct run, following
/// the diff row of `crates/cli/src/io_reach.rs`), then the replacement
/// armed after each of its opens in turn, in both reopen modes. `opens` is
/// the expected open count per input, fixed so an open that stopped
/// deriving from its pin (and so stopped counting) cannot shrink the arming
/// range unnoticed. The reference also checks that the replacement changes
/// the answer, so the comparisons can tell which file a run read.
fn check_route(route: Route, opens: u64) {
    assert_eq!(route.opens(), opens, "{route:?}");
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        for target in [Side::Old, Side::New] {
            check_target(route, target);
        }
    }
}

fn check_target(route: Route, target: Side) {
    let inputs = inputs(route);
    let input = inputs.path(target);
    let replacement = inputs.dir.path().join("replacement.osm.pbf");
    let cell = Cell::new(
        format!("{route:?}, {}", input.display()),
        route.direct,
        input,
        &replacement,
        route.opens(),
        "DD",
    )
    .scratch(inputs.dir.path());
    pinned_cell::check(
        &cell,
        |host| run_paths(route, &inputs, None, host),
        |expected| {
            let swapped = inputs.dir.path().join("swapped.osm.pbf");
            write_input(&swapped, route, target, true);
            let (old, new) = match target {
                Side::Old => (swapped.as_path(), inputs.new.as_path()),
                Side::New => (inputs.old.as_path(), swapped.as_path()),
            };
            let other = run(
                route,
                InputSource::Path(old),
                InputSource::Path(new),
                &crate::common::test_host(),
            )
            .unwrap_or_else(|e| panic!("{}: run on the replacement: {}", cell.what(), e.report()));
            assert_ne!(
                other,
                expected,
                "{}: the replacement must change the answer",
                cell.what()
            );
            std::fs::remove_file(&swapped).expect("remove swapped input");
            expected
        },
        |_, _| {
            write_input(input, route, target, false);
            write_input(&replacement, route, target, true);
        },
        |(), step| run_paths(route, &inputs, Some((target, step.pin)), step.host),
        |expected, (), step, judged| {
            if let Judged::Unaffected(out) = judged {
                assert_eq!(
                    &out, expected,
                    "{}: the answer must come from the pinned inputs alone",
                    step.what
                );
            }
        },
    );
}

#[test]
fn sequential_diff_reads_only_the_pinned_inputs() {
    for count in [false, true] {
        check_route(
            Route {
                count,
                ..Route::SEQUENTIAL
            },
            2,
        );
        // The excluded tail is walked by the same stream, not reopened.
        check_route(
            Route {
                count,
                nodes_only: true,
                ..Route::SEQUENTIAL
            },
            2,
        );
    }
}

#[test]
fn parallel_diff_reads_only_the_pinned_inputs() {
    for count in [false, true] {
        check_route(
            Route {
                count,
                ..Route::PARALLEL
            },
            2,
        );
        // The cursor streams the excluded tail too; nothing is reopened.
        check_route(
            Route {
                count,
                nodes_only: true,
                ..Route::PARALLEL
            },
            2,
        );
    }
}

/// Mixed-sign inputs take the same planner as any other, cut in canonical
/// key windows, and open the same pinned inputs.
#[test]
fn mixed_sign_inputs_read_only_the_pinned_inputs() {
    for count in [false, true] {
        let negative = Route {
            count,
            negative: true,
            ..Route::PARALLEL
        };
        check_route(negative, 2);
        check_route(
            Route {
                nodes_only: true,
                ..negative
            },
            2,
        );
    }
}

/// An input without indexdata takes the element stream, whatever `-j`.
#[test]
fn the_streaming_diff_reads_only_the_pinned_inputs() {
    for unindexed in [[true, true], [true, false], [false, true]] {
        check_route(
            Route {
                unindexed,
                ..Route::PARALLEL
            },
            2,
        );
    }
    check_route(
        Route {
            unindexed: [false, true],
            nodes_only: true,
            count: true,
            ..Route::SEQUENTIAL
        },
        2,
    );
}

/// `diff a a`, replaced after any of its opens, compares the file it
/// started on with itself (stable reopen name) or refuses (path reopen), never a file
/// with its replacement, and the fault plan counts both sides' opens on the
/// one path. That the two sides share one pin, rather than two pins taken
/// back to back, is not observable here (no fault point lies between the
/// pins); `pin_sources`' unit tests in `crates/lib/src/read/input.rs` prove it.
///
/// The cell runs through [`pinned_cell::check`] in every read mode, its
/// spec both sides' opens. The path-reopen matrix arms keep the repeated
/// path arguments, so the library's own pinning of `diff a a` stays
/// exercised; the stable-name arms and the caller-pin arm pass the step's
/// pin as both sides.
#[test]
fn a_repeated_input_is_one_pin() {
    for route in [Route::SEQUENTIAL, Route::PARALLEL] {
        for &direct in pinned_cell::modes() {
            let route = Route { direct, ..route };
            let inputs = inputs(route);
            let input = &inputs.old;
            let replacement = inputs.dir.path().join("replacement.osm.pbf");
            let opens = 2 * route.opens();
            let cell = Cell::new(
                format!("{route:?}, repeated input"),
                direct,
                input,
                &replacement,
                opens,
                "DDDD",
            )
            .scratch(inputs.dir.path());
            pinned_cell::check(
                &cell,
                |host| {
                    run(
                        route,
                        InputSource::Path(input),
                        InputSource::Path(input),
                        host,
                    )
                },
                |expected| {
                    assert!(
                        expected.contains("created: 0, modified: 0, deleted: 0"),
                        "{route:?}: {expected}"
                    );
                    expected
                },
                |_, _| {
                    write_input(input, route, Side::Old, false);
                    write_input(&replacement, route, Side::Old, true);
                },
                |(), step| {
                    let source = if step.scenario == reopen_cell::Scenario::Path && step.after > 0 {
                        step.host
                            .invocation
                            .faults()
                            .force_pin_path_reopen
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        InputSource::Path(input)
                    } else {
                        InputSource::Pinned(step.pin)
                    };
                    run(route, source, source, step.host)
                },
                |expected, (), step, judged| {
                    if let Judged::Unaffected(out) = judged {
                        assert_eq!(
                            &out, expected,
                            "{}: both sides must read the pinned file",
                            step.what
                        );
                    }
                },
            );
        }
    }
}

/// A caller's pin is the run's pin, and it is never merged with a path:
/// with the input replaced between the caller's pin and the call, the
/// pinned side reads the file the caller pinned (stable reopen name) or is refused at
/// its first open (path reopen), and a path argument naming the same input
/// is pinned afresh, so `diff(pin of a, a)` compares the old file with its
/// replacement.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    for route in [
        Route::SEQUENTIAL,
        Route::PARALLEL,
        Route {
            count: true,
            ..Route::PARALLEL
        },
    ] {
        let inputs = inputs(route);
        let expected = run_paths(route, &inputs, None, &crate::common::test_host())
            .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {}", e.report()));
        let replacement = inputs.dir.path().join("replacement.osm.pbf");
        for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
            write_input(&inputs.old, route, Side::Old, false);
            write_input(&replacement, route, Side::Old, true);
            let host = crate::common::test_host();
            scenario.run(&inputs.old, &host, |pin, scenario| {
                std::fs::rename(&replacement, &inputs.old).expect("replace the input");
                let cell = format!("{route:?}, scenario {scenario:?}");

                let result = run(
                    route,
                    InputSource::Pinned(pin),
                    InputSource::Path(&inputs.new),
                    &host,
                );
                match scenario {
                    reopen_cell::Scenario::Pinned => {
                        let out = result.unwrap_or_else(|e| panic!("{cell}: {}", e.report()));
                        assert_eq!(out, expected, "{cell}: the old side is the pinned file");

                        // The path now names the replacement; pinned afresh, it is
                        // a different file from the caller's pin of the same path.
                        let against = run(
                            route,
                            InputSource::Pinned(pin),
                            InputSource::Path(&inputs.old),
                            &host,
                        )
                        .unwrap_or_else(|e| panic!("{cell}: pin against path: {}", e.report()));
                        assert!(
                            !against.contains("created: 0, modified: 0, deleted: 0"),
                            "{cell}: a path must not share a caller's pin: {against}"
                        );
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

/// Both inputs are pinned before either is read, so a FIFO is refused up
/// front in either position, before the other input is opened (a blocking
/// open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_before_either_input_is_read() {
    let inputs = inputs(Route::SEQUENTIAL);
    let dir = inputs.dir.path();
    let regular = inputs.old.as_path();
    let fifo = crate::common::make_fifo(dir, "in.fifo");
    for route in [
        Route::SEQUENTIAL,
        Route::PARALLEL,
        Route {
            count: true,
            ..Route::PARALLEL
        },
    ] {
        for (old, new) in [(regular, fifo.as_path()), (fifo.as_path(), regular)] {
            let cell = format!("{route:?}, {} against {}", old.display(), new.display());
            let host = crate::common::test_host();
            host.invocation.faults().arm_input_replacement(
                regular,
                &dir.join("unused.osm.pbf"),
                u64::MAX,
            );
            let err = run(route, InputSource::Path(old), InputSource::Path(new), &host)
                .err()
                .unwrap_or_else(|| panic!("{cell}: a FIFO must be refused"));
            assert!(
                matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
                "{cell}: {err:?}"
            );
            assert!(
                err.to_string().contains("not a regular file"),
                "{cell}: {err}"
            );
            let attempts = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed")
                .attempts;
            // Counts derived opens only: the pin itself is not one.
            assert_eq!(attempts, 0, "{cell}: no input may be read first");
        }
    }
}
