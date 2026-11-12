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

use crate::reopen_cell;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pbfhogg::altw::{AltwOptions, IndexType, add_locations_to_ways};
use pbfhogg::write::WriteOptions;

use crate::common::{
    generate_nodes, generate_relations, generate_ways, snapshot_dir,
    write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
};

/// One way of running the command, and how many times it opens the input
/// after pinning it.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// `add_locations_to_ways` with this index type, on an indexed input:
    /// sparse is front, pass 0, pass 1, the relation scan and the pass 2
    /// walk; external is front, the metadata scan, stage 1 and the relation
    /// scan (concurrently), stage 2 and stage 4. Auto adds the estimate walk
    /// after the front when it has a page-cache budget to route on.
    Indexed(IndexType),
    /// Forced sparse on an unindexed input: the pass 2 walk is replaced by
    /// the decode-all reader.
    DecodeAll,
    /// The public `external_join`, given the cell's pin.
    PublicExternal,
}

/// Write the run's input: 60 nodes, ways over the first six, relations over
/// the ways, five elements per blob.
fn write_input(path: &Path, indexed: bool) {
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
) -> pbfhogg::Result<()> {
    let input = input.into();
    let scratch = pbfhogg::scratch::RunScratch::create(scratch_dir, host)?;
    if let Route::PublicExternal = route {
        return pbfhogg::altw::external::external_join(
            input,
            crate::common::out(output),
            false,
            &WriteOptions::default(),
            &pbfhogg::HeaderOverrides::default(),
            false,
            host,
            scratch,
        )
        .map(drop);
    }
    let (index_type, force) = match route {
        Route::Indexed(index_type) => (index_type, false),
        Route::DecodeAll => (IndexType::Sparse, true),
        Route::PublicExternal => unreachable!("handled above"),
    };
    add_locations_to_ways(
        input,
        crate::common::out(output),
        &AltwOptions {
            // Off, so both backends run their relation member scan.
            keep_untagged_nodes: false,
            write: WriteOptions {
                force,
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

/// Run `route` with the replacement armed after each of its `opens` in
/// turn, in both reopen modes. `racing` is the arming point after which
/// two concurrent opens (external stage 1 and the relation scan) both
/// follow the replacement.
fn check_route(route: Route, memory_available: Option<u64>, opens: u64, racing: Option<u64>) {
    // One run at a time; any route may be external.
    let Some(ticket) = crate::common::heavy_ticket(1, &format!("check_route {route:?}")) else {
        return;
    };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let indexed = !matches!(route, Route::DecodeAll);
    write_input(&input, indexed);

    let reference = dir.path().join("reference.osm.pbf");
    run(
        route,
        &input,
        &reference,
        &host(&ticket, memory_available),
        dir.path(),
    )
    .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
    let expected = std::fs::read(&reference).expect("read reference output");

    for after in 1..opens {
        for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
            write_input(&input, indexed);
            write_replacement(&replacement);
            let host = host(&ticket, memory_available);
            let run_cell = |pin: &pbfhogg::read::input::PinnedInput, scenario| {
                host.invocation
                    .faults()
                    .arm_input_replacement(&input, &replacement, after);
                let output = dir.path().join("out.osm.pbf");
                let before = snapshot_dir(dir.path());
                let result = run(route, pin, &output, &host, dir.path());

                let cell = format!("{route:?}, replaced after open {after}, scenario {scenario:?}");
                let report = host
                    .invocation
                    .faults()
                    .input_replacement()
                    .expect("armed replacement");
                assert!(
                    report.fired,
                    "{cell}: the replacement must fire: {report:?}"
                );

                match scenario {
                    reopen_cell::Scenario::Pinned => {
                        result.unwrap_or_else(|e| {
                            panic!("{cell}: a pinned run is unaffected by the replacement: {e}")
                        });
                        assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                        assert_eq!(report.verified, opens, "{cell}: {report:?}");
                        assert_eq!(
                            report.attempts_after_fired,
                            opens - after,
                            "{cell}: {report:?}"
                        );
                        assert_eq!(
                            std::fs::read(&output).expect("read output"),
                            expected,
                            "{cell}: the run must read only the input it pinned"
                        );
                        std::fs::remove_file(&output).expect("remove output");
                    }
                    reopen_cell::Scenario::Path => {
                        let err = result
                            .err()
                            .unwrap_or_else(|| panic!("{cell}: must refuse"));
                        assert!(
                            matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                            "{cell}: {err:?}"
                        );
                        // The run stops at the first open after the replacement,
                        // except where two opens race past it together.
                        let refused = if racing == Some(after) { 2 } else { 1 };
                        assert_eq!(report.refused, refused, "{cell}: {report:?}");
                        assert_eq!(report.attempts, after + refused, "{cell}: {report:?}");
                        assert_eq!(report.attempts_after_fired, refused, "{cell}: {report:?}");
                        assert!(
                            !output.exists(),
                            "{cell}: a refused run must publish nothing"
                        );
                        // The replacement was renamed over the input, so it is the
                        // one path the run itself did not account for.
                        let mut after_run = snapshot_dir(dir.path());
                        after_run.insert(PathBuf::from("replacement.osm.pbf"));
                        crate::common::assert_scratch_unchanged(&before, &after_run);
                    }
                }
            };
            scenario.run(&input, &host, run_cell);
        }
    }
}

#[test]
fn sparse_reads_only_the_pinned_input() {
    check_route(Route::Indexed(IndexType::Sparse), Some(8 << 30), 5, None);
}

#[test]
fn sparse_decode_all_reads_only_the_pinned_input() {
    check_route(Route::DecodeAll, Some(8 << 30), 5, None);
}

/// Stage 1 and the relation scan open concurrently after the metadata
/// scan, so a replacement after the metadata scan's open is followed by
/// both. A replacement after the third open lands between them in either
/// order (the hook serializes opens), and only the later one is refused:
/// which of the two that is varies between runs.
#[test]
fn external_reads_only_the_pinned_input() {
    check_route(
        Route::Indexed(IndexType::External),
        Some(8 << 30),
        6,
        Some(2),
    );
}

#[test]
fn public_external_join_reads_only_the_pinned_input() {
    check_route(Route::PublicExternal, Some(8 << 30), 6, Some(2));
}

#[test]
fn auto_routed_sparse_reads_only_the_pinned_input() {
    check_route(Route::Indexed(IndexType::Auto), Some(8 << 30), 6, None);
}

/// One byte above the sparse run's fixed 3 GiB anon allowance leaves a
/// one-byte page-cache budget: auto routes this indexed input external,
/// after its estimate walk.
#[test]
fn auto_routed_external_reads_only_the_pinned_input() {
    check_route(
        Route::Indexed(IndexType::Auto),
        Some((3 << 30) + 1),
        7,
        Some(3),
    );
}

/// Unknown memory: auto keeps sparse and skips the estimate walk, so it
/// opens the input exactly as many times as explicit sparse.
#[test]
fn auto_unknown_memory_skips_the_estimate_walk() {
    check_route(Route::Indexed(IndexType::Auto), None, 5, None);
}
