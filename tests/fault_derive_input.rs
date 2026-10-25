//! `derive_changes` (`diff --format osc`) against an input renamed over
//! mid-run, through the fault plan on the run's token
//! (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Both inputs are pinned before either is read, and every later open of
//! each is derived from its pin, so on a host that reopens through
//! `/proc/self/fd` a replaced input does not affect the run, and where opens
//! go through the path the run fails with `InputReplaced` and publishes
//! nothing. The replacement is armed on each input in turn, after each of
//! its opens, the last included. Every open is taken on the calling thread
//! (the shard workers pread the walker's descriptor), one at a time, so the
//! counts are exact. They count derived opens only: the pin itself is not
//! one.
//!
//! The opens of one input, in order (see `Route::opens`): the front read of
//! the header and the first blob's indexdata; then the block-pair merge's
//! header walk (at any shard count, negative IDs included), or the
//! streaming reader of a route with an unindexed side.
//!
//! The output is written only after every input open, and published only
//! when the run succeeds: a refused run leaves an absent destination absent
//! and an existing one (replace mode) as it was.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{
    TestNode, generate_nodes, generate_nodes_with_negatives, generate_relations, generate_ways,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

/// What a refused run leaves at the destination it was given.
const SENTINEL: &[u8] = b"an earlier output";

#[derive(Clone, Copy, Debug)]
struct Route {
    /// Fixed shard count: 1 runs the sequential paths, 3 the parallel one
    /// when both inputs carry indexdata.
    jobs: usize,
    /// The fixtures hold negative IDs.
    negative: bool,
    /// Which of the two inputs carry no indexdata.
    unindexed: [bool; 2],
    direct: bool,
    /// `.osc.gz` instead of `.osc`.
    gzip: bool,
}

impl Route {
    const SEQUENTIAL: Self = Self {
        jobs: 1,
        negative: false,
        unindexed: [false, false],
        direct: false,
        gzip: false,
    };
    const PARALLEL: Self = Self {
        jobs: 3,
        ..Self::SEQUENTIAL
    };

    /// Opens of either input through its pin: the front read, then the
    /// header walk (both inputs indexed, at any shard count) or the
    /// streaming reader (an unindexed side).
    fn opens(self) -> u64 {
        2
    }

