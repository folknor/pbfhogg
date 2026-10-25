//! extract and multi-extract against an input renamed over mid-run, through
//! the fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens but the last in turn, so every
//! later open is shown to follow the pin and the refusal path of every open
//! after the first runs. Every open is taken on the calling thread, one at a
//! time, so the counts are exact.
//!
//! The opens, in order, per strategy (see `Strategy::opens`):
//!
//! - simple, sorted: the sortedness probe, the schedule walk, the header
//!   read, then per kind (nodes, ways, relations) the classify descriptor
//!   and the write pass's descriptor.
//! - simple, unsorted: the probe, one reader per kind's classify sweep, the
//!   write pass's reader.
//! - complete and smart: the indexdata front read, pass 1's sortedness
//!   probe, then sorted: pass 1's schedule walk (its descriptor shared by
//!   the three classify phases), smart's pass 2 descriptor (a schedule walk
//!   of its own when pass 1 found no way blob), the header read and the
//!   write pass; unsorted: one reader per kind, smart's pass 2 schedule
//!   walk, the header read, the write pass's schedule walk and the write
//!   pass.
//! - multi-extract, simple without cleaning on a sorted input: the probe
//!   and the schedule walk, whose descriptor serves every phase. Otherwise
//!   (a declined probe, or a route that skips it) one extract per region,
//!   every one of them on the run's one pin.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::MemberId;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::commands::extract::{ExtractStrategy, Region, extract, extract_multi, parse_bbox};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, generate_ways, snapshot_dir,
    write_multi_block_test_pbf, write_test_pbf_impl,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    Simple,
    Complete,
    Smart,
}

impl Strategy {
    fn extract(self) -> ExtractStrategy {
        match self {
            Self::Simple => ExtractStrategy::Simple,
            Self::Complete => ExtractStrategy::CompleteWays,
            Self::Smart => ExtractStrategy::Smart,
        }
    }

    /// Opens of one extract of this strategy, on an input whose node,
    /// way (but see smart's no-way route) and relation schedules are all
    /// non-empty.
    fn opens(self, sorted: bool) -> u64 {
        match (self, sorted) {
            (Self::Simple, true) => 9,
            (Self::Simple, false) => 5,
            (Self::Complete, true) => 5,
            (Self::Complete, false) => 8,
            (Self::Smart, true) => 6,
            (Self::Smart, false) => 9,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Route {
    /// A two-region multi-extract rather than one extract.
    multi: bool,
    strategy: Strategy,
    /// Whether the header claims `Sort.Type_then_ID`; the blocks are in
    /// sorted order either way, so this selects the engine only.
    sorted: bool,
    /// Whether the input has ways at all.
    ways: bool,
    clean: bool,
    direct: bool,
}

impl Route {
    const fn single(strategy: Strategy, sorted: bool) -> Self {
        Self {
            multi: false,
            strategy,
            sorted,
            ways: true,
            clean: false,
            direct: false,
        }
    }

    const fn multi(strategy: Strategy, sorted: bool) -> Self {
        Self {
            multi: true,
            ..Self::single(strategy, sorted)
        }
    }

    fn opens(self) -> u64 {
        let per_extract = self.strategy.opens(self.sorted);
        if !self.multi {
            return per_extract;
        }
        if self.strategy == Strategy::Simple && !self.clean {
            if self.sorted {
                // The single-pass probe accepts: probe and schedule walk.
                2
            } else {
                // The probe declines, then one extract per region.
                1 + 2 * per_extract
            }
        } else {
            // No probe: one extract per region.
            2 * per_extract
        }
    }

    fn outputs(self, dir: &Path) -> Vec<PathBuf> {
        let n = if self.multi { 2 } else { 1 };
        (0..n)
            .map(|i| dir.join(format!("out-{i}.osm.pbf")))
            .collect()
    }
}

/// Nodes 1..=100 on the diagonal at `id * 1e-4` degrees, 25 per blob, so
/// the first node blob lies inside the first region (a raw passthrough
/// candidate on the simple routes); ways inside and outside the regions;
/// a multipolygon pulling an outside way into the smart extract, and a
/// relation on an outside node.
fn write_input(path: &Path, route: Route) {
    let nodes = generate_nodes(100, 1);
    let mut ways: Vec<TestWay> = Vec::new();
    if route.ways {
        ways.extend((0..5_i64).map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![k + 1, k + 2, k + 3],
            tags: vec![("highway", "path")],
            meta: None,
        }));
        ways.extend((0..3_i64).map(|k| TestWay {
            id: 2_000 + k,
            refs: vec![90 + k, 91 + k, 92 + k],
            tags: vec![("highway", "path")],
            meta: None,
        }));
    }
    let relations = vec![
        TestRelation {
            id: 3_000,
            members: vec![
                TestMember {
                    id: MemberId::Way(1_000),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(2_000),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Node(10),
                    role: "label",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        },
        TestRelation {
            id: 3_001,
            members: vec![TestMember {
                id: MemberId::Node(95),
                role: "",
            }],
            tags: vec![("type", "site")],
            meta: None,
        },
    ];
    write_test_pbf_impl(path, &nodes, &ways, &relations, route.sorted, Some(25));
}

/// A valid PBF whose nodes sit where the input's do under other IDs: a run
/// that read any of it would write different bytes, or fail.
fn write_replacement(path: &Path) {
    let nodes = generate_nodes(80, 10_001);
    let ways = generate_ways(4, 30_000, 3, 10_001);
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

fn regions() -> [Region; 2] {
    [
        Region::Bbox(parse_bbox("0,0,0.0045,0.0045").expect("bbox")),
        Region::Bbox(parse_bbox("0.002,0.002,0.008,0.008").expect("bbox")),
    ]
}

fn run(
    route: Route,
    input: &Path,
    outputs: &[PathBuf],
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let [first, second] = regions();
    let clean = CleanAttrs {
        version: route.clean,
        ..CleanAttrs::default()
    };
    let write = WriteOptions {
        direct_reads: route.direct,
        ..WriteOptions::default()
    };
    let overrides = pbfhogg::HeaderOverrides::default();
    if route.multi {
        extract_multi(
            input,
            &[first, second],
            pbfhogg::OutputSet::files(
                outputs,
                pbfhogg::Overwrite::Refuse,
                &pbfhogg::host::Diagnostics::discard(),
            )?,
            route.strategy.extract(),
            false,
            &clean,
            &write,
            &overrides,
            host,
        )
        .map(drop)
    } else {
        extract(
            input,
            crate::common::out(&outputs[0]),
            &first,
            route.strategy.extract(),
            false,
            &clean,
            &write,
            &overrides,
            host,
        )
        .map(drop)
    }
}

/// The unreplaced run's outputs, with its open count checked: armed past
/// the last open, the replacement never fires and only counts. Skipped when
/// the run failed and `skippable` accepts its error; any other failure
/// panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let reference_dir = dir.join("reference");
    std::fs::create_dir_all(&reference_dir).expect("create reference dir");
    let outputs = route.outputs(&reference_dir);
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), opens + 1);
    match run(route, input, &outputs, &host) {
        Ok(()) => {}
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    }
    let counted = host
        .cancel
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts_after_fired, 0, "{route:?}: {counted:?}");
    let bytes = outputs
        .iter()
        .map(|p| std::fs::read(p).expect("read reference output"))
        .collect();
    std::fs::remove_dir_all(&reference_dir).expect("remove reference dir");
    Reference::Outputs(bytes)
}

