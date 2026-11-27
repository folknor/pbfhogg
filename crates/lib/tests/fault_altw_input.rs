//! add-locations-to-ways against an input renamed over mid-run, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens but the last in turn, so every
//! later open is shown to follow the pin and the refusal path of every open
//! after the first runs.

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;
use std::sync::Arc;

use pbfhogg::altw::{AltwOptions, IndexType, add_locations_to_ways};
use pbfhogg::write::WriteOptions;

use crate::common::{
    generate_nodes, generate_relations, generate_ways, write_multi_block_non_indexed_pbf,
    write_multi_block_test_pbf,
};

/// One way of running the command, and how many times it opens the input
/// after pinning it.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// `add_locations_to_ways` with this index type, on an indexed input:
    /// sparse is front, the all-node estimate and/or pass 0, the relation
    /// scan and the shared output schedule; external is front, the metadata
    /// scan, stage 1 and the relation scan (concurrently), stage 2 and stage
    /// 4. Auto estimates after the front when it has a page-cache budget.
    Indexed(IndexType),
    /// Forced sparse on an unindexed input: the pass 2 walk is replaced by
    /// the decode-all reader.
    DecodeAll,
    /// Auto on an indexed input holding negative node IDs, under a budget
    /// its header estimate fits but its exact store does not: sparse up
    /// front, then the post-pass-0 recheck re-routes external, which opens
    /// a second front read.
    Rechecked,
}

/// Write the run's input: 60 nodes, ways over the first six, relations over
/// the ways, five elements per blob. [`Route::Rechecked`] takes one way over
/// four nodes, two of them at IDs 0 and -1.
fn write_input(path: &Path, route: Route) {
    if matches!(route, Route::Rechecked) {
        let nodes: Vec<crate::common::TestNode> = [0, -1, 1, 2]
            .into_iter()
            .map(|id| crate::common::TestNode {
                id,
                lat: 550_000_000,
                lon: 120_000_000,
                tags: vec![],
                meta: None,
            })
            .collect();
        let way = [crate::common::TestWay {
            id: 1,
            refs: vec![-1, 0, 1, 2],
            tags: vec![],
            meta: None,
        }];
        crate::common::write_indexed_pbf(path, &nodes, &way, &[]);
        return;
    }
    let indexed = !matches!(route, Route::DecodeAll);
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(10, 10_000, 6, 1);
    let relations = generate_relations(2, 20_000, 2, 10_000);
    if indexed {
        write_multi_block_test_pbf(path, &nodes, &ways, &relations, 5);
    } else {
        write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 5);
    }
}

/// A valid PBF with different IDs and blob layout: a run that read any of
/// it would write different bytes, or fail.
fn write_replacement(path: &Path) {
    let nodes = generate_nodes(80, 101);
    let ways = generate_ways(4, 30_000, 3, 101);
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

/// The test host, reporting `memory_available` as given: auto routes
/// external when a known page-cache budget cannot hold the store estimate.
/// Built from a heavy ticket, since several routes run an external join.
fn host(
    ticket: &Arc<crate::common::FdTicket>,
    memory_available: Option<u64>,
) -> pbfhogg::host::Host {
    let mut host = crate::common::heavy_host(ticket);
    host.resources = Arc::new(pbfhogg::host::Fixed {
        memory_available,
        ..crate::common::fixed_resources()
    });
    host
}

fn run<'a>(
    route: Route,
    input: impl Into<pbfhogg::read::input::InputSource<'a>>,
    output: &Path,
    host: &pbfhogg::host::Host,
    scratch_dir: &Path,
    direct: bool,
) -> pbfhogg::Result<()> {
    let input = input.into();
    let scratch = pbfhogg::scratch::RunScratch::create(scratch_dir, host)?;
    let (index_type, force) = match route {
        Route::Indexed(index_type) => (index_type, false),
        Route::DecodeAll => (IndexType::Sparse, true),
        Route::Rechecked => (IndexType::Auto, false),
    };
    add_locations_to_ways(
        input,
        crate::common::out(output),
        &AltwOptions {
            // Off, so both backends run their relation member scan.
            keep_untagged_nodes: false,
            write: WriteOptions {
                force,
                direct_reads: direct,
                ..WriteOptions::default()
            },
            index_type,
            inject_prepass: false,
            ignore_missing_nodes: false,
        },
        &pbfhogg::HeaderOverrides::default(),
        host,
        scratch,
    )
    .map(drop)
}

/// One route's shape: the host's `memory_available`, its `opens`, `racing`
/// (the arming point after which two concurrent opens, external stage 1
/// and the relation scan, both follow the replacement), and `transports`,
/// the transport of each open when run direct (`pinned_cell::check`),
/// following the add-locations-to-ways row of `io_reach.rs`: the front read
/// and the unindexed fallback's streaming reader direct, the index,
/// dependency and external-stage walks and preads buffered.
#[derive(Clone, Copy, Debug)]
struct Shape {
    memory_available: Option<u64>,
    opens: u64,
    racing: Option<u64>,
    transports: &'static str,
}

/// Run `route` in every read mode through [`pinned_cell::check`], every
/// run on a fresh host of the route's shape: the reference with its exact
/// transports, then the replacement armed after each of its opens but the
/// last in turn, in both reopen modes.
fn check_route(route: Route, shape: Shape) {
    for &direct in pinned_cell::modes() {
        // One run at a time; any route may be external.
        crate::common::with_heavy_ticket(1, &format!("check_route {route:?}"), |ticket| {
            check_admitted_route(ticket, route, shape, direct);
        });
    }
}

