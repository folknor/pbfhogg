//! tags-filter against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
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
//!   `distrust_metadata`), pass 1's header read, pass 1's schedule walk (the
//!   run's only walk: the closure, way-dependency and pass-2 schedules are
//!   cut from its catalog, and its descriptor serves every pool), and pass
//!   2's header read.
//! - `-R`: the probe (under the same condition, and only for expressions
//!   that leave some kind out, as every route's here do: no `n/`) and the
//!   pipelined reader.

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;

use pbfhogg::MemberId;
use pbfhogg::tags_filter::{AddReferenced, TagsFilterOptions, TagsFilterStats, tags_filter};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, generate_ways,
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

    /// Opens of one run. The two-pass count does not depend on whether the
    /// closure or the way-dependency phase runs: both read through pass 1's
    /// walk descriptor.
    fn opens(self) -> u64 {
        let probe = u64::from(self.probes());
        if self.omit_referenced {
            return probe + 1;
        }
        probe + 3
    }

    /// The transport of each open on a direct run, following the
    /// tags-filter row of `crates/cli/src/io_reach.rs`: the probe, the header reads and the
    /// `-R` stream direct, the two-pass schedule walk buffered.
    fn transports(self) -> String {
        let probe = if self.probes() { "D" } else { "" };
        let rest = if self.omit_referenced { "D" } else { "DBD" };
        format!("{probe}{rest}")
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

fn run<'a>(
    route: Route,
    input: impl Into<pbfhogg::read::input::InputSource<'a>>,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<TagsFilterStats> {
    let expressions = route.expressions();
    let opts = TagsFilterOptions {
        expression_strs: &expressions,
        add_referenced: (!route.omit_referenced).then_some(AddReferenced {
            remove_tags: false,
            parallelism: pbfhogg::Parallelism::Auto,
        }),
        invert: route.invert,
        write: WriteOptions {
            direct_reads: route.direct,
            force: route.unindexed,
            ..WriteOptions::default()
        },
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

/// Check `route` in every read mode through [`pinned_cell::check`]: the
/// reference with its exact transports, then the replacement armed after
/// each open in turn, in both reopen modes. `expected_opens` is fixed, so
/// an open that stopped deriving from the pin (and so stopped counting)
/// cannot shrink the arming range unnoticed.
fn check_route(route: Route, expected_opens: u64) {
    assert_eq!(route.opens(), expected_opens, "{route:?}");
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_input(&input, route);
        let spec = route.transports();
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            route.opens(),
            &spec,
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| run(route, &input, &output, host),
            |stats| {
                if route.relations && !route.omit_referenced && !route.invert {
                    // The closure ran both rounds: relation 3001, then way 2000.
                    assert_eq!(stats.relations_from_relations, 1, "{route:?}");
                    assert_eq!(stats.ways_from_relations, 1, "{route:?}");
                }
                pinned_cell::take(&output)
            },
            |_, _| {
                write_input(&input, route);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, &output, step.host),
            |expected, (), step, judged| match judged {
                Judged::Unaffected(_) => assert_eq!(
                    &pinned_cell::take(&output),
                    expected,
                    "{}: the output must hold only what the pinned input yields",
                    step.what
                ),
                Judged::Refused => assert!(
                    !output.exists(),
                    "{}: a refused run must publish nothing",
                    step.what
                ),
            },
        );
    }
}

#[test]
fn two_pass_reads_only_the_pinned_input() {
    check_route(Route::TWO_PASS, 4);
    // No relation selected: no closure and no way-dependency phase, which
    // open nothing anyway.
    check_route(
        Route {
            relations: false,
            ..Route::TWO_PASS
        },
        4,
    );
    // Forced on an input without indexdata: the probe still runs.
    check_route(
        Route {
            unindexed: true,
            ..Route::TWO_PASS
        },
        4,
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
        3,
    );
    check_route(
        Route {
            distrust: true,
            ..Route::TWO_PASS
        },
        3,
    );
    check_route(
        Route {
            distrust: true,
            relations: false,
            ..Route::TWO_PASS
        },
        3,
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

/// Every mode pins, the single-open ones included, so a FIFO is refused up
/// front rather than read once.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_in_every_mode() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
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
