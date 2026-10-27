//! add-locations-to-ways against an input renamed over mid-run, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens but the last in turn, so every
//! later open is shown to follow the pin and the refusal path of every open
//! after the first runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
    /// after the front.
    Indexed(IndexType),
    /// Forced sparse on an unindexed input: the pass 2 walk is replaced by
    /// the decode-all reader.
    DecodeAll,
    /// The public `external_join`, which pins on its own.
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
/// external when the page-cache budget cannot hold the store estimate.
fn host(memory_available: Option<u64>) -> pbfhogg::host::Host {
    let mut host = crate::common::test_host();
    host.resources = Arc::new(pbfhogg::host::Fixed {
        memory_available,
        ..crate::common::fixed_resources()
    });
    host
}

fn run(
    route: Route,
    input: &Path,
    output: &Path,
    host: &pbfhogg::host::Host,
    scratch_dir: &Path,
) -> pbfhogg::Result<()> {
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
        &host(memory_available),
        dir.path(),
    )
    .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
    let expected = std::fs::read(&reference).expect("read reference output");

    for after in 1..opens {
        for force_path in [true, false] {
            write_input(&input, indexed);
            write_replacement(&replacement);
            let host = host(memory_available);
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(&input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.invocation
                .faults()
                .arm_input_replacement(&input, &replacement, after);
            let output = dir.path().join("out.osm.pbf");
            let before = snapshot_dir(dir.path());
            let result = run(route, &input, &output, &host, dir.path());

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

            if procfs {
                result.unwrap_or_else(|e| {
                    panic!("{cell}: a procfs-pinned run is unaffected by the replacement: {e}")
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

/// No memory budget: auto routes this indexed input external, after its
/// estimate walk.
#[test]
fn auto_routed_external_reads_only_the_pinned_input() {
    check_route(Route::Indexed(IndexType::Auto), Some(0), 7, Some(3));
}
