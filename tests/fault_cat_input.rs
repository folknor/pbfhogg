//! cat and `cat --dedupe` against an input renamed over mid-run, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every input is pinned before any is opened, and every later open of each
//! is derived from its pin, so on a host that reopens through
//! `/proc/self/fd` a replaced input does not affect the run, and where opens
//! go through the path the run fails with `InputReplaced` and publishes
//! nothing. Each route runs two inputs, and the replacement is armed on each
//! of them in turn, after each of its opens, the last included. Every open
//! is taken on the calling thread, one at a time, so the counts are exact.
//!
//! The opens of one input, in order (see `Route::opens`): on a type-filtered
//! route the indexdata requirement; the header read (dedupe: its index walk,
//! which reads header and blobs in one pass); on the raw-copy routes (plain
//! and type-filtered without cleaning) the indexdata probe, taken only when
//! some header declares a way enrichment and only up to the first input
//! found without indexdata; then the body reader, the cleaning route's
//! schedule walk (whose descriptor the classify pool shares) or dedupe's
//! pass-2 source.
//!
//! Also here: a path given twice shares one pin, the descriptors a run asks
//! its host for, and a FIFO refused before any input is read.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::host::ProcessActions;
use pbfhogg::process::{FdAllowance, FdReservation};
use pbfhogg::read::input::PinnedInput;
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{MemberId, TypeFilter};

use crate::common::snapshot_dir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// No type filter, no cleaning: raw copy, reindexing unindexed blobs.
    Plain,
    /// `--type node,way`: raw copy of the selected kinds.
    Type,
    /// `--type node,way,relation` without cleaning: still the type route.
    TypeAllKinds,
    /// `--type node,way --clean version`: the classify pool.
    TypeClean,
    /// `--clean version`: the classify pool, no indexdata requirement.
    Clean,
    /// `--type node,way,relation --clean version`: normalised to `Clean`.
    CleanAllKinds,
    /// `--dedupe`.
    Dedupe,
}

#[derive(Clone, Copy, Debug)]
struct Route {
    mode: Mode,
    /// The headers declare `LocationsOnWays`, so the raw-copy routes probe.
    enriched: bool,
    /// Which of the two inputs carry no indexdata.
    unindexed: [bool; 2],
    direct: bool,
}

impl Route {
    const fn of(mode: Mode) -> Self {
        Self {
            mode,
            enriched: false,
            unindexed: [false, false],
            direct: false,
        }
    }

    fn type_filter(self) -> Option<TypeFilter> {
        let some = TypeFilter {
            nodes: true,
            ways: true,
            relations: false,
        };
        match self.mode {
            Mode::Plain | Mode::Clean | Mode::Dedupe => None,
            Mode::Type | Mode::TypeClean => Some(some),
            Mode::TypeAllKinds | Mode::CleanAllKinds => Some(TypeFilter::all()),
        }
    }

    fn clean(self) -> CleanAttrs {
        CleanAttrs {
            version: matches!(
                self.mode,
                Mode::TypeClean | Mode::Clean | Mode::CleanAllKinds
            ),
            ..CleanAttrs::default()
        }
    }

    /// Opens of input `index` through its pin.
    fn opens(self, index: usize) -> u64 {
        let base = match self.mode {
            Mode::Plain | Mode::Clean | Mode::CleanAllKinds => 2,
            Mode::Type | Mode::TypeAllKinds | Mode::TypeClean | Mode::Dedupe => 3,
        };
        let raw_copy = matches!(self.mode, Mode::Plain | Mode::Type | Mode::TypeAllKinds);
        // The probe stops at the first input without indexdata.
        let probed = raw_copy && self.enriched && !self.unindexed[..index].contains(&true);
        base + u64::from(probed)
    }
}

/// What a fixture holds: nodes `first..first + 30` in blobs of ten, two ways
/// and a relation, sorted.
#[derive(Clone, Copy)]
struct Fixture {
    first: i64,
    /// Added to every node's latitude.
    shift: i32,
    indexed: bool,
    enriched: bool,
    source: &'static str,
}

