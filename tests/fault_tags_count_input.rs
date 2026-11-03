//! tags-count against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced`. Each route is checked with the replacement armed after
//! each of its opens in turn, the last included. Both opens are taken on the
//! calling thread, one at a time, so the counts are exact.
//!
//! The opens, in order: the indexdata probe, on a route that skips blobs on
//! indexdata (a single-kind type filter here), then on every route the
//! schedule walk, whose descriptor the classify pool preads. Expressions (a
//! tag index that skips some or every blob), a forced unindexed input and
//! direct reads change neither.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::tags_count::{TagCountOptions, TagCountSort, tags_count};

use crate::common::{
    TestWay, generate_nodes, write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

impl Route {
    /// Opens of the input after the pin: the schedule walk, and before it
    /// the indexdata probe on a route that skips blobs on indexdata (the
    /// ways-only filter; `amenity=*` and `craft=*` name every kind).
    fn opens(self) -> u64 {
        1 + u64::from(matches!(self.filter, Filter::Ways))
    }
}

#[derive(Clone, Copy, Debug)]
enum Filter {
    None,
    /// Ways only: the schedule narrows to way blobs.
    Ways,
    /// `amenity=*`: the tag index skips the blobs without `amenity`.
    Amenity,
    /// A key no blob carries: the tag index skips every blob, so the
    /// schedule is empty and the result is too, from either file.
    Absent,
}

#[derive(Clone, Copy, Debug)]
struct Route {
    filter: Filter,
    /// The input carries no indexdata and the run is forced.
    unindexed: bool,
    direct: bool,
}

impl Route {
    const PLAIN: Self = Self {
        filter: Filter::None,
        unindexed: false,
        direct: false,
    };
}

/// `(key, value, count)`, sorted by name; `None` is a key row.
type Counts = Vec<(String, Option<String>, u64)>;

/// One fixture's three tags: `(key, value)` on the first 50 nodes, on the
/// last 50, and on the ways.
type Tags = [(&'static str, &'static str); 3];

/// Nodes 1..=100, 25 per blob: the first half `amenity=cafe`, the second
/// `shop=bakery`; five ways `highway=primary`.
const INPUT: Tags = [
    ("amenity", "cafe"),
    ("shop", "bakery"),
    ("highway", "primary"),
];

/// The same layout with other keys and values: a run that read any blob of
/// it would count different rows on every route that counts any, key rows
/// included. `craft` stays absent from both.
const REPLACEMENT: Tags = [
    ("leisure", "pub"),
    ("office", "butcher"),
    ("railway", "rail"),
];

fn write_input(path: &Path, route: Route) {
    write_fixture(path, route.unindexed, INPUT);
}

fn write_replacement(path: &Path) {
    write_fixture(path, false, REPLACEMENT);
}

fn write_fixture(path: &Path, unindexed: bool, tags: Tags) {
    let [first, second, way] = tags;
    let mut nodes = generate_nodes(100, 1);
    for (i, node) in nodes.iter_mut().enumerate() {
        node.tags = if i < 50 { vec![first] } else { vec![second] };
    }
    let ways: Vec<TestWay> = (0..5_i64)
        .map(|k| TestWay {
            id: 1_000 + k,
            refs: vec![k + 1, k + 2, k + 3],
            tags: vec![way],
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
) -> pbfhogg::Result<Counts> {
    let expressions: Vec<String> = match route.filter {
        Filter::Amenity => vec!["amenity=*".to_string()],
        Filter::Absent => vec!["craft=*".to_string()],
        Filter::None | Filter::Ways => Vec::new(),
    };
    let type_filter = match route.filter {
        Filter::Ways => Some(pbfhogg::TypeFilter {
            nodes: false,
            ways: true,
            relations: false,
        }),
        _ => None,
    };
    let opts = TagCountOptions {
        min_count: 0,
        max_count: None,
        sort: TagCountSort::NameAsc,
        expressions: &expressions,
        type_filter,
        direct_io: route.direct,
        force: route.unindexed,
        parallelism: pbfhogg::Parallelism::from_jobs(2),
    };
    Ok(tags_count(input, &opts, host)?
        .rows
        .into_iter()
        .map(|t| (t.key, t.value, t.count))
        .collect())
}

/// What a run of `route` counts on a fixture with `tags`, from its
/// construction.
fn counted(route: Route, tags: Tags) -> Counts {
    let [first, second, way] = tags;
    let key = |(k, _): (&str, &str), n: u64| (k.to_string(), None, n);
    let pair = |(k, v): (&str, &str), n: u64| (k.to_string(), Some(v.to_string()), n);
    let mut counts = match route.filter {
        Filter::None => vec![key(first, 50), key(second, 50), key(way, 5)],
        Filter::Ways => vec![key(way, 5)],
        // `amenity=*`, which only the input carries.
        Filter::Amenity if first.0 == "amenity" => vec![pair(first, 50)],
        Filter::Amenity | Filter::Absent => Vec::new(),
    };
    counts.sort();
    counts
}

/// What the unreplaced run of `route` counts.
fn expected(route: Route) -> Counts {
    counted(route, INPUT)
}

/// The fixtures are told apart by every route that counts anything: a run
/// that read the replacement could not pass for one that read the input.
#[test]
fn the_replacement_counts_differently_on_every_counting_route() {
    let dir = crate::common::test_scratch();
    let replacement = dir.path().join("replacement.osm.pbf");
    write_replacement(&replacement);
    let host = crate::common::test_host();
    for filter in [Filter::None, Filter::Ways, Filter::Amenity] {
        let route = Route {
            filter,
            ..Route::PLAIN
        };
        let got = run(route, &replacement, &host).expect("count the replacement");
        assert_eq!(got, counted(route, REPLACEMENT), "{filter:?}");
        assert_ne!(got, expected(route), "{filter:?}");
    }
}

/// What [`reference`] found.
enum Reference {
    Counts(Counts),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's counts, with its open count checked: armed past the
/// last open, the replacement never fires and only counts. Skipped when the
/// run failed and `skippable` accepts its error; any other failure panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), opens + 1);
    let counts = match run(route, input, &host) {
        Ok(counts) => counts,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    };
    let counted = host
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_eq!(counts, expected(route), "{route:?}: the unreplaced counts");
    Reference::Counts(counts)
}

/// Run `route` with the replacement armed after each of its opens in turn,
/// in both reopen modes, against the unreplaced `expected`.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &Counts) {
    let replacement = dir.join("replacement.osm.pbf");
    let opens = route.opens();
    for after in 1..=opens {
        for force_path in [true, false] {
            write_input(input, route);
            write_replacement(&replacement);
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
            let before = crate::common::snapshot_dir(dir);
            let result = run(route, input, &host);

            let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
            let report = host
                .invocation
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
                let counts = result.unwrap_or_else(|e| {
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
                    &counts, expected,
                    "{cell}: the counts must come only from the pinned input"
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
    write_input(&input, route);
    let Reference::Counts(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
}

#[test]
fn every_filter_reads_only_the_pinned_input() {
    for filter in [Filter::None, Filter::Ways, Filter::Amenity, Filter::Absent] {
        check_route(Route {
            filter,
            ..Route::PLAIN
        });
    }
}

/// On an input without indexdata: forced where the route probes, the probe
/// and the walk still open once each; where it does not, the walk alone.
#[test]
fn a_forced_unindexed_input_reads_only_the_pinned_input() {
    for filter in [Filter::None, Filter::Ways] {
        check_route(Route {
            filter,
            unindexed: true,
            ..Route::PLAIN
        });
    }
}

/// An unindexed input on a route that skips nothing on indexdata is counted
/// without `force`, and only the selected kind is counted when a forced
/// route has lost its kind skipping.
#[test]
fn an_unindexed_input_is_refused_only_where_indexdata_would_skip_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input, true, INPUT);
    let host = crate::common::test_host();
    for filter in [Filter::None, Filter::Amenity, Filter::Absent] {
        let route = Route {
            filter,
            ..Route::PLAIN
        };
        let counts = run(route, &input, &host)
            .unwrap_or_else(|e| panic!("{filter:?}: an unforced run must count: {e}"));
        assert_eq!(counts, expected(route), "{filter:?}");
    }
    let ways = Route {
        filter: Filter::Ways,
        ..Route::PLAIN
    };
    let err = run(ways, &input, &host)
        .err()
        .unwrap_or_else(|| panic!("a ways-only run must refuse without force"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::MissingIndexdata { .. }),
        "{err:?}"
    );
    let forced = Route {
        unindexed: true,
        ..ways
    };
    assert_eq!(
        run(forced, &input, &host).expect("forced"),
        expected(ways),
        "a forced ways-only run counts way tags only"
    );
}

/// The probe opens `O_DIRECT` when asked to, through the pin's own direct
/// open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    let route = Route {
        direct: true,
        filter: Filter::Ways,
        ..Route::PLAIN
    };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    let Reference::Counts(expected) =
        reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
    else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (procfs) or the first
/// open is refused (path reopen). An entry point that pinned the path
/// afresh would count the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let route = Route::PLAIN;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for force_path in [true, false] {
        write_input(&input, route);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        std::fs::rename(&replacement, &input).expect("replace the input");
        let result = run(route, &pin, &host);
        let cell = format!("procfs {procfs}");
        if procfs {
            let counts = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            assert_eq!(
                counts,
                expected(route),
                "{cell}: the counts must come from the pinned file"
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

/// The input is pinned, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer).
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    for filter in [Filter::None, Filter::Ways, Filter::Amenity] {
        let route = Route {
            filter,
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

/// A malformed expression is refused before the input is touched: ahead of
/// a FIFO's refusal and of a missing input's.
#[test]
fn a_malformed_expression_is_refused_before_the_input_is_touched() {
    let dir = crate::common::test_scratch();
    let mut inputs = vec![dir.path().join("missing.osm.pbf")];
    #[cfg(unix)]
    inputs.push(crate::common::make_fifo(dir.path(), "in.fifo"));
    let expressions = vec![String::new()];
    let parse_error = pbfhogg::tag_expr::validate_expressions(&expressions)
        .expect_err("an empty expression is malformed")
        .to_string();
    let opts = TagCountOptions {
        min_count: 0,
        max_count: None,
        sort: TagCountSort::NameAsc,
        expressions: &expressions,
        type_filter: None,
        direct_io: false,
        force: false,
        parallelism: pbfhogg::Parallelism::Auto,
    };
    for input in inputs {
        let err = tags_count(&input, &opts, &crate::common::test_host())
            .err()
            .unwrap_or_else(|| panic!("{}: must be refused", input.display()));
        assert_eq!(
            err.to_string(),
            parse_error,
            "{}: the expression must be refused first",
            input.display()
        );
    }
}
