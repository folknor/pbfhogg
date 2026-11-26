//! getid and removeid against an input renamed over mid-run, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
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
//!   the public entry; a unit test in `crates/lib/src/commands/getid` covers it.
//! - `add_referenced`: the probe, pass 1's one schedule walk (only with a
//!   way or relation ID requested; its descriptor serves every reference
//!   phase, so the relation reads, the second of which runs only when a
//!   relation in a relation is reached, open nothing of their own) and
//!   pass 2's reader.
//! - removeid: the probe and the walker.

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;

use pbfhogg::getid::{GetidOptions, GetidStats, getid, parse_ids, removeid};
use pbfhogg::read::input::InputSource;
use pbfhogg::write::WriteOptions;

use pbfhogg::MemberId;

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, generate_ways,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf, write_test_pbf_impl,
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
    /// Whether a way ID is requested.
    ways: bool,
    /// Whether a relation ID is requested: one holding a way and a relation
    /// that holds a node and a way, so with `add_referenced` both relation
    /// reads and the way phase run. With `ways`, whether pass 1 runs.
    relations: bool,
    /// The input carries no indexdata and the run is forced.
    unindexed: bool,
    direct: bool,
}

impl Route {
    const INCLUDE: Self = Self {
        mode: Mode::Include,
        ways: true,
        relations: false,
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

    /// Whether the run takes `add_referenced`'s reference route: with no
    /// way or relation requested nothing is pulled in, and the run is the
    /// include route.
    fn references(self) -> bool {
        self.mode == Mode::AddReferenced && (self.ways || self.relations)
    }

    fn opens(self) -> u64 {
        match self.mode {
            _ if self.references() => 3,
            Mode::Include | Mode::AddReferenced => 2 + u64::from(!self.unindexed),
            Mode::Remove => 2,
        }
    }

    /// The transport of each open on a direct run, following the getid row
    /// of `crates/cli/src/io_reach.rs`: the probe direct, the estimate and
    /// schedule walks buffered (the scheduled preads use the walk's
    /// descriptor), and `add_referenced`'s front read of the header for
    /// pass 2 direct.
    fn transports(self) -> &'static str {
        match self.mode {
            _ if self.references() => "DBD",
            Mode::Include | Mode::AddReferenced if self.unindexed => "DB",
            Mode::Include | Mode::AddReferenced => "DBB",
            Mode::Remove => "DB",
        }
    }

    fn ids(self) -> Vec<String> {
        let mut ids = vec!["n10".to_string(), "n60".to_string()];
        if self.ways {
            ids.push("w1002".to_string());
        }
        if self.relations {
            ids.push("r2000".to_string());
        }
        ids
    }
}

/// Nodes 1..=100, 25 per blob, five ways on nodes 1..=7, and relation 2000
/// holding way 1003 and relation 2001, which holds node 50 and way 1004.
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
    let member = |id| TestMember { id, role: "" };
    let relations = [
        TestRelation {
            id: 2_000,
            members: vec![
                member(MemberId::Way(1_003)),
                member(MemberId::Relation(2_001)),
            ],
            tags: vec![("type", "route")],
            meta: None,
        },
        TestRelation {
            id: 2_001,
            members: vec![member(MemberId::Node(50)), member(MemberId::Way(1_004))],
            tags: vec![("type", "route")],
            meta: None,
        },
    ];
    if route.unindexed {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 25);
    } else {
        write_test_pbf_impl(path, &nodes, &ways, &relations, true, Some(25));
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
                add_referenced: (route.mode == Mode::AddReferenced)
                    .then_some(pbfhogg::getid::AddReferenced { remove_tags: false }),
                report_found: false,
            };
            getid(input, output, &ids, &opts, &write, &overrides, host)
        }
    }
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
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            route.opens(),
            route.transports(),
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| run(route, &input, &output, host),
            |stats| {
                // The run selected something, so an output read from the
                // replacement could not pass for it by being empty.
                assert!(
                    stats.nodes_written + stats.ways_written > 0,
                    "{route:?}: the reference run wrote nothing"
                );
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
    // No way or relation requested: nothing is pulled in, and the run is
    // the include route, arm estimate included.
    check_route(
        Route {
            ways: false,
            ..Route::ADD_REFERENCED
        },
        3,
    );
    // A relation and no way: one walk serves both relation reads and the
    // way phase, indexed or forced on an input without indexdata.
    for unindexed in [false, true] {
        check_route(
            Route {
                ways: false,
                relations: true,
                unindexed,
                ..Route::ADD_REFERENCED
            },
            3,
        );
    }
}

#[test]
fn removeid_reads_only_the_pinned_input() {
    check_route(Route::REMOVE, 2);
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
