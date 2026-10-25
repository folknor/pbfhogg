//! tags-filter against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn: after any open but the
//! last, every later open is shown to follow the pin and the refusal path of
//! every open after the first runs; after the last, nothing opens again and
//! the run is unaffected in either mode. Every open is taken on the calling
//! thread, one at a time, so the counts are exact.
//!
//! The opens, in order (see `Route::opens`):
//!
//! - two-pass: the indexdata probe (not under `invert` or
//!   `distrust_metadata`), pass 1's header read, pass 1's schedule walk (its
//!   descriptor serves the classify pool), the relation closure's schedule
//!   walk (only when a relation was selected; its descriptor serves every
//!   convergence round), the way-dependency schedule walk, pass 2's header
//!   read and pass 2's schedule walk (its descriptor serves the decode
//!   workers).
//! - `-R`: the probe (under the same condition) and the pipelined reader.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::MemberId;
use pbfhogg::tags_filter::{TagsFilterOptions, TagsFilterStats, tags_filter};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, generate_ways, snapshot_dir,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf, write_test_pbf_impl,
};

#[derive(Clone, Copy, Debug)]
struct Route {
    /// `-R`: direct matches only, one pass.
    omit_referenced: bool,
    invert: bool,
    distrust: bool,
    /// Whether an expression selects a relation, so the two-pass run walks
    /// the relation closure.
    relations: bool,
    /// The input carries no indexdata and the run is forced.
    unindexed: bool,
    direct: bool,
}

impl Route {
    const TWO_PASS: Self = Self {
        omit_referenced: false,
        invert: false,
        distrust: false,
        relations: true,
        unindexed: false,
        direct: false,
    };

    const SINGLE_PASS: Self = Self {
        omit_referenced: true,
        ..Self::TWO_PASS
    };

    const fn probes(self) -> bool {
        !self.invert && !self.distrust
    }

    /// Opens of one run. Under `invert` every relation is selected (no
    /// expression admits a relation, so none matches), so the closure runs.
    fn opens(self) -> u64 {
        let probe = u64::from(self.probes());
        if self.omit_referenced {
            return probe + 1;
        }
        let closure = u64::from(self.relations || self.invert);
        probe + 5 + closure
    }

    fn expressions(self) -> Vec<String> {
        let mut out = vec!["w/highway=primary".to_string()];
        if self.relations {
            out.push("r/type=route".to_string());
        }
        out
    }
}

/// Nodes 1..=100, 25 per blob; five primary ways and one path way; route
/// 3000 holding relation 3001 and node 10, and 3001 holding the path way,
/// so the closure needs two rounds and pulls in a way and its nodes.
fn write_input(path: &Path, route: Route) {
    let nodes = generate_nodes(100, 1);
    let mut ways: Vec<TestWay> = (0..5_i64)
        .map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![k + 1, k + 2, k + 3],
            tags: vec![("highway", "primary")],
            meta: None,
        })
        .collect();
    ways.push(TestWay {
        id: 2_000,
        refs: vec![90, 91, 92],
        tags: vec![("highway", "path")],
        meta: None,
    });
    let relations = vec![
        TestRelation {
            id: 3_000,
            members: vec![
                TestMember {
                    id: MemberId::Relation(3_001),
                    role: "",
                },
                TestMember {
                    id: MemberId::Node(10),
                    role: "stop",
                },
            ],
            tags: vec![("type", "route")],
            meta: None,
        },
        TestRelation {
            id: 3_001,
            members: vec![TestMember {
                id: MemberId::Way(2_000),
                role: "",
            }],
            tags: vec![],
            meta: None,
        },
    ];
    if route.unindexed {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 25);
    } else {
        write_test_pbf_impl(path, &nodes, &ways, &relations, true, Some(25));
    }
}

/// A valid PBF holding other primary ways on other nodes: a run that read
/// any of it would write different bytes, or fail.
fn write_replacement(path: &Path) {
    let nodes = generate_nodes(80, 10_001);
    let mut ways = generate_ways(4, 30_000, 3, 10_001);
    for w in &mut ways {
        w.tags.push(("highway", "primary"));
    }
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

fn run(
    route: Route,
    input: &Path,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<TagsFilterStats> {
    let expressions = route.expressions();
    let opts = TagsFilterOptions {
        expression_strs: &expressions,
        omit_referenced: route.omit_referenced,
        invert: route.invert,
        remove_tags: false,
        write: WriteOptions {
            direct_reads: route.direct,
            force: route.unindexed,
            ..WriteOptions::default()
        },
        parallelism: pbfhogg::Parallelism::Auto,
        distrust_metadata: route.distrust,
    };
    tags_filter(
        input,
        crate::common::out(output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
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
    assert_eq!(counted.attempts_after_fired, 0, "{route:?}: {counted:?}");
    if route.relations && !route.omit_referenced && !route.invert {
        // The closure ran both rounds: relation 3001, then way 2000.
        assert_eq!(stats.relations_from_relations, 1, "{route:?}");
        assert_eq!(stats.ways_from_relations, 1, "{route:?}");
    }
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");
    Reference::Output(bytes)
}

/// What [`reference`] found.
enum Reference {
    /// The output's bytes.
    Output(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
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
            let procfs = pbfhogg::read::input::PinnedInput::pin(input, &host)
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
fn two_pass_reads_only_the_pinned_input() {
    check_route(Route::TWO_PASS, 7);
    // No relation selected: no closure walk.
    check_route(
        Route {
            relations: false,
            ..Route::TWO_PASS
        },
        6,
    );
    // Forced on an input without indexdata: the probe still runs.
    check_route(
        Route {
            unindexed: true,
            ..Route::TWO_PASS
        },
        7,
    );
}

#[test]
fn two_pass_without_the_probe_reads_only_the_pinned_input() {
    check_route(
        Route {
            invert: true,
            relations: false,
            ..Route::TWO_PASS
        },
        6,
    );
    check_route(
        Route {
            distrust: true,
            ..Route::TWO_PASS
        },
        6,
    );
    check_route(
        Route {
            distrust: true,
            relations: false,
            ..Route::TWO_PASS
        },
        5,
    );
}

#[test]
fn single_pass_reads_only_the_pinned_input() {
    check_route(Route::SINGLE_PASS, 2);
    // One open: the replacement after it must leave the run unaffected.
    check_route(
        Route {
            invert: true,
            ..Route::SINGLE_PASS
        },
        1,
    );
    check_route(
        Route {
            distrust: true,
            ..Route::SINGLE_PASS
        },
        1,
    );
}

/// The probe, the header reads and `-R`'s reader open `O_DIRECT` when asked
/// to, through the pin's own direct open.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for (route, expected_opens) in [
        (
            Route {
                direct: true,
                ..Route::TWO_PASS
            },
            7,
        ),
        (
            Route {
                direct: true,
                ..Route::SINGLE_PASS
            },
            2,
        ),
    ] {
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

/// Every mode pins, the single-open ones included, so a FIFO is refused up
/// front rather than read once.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_in_every_mode() {
    let dir = crate::common::test_scratch();
    let fifo = dir.path().join("in.fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("fifo path");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    for route in [
        Route::TWO_PASS,
        Route::SINGLE_PASS,
        Route {
            invert: true,
            ..Route::SINGLE_PASS
        },
        Route {
            distrust: true,
            ..Route::SINGLE_PASS
        },
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
            !output.exists(),
            "{route:?}: a refused run publishes nothing"
        );
    }
}
