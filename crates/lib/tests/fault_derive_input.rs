//! `derive_changes` (`diff --format osc`) against an input renamed over
//! mid-run, through the fault plan on the run's token
//! (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Both inputs are pinned before either is read, and every later open of
//! each is derived from its pin. With an injected stable scratch reopen name
//! a replaced input does not affect the run, and where opens
//! go through the path the run fails with `InputReplaced` and publishes
//! nothing. The replacement is armed on each input in turn, after each of
//! its opens, the last included. Every open is taken on the calling thread,
//! one at a time, so the counts are exact: the planner thread streams frames
//! from the cursors the caller opened, and the workers merge in-memory frame
//! bodies. They count derived opens only: the pin itself is not one.
//!
//! The opens of one input, in order (see `Route::opens`): the front read of
//! the header and the first blob's indexdata; then, when both inputs carry
//! indexdata, the one sequential blob cursor the work-unit planner reads (at
//! any `-j`, mixed-sign inputs included), or the element stream reader of a
//! route with an unindexed side.
//!
//! The output is written only after every input open, and published only
//! when the run succeeds: a refused run leaves an absent destination absent
//! and an existing one (replace mode) as it was.

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::io::Read as _;
use std::path::{Path, PathBuf};

use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{
    TestNode, generate_nodes, generate_nodes_with_negatives, generate_relations, generate_ways,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

/// What a refused run leaves at the destination it was given.
const SENTINEL: &[u8] = b"an earlier output";

#[derive(Clone, Copy, Debug)]
struct Route {
    /// Worker limit (`-j`): 1 or 3 merge workers over the same planner.
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
    /// planner's blob cursor (both inputs indexed, at any `-j`) or the
    /// element stream reader (an unindexed side).
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
    pinned: Option<(Side, &PinnedInput)>,
    output: &Path,
    replace: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<String> {
    let stats = run(
        route,
        match pinned {
            Some((Side::Old, pin)) => InputSource::Pinned(pin),
            _ => InputSource::Path(&inputs.old),
        },
        match pinned {
            Some((Side::New, pin)) => InputSource::Pinned(pin),
            _ => InputSource::Path(&inputs.new),
        },
        output,
        replace,
        &inputs.scratch(),
        host,
    )?;
    Ok(format!("{stats}\n{}", read_osc(route, output)))
}

/// Check `route` in every read mode through [`pinned_cell::check`], with
/// each input as the replacement target in turn, into an absent
/// destination and over an existing one: the reference with the target's
/// exact transports (both opens direct on a direct run, following the diff
/// row of `crates/cli/src/io_reach.rs`, which `diff --format osc` shares),
/// then the replacement armed after each of its opens in turn, in both
/// reopen modes. `opens` is the expected open count per input, fixed so an
/// open that stopped deriving from its pin (and so stopped counting) cannot
/// shrink the arming range unnoticed. The reference also checks that the
/// replacement changes the answer, so the comparisons can tell which file a
/// run read.
fn check_route(route: Route, opens: u64) {
    assert_eq!(route.opens(), opens, "{route:?}");
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        for target in [Side::Old, Side::New] {
            for existing in [false, true] {
                check_target(route, target, existing);
            }
        }
    }
}

fn check_target(route: Route, target: Side, existing: bool) {
    let inputs = inputs(route);
    let dir = inputs.dir.path();
    let input = inputs.path(target);
    let replacement = dir.join("replacement.osm.pbf");
    let reference = dir.join(route.output_name("reference"));
    let output = dir.join(route.output_name("changes"));
    let cell = Cell::new(
        format!("{route:?}, {}, existing output {existing}", input.display()),
        route.direct,
        input,
        &replacement,
        route.opens(),
        "DD",
    )
    .scratch(dir);
    pinned_cell::check(
        &cell,
        |host| run_paths(route, &inputs, None, &reference, false, host),
        |expected| {
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
            .unwrap_or_else(|e| panic!("{}: run on the replacement: {}", cell.what(), e.report()));
            assert_ne!(
                read_osc(route, &other_output),
                read_osc(route, &reference),
                "{}: the replacement must change the answer",
                cell.what()
            );
            for path in [&swapped, &reference, &other_output] {
                std::fs::remove_file(path).expect("remove reference files");
            }
            expected
        },
        |_, _| {
            write_input(input, route, target, false);
            write_input(&replacement, route, target, true);
            if existing {
                std::fs::write(&output, SENTINEL).expect("earlier output");
            } else if output.exists() {
                std::fs::remove_file(&output).expect("clear output");
            }
        },
        |(), step| {
            run_paths(
                route,
                &inputs,
                Some((target, step.pin)),
                &output,
                existing,
                step.host,
            )
        },
        |expected, (), step, judged| match judged {
            Judged::Unaffected(out) => assert_eq!(
                &out, expected,
                "{}: the answer must come from the pinned inputs alone",
                step.what
            ),
            Judged::Refused if existing => assert_eq!(
                std::fs::read(&output).expect("earlier output"),
                SENTINEL,
                "{}: a refused run replaced the earlier output",
                step.what
            ),
            Judged::Refused => assert!(!output.exists(), "{}: a refused run published", step.what),
        },
    );
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
    // Mixed-sign inputs at one worker.
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

/// Mixed-sign inputs take the same planner as any other, cut in canonical
/// key windows, and open the same pinned inputs.
#[test]
fn mixed_sign_inputs_read_only_the_pinned_inputs() {
    check_route(
        Route {
            negative: true,
            ..Route::PARALLEL
        },
        2,
    );
}

/// An input without indexdata takes the element stream, whatever `-j`.
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

/// Deriving a file against itself, replaced after any of its opens, reads
/// the file it started on on both sides (stable reopen name) or refuses
/// (path reopen), never a file against its replacement, and the fault plan
/// counts both sides' opens on the one path. That the two sides share one
/// pin is `pin_sources`' unit tests' to prove (no fault point lies between
/// the pins).
///
/// The cell runs through [`pinned_cell::check`] in every read mode, its
/// spec both sides' opens. The path-reopen matrix arms keep the repeated
/// path arguments, so the library's own pinning of a repeated input stays
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
            let output = inputs.dir.path().join(route.output_name("same"));
            let opens = 2 * route.opens();
            let same = |source: InputSource<'_>, host: &pbfhogg::host::Host| {
                run(
                    route,
                    source,
                    source,
                    &output,
                    true,
                    &inputs.scratch(),
                    host,
                )
            };
            let cell = Cell::new(
                format!("{route:?}, repeated input"),
                direct,
                input,
                &replacement,
                opens,
                "DDDD",
            );
            pinned_cell::check(
                &cell,
                |host| same(InputSource::Path(input), host),
                |expected| {
                    assert!(
                        expected.contains("creates: 0, modifies: 0, deletes: 0"),
                        "{route:?}: {expected}"
                    );
                    expected
                },
                |_, _| {
                    write_input(input, route, Side::Old, false);
                    write_input(&replacement, route, Side::Old, true);
                },
                |(), step| {
                    // The caller-pin arm replaced the path before the call.
                    // Both arguments have the same path, but the path argument
                    // must open the replacement, through its own pin.
                    if step.after == 0 {
                        return run(
                            route,
                            InputSource::Pinned(step.pin),
                            InputSource::Path(input),
                            &output,
                            true,
                            &inputs.scratch(),
                            step.host,
                        );
                    }
                    let source = if step.scenario == reopen_cell::Scenario::Path {
                        step.host
                            .invocation
                            .faults()
                            .force_pin_path_reopen
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        InputSource::Path(input)
                    } else {
                        InputSource::Pinned(step.pin)
                    };
                    same(source, step.host)
                },
                |expected, (), step, judged| {
                    if let Judged::Unaffected(out) = judged {
                        if step.after == 0 {
                            assert!(
                                !out.contains("creates: 0, modifies: 0, deletes: 0"),
                                "{}: the path must read the replacement: {out}",
                                step.what,
                            );
                        } else {
                            assert_eq!(
                                &out, expected,
                                "{}: both sides must read the pinned file",
                                step.what
                            );
                        }
                    }
                },
            );
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
            host.invocation.faults().arm_input_replacement(
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
                .invocation
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
