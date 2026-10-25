//! sort against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens but the last in turn, so every
//! later open is shown to follow the pin and the refusal path of every open
//! after the first runs.
//!
//! The opens, in order: the front read (the indexdata precondition), pass
//! 1's header walker, pass 2's source (the passthrough read and the
//! writer's copy source share it), then one per overlap run, opened by the
//! rayon workers that decode the runs.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};

use crate::common::{generate_nodes, generate_ways, snapshot_dir, write_multi_block_test_pbf};

/// Which element kinds carry an interleaved pair of blobs, each pair one
/// overlap run.
#[derive(Clone, Copy, Debug)]
enum Overlaps {
    Nodes,
    Ways,
    Both,
}

impl Overlaps {
    fn runs(self) -> u64 {
        match self {
            Self::Nodes | Self::Ways => 1,
            Self::Both => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Route {
    /// An indexed input whose header claims `Sort.Type_then_ID`: pass 1
    /// reads headers only and pass 2 copies every blob through.
    DeclaredSorted,
    /// An input without a sorted claim, with overlap runs and passthrough
    /// blobs. Indexed, its passthrough blobs take the writer's copy path;
    /// unindexed (forced), they take the seek-and-read fallback.
    Unsorted { overlaps: Overlaps, indexed: bool },
}

impl Route {
    fn opens(self) -> u64 {
        match self {
            Self::DeclaredSorted => 3,
            Self::Unsorted { overlaps, .. } => 3 + overlaps.runs(),
        }
    }
}

/// Unsorted input: per kind with an overlap, two interleaved blobs (odd
/// IDs, then even IDs) followed by a disjoint blob that passes through;
/// a kind without one gets a single sorted blob. A relation blob closes
/// the file. The header makes no sorted claim.
fn write_unsorted(path: &Path, overlaps: Overlaps, indexed: bool) {
    let file = std::fs::File::create(path).expect("create input");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    let mut bb = BlockBuilder::new();
    let flush = |bb: &mut BlockBuilder, writer: &mut PbfWriter<_>| {
        let bytes = bb.take().expect("take").expect("non-empty block");
        if indexed {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    };
    let node_blobs: Vec<Vec<i64>> = if matches!(overlaps, Overlaps::Nodes | Overlaps::Both) {
        vec![vec![1, 3, 5, 7, 9], vec![2, 4, 6, 8, 10], vec![11, 12, 13]]
    } else {
        vec![(1..=13).collect()]
    };
    for ids in node_blobs {
        for id in ids {
            let coord = i32::try_from(id).expect("small id") * 1_000_000;
            bb.add_node(id, coord, coord, [("n", "x")], None);
        }
        flush(&mut bb, &mut writer);
    }
    let way_blobs: Vec<Vec<i64>> = if matches!(overlaps, Overlaps::Ways | Overlaps::Both) {
        vec![vec![101, 103, 105], vec![102, 104, 106], vec![107, 108]]
    } else {
        vec![(101..=108).collect()]
    };
    for ids in way_blobs {
        for id in ids {
            bb.add_way(id, [("highway", "path")], &[1, 2, 3], None);
        }
        flush(&mut bb, &mut writer);
    }
    bb.add_relation(
        300,
        [("type", "route")],
        &[MemberData {
            id: pbfhogg::MemberId::Way(101),
            role: "outer",
        }],
        None,
    );
    flush(&mut bb, &mut writer);
    writer.flush().expect("flush input");
}

fn write_input(route: Route, path: &Path) {
    match route {
        Route::DeclaredSorted => write_multi_block_test_pbf(
            path,
            &generate_nodes(30, 1),
            &generate_ways(6, 10_000, 3, 1),
            &[],
            5,
        ),
        Route::Unsorted { overlaps, indexed } => write_unsorted(path, overlaps, indexed),
    }
}

/// A valid PBF with different IDs and blob layout: a run that read any of
/// it would write different bytes, or fail.
fn write_replacement(path: &Path) {
    let nodes = generate_nodes(80, 501);
    let ways = generate_ways(4, 30_000, 3, 501);
    write_multi_block_test_pbf(path, &nodes, &ways, &[], 7);
}

fn run(
    route: Route,
    input: &Path,
    output: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let force = matches!(route, Route::Unsorted { indexed: false, .. });
    pbfhogg::commands::sort::sort(
        input,
        crate::common::out(output),
        &WriteOptions {
            force,
            ..WriteOptions::default()
        },
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// Run `route` with the replacement armed after each of its opens but the
/// last in turn, in both reopen modes.
fn check_route(route: Route) {
    let opens = route.opens();
    // After pass 2's open the overlap-run opens race on rayon workers, and
    // the collect stops at the first error, so where there are two runs
    // either one or both are refused. That arming point therefore does not
    // prove both refusal paths ran; the one-run layouts do, each alone.
    let racing = match route {
        Route::Unsorted { overlaps, .. } if overlaps.runs() > 1 => Some(3),
        _ => None,
    };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    write_input(route, &input);

    let reference = dir.path().join("reference.osm.pbf");
    run(route, &input, &reference, &crate::common::test_host())
        .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
    let expected = std::fs::read(&reference).expect("read reference output");

    for after in 1..opens {
        for force_path in [true, false] {
            write_input(route, &input);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(&input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.cancel
                .faults()
                .arm_input_replacement(&input, &replacement, after);
            let output = dir.path().join("out.osm.pbf");
            let before = snapshot_dir(dir.path());
            let result = run(route, &input, &output, &host);

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
                if racing == Some(after) {
                    assert!((1..=2).contains(&report.refused), "{cell}: {report:?}");
                } else {
                    assert_eq!(report.refused, 1, "{cell}: {report:?}");
                }
                assert_eq!(
                    report.attempts,
                    after + report.refused,
                    "{cell}: {report:?}"
                );
                assert_eq!(
                    report.attempts_after_fired, report.refused,
                    "{cell}: {report:?}"
                );
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
fn declared_sorted_reads_only_the_pinned_input() {
    check_route(Route::DeclaredSorted);
}

#[test]
fn node_overlap_run_reads_only_the_pinned_input() {
    for indexed in [true, false] {
        check_route(Route::Unsorted {
            overlaps: Overlaps::Nodes,
            indexed,
        });
    }
}

#[test]
fn way_overlap_run_reads_only_the_pinned_input() {
    for indexed in [true, false] {
        check_route(Route::Unsorted {
            overlaps: Overlaps::Ways,
            indexed,
        });
    }
}

#[test]
fn concurrent_overlap_runs_read_only_the_pinned_input() {
    for indexed in [true, false] {
        check_route(Route::Unsorted {
            overlaps: Overlaps::Both,
            indexed,
        });
    }
}
