//! `diff` and `diff_stats` against an input renamed over mid-run, through
//! the fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Both inputs are pinned before either is read, and every later open of
//! each is derived from its pin, so on a host that reopens through
//! `/proc/self/fd` a replaced input does not affect the run, and where opens
//! go through the path the run fails with `InputReplaced`. The replacement
//! is armed on each input in turn, after each of its opens, the last
//! included. Every open is taken on the calling thread (the shard workers
//! pread the walker's descriptor), one at a time, so the counts are exact.
//!
//! The opens of one input, in order (see `Route::opens`): the front read of
//! the header and the first blob's indexdata; then the block-pair merge's
//! header walk (more than one shard, negative IDs and type filters
//! included), its one stream per input at one shard, or the streaming
//! reader of a route with an unindexed side.
//!
//! `diff` writes to a caller's `Write`, so a run refused at a later open
//! may already have written text; what it owes is the typed refusal.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::TypeFilter;
use pbfhogg::diff::DiffOptions;
use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{
    TestNode, generate_nodes, generate_nodes_with_negatives, generate_relations, generate_ways,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

#[derive(Clone, Copy, Debug)]
struct Route {
    /// Fixed shard count: 1 runs one shard on the calling thread, 3 the
    /// sharded merge when both inputs carry indexdata.
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
    /// header walk or the one-shard stream (both inputs indexed) or
    /// the streaming reader (an unindexed side).
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
    dir: tempfile::TempDir,
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
    std::fs::create_dir(dir.path().join("scratch")).expect("scratch parent");
    Inputs { dir, old, new }
}

/// Run `route`: the text and the counts for `diff`, the counts alone for
/// `diff_stats`.
fn run(
    route: Route,
    old: InputSource<'_>,
    new: InputSource<'_>,
    scratch_parent: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    let parallelism = pbfhogg::Parallelism::from_jobs(route.jobs);
    if route.count {
        let stats = pbfhogg::diff::diff_stats(
            old,
            new,
            route.type_filter(),
            parallelism,
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
    };
    let scratch = (route.jobs > 1).then(|| crate::common::scratch_in(scratch_parent));
    let mut text = Vec::new();
    let stats = pbfhogg::diff::diff(old, new, &mut text, &options, route.direct, host, scratch)?;
    Ok(format!("{}{stats:?}", String::from_utf8_lossy(&text)))
}

fn run_paths(route: Route, inputs: &Inputs, host: &pbfhogg::host::Host) -> pbfhogg::Result<String> {
    run(
        route,
        InputSource::Path(&inputs.old),
        InputSource::Path(&inputs.new),
        &inputs.dir.path().join("scratch"),
        host,
    )
}

enum Reference {
    Output(String),
    Skipped,
}

/// The unreplaced run's answer, with the open count of `target` checked:
/// armed past its last open, the replacement never fires and only counts.
/// Also checks that the replacement changes the answer, so the comparisons
/// below can tell which file a run read.
fn reference(
    route: Route,
    inputs: &Inputs,
    target: Side,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let dir = inputs.dir.path();
    let host = crate::common::test_host();
    host.invocation.faults().arm_input_replacement(
        inputs.path(target),
        &dir.join("unused.osm.pbf"),
        opens + 1,
    );
    let cell = format!("{route:?}, {}", inputs.path(target).display());
    let expected = match run_paths(route, inputs, &host) {
        Ok(out) => out,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{cell}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{cell}: unreplaced run: {}", e.report()),
    };
    let counted = host
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{cell}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{cell}: {counted:?}");
    assert_eq!(counted.verified, opens, "{cell}: {counted:?}");
    assert_eq!(counted.refused, 0, "{cell}: {counted:?}");

    let swapped = dir.join("swapped.osm.pbf");
    write_input(&swapped, route, target, true);
    let (old, new) = match target {
        Side::Old => (swapped.as_path(), inputs.new.as_path()),
        Side::New => (inputs.old.as_path(), swapped.as_path()),
    };
    let other = run(
        route,
        InputSource::Path(old),
        InputSource::Path(new),
        &dir.join("scratch"),
        &crate::common::test_host(),
    )
    .unwrap_or_else(|e| panic!("{cell}: run on the replacement: {}", e.report()));
    assert_ne!(
        other, expected,
        "{cell}: the replacement must change the answer"
    );
    std::fs::remove_file(&swapped).expect("remove swapped input");
    Reference::Output(expected)
}

/// Run `route` with the replacement of `target` armed after each of its
/// opens in turn, in both reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, inputs: &Inputs, target: Side, expected: &str) {
    let opens = route.opens();
    let input = inputs.path(target);
    let replacement = inputs.dir.path().join("replacement.osm.pbf");
    for after in 1..=opens {
        for force_path in [true, false] {
            write_input(input, route, target, false);
            write_input(&replacement, route, target, true);
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
            let result = run_paths(route, inputs, &host);

            let cell = format!(
                "{route:?}, {} replaced after open {after}, procfs {procfs}",
                input.display()
            );
            let report = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(
                report.fired,
                "{cell}: the replacement must fire: {report:?}"
            );

            // After the input's last open nothing opens it again, so the run
            // reads only what it already holds, in either mode.
            if procfs || after == opens {
                let out = result.unwrap_or_else(|e| {
                    panic!(
                        "{cell}: the run is unaffected by the replacement: {}",
                        e.report()
                    )
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
                    out, expected,
                    "{cell}: the answer must come from the pinned inputs alone"
                );
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is a
                // Unix guarantee.
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
            }
        }
    }
}

/// Check `route` with each input replaced in turn. `opens` is the expected
/// open count per input, fixed so an open that stopped deriving from its pin
/// (and so stopped counting) cannot shrink the arming range unnoticed.
fn check_route_with(route: Route, opens: u64, skippable: impl Fn(&pbfhogg::Error) -> bool) {
    assert_eq!(route.opens(), opens, "{route:?}");
    for target in [Side::Old, Side::New] {
        let inputs = inputs(route);
        let Reference::Output(expected) = reference(route, &inputs, target, &skippable) else {
            return;
        };
        check_replacements(route, &inputs, target, &expected);
    }
}

fn check_route(route: Route, opens: u64) {
    check_route_with(route, opens, |_| false);
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
        // The walk covers the whole of both files; no tail walk.
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

/// Negative IDs make a sharded run one unbounded shard, over the same walk.
#[test]
fn the_negative_id_fallback_reads_only_the_pinned_inputs() {
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

/// An input without indexdata takes the streaming merge, whatever the
/// shard count.
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

/// The front reads and the streaming readers open `O_DIRECT` when asked
/// to, through the pin's own direct open: the unindexed reader and the
/// one-shard block-pair streams (a multi-shard walk stays buffered).
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_inputs() {
    for (route, opens) in [
        (
            Route {
                nodes_only: true,
                ..Route::SEQUENTIAL
            },
            2,
        ),
        (Route::PARALLEL, 2),
        (
            Route {
                unindexed: [true, false],
                ..Route::SEQUENTIAL
            },
            2,
        ),
    ] {
        let route = Route {
            direct: true,
            ..route
        };
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        check_route_with(route, opens, |e| crate::common::is_einval(e));
    }
}

/// `diff a a`, replaced after any of its opens, compares the file it
/// started on with itself (procfs) or refuses (path reopen), never a file
/// with its replacement, and the fault plan counts both sides' opens on the
/// one path. That the two sides share one pin, rather than two pins taken
/// back to back, is not observable here (no fault point lies between the
/// pins); `pin_sources`' unit tests in `src/read/input.rs` prove it.
#[test]
fn a_repeated_input_is_one_pin() {
    for route in [Route::SEQUENTIAL, Route::PARALLEL] {
        let inputs = inputs(route);
        let input = &inputs.old;
        let replacement = inputs.dir.path().join("replacement.osm.pbf");
        let scratch = inputs.dir.path().join("scratch");
        let opens = 2 * route.opens();
        let same = |host: &pbfhogg::host::Host| {
            run(
                route,
                InputSource::Path(input),
                InputSource::Path(input),
                &scratch,
                host,
            )
        };

        let host = crate::common::test_host();
        host.invocation
            .faults()
            .arm_input_replacement(input, &replacement, opens + 1);
        let expected =
            same(&host).unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {}", e.report()));
        let counted = host.invocation.faults().input_replacement().expect("armed");
        assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
        assert!(
            expected.contains("created: 0, modified: 0, deleted: 0"),
            "{route:?}: {expected}"
        );

        for after in 1..opens {
            for force_path in [true, false] {
                write_input(input, route, Side::Old, false);
                write_input(&replacement, route, Side::Old, true);
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
                let result = same(&host);
                let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
                let report = host.invocation.faults().input_replacement().expect("armed");
                assert!(report.fired, "{cell}: {report:?}");
                if procfs {
                    let out = result.unwrap_or_else(|e| panic!("{cell}: {}", e.report()));
                    assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                    assert_eq!(
                        out, expected,
                        "{cell}: both sides must read the pinned file"
                    );
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
}

/// A caller's pin is the run's pin, and it is never merged with a path:
/// with the input replaced between the caller's pin and the call, the
/// pinned side reads the file the caller pinned (procfs) or is refused at
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
        let Reference::Output(expected) = reference(route, &inputs, Side::Old, |_| false) else {
            unreachable!("no failure is skippable here");
        };
        let replacement = inputs.dir.path().join("replacement.osm.pbf");
        let scratch = inputs.dir.path().join("scratch");
        for force_path in [true, false] {
            write_input(&inputs.old, route, Side::Old, false);
            write_input(&replacement, route, Side::Old, true);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&inputs.old, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &inputs.old).expect("replace the input");
            let cell = format!("{route:?}, procfs {procfs}");

            let result = run(
                route,
                InputSource::Pinned(&pin),
                InputSource::Path(&inputs.new),
                &scratch,
                &host,
            );
            if procfs {
                let out = result.unwrap_or_else(|e| panic!("{cell}: {}", e.report()));
                assert_eq!(out, expected, "{cell}: the old side is the pinned file");

                // The path now names the replacement; pinned afresh, it is
                // a different file from the caller's pin of the same path.
                let against = run(
                    route,
                    InputSource::Pinned(&pin),
                    InputSource::Path(&inputs.old),
                    &scratch,
                    &host,
                )
                .unwrap_or_else(|e| panic!("{cell}: pin against path: {}", e.report()));
                assert!(
                    !against.contains("created: 0, modified: 0, deleted: 0"),
                    "{cell}: a path must not share a caller's pin: {against}"
                );
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
            let err = run(
                route,
                InputSource::Path(old),
                InputSource::Path(new),
                &dir.join("scratch"),
                &host,
            )
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

/// The option refusals come before the inputs are touched: ahead of a
/// FIFO's refusal and of a missing input's.
#[test]
fn an_invalid_option_is_refused_before_the_inputs_are_touched() {
    let dir = crate::common::test_scratch();
    let mut inputs = vec![dir.path().join("missing.osm.pbf")];
    #[cfg(unix)]
    inputs.push(crate::common::make_fifo(dir.path(), "in.fifo"));
    let host = crate::common::test_host();
    for input in &inputs {
        let unsharded = DiffOptions {
            suppress_common: false,
            verbose: false,
            type_filter: None,
            parallelism: pbfhogg::Parallelism::from_jobs(3),
        };
        let err = pbfhogg::diff::diff(
            input,
            input,
            &mut Vec::new(),
            &unsharded,
            false,
            &host,
            None,
        )
        .err()
        .unwrap_or_else(|| panic!("{}: must be refused", input.display()));
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
            "{}: {err:?}",
            input.display()
        );
        assert!(
            err.to_string().contains("scratch"),
            "{}: {err}",
            input.display()
        );
    }
}