fn write_fixture(path: &Path, fx: Fixture) {
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = HeaderBuilder::new().sorted().source(fx.source);
    if fx.enriched {
        header = header.optional_feature("LocationsOnWays");
    }
    writer
        .write_header(&header.build().expect("header"))
        .expect("write header");
    let mut write = |bb: &mut BlockBuilder| {
        let bytes = bb.take().expect("take").expect("non-empty block");
        if fx.indexed {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    };
    let mut bb = BlockBuilder::new();
    for blob in 0..3 {
        for k in 0..10 {
            let id = fx.first + blob * 10 + k;
            #[allow(clippy::cast_possible_truncation)]
            let lat = id as i32 * 1_000 + fx.shift;
            bb.add_node(id, lat, 100_000, [("name", "n")], None);
        }
        write(&mut bb);
    }
    let refs = [fx.first, fx.first + 1, fx.first + 2];
    bb.add_way(1_000 + fx.first, [("highway", "primary")], &refs, None);
    bb.add_way(1_001 + fx.first, [("highway", "service")], &refs[1..], None);
    write(&mut bb);
    bb.add_relation(
        2_000 + fx.first,
        [("type", "route")],
        &[MemberData {
            id: MemberId::Way(1_000 + fx.first),
            role: "",
        }],
        None,
    );
    write(&mut bb);
    writer.flush().expect("flush fixture");
}

/// The fixture of input `index` of `route`: the two overlap on nodes
/// 21..=30, so dedupe has an overlap run to merge.
fn input_fixture(route: Route, index: usize) -> Fixture {
    Fixture {
        first: [1, 21][index],
        shift: 0,
        indexed: !route.unindexed[index],
        enriched: route.enriched,
        source: ["a", "b"][index],
    }
}

/// A valid replacement of input `index`: other IDs, other coordinates and
/// another header, its indexdata and enrichment as the input's, so a run
/// that read any of it writes other bytes on every route.
fn replacement_fixture(route: Route, index: usize) -> Fixture {
    Fixture {
        first: input_fixture(route, index).first + 5,
        shift: 7,
        source: "replacement",
        ..input_fixture(route, index)
    }
}

struct Inputs {
    dir: tempfile::TempDir,
    paths: [PathBuf; 2],
}

fn inputs(route: Route) -> Inputs {
    let dir = crate::common::test_scratch();
    let paths = [dir.path().join("a.osm.pbf"), dir.path().join("b.osm.pbf")];
    for (index, path) in paths.iter().enumerate() {
        write_fixture(path, input_fixture(route, index));
    }
    Inputs { dir, paths }
}

fn run(
    route: Route,
    files: &[&Path],
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let write = WriteOptions {
        direct_reads: route.direct,
        force: route.unindexed.contains(&true),
        ..WriteOptions::default()
    };
    let overrides = pbfhogg::HeaderOverrides::default();
    let output = crate::common::out(output);
    if route.mode == Mode::Dedupe {
        pbfhogg::cat::dedupe::merge_pbf(files, output, &write, &overrides, host).map(drop)
    } else {
        pbfhogg::cat::cat(
            files,
            output,
            route.type_filter(),
            &route.clean(),
            &write,
            &overrides,
            host,
        )
        .map(drop)
    }
}

/// What [`reference`] found.
enum Reference {
    /// The output's bytes.
    Output(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's output, with the open count of input `target`
/// checked: armed past its last open, the replacement never fires and only
/// counts. Also checks that the replacement yields another output, so the
/// byte comparisons below can tell which file a run read. Skipped when the
/// run failed and `skippable` accepts its error; any other failure panics.
fn reference(
    route: Route,
    inputs: &Inputs,
    target: usize,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens(target);
    let dir = inputs.dir.path();
    let output = dir.join("reference.osm.pbf");
    let host = crate::common::test_host();
    host.invocation.faults().arm_input_replacement(
        &inputs.paths[target],
        &dir.join("unused.osm.pbf"),
        opens + 1,
    );
    let files: Vec<&Path> = inputs.paths.iter().map(PathBuf::as_path).collect();
    match run(route, &files, &output, &host) {
        Ok(()) => {}
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {}", e.report()),
    }
    let counted = host
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    let cell = format!("{route:?}, input {target}");
    assert!(!counted.fired, "{cell}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{cell}: {counted:?}");
    assert_eq!(counted.verified, opens, "{cell}: {counted:?}");
    assert_eq!(counted.refused, 0, "{cell}: {counted:?}");
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");

    let replacement = dir.join("swapped.osm.pbf");
    write_fixture(&replacement, replacement_fixture(route, target));
    let mut swapped = files.clone();
    swapped[target] = replacement.as_path();
    run(route, &swapped, &output, &crate::common::test_host())
        .unwrap_or_else(|e| panic!("{cell}: run on the replacement: {}", e.report()));
    assert_ne!(
        std::fs::read(&output).expect("read swapped output"),
        bytes,
        "{cell}: the replacement must change the output"
    );
    std::fs::remove_file(&output).expect("remove swapped output");
    std::fs::remove_file(&replacement).expect("remove swapped input");
    Reference::Output(bytes)
}

/// Run `route` with the replacement of input `target` armed after each of
/// its opens in turn, in both reopen modes, against the unreplaced
/// `expected`.
fn check_replacements(route: Route, inputs: &Inputs, target: usize, expected: &[u8]) {
    let opens = route.opens(target);
    let dir = inputs.dir.path();
    let input = &inputs.paths[target];
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    let files: Vec<&Path> = inputs.paths.iter().map(PathBuf::as_path).collect();
    for after in 1..=opens {
        for force_path in [true, false] {
            write_fixture(input, input_fixture(route, target));
            write_fixture(&replacement, replacement_fixture(route, target));
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
            let result = run(route, &files, &output, &host);

            let cell =
                format!("{route:?}, input {target} replaced after open {after}, procfs {procfs}");
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
                result.unwrap_or_else(|e| {
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
                    std::fs::read(&output).expect("read output"),
                    expected,
                    "{cell}: the output must hold only what the pinned inputs yield"
                );
                std::fs::remove_file(&output).expect("remove output");
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

/// Check `route` with each input replaced in turn; `opens` are the expected
/// open counts of the two inputs, fixed so an open that stopped deriving
/// from its pin (and so stopped counting) cannot shrink the arming range
/// unnoticed.
fn check_route_with(route: Route, opens: [u64; 2], skippable: impl Fn(&pbfhogg::Error) -> bool) {
    for (target, opens) in opens.into_iter().enumerate() {
        assert_eq!(route.opens(target), opens, "{route:?}, input {target}");
        let inputs = inputs(route);
        let Reference::Output(expected) = reference(route, &inputs, target, &skippable) else {
            return;
        };
        check_replacements(route, &inputs, target, &expected);
    }
}

fn check_route(route: Route, opens: [u64; 2]) {
    check_route_with(route, opens, |_| false);
}

#[test]
fn plain_cat_reads_only_the_pinned_inputs() {
    check_route(Route::of(Mode::Plain), [2, 2]);
    // An unindexed blob is decoded and reindexed from the same body reader.
    check_route(
        Route {
            unindexed: [false, true],
            ..Route::of(Mode::Plain)
        },
        [2, 2],
    );
}

/// A declared way enrichment adds the indexdata probe, up to the first
/// input found without indexdata.
#[test]
fn plain_cat_probes_enriched_inputs_through_their_pins() {
    let enriched = Route {
        enriched: true,
        ..Route::of(Mode::Plain)
    };
    check_route(enriched, [3, 3]);
    check_route(
        Route {
            unindexed: [true, false],
            ..enriched
        },
        [3, 2],
    );
    check_route(
        Route {
            unindexed: [false, true],
            ..enriched
        },
        [3, 3],
    );
}

#[test]
fn type_filtered_cat_reads_only_the_pinned_inputs() {
    check_route(Route::of(Mode::Type), [3, 3]);
    check_route(Route::of(Mode::TypeAllKinds), [3, 3]);
    check_route(
        Route {
            enriched: true,
            unindexed: [false, true],
            ..Route::of(Mode::Type)
        },
        [4, 4],
    );
}

#[test]
fn cleaning_cat_reads_only_the_pinned_inputs() {
    check_route(Route::of(Mode::TypeClean), [3, 3]);
    check_route(
        Route {
            unindexed: [true, false],
            ..Route::of(Mode::TypeClean)
        },
        [3, 3],
    );
    check_route(Route::of(Mode::Clean), [2, 2]);
    check_route(
        Route {
            unindexed: [false, true],
            ..Route::of(Mode::Clean)
        },
        [2, 2],
    );
    check_route(Route::of(Mode::CleanAllKinds), [2, 2]);
}

#[test]
fn dedupe_reads_only_the_pinned_inputs() {
    check_route(Route::of(Mode::Dedupe), [3, 3]);
    check_route(
        Route {
            unindexed: [true, false],
            ..Route::of(Mode::Dedupe)
        },
        [3, 3],
    );
}

/// The probes, header reads and body readers open `O_DIRECT` when asked to,
/// through the pin's own direct open (the schedule walk and dedupe's pass-2
/// sources stay buffered).
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_inputs() {
    for (mode, opens) in [
        (Mode::Plain, 3),
        (Mode::Type, 4),
        (Mode::TypeClean, 3),
        (Mode::Dedupe, 3),
    ] {
        let route = Route {
            enriched: true,
            direct: true,
            ..Route::of(mode)
        };
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        check_route_with(route, [opens, opens], |e| crate::common::is_einval(e));
    }
}

/// A path given twice is one pin read twice: `cat a a` reads the one file
/// it pinned for both arguments (procfs) or refuses (path reopen), and the
/// fault plan counts both arguments' opens on the one path.
#[test]
fn a_repeated_input_is_one_pin() {
    for (mode, opens) in [(Mode::Plain, 4), (Mode::Dedupe, 6)] {
        let route = Route::of(mode);
        let inputs = inputs(route);
        let dir = inputs.dir.path();
        let input = &inputs.paths[0];
        let files = [input.as_path(), input.as_path()];
        let output = dir.join("out.osm.pbf");
        let replacement = dir.join("replacement.osm.pbf");

        let host = crate::common::test_host();
        host.invocation
            .faults()
            .arm_input_replacement(input, &replacement, opens + 1);
        run(route, &files, &output, &host)
            .unwrap_or_else(|e| panic!("{mode:?}: unreplaced run: {}", e.report()));
        let counted = host.invocation.faults().input_replacement().expect("armed");
        assert_eq!(counted.attempts, opens, "{mode:?}: {counted:?}");
        let expected = std::fs::read(&output).expect("read reference");
        std::fs::remove_file(&output).expect("remove reference");

        for after in 1..opens {
            for force_path in [true, false] {
                write_fixture(input, input_fixture(route, 0));
                write_fixture(&replacement, replacement_fixture(route, 0));
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
                let result = run(route, &files, &output, &host);
                let cell = format!("{mode:?}, replaced after open {after}, procfs {procfs}");
                let report = host.invocation.faults().input_replacement().expect("armed");
                assert!(report.fired, "{cell}: {report:?}");
                if procfs {
                    result.unwrap_or_else(|e| panic!("{cell}: {}", e.report()));
                    assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                    assert_eq!(
                        std::fs::read(&output).expect("read output"),
                        expected,
                        "{cell}: both arguments must read the pinned file"
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
}

/// Grants what it is asked for, less `short`, and records each request.
struct Grant {
    asked: Mutex<Vec<u64>>,
    short: u64,
}

impl ProcessActions for Grant {
    fn cross_thread_alloc_pool(&self) {}
    fn fd_allowance(&self, wanted: u64) -> FdReservation {
        self.asked.lock().expect("requests").push(wanted);
        FdReservation::unreserved(FdAllowance::host_grant(wanted - self.short))
    }
}

/// The run asks its host for one descriptor per distinct input (plain) or
/// per argument (dedupe, whose pass 2 opens one source per argument), plus
/// a fixed allowance, and is refused before it reads anything when granted
/// one fewer.
#[test]
fn descriptors_are_asked_for_per_pinned_input_and_refused_when_short() {
    let route = Route::of(Mode::Plain);
    let inputs = inputs(route);
    let dir = inputs.dir.path();
    let [a, b] = [inputs.paths[0].as_path(), inputs.paths[1].as_path()];
    let output = dir.join("out.osm.pbf");

    let ask = |mode: Mode, files: &[&Path], short: u64| {
        let grant = std::sync::Arc::new(Grant {
            asked: Mutex::new(Vec::new()),
            short,
        });
        let mut host = crate::common::test_host();
        host.process = std::sync::Arc::clone(&grant) as std::sync::Arc<dyn ProcessActions>;
        host.invocation
            .faults()
            .arm_input_replacement(a, &dir.join("unused.osm.pbf"), u64::MAX);
        let result = run(Route::of(mode), files, &output, &host);
        let asked = grant.asked.lock().expect("requests").clone();
        let attempts = host
            .invocation
            .faults()
            .input_replacement()
            .expect("armed")
            .attempts;
        (result, asked, attempts)
    };

    let (one, asked, _) = ask(Mode::Plain, &[a], 0);
    one.expect("one input");
    std::fs::remove_file(&output).expect("remove output");
    let [base] = asked[..] else {
        panic!("one request per run: {asked:?}");
    };
    assert!(base > 1, "the fixed allowance is on top of the input");

    for (mode, files, wanted) in [
        (Mode::Plain, vec![a, b], base + 1),
        (Mode::Plain, vec![a, a], base),
        (Mode::Plain, vec![a, b, a], base + 1),
        (Mode::Dedupe, vec![a, b], base + 1),
        (Mode::Dedupe, vec![a, a, a], base + 2),
    ] {
        let cell = format!("{mode:?} of {} inputs", files.len());
        let (result, asked, _) = ask(mode, &files, 0);
        result.unwrap_or_else(|e| panic!("{cell}: granted exactly: {}", e.report()));
        std::fs::remove_file(&output).expect("remove output");
        assert_eq!(asked, [wanted], "{cell}");

        let (result, asked, attempts) = ask(mode, &files, 1);
        assert_eq!(asked, [wanted], "{cell}");
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{cell}: one short must refuse"));
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
            "{cell}: {err:?}"
        );
        assert!(err.to_string().contains("descriptor"), "{cell}: {err}");
        // Counts derived opens only, so this says no input was read.
        assert_eq!(attempts, 0, "{cell}: refused before any input is read");
        assert!(!output.exists(), "{cell}: a refused run publishes nothing");
    }

    // A caller's pin is open already, so plain cat takes no new descriptor
    // for it: pinned `a` plus path `b` asks what `b` alone does. Dedupe's
    // pass 2 opens a source per argument, caller's pin included.
    let pinned = PinnedInput::pin(a, &crate::common::test_host()).expect("pin a");
    let plain_with_pin = |mode: Mode| {
        let grant = std::sync::Arc::new(Grant {
            asked: Mutex::new(Vec::new()),
            short: 0,
        });
        let mut host = crate::common::test_host();
        host.process = std::sync::Arc::clone(&grant) as std::sync::Arc<dyn ProcessActions>;
        let sources = [
            pbfhogg::read::input::InputSource::from(&pinned),
            pbfhogg::read::input::InputSource::from(b),
        ];
        let write = WriteOptions::default();
        let overrides = pbfhogg::HeaderOverrides::default();
        let out = crate::common::out(&output);
        let result = if mode == Mode::Dedupe {
            pbfhogg::cat::dedupe::merge_pbf(&sources, out, &write, &overrides, &host).map(drop)
        } else {
            pbfhogg::cat::cat(
                &sources,
                out,
                None,
                &CleanAttrs::default(),
                &write,
                &overrides,
                &host,
            )
            .map(drop)
        };
        result.unwrap_or_else(|e| panic!("{mode:?} with a caller's pin: {}", e.report()));
        std::fs::remove_file(&output).expect("remove output");
        grant.asked.lock().expect("requests").clone()
    };
    assert_eq!(plain_with_pin(Mode::Plain), [base]);
    assert_eq!(plain_with_pin(Mode::Dedupe), [base + 1]);
}

/// A refusal names what the caller can change: a host grant points at the
/// host, a probed limit at the process's soft limit and, when the hard
/// limit leaves room, the value to raise it to.
#[test]
fn a_descriptor_refusal_names_the_callers_remedy() {
    use pbfhogg::process::{FdCount, FdProbe};
    let inputs = inputs(Route::of(Mode::Plain));
    let [a, b] = [inputs.paths[0].as_path(), inputs.paths[1].as_path()];
    let output = inputs.dir.path().join("out.osm.pbf");

    struct Fixed(FdAllowance);
    impl ProcessActions for Fixed {
        fn cross_thread_alloc_pool(&self) {}
        fn fd_allowance(&self, _wanted: u64) -> FdReservation {
            FdReservation::unreserved(self.0)
        }
    }
    let refusal = |allowance: FdAllowance| {
        let mut host = crate::common::test_host();
        host.process = std::sync::Arc::new(Fixed(allowance));
        let err = run(Route::of(Mode::Plain), &[a, b], &output, &host).expect_err("refused");
        assert!(!output.exists(), "a refused run publishes nothing");
        err.to_string()
    };
    let probe = |hard_limit| {
        FdAllowance::from_probe(FdProbe {
            soft_limit: 6,
            adjustment: None,
            open: FdCount::Counted(3),
            hard_limit,
        })
    };

    let text = refusal(FdAllowance::host_grant(1));
    assert!(text.contains("ProcessActions::fd_allowance"), "{text}");
    assert!(!text.contains("ulimit"), "{text}");

    let text = refusal(probe(Some(4096)));
    assert!(text.contains("soft open-file limit to at least"), "{text}");
    assert!(text.contains("hard limit 4096 allows it"), "{text}");

    let text = refusal(probe(Some(6)));
    assert!(text.contains("above the hard limit 6"), "{text}");

    let text = refusal(probe(None));
    assert!(text.contains("soft open-file limit (ulimit -n"), "{text}");
}

/// Every input is pinned before any is read, so a FIFO is refused up front,
/// in any position, before an earlier input is opened (a blocking open of a
/// FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_before_any_input_is_read() {
    let inputs = inputs(Route::of(Mode::Plain));
    let dir = inputs.dir.path();
    let a = inputs.paths[0].as_path();
    let fifo = crate::common::make_fifo(dir, "in.fifo");
    let output = dir.join("out.osm.pbf");
    for mode in [Mode::Plain, Mode::Type, Mode::Clean, Mode::Dedupe] {
        for files in [[a, fifo.as_path()], [fifo.as_path(), a]] {
            let cell = format!("{mode:?}, {files:?}");
            let host = crate::common::test_host();
            host.invocation.faults().arm_input_replacement(
                a,
                &dir.join("unused.osm.pbf"),
                u64::MAX,
            );
            let err = run(Route::of(mode), &files, &output, &host)
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
            assert!(!output.exists(), "{cell}: a refused run publishes nothing");
        }
    }
}