fn check_admitted_route(
    ticket: &Arc<crate::common::FdTicket>,
    route: Route,
    shape: Shape,
    direct: bool,
) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_input(&input, route);
    let cell = Cell::new(
        format!("{route:?}, {shape:?}"),
        direct,
        &input,
        &replacement,
        shape.opens,
        shape.transports,
    )
    .without_last_arm()
    .racing(shape.racing)
    .scratch(dir.path())
    .host(|| host(ticket, shape.memory_available));
    pinned_cell::check(
        &cell,
        |host| run(route, &input, &output, host, dir.path(), direct),
        |()| pinned_cell::take(&output),
        |_, _| {
            write_input(&input, route);
            write_replacement(&replacement);
        },
        |(), step| run(route, step.pin, &output, step.host, dir.path(), direct),
        |expected, (), step, judged| match judged {
            Judged::Unaffected(()) => assert_eq!(
                &pinned_cell::take(&output),
                expected,
                "{}: the run must read only the input it pinned",
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

const SPARSE: (Route, Shape) = (
    Route::Indexed(IndexType::Sparse),
    Shape {
        memory_available: Some(8 << 30),
        // The front read, all-node estimate, relation scan and output schedule,
        // whose shared file the node and way passes both read.
        opens: 4,
        racing: None,
        transports: "DBBB",
    },
);

const DECODE_ALL: (Route, Shape) = (
    Route::DecodeAll,
    Shape {
        memory_available: Some(8 << 30),
        opens: 5,
        racing: None,
        // The decode-all reader streams the file sequentially, direct.
        transports: "DBBBD",
    },
);

/// Stage 1 and the relation scan open concurrently after the metadata
/// scan, so a replacement after the metadata scan's open is followed by
/// both. A replacement after the third open lands between them in either
/// order (the hook serializes opens), and only the later one is refused:
/// which of the two that is varies between runs.
const EXTERNAL: (Route, Shape) = (
    Route::Indexed(IndexType::External),
    Shape {
        memory_available: Some(8 << 30),
        opens: 6,
        racing: Some(2),
        transports: "DBBBBB",
    },
);

const AUTO_SPARSE: (Route, Shape) = (
    Route::Indexed(IndexType::Auto),
    Shape {
        memory_available: Some(8 << 30),
        // The all-node store fits. The last replacement arm fires after
        // the relation scan opens, before the shared output schedule opens.
        opens: 4,
        racing: None,
        transports: "DBBB",
    },
);

/// The 480-byte all-node store fits auto's 80% threshold on a 1 KiB
/// page-cache budget, but exceeds the eager store's 35% share. Pass 0
/// counts six referenced IDs, which fit the budget on the recheck.
const AUTO_REFERENCE_SELECTED: (Route, Shape) = (
    Route::Indexed(IndexType::Auto),
    Shape {
        memory_available: Some((3 << 30) + 1024),
        opens: 5,
        racing: None,
        transports: "DBBBB",
    },
);

/// One byte above the sparse run's fixed 3 GiB anon allowance leaves a
/// one-byte page-cache budget: auto routes this indexed input external,
/// after its estimate walk. Racing as [`EXTERNAL`], one open later.
const AUTO_EXTERNAL: (Route, Shape) = (
    Route::Indexed(IndexType::Auto),
    Shape {
        memory_available: Some((3 << 30) + 1),
        opens: 7,
        racing: Some(3),
        transports: "DBBBBBB",
    },
);

/// Unknown memory: auto keeps reference-selected sparse and skips the
/// estimate walk. Pass 0 opens the input in its place.
const AUTO_UNKNOWN_MEMORY: (Route, Shape) = (
    Route::Indexed(IndexType::Auto),
    Shape {
        memory_available: None,
        opens: 4,
        racing: None,
        transports: "DBBB",
    },
);

/// A 40-byte page-cache budget above the sparse allowance: the header
/// estimate fits, the exact store does not. The front read, the estimate
/// walk and pass 0, then the re-route's second front read (direct) and the
/// external join's opens on this input, its stage 1 and relation scan
/// racing after the sixth.
const RECHECKED: (Route, Shape) = (
    Route::Rechecked,
    Shape {
        memory_available: Some((3 << 30) + 40),
        opens: 10,
        racing: Some(6),
        transports: "DBBDBBBBBB",
    },
);

#[test]
fn sparse_reads_only_the_pinned_input() {
    check_route(SPARSE.0, SPARSE.1);
}

#[test]
fn sparse_decode_all_reads_only_the_pinned_input() {
    check_route(DECODE_ALL.0, DECODE_ALL.1);
}

#[test]
fn external_reads_only_the_pinned_input() {
    check_route(EXTERNAL.0, EXTERNAL.1);
}

#[test]
fn auto_routed_sparse_reads_only_the_pinned_input() {
    check_route(AUTO_SPARSE.0, AUTO_SPARSE.1);
}

#[test]
fn auto_reference_selected_sparse_reads_only_the_pinned_input() {
    check_route(AUTO_REFERENCE_SELECTED.0, AUTO_REFERENCE_SELECTED.1);
}

#[test]
fn auto_routed_external_reads_only_the_pinned_input() {
    check_route(AUTO_EXTERNAL.0, AUTO_EXTERNAL.1);
}

#[test]
fn auto_rechecked_external_reads_only_the_pinned_input() {
    check_route(RECHECKED.0, RECHECKED.1);
}

#[test]
fn auto_unknown_memory_skips_the_estimate_walk() {
    check_route(AUTO_UNKNOWN_MEMORY.0, AUTO_UNKNOWN_MEMORY.1);
}