    fn output_name(self, stem: &str) -> String {
        if self.gzip {
            format!("{stem}.osc.gz")
        } else {
            format!("{stem}.osc")
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// Input `side` of `route`: the new side has four more nodes and every
/// fifth node moved. The replacement moves every node, so a run that read
/// any of it derives other changes.
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

    fn scratch(&self) -> PathBuf {
        self.dir.path().join("scratch")
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

/// The OSC text at `path`; for `.osc.gz`, the gunzipped text followed by
/// the compressed bytes, so a comparison holds the framing to byte
/// identity too.
fn read_osc(route: Route, path: &Path) -> String {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("read output {}: {e}", path.display()));
    if !route.gzip {
        return String::from_utf8(bytes).expect("UTF-8 OSC");
    }
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(bytes.as_slice())
        .read_to_string(&mut text)
        .expect("gunzip OSC");
    format!("{text}\n{bytes:?}")
}

/// Run `route` into `output`, a fresh destination or, with `replace`, one
/// that replaces the file already there. Returns the counts.
fn run(
    route: Route,
    old: InputSource<'_>,
    new: InputSource<'_>,
    output: &Path,
    replace: bool,
    scratch_parent: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    let out = if replace {
        crate::common::out_replace(output)
    } else {
        crate::common::out(output)
    };
    let stats = pbfhogg::diff::derive::derive_changes(
        old,
        new,
        out,
        route.direct,
        /* increment_version */ false,
        /* delete timestamp */ None,
        pbfhogg::Parallelism::from_jobs(route.jobs),
        host,
        crate::common::scratch_in(scratch_parent),
    )?;
    Ok(format!("{stats:?}"))
}

/// Run `route` on the fixture paths into `output` and return the counts
/// and the OSC text.
fn run_paths(
    route: Route,
    inputs: &Inputs,
    output: &Path,
    replace: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    let stats = run(
        route,
        InputSource::Path(&inputs.old),
        InputSource::Path(&inputs.new),
        output,
        replace,
        &inputs.scratch(),
        host,
    )?;
    Ok(format!("{stats}\n{}", read_osc(route, output)))
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
    host.cancel.faults().arm_input_replacement(
        inputs.path(target),
        &dir.join("unused.osm.pbf"),
        opens + 1,
    );
    let cell = format!("{route:?}, {}", inputs.path(target).display());
    let output = dir.join(route.output_name("reference"));
    let expected = match run_paths(route, inputs, &output, false, &host) {
        Ok(out) => out,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{cell}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{cell}: unreplaced run: {}", e.report()),
    };
    let counted = host
        .cancel
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
    let other_output = dir.join(route.output_name("swapped"));
    run(
        route,
        InputSource::Path(old),
        InputSource::Path(new),
        &other_output,
        false,
        &inputs.scratch(),
        &crate::common::test_host(),
    )
    .unwrap_or_else(|e| panic!("{cell}: run on the replacement: {}", e.report()));
    assert_ne!(
        read_osc(route, &other_output),
        read_osc(route, &output),
        "{cell}: the replacement must change the answer"
    );
    for path in [&swapped, &output, &other_output] {
        std::fs::remove_file(path).expect("remove reference files");
    }
    Reference::Output(expected)
}

/// Run `route` with the replacement of `target` armed after each of its
/// opens in turn, in both reopen modes, into an absent destination and over
/// an existing one, against the unreplaced `expected`.
fn check_replacements(route: Route, inputs: &Inputs, target: Side, expected: &str) {
    let opens = route.opens();
    let input = inputs.path(target);
    let replacement = inputs.dir.path().join("replacement.osm.pbf");
    let output = inputs.dir.path().join(route.output_name("changes"));
    for after in 1..=opens {
        for force_path in [true, false] {
            for existing in [false, true] {
                write_input(input, route, target, false);
                write_input(&replacement, route, target, true);
                if existing {
                    std::fs::write(&output, SENTINEL).expect("earlier output");
                } else if output.exists() {
                    std::fs::remove_file(&output).expect("clear output");
                }
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
                let result = run_paths(route, inputs, &output, existing, &host);

                let cell = format!(
                    "{route:?}, {} replaced after open {after}, procfs {procfs}, \
                     existing output {existing}",
                    input.display()
                );
                let report = host
                    .cancel
                    .faults()
                    .input_replacement()
                    .expect("armed replacement");
                assert!(
                    report.fired,
                    "{cell}: the replacement must fire: {report:?}"
                );

                // After the input's last open nothing opens it again, so the
                // run reads only what it already holds, in either mode.
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
                    if existing {
                        assert_eq!(
                            std::fs::read(&output).expect("earlier output"),
                            SENTINEL,
                            "{cell}: a refused run replaced the earlier output"
                        );
                    } else {
                        assert!(!output.exists(), "{cell}: a refused run published");
                    }
                }
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
fn sequential_derive_reads_only_the_pinned_inputs() {
    for gzip in [false, true] {
        check_route(
            Route {
                gzip,
                ..Route::SEQUENTIAL
            },
            2,
        );
    }
    // One shard takes negative IDs as they are.
    check_route(
        Route {
            negative: true,
            ..Route::SEQUENTIAL
        },
        2,
    );
}

#[test]
fn parallel_derive_reads_only_the_pinned_inputs() {
    for gzip in [false, true] {
        check_route(
            Route {
                gzip,
                ..Route::PARALLEL
            },
            2,
        );
    }
}

/// Negative IDs make a sharded run one unbounded shard, over the same walk.
#[test]
fn the_negative_id_fallback_reads_only_the_pinned_inputs() {
    check_route(
        Route {
            negative: true,
            ..Route::PARALLEL
        },
        2,
    );
}

/// An input without indexdata takes the streaming merge, whatever the
/// shard count.
#[test]
fn the_streaming_derive_reads_only_the_pinned_inputs() {
    for unindexed in [[true, true], [true, false], [false, true]] {
        check_route(
            Route {
                unindexed,
                ..Route::PARALLEL
            },
            2,
        );
    }
}

/// The front reads and the streaming readers open `O_DIRECT` when asked
/// to, through the pin's own direct open (the block-pair walk stays
/// buffered).
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_inputs() {
    for (route, opens) in [
        (Route::SEQUENTIAL, 2),
        (Route::PARALLEL, 2),
        (
            Route {
                negative: true,
                ..Route::PARALLEL
            },
            2,
        ),
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

/// Deriving a file against itself, replaced after any of its opens, reads
/// the file it started on on both sides (procfs) or refuses (path reopen),
/// never a file against its replacement, and the fault plan counts both
/// sides' opens on the one path. That the two sides share one pin is
/// `pin_sources`' unit tests' to prove (no fault point lies between the
/// pins).
#[test]
fn a_repeated_input_is_one_pin() {
    for route in [Route::SEQUENTIAL, Route::PARALLEL] {
        let inputs = inputs(route);
        let input = &inputs.old;
        let replacement = inputs.dir.path().join("replacement.osm.pbf");
        let output = inputs.dir.path().join(route.output_name("same"));
        let opens = 2 * route.opens();
        let same = |host: &pbfhogg::host::Host| {
            run(
                route,
                InputSource::Path(input),
                InputSource::Path(input),
                &output,
                true,
                &inputs.scratch(),
                host,
            )
        };

        let host = crate::common::test_host();
        host.cancel
            .faults()
            .arm_input_replacement(input, &replacement, opens + 1);
        let expected =
            same(&host).unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {}", e.report()));
        let counted = host.cancel.faults().input_replacement().expect("armed");
        assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
        assert!(
            expected.contains("creates: 0, modifies: 0, deletes: 0"),
            "{route:?}: {expected}"
        );

        for after in 1..opens {
            for force_path in [true, false] {
                write_input(input, route, Side::Old, false);
                write_input(&replacement, route, Side::Old, true);
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
                let result = same(&host);
                let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
                let report = host.cancel.faults().input_replacement().expect("armed");
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
/// is pinned afresh, so `derive(pin of a, a)` derives the replacement's
/// changes against the old file.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    for route in [Route::SEQUENTIAL, Route::PARALLEL] {
        let inputs = inputs(route);
        let Reference::Output(expected) = reference(route, &inputs, Side::Old, |_| false) else {
            unreachable!("no failure is skippable here");
        };
        let replacement = inputs.dir.path().join("replacement.osm.pbf");
        let output = inputs.dir.path().join(route.output_name("changes"));
        for force_path in [true, false] {
            write_input(&inputs.old, route, Side::Old, false);
            write_input(&replacement, route, Side::Old, true);
            let host = crate::common::test_host();
            host.cancel
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
                &output,
                true,
                &inputs.scratch(),
                &host,
            );
            if procfs {
                let stats = result.unwrap_or_else(|e| panic!("{cell}: {}", e.report()));
                let out = format!("{stats}\n{}", read_osc(route, &output));
                assert_eq!(out, expected, "{cell}: the old side is the pinned file");

                // The path now names the replacement; pinned afresh, it is
                // a different file from the caller's pin of the same path.
                let against = run(
                    route,
                    InputSource::Pinned(&pin),
                    InputSource::Path(&inputs.old),
                    &output,
                    true,
                    &inputs.scratch(),
                    &host,
                )
                .unwrap_or_else(|e| panic!("{cell}: pin against path: {}", e.report()));
                assert!(
                    !against.contains("creates: 0, modifies: 0, deletes: 0"),
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
                assert!(!output.exists(), "{cell}: a refused run published");
            }
            if output.exists() {
                std::fs::remove_file(&output).expect("clear output");
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
    for route in [Route::SEQUENTIAL, Route::PARALLEL] {
        for (old, new) in [(regular, fifo.as_path()), (fifo.as_path(), regular)] {
            let cell = format!("{route:?}, {} against {}", old.display(), new.display());
            let host = crate::common::test_host();
            host.cancel.faults().arm_input_replacement(
                regular,
                &dir.join("unused.osm.pbf"),
                u64::MAX,
            );
            let output = dir.join(route.output_name("changes"));
            let err = run(
                route,
                InputSource::Path(old),
                InputSource::Path(new),
                &output,
                false,
                &inputs.scratch(),
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
                .cancel
                .faults()
                .input_replacement()
                .expect("armed")
                .attempts;
            // Counts derived opens only: the pin itself is not one.
            assert_eq!(attempts, 0, "{cell}: no input may be read first");
            assert!(!output.exists(), "{cell}: a refused run published");
        }
    }
}

/// An output name that is neither `.osc` nor `.osc.gz` is refused before
/// the inputs are touched: ahead of a FIFO's refusal and of a missing
/// input's.
#[test]
fn a_bad_output_name_is_refused_before_the_inputs_are_touched() {
    let dir = crate::common::test_scratch();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).expect("scratch parent");
    let mut inputs = vec![dir.path().join("missing.osm.pbf")];
    #[cfg(unix)]
    inputs.push(crate::common::make_fifo(dir.path(), "in.fifo"));
    let host = crate::common::test_host();
    let output = dir.path().join("changes.txt");
    for input in &inputs {
        let err = run(
            Route::PARALLEL,
            InputSource::Path(input),
            InputSource::Path(input),
            &output,
            false,
            &scratch,
            &host,
        )
        .err()
        .unwrap_or_else(|| panic!("{}: must be refused", input.display()));
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
            "{}: {err:?}",
            input.display()
        );
        assert!(
            err.to_string().contains(".osc"),
            "{}: {err}",
            input.display()
        );
        assert!(
            !output.exists(),
            "{}: a refused run published",
            input.display()
        );
    }
}
