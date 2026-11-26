//! extract and multi-extract against an input renamed over mid-run, through
//! the fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn, the last included,
//! so every later open is shown to follow the pin and the refusal path of
//! every open after the first runs. Every open is taken on the calling thread, one at a
//! time, so the counts are exact.
//!
//! The opens, in order, per strategy (see `Strategy::opens`):
//!
//! - simple, sorted: the sortedness probe, the schedule walk, the header
//!   read, then per kind (nodes, ways, relations) the fused pass's
//!   descriptor (these fixtures carry indexdata ID ranges the fused planner
//!   admits; a kind it refuses takes a classify and a write descriptor
//!   instead).
//! - simple, unsorted: the probe, one reader per kind's classify sweep, the
//!   write pass's reader.
//! - complete and smart: the indexdata front read, pass 1's sortedness
//!   probe, then sorted: pass 1's schedule walk (its descriptor shared by
//!   the three classify phases), smart's pass 2 descriptor (a schedule walk
//!   of its own when pass 1 found no way blob), the header read and the
//!   write pass; unsorted: one reader per kind, smart's pass 2 schedule
//!   walk, the header read, the write pass's schedule walk and the write
//!   pass.
//! - multi-extract, simple (cleaned or not) on a sorted input: the probe
//!   and the schedule walk, whose descriptor serves every phase.
//!   Complete and smart on a sorted input: the indexdata front read, the
//!   probe, the schedule walk (every classify phase's descriptor) and the
//!   write pass. On a declined probe, after the reads before it, one
//!   extract per region, every one of them on the run's one pin.

use crate::pinned_cell::{self, Cell, Judged};

use std::path::{Path, PathBuf};

use pbfhogg::MemberId;
use pbfhogg::cat::CleanAttrs;
use pbfhogg::commands::extract::{ExtractStrategy, Region, extract, extract_multi, parse_bbox};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestMember, TestRelation, TestWay, generate_nodes, generate_ways, write_multi_block_test_pbf,
    write_test_pbf_impl,
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
            (Self::Simple, true) => 6,
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

    /// The transport of each open on every route run direct
    /// (`pinned_cell::check`), following the extract row of
    /// `io_reach.rs`: the front reads (the indexdata front read, the
    /// probes, the header read) and the unsorted fallback's streaming
    /// readers direct; the schedule walks and the classify and write-pass
    /// preads buffered. A declined multi-extract probe is followed by each
    /// region's single extract without its front read.
    fn transports(self) -> &'static str {
        match (self.multi, self.strategy, self.sorted) {
            (false, Strategy::Simple, true) => "DBDBBB",
            (false, Strategy::Complete, true) => "DDBDB",
            (false, Strategy::Simple, false) => "DDDDD",
            (false, Strategy::Complete, false) => "DDDDDDBB",
            (false, Strategy::Smart, true) => "DDBBDB",
            (true, Strategy::Simple, true) => "DB",
            (true, Strategy::Simple, false) => "DDDDDDDDDDD",
            (false, Strategy::Smart, false) => "DDDDDBDBB",
            (true, Strategy::Complete | Strategy::Smart, true) => "DDBB",
            (true, Strategy::Complete, false) => "DDDDDDDBBDDDDDBB",
            (true, Strategy::Smart, false) => "DDDDDDBDBBDDDDBDBB",
        }
    }

    fn opens(self) -> u64 {
        let per_extract = self.strategy.opens(self.sorted);
        if !self.multi {
            return per_extract;
        }
        // Complete and smart take the indexdata front read before the probe.
        let front = u64::from(self.strategy != Strategy::Simple);
        match (self.strategy, self.sorted) {
            // The single-pass probe accepts: probe and schedule walk.
            (Strategy::Simple, true) => 2,
            // The front read, the probe, the schedule walk (its descriptor
            // shared by every classify phase and smart's extra-way sweep)
            // and the write pass.
            (_, true) => 4,
            // The probe declines, then one extract per region, each without
            // the front read the run already took.
            (_, false) => front + 1 + 2 * (per_extract - front),
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

fn run<'a>(
    route: Route,
    input: impl Into<pbfhogg::read::input::InputSource<'a>>,
    outputs: &[PathBuf],
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let input = input.into();
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

/// Each output's bytes, in output order, the files removed.
fn take_outputs(outputs: &[PathBuf]) -> Vec<Vec<u8>> {
    outputs.iter().map(|p| pinned_cell::take(p)).collect()
}

/// Run `route` in every read mode through [`pinned_cell::check`]: the
/// reference with its exact transports, then the replacement armed after
/// each open in turn, in both reopen modes.
fn check_route(route: Route) {
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let reference_dir = dir.path().join("reference");
        std::fs::create_dir_all(&reference_dir).expect("create reference dir");
        let references = route.outputs(&reference_dir);
        let outputs = route.outputs(dir.path());
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
            |host| run(route, &input, &references, host),
            |()| {
                let bytes = take_outputs(&references);
                std::fs::remove_dir_all(&reference_dir).expect("remove reference dir");
                bytes
            },
            |_, _| {
                write_input(&input, route);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, &outputs, step.host),
            |expected, (), step, judged| match judged {
                Judged::Unaffected(()) => assert_eq!(
                    &take_outputs(&outputs),
                    expected,
                    "{}: every output must hold only what the pinned input yields",
                    step.what
                ),
                Judged::Refused => {
                    for output in &outputs {
                        assert!(
                            !output.exists(),
                            "{}: a refused run must publish nothing",
                            step.what
                        );
                    }
                }
            },
        );
    }
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
    // The dependency strategies' single pass, and their declined probe.
    check_route(Route::multi(Strategy::Complete, true));
    check_route(Route::multi(Strategy::Smart, true));
    check_route(Route::multi(Strategy::Complete, false));
    check_route(Route::multi(Strategy::Smart, false));
    // Cleaning takes the single pass too.
    check_route(Route {
        clean: true,
        ..Route::multi(Strategy::Simple, true)
    });
}