/// What [`reference`] found.
enum Reference {
    /// Each output's bytes, in output order.
    Outputs(Vec<Vec<u8>>),
    /// The host cannot run the route.
    Skipped,
}

/// Run `route` with the replacement armed after each of its opens but the
/// last in turn, in both reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[Vec<u8>]) {
    let opens = route.opens();
    let replacement = dir.join("replacement.osm.pbf");
    for after in 1..opens {
        for force_path in [true, false] {
            write_input(input, route);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.cancel
                .faults()
                .arm_input_replacement(input, &replacement, after);
            let outputs = route.outputs(dir);
            let before = snapshot_dir(dir);
            let result = run(route, input, &outputs, &host);

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

            if procfs {
                result.unwrap_or_else(|e| {
                    panic!("{cell}: a procfs-pinned run is unaffected by the replacement: {e}")
                });
                assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                assert_eq!(report.verified, opens, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    opens - after,
                    "{cell}: {report:?}"
                );
                for (output, expected) in outputs.iter().zip(expected) {
                    assert_eq!(
                        &std::fs::read(output).expect("read output"),
                        expected,
                        "{cell}: {} must hold only what the pinned input yields",
                        output.display()
                    );
                    std::fs::remove_file(output).expect("remove output");
                }
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
                for output in &outputs {
                    assert!(
                        !output.exists(),
                        "{cell}: a refused run must publish nothing"
                    );
                }
                // The replacement was renamed over the input, so it is the
                // one path the run itself did not account for.
                let mut after_run = snapshot_dir(dir);
                after_run.insert(PathBuf::from("replacement.osm.pbf"));
                crate::common::assert_scratch_unchanged(&before, &after_run);
            }
        }
    }
}

fn check_route(route: Route) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Outputs(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
}

#[test]
fn simple_reads_only_the_pinned_input() {
    for sorted in [true, false] {
        check_route(Route::single(Strategy::Simple, sorted));
    }
}

#[test]
fn complete_ways_reads_only_the_pinned_input() {
    for sorted in [true, false] {
        check_route(Route::single(Strategy::Complete, sorted));
    }
}

#[test]
fn smart_reads_only_the_pinned_input() {
    for sorted in [true, false] {
        check_route(Route::single(Strategy::Smart, sorted));
    }
    // No way blob: pass 1 hands smart's pass 2 no way schedule, so pass 2
    // walks the input itself instead of opening a plain descriptor.
    check_route(Route {
        ways: false,
        ..Route::single(Strategy::Smart, true)
    });
}

#[test]
fn multi_extract_reads_only_the_pinned_input() {
    // Single pass.
    check_route(Route::multi(Strategy::Simple, true));
    // The probe declines; arming after open 6 replaces the input between
    // the two regions' extracts.
    check_route(Route::multi(Strategy::Simple, false));
    // Strategies and cleaning that skip the probe.
    check_route(Route::multi(Strategy::Complete, true));
    check_route(Route {
        clean: true,
        ..Route::multi(Strategy::Simple, true)
    });
}

/// The probe and header reads open `O_DIRECT` when asked to, through the
/// pin's own direct open.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for strategy in [Strategy::Simple, Strategy::Complete] {
        let route = Route {
            direct: true,
            ..Route::single(strategy, true)
        };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        write_input(&input, route);
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        let Reference::Outputs(expected) =
            reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
        else {
            return;
        };
        check_replacements(route, dir.path(), &input, &expected);
    }
}
