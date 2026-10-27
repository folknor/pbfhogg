//! Fault-injection tests for `apply_changes::merge`, through the fault plan
//! on the run's token (see `pbfhogg::test_hooks`): worker panic recovery
//! (`apply_changes_panic_at_blob_seq`), a base renamed over mid-run
//! (`arm_input_replacement`), and the `--locations-on-ways` node-to-way
//! barrier under deterministic orderings: a node boundary held until after
//! the last node item, a worker panic while the scanner is in its barrier
//! wait or blocked dispatching, and a cancellation during the barrier wait,
//! on both output routes; and the missing-node refusal taken at that
//! barrier, at each of those orderings, with the scanner stopped short of
//! end of input, and ranked below a failure or cancellation landing at the
//! decision. A drain-side output failure releasing blocked
//! producers is `fault_parallel_writer.rs`'s.
//!
//! Companion: `fault_parallel_writer.rs` covers apply-changes'
//! default output backend.

#![allow(clippy::unwrap_used)]

#[cfg(feature = "test-hooks")]
mod apply_changes {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use flate2::write::GzEncoder;
    use pbfhogg::apply_changes::{MergeOptions, merge};
    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::write::WriteOptions;
    use pbfhogg::writer::{Compression, PbfWriter};

    use crate::common::{self, TestNode, generate_nodes, write_multi_block_test_pbf};

    fn write_osc(path: &Path, xml: &str) {
        let file = File::create(path).expect("create osc file");
        let mut enc = GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).expect("write xml");
        enc.finish().expect("finish gz");
    }

    /// Node-only sorted base whose header claims LocationsOnWays, one blob
    /// per `block_size` nodes, written like `write_multi_block_test_pbf`.
    fn write_multi_block_locations_pbf(path: &Path, nodes: &[TestNode], block_size: usize) {
        let file = File::create(path).expect("create base");
        let mut writer = PbfWriter::new(file, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .optional_feature("LocationsOnWays")
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();
        for chunk in nodes.chunks(block_size) {
            for n in chunk {
                bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
            }
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(bytes).expect("write block");
            }
        }
        writer.flush().expect("flush base");
    }

    /// A worker panic must return its original message as an error, release
    /// the node barrier when enabled, and leave no scratch behind.
    #[test]
    fn fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean() {
        check_worker_panic(false);
        check_worker_panic(true);
    }

    fn check_worker_panic(locations_on_ways: bool) {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");

        // Multi-blob base: 40 nodes split across blocks of 5 -> 8 node blobs.
        // Enough blobs that the scanner dispatches several candidates before
        // the injected panic fires.
        let nodes = generate_nodes(40, 1);
        if locations_on_ways {
            // `merge --locations-on-ways` refuses a base whose header lacks
            // LocationsOnWays before any worker runs, so the panic path
            // needs a base that claims it.
            write_multi_block_locations_pbf(&base, &nodes, 5);
        } else {
            write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);
        }

        // OSC that creates node candidates overlapping every base blob.
        // The scanner will route every node blob to the worker pool.
        write_osc(
            &osc,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="0.0000001" lon="0.0000001" version="2"/>
    <node id="10" lat="0.0000010" lon="0.0000010" version="2"/>
    <node id="20" lat="0.0000020" lon="0.0000020" version="2"/>
    <node id="30" lat="0.0000030" lon="0.0000030" version="2"/>
  </modify>
</osmChange>"#,
        );

        // Snapshot the tempdir before the run. The three input files are
        // already present; the output file is not. After the panic we
        // compare and assert nothing beyond those + the output file
        // changed.
        let before = common::snapshot_dir(dir.path());

        // Arm the hook (below, on the run's token) at blob seq 3 (an
        // arbitrary early-middle blob that
        // corresponds to a Candidate dispatched to the worker pool).
        //
        // Two workers, so the panic lands while another worker is alive;
        // the barrier tests below cover one worker. FailureSlot publication
        // and drain shutdown handle the panic. `force` is refused alongside
        // `locations_on_ways` (it can never take effect there).
        let opts = MergeOptions {
            write: WriteOptions {
                force: !locations_on_ways,
                ..WriteOptions::default()
            },
            locations_on_ways,
            ignore_missing_nodes: false,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
            allow_version_regression: false,
        };
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .apply_changes_panic_at_blob_seq
            .store(3, std::sync::atomic::Ordering::Relaxed);

        // Catch only as a guard: the public API must return an error,
        // including when a missing node seq would otherwise hang the barrier.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            merge(
                &base,
                &osc,
                common::out(&output),
                &opts,
                &pbfhogg::HeaderOverrides::default(),
                &host,
            )
        }));
        let err = match result {
            Ok(Err(e)) => e.to_string(),
            Ok(Ok(_)) => panic!("merge must fail when the worker panic hook fires"),
            Err(_) => panic!("merge must return a worker panic as an error, not unwind"),
        };
        assert!(
            err.contains("streaming worker panicked")
                && err.contains("panic_at_blob_seq=3 triggered"),
            "merge must preserve the worker panic payload, got: {err}"
        );

        // A forced mid-run failure leaves nothing behind: no output file
        // under the name the caller chose, no staging file beside it, no
        // scratch.
        let after = common::snapshot_dir(dir.path());
        common::assert_scratch_unchanged(&before, &after);
        assert!(
            !output.exists(),
            "a worker panic mid-stream must leave no output file at the destination"
        );
    }

    // ---------------------------------------------------------------------
    // The --locations-on-ways node-to-way barrier
    // ---------------------------------------------------------------------

    /// The decimicro coordinate the fixtures give node `id`.
    fn coord(id: i64) -> (i32, i32) {
        let id = i32::try_from(id).unwrap();
        (id * 1000, id * 2000)
    }

    /// A sorted LocationsOnWays base: nodes `node_ids` (`node_block` per
    /// blob, at [`coord`]), then one way blob per way (refs carrying their
    /// nodes' locations), then one relation blob per relation (a single way
    /// member). Any part may be empty.
    fn write_low_base(
        path: &Path,
        node_ids: &[i64],
        node_block: usize,
        ways: &[(i64, Vec<i64>)],
        relations: &[(i64, i64)],
    ) {
        let file = File::create(path).expect("create base");
        let mut writer = PbfWriter::new(file, Compression::default());
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .optional_feature("LocationsOnWays")
            .build()
            .expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();
        let flush = |bb: &mut BlockBuilder, writer: &mut PbfWriter<File>| {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(bytes).expect("write block");
            }
        };
        for chunk in node_ids.chunks(node_block.max(1)) {
            for &id in chunk {
                let (lat, lon) = coord(id);
                bb.add_node(id, lat, lon, std::iter::empty(), None);
            }
            flush(&mut bb, &mut writer);
        }
        for (id, refs) in ways {
            let locations: Vec<(i32, i32)> = refs.iter().map(|&r| coord(r)).collect();
            bb.add_way_with_locations(*id, [("highway", "road")], refs, &locations, None);
            flush(&mut bb, &mut writer);
        }
        for &(id, way) in relations {
            let members = [block_builder::MemberData {
                id: pbfhogg::MemberId::Way(way),
                role: "outer",
            }];
            bb.add_relation(id, [("type", "multipolygon")], &members, None);
            flush(&mut bb, &mut writer);
        }
        writer.flush().expect("flush base");
    }

    /// An OSC body (inside `<osmChange>`) as a gzipped file.
    fn write_osc_body(path: &Path, body: &str) {
        write_osc(
            path,
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <osmChange version=\"0.6\">{body}</osmChange>"
            ),
        );
    }

    /// A node element at [`coord`] (degrees, as OSC writes them).
    fn osc_node(id: i64, version: u32) -> String {
        let (lat, lon) = coord(id);
        format!(
            "<node id=\"{id}\" lat=\"{:.7}\" lon=\"{:.7}\" version=\"{version}\"/>",
            f64::from(lat) / 1e7,
            f64::from(lon) / 1e7
        )
    }

    fn osc_way(id: i64, version: u32, refs: &[i64]) -> String {
        let nds: String = refs.iter().map(|r| format!("<nd ref=\"{r}\"/>")).collect();
        format!("<way id=\"{id}\" version=\"{version}\">{nds}<tag k=\"highway\" v=\"road\"/></way>")
    }

    /// Every way's id and decoded locations, in file order.
    fn way_locations(path: &Path) -> Vec<(i64, Vec<(i32, i32)>)> {
        let mut ways = Vec::new();
        pbfhogg::ElementReader::from_path(path)
            .expect("open output")
            .for_each(|element| {
                if let pbfhogg::Element::Way(w) = element {
                    let locs = w
                        .node_locations()
                        .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
                        .collect();
                    ways.push((w.id(), locs));
                }
            })
            .expect("read output");
        ways
    }

    /// The barrier shapes the tests run, each a base and an OSC whose way
    /// edits need coordinates from both the base and the OSC.
    struct Shape {
        name: &'static str,
        write: fn(&Path, &Path),
        /// Every way the output must hold, with the nodes its locations
        /// must resolve to (each at [`coord`]).
        ways: &'static [(i64, &'static [i64])],
    }

    const SHAPES: &[Shape] = &[
        // Nodes, ways and a relation. Node 1 moves in the OSC; way 101 is
        // rewritten onto it and an untouched base node; way 200 is created
        // on base nodes.
        Shape {
            name: "ordinary",
            write: |base, osc| {
                write_low_base(
                    base,
                    &(1..=12).collect::<Vec<_>>(),
                    2,
                    &[
                        (100, vec![1, 2]),
                        (101, vec![3, 4]),
                        (102, vec![5, 6]),
                        (103, vec![7, 8]),
                    ],
                    &[(500, 100)],
                );
                write_osc_body(
                    osc,
                    &format!(
                        "<modify>{}{}</modify><create>{}</create>",
                        osc_node(1, 2),
                        osc_way(101, 2, &[1, 5]),
                        osc_way(200, 1, &[6, 7]),
                    ),
                );
            },
            ways: &[
                (100, &[1, 2]),
                (101, &[1, 5]),
                (102, &[5, 6]),
                (103, &[7, 8]),
                (200, &[6, 7]),
            ],
        },
        // No base blobs at all: every coordinate comes from the OSC.
        Shape {
            name: "empty",
            write: |base, osc| {
                write_low_base(base, &[], 1, &[], &[]);
                write_osc_body(
                    osc,
                    &format!(
                        "<create>{}{}{}</create>",
                        osc_node(1, 1),
                        osc_node(2, 1),
                        osc_way(10, 1, &[1, 2]),
                    ),
                );
            },
            ways: &[(10, &[1, 2])],
        },
        // Nodes only: the boundary is sent at end of input.
        Shape {
            name: "node-only",
            write: |base, osc| {
                write_low_base(base, &(1..=6).collect::<Vec<_>>(), 2, &[], &[]);
                write_osc_body(
                    osc,
                    &format!("<create>{}</create>", osc_way(10, 1, &[2, 5])),
                );
            },
            ways: &[(10, &[2, 5])],
        },
        // Ways only: the boundary is the empty one, sent at the first way.
        Shape {
            name: "way-only",
            write: |base, osc| {
                write_low_base(base, &[], 1, &[(100, vec![1, 2]), (101, vec![3, 4])], &[]);
                write_osc_body(
                    osc,
                    &format!(
                        "<create>{}{}</create><modify>{}</modify>",
                        osc_node(1, 1),
                        osc_node(3, 1),
                        osc_way(101, 2, &[1, 3]),
                    ),
                );
            },
            ways: &[(100, &[1, 2]), (101, &[1, 3])],
        },
    ];

    fn low_options(jobs: usize) -> MergeOptions {
        MergeOptions {
            locations_on_ways: true,
            parallelism: pbfhogg::Parallelism::from_jobs(jobs),
            ..MergeOptions::default()
        }
    }

    /// Run a `--locations-on-ways` merge of `base` and `osc` into `output`
    /// under a fresh token, armed by `arm` (owned route when `owned`).
    fn run_low(
        base: &Path,
        osc: &Path,
        output: &Path,
        jobs: usize,
        owned: bool,
        arm: impl FnOnce(&pbfhogg::test_hooks::FaultPlan),
    ) -> (pbfhogg::Result<()>, pbfhogg::host::Host) {
        let host = crate::common::test_host();
        let faults = host.invocation.faults();
        faults
            .apply_changes_force_owned_route
            .store(owned, std::sync::atomic::Ordering::SeqCst);
        arm(faults);
        let result = merge(
            base,
            osc,
            common::out(output),
            &low_options(jobs),
            &pbfhogg::HeaderOverrides::default(),
            &host,
        )
        .map(|_| ());
        (result, host)
    }

    /// Worker counts the barrier is exercised at.
    const JOBS: &[usize] = &[1, 2, 3];

    /// Every barrier shape, at every worker count, on both routes, with the
    /// node boundary on time and held until after the last node item: the
    /// output is the same bytes every time, and every way carries its
    /// nodes' coordinates. The held runs prove their case ran (the drain saw
    /// a boundary after it had processed every node item), which is the
    /// ordering the drain once needed a 25 ms idle re-poll to survive.
    #[test]
    fn barrier_shapes_publish_the_same_output_however_the_boundary_lands() {
        use std::sync::atomic::Ordering::SeqCst;

        for shape in SHAPES {
            let dir = crate::common::test_scratch();
            let base = dir.path().join("base.osm.pbf");
            let osc = dir.path().join("diff.osc.gz");
            (shape.write)(&base, &osc);

            let reference = dir.path().join("reference.osm.pbf");
            let (result, _) = run_low(&base, &osc, &reference, 2, false, |_| {});
            result.unwrap_or_else(|e| panic!("{}: reference run: {e}", shape.name));
            let expected_ways: Vec<(i64, Vec<(i32, i32)>)> = shape
                .ways
                .iter()
                .map(|(id, refs)| (*id, refs.iter().map(|&r| coord(r)).collect()))
                .collect();
            assert_eq!(
                way_locations(&reference),
                expected_ways,
                "{}: every way must carry its nodes' coordinates",
                shape.name
            );
            let expected = std::fs::read(&reference).expect("read reference");

            for &jobs in JOBS {
                for owned in [false, true] {
                    for late in [false, true] {
                        let label = format!("{} jobs={jobs} owned={owned} late={late}", shape.name);
                        let output = dir.path().join("output.osm.pbf");
                        let (result, host) = run_low(&base, &osc, &output, jobs, owned, |f| {
                            f.apply_changes_late_boundary.store(late, SeqCst);
                        });
                        result.unwrap_or_else(|e| panic!("{label}: {e}"));
                        let faults = host.invocation.faults();
                        assert!(
                            !faults.apply_changes_hook_timed_out.load(SeqCst),
                            "{label}: a hook wait expired"
                        );
                        if late {
                            assert_eq!(
                                faults.apply_changes_late_boundaries_seen.load(SeqCst),
                                1,
                                "{label}: the boundary must arrive after the last node item"
                            );
                        }
                        assert_eq!(
                            std::fs::read(&output).expect("read output"),
                            expected,
                            "{label}: output differs from the reference"
                        );
                        std::fs::remove_file(&output).expect("remove output");
                    }
                }
            }
        }
    }

    /// A node the OSC moves without touching the ways using it is refreshed
    /// in every way, on both output routes and at every worker count, with
    /// the same bytes each time: way 102's blob is rewritten only for that,
    /// and ways 100 and 103's blobs are searched and passed through (from
    /// the frame the worker read, on the owned route).
    #[test]
    fn untouched_ways_are_refreshed_the_same_on_both_routes() {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        write_low_base(
            &base,
            &(1..=12).collect::<Vec<_>>(),
            2,
            &[
                (100, vec![1, 2]),
                (101, vec![3, 4]),
                (102, vec![5, 6, 5]),
                (103, vec![7, 8]),
            ],
            &[(500, 100)],
        );
        write_osc_body(
            &osc,
            &format!(
                "<modify><node id=\"5\" lat=\"1.2345678\" lon=\"-2.3456789\" \
                 version=\"2\"/>{}</modify>",
                osc_way(101, 2, &[1, 5]),
            ),
        );
        let moved = (12_345_678, -23_456_789);
        let expected = vec![
            (100, vec![coord(1), coord(2)]),
            (101, vec![coord(1), moved]),
            (102, vec![moved, coord(6), moved]),
            (103, vec![coord(7), coord(8)]),
        ];

        let reference = dir.path().join("reference.osm.pbf");
        let (result, _) = run_low(&base, &osc, &reference, 2, false, |_| {});
        result.expect("reference run");
        assert_eq!(way_locations(&reference), expected);
        let reference = std::fs::read(&reference).expect("read reference");

        // The base's frames: header, six node blobs, then one blob per way
        // (100, 101, 102, 103) and the relation blob.
        let base_bytes = std::fs::read(&base).expect("read base");
        let mut offsets: Vec<usize> = pbfhogg::BlobReader::from_path(&base)
            .expect("open base")
            .map(|blob| usize::try_from(blob.expect("blob").offset().expect("offset").0).unwrap())
            .collect();
        offsets.push(base_bytes.len());
        let frame = |i: usize| &base_bytes[offsets[i]..offsets[i + 1]];
        let holds = |out: &[u8], f: &[u8]| out.windows(f.len()).any(|w| w == f);

        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
                let output = dir.path().join("output.osm.pbf");
                let (result, _) = run_low(&base, &osc, &output, jobs, owned, |_| {});
                result.unwrap_or_else(|e| panic!("{label}: {e}"));
                let bytes = std::fs::read(&output).expect("read output");
                // Ways 100 and 103 were searched and passed through as read;
                // 101 (edited) and 102 (stale) were rewritten.
                assert!(holds(&bytes, frame(7)), "{label}: way 100's blob rewritten");
                assert!(
                    holds(&bytes, frame(10)),
                    "{label}: way 103's blob rewritten"
                );
                assert!(!holds(&bytes, frame(8)), "{label}: way 101's blob copied");
                assert!(!holds(&bytes, frame(9)), "{label}: way 102's blob copied");
                assert_eq!(
                    bytes, reference,
                    "{label}: output differs from the reference"
                );
                std::fs::remove_file(&output).expect("remove output");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Route parity of the merge statistics
    // ---------------------------------------------------------------------

    /// Every `MergeStats` figure a route could move, by name.
    fn stats_figures(s: &pbfhogg::apply_changes::MergeStats) -> Vec<(&'static str, u64)> {
        vec![
            ("base_nodes", s.base_nodes),
            ("base_ways", s.base_ways),
            ("base_relations", s.base_relations),
            ("diff_nodes", s.diff_nodes),
            ("diff_ways", s.diff_ways),
            ("diff_relations", s.diff_relations),
            ("deleted", s.deleted),
            ("blobs_passthrough", s.blobs_passthrough),
            ("blobs_rewritten", s.blobs_rewritten),
            ("blobs_index_hit", s.blobs_index_hit),
            ("bytes_passthrough", s.bytes_passthrough),
            ("bytes_rewritten", s.bytes_rewritten),
            ("loc_nodes_from_base", s.loc_nodes_from_base),
            ("loc_missing", s.loc_missing),
            ("loc_node_blobs_scanned", s.loc_node_blobs_scanned),
            ("loc_refresh_blobs_scanned", s.loc_refresh_blobs_scanned),
            ("loc_refresh_only_blobs", s.loc_refresh_only_blobs),
            ("loc_refreshed_ways", s.loc_refreshed_ways),
            ("loc_refreshed_refs", s.loc_refreshed_refs),
        ]
    }

    /// Merge `base` and `osc` on the splice route and on the owned route
    /// (what `--direct-io` takes), assert the two outputs are the same bytes
    /// and every statistic agrees, and return the splice run's figures.
    ///
    /// `workers` is what each route's worker pool must report, splice first:
    /// `[preads, blobs processed, precise-check false positives, owned
    /// passthroughs]`. One pread and one processed count per blob a worker
    /// handles is what pins the owned route's false positive to a single
    /// read: it once read its frame a second time and counted itself twice.
    fn merge_on_both_routes(
        base: &Path,
        osc: &Path,
        opts: &MergeOptions,
        label: &str,
        workers: [[u64; 4]; 2],
    ) -> Vec<(&'static str, u64)> {
        const WORKER_COUNTERS: [&str; 4] = [
            "merge_streaming_blob_reads",
            "merge_streaming_blobs_processed",
            "merge_streaming_blobs_false_positive",
            "merge_streaming_blobs_owned_passthrough",
        ];
        let dir = crate::common::test_scratch();
        let mut runs = Vec::new();
        for (owned, expected) in [false, true].into_iter().zip(workers) {
            let output = dir.path().join(format!("owned-{owned}.osm.pbf"));
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .apply_changes_force_owned_route
                .store(owned, std::sync::atomic::Ordering::SeqCst);
            let stats = merge(
                base,
                osc,
                common::out(&output),
                opts,
                &pbfhogg::HeaderOverrides::default(),
                &host,
            )
            .unwrap_or_else(|e| panic!("{label} owned={owned}: {e}"));
            let got = WORKER_COUNTERS.map(|name| {
                host.invocation
                    .faults()
                    .apply_changes_worker_counter(name)
                    .unwrap_or_else(|| panic!("{label} owned={owned}: no counter {name}"))
            });
            assert_eq!(
                got, expected,
                "{label} owned={owned}: worker {WORKER_COUNTERS:?}"
            );
            runs.push((
                stats_figures(&stats),
                std::fs::read(&output).expect("read output"),
            ));
        }
        let (owned_figures, owned_bytes) = runs.pop().expect("owned run");
        let (splice_figures, splice_bytes) = runs.pop().expect("splice run");
        assert_eq!(
            owned_bytes, splice_bytes,
            "{label}: the owned route's output differs from the splice route's"
        );
        assert_eq!(
            owned_figures, splice_figures,
            "{label}: the owned route's statistics differ from the splice route's"
        );
        splice_figures
    }

    fn figure(figures: &[(&'static str, u64)], name: &str) -> u64 {
        figures
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("no figure {name}"))
    }

    /// The statistics of a merge do not depend on the output route. Every
    /// way a blob can pass through is exercised: the scanner passing it on
    /// its indexdata, a worker finding a precise-check false positive (a
    /// delete of a node in a gap of the blob's range, and under
    /// `--locations-on-ways` a node blob decompressed only for its
    /// coordinates), and a refresh scan finding a way blob clean. On the
    /// owned route each of them is the input's own frame, so it is an index
    /// hit as its splice is; `blobs_index_hit` used to read 0 there. An
    /// unindexed `--force` base, whose untouched blobs are reframed with
    /// derived sidecars, has no index hit on either route.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn merge_statistics_are_the_same_on_both_routes() {
        use crate::common::{TestMember, TestRelation, TestWay};

        let dir = crate::common::test_scratch();
        let osc = dir.path().join("diff.osc.gz");

        // Plain merge: five node blobs of odd IDs 1..=39, two way blobs and
        // one relation blob. Node 3 is modified (its blob is rewritten) and
        // node 10, inside the second blob's range but absent, is deleted (a
        // false positive). The rest pass through on their indexdata.
        let nodes: Vec<TestNode> = generate_nodes(40, 1)
            .into_iter()
            .filter(|n| n.id % 2 == 1)
            .collect();
        let ways: Vec<TestWay> = (0..8)
            .map(|i| TestWay {
                id: 100 + i,
                refs: vec![2 * i + 1, 2 * i + 3],
                tags: vec![("highway", "road")],
                meta: None,
            })
            .collect();
        let relations = vec![
            TestRelation {
                id: 500,
                members: vec![TestMember {
                    id: pbfhogg::MemberId::Way(100),
                    role: "outer",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
            TestRelation {
                id: 501,
                members: vec![TestMember {
                    id: pbfhogg::MemberId::Way(101),
                    role: "outer",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
        ];
        write_osc_body(
            &osc,
            "<modify><node id=\"3\" lat=\"1.5\" lon=\"2.5\" version=\"2\"/></modify>\
             <delete><node id=\"10\" version=\"2\"/></delete>",
        );
        let plain = MergeOptions {
            parallelism: pbfhogg::Parallelism::from_jobs(2),
            ..MergeOptions::default()
        };

        let indexed = dir.path().join("indexed.osm.pbf");
        write_multi_block_test_pbf(&indexed, &nodes, &ways, &relations, 4);
        // Splice: workers see the two candidates. Owned: also the six
        // scanner passthroughs, and the false positive leaves as the frame
        // it read, so eight blobs cost eight reads.
        let figures = merge_on_both_routes(
            &indexed,
            &osc,
            &plain,
            "indexed",
            [[2, 2, 1, 0], [8, 8, 1, 7]],
        );
        assert_eq!(figure(&figures, "blobs_rewritten"), 1, "{figures:?}");
        assert_eq!(figure(&figures, "blobs_passthrough"), 7, "{figures:?}");
        assert_eq!(
            figure(&figures, "blobs_index_hit"),
            7,
            "every blob of an indexed base that passes through is an index hit: {figures:?}"
        );

        let unindexed = dir.path().join("unindexed.osm.pbf");
        crate::common::write_multi_block_non_indexed_pbf(&unindexed, &nodes, &ways, &relations, 4);
        let forced = MergeOptions {
            write: WriteOptions {
                force: true,
                ..WriteOptions::default()
            },
            ..plain
        };
        // Every unindexed blob is a candidate on both routes; the reframed
        // false positives are not owned passthroughs.
        let figures = merge_on_both_routes(
            &unindexed,
            &osc,
            &forced,
            "unindexed",
            [[8, 8, 7, 0], [8, 8, 7, 0]],
        );
        assert_eq!(figure(&figures, "blobs_rewritten"), 1, "{figures:?}");
        assert_eq!(figure(&figures, "blobs_passthrough"), 7, "{figures:?}");
        assert_eq!(
            figure(&figures, "blobs_index_hit"),
            0,
            "a reframed unindexed blob is not an index hit: {figures:?}"
        );

        // --locations-on-ways: node blobs [1, 3], [5, 7], [9, 11], one blob
        // per way, a relation blob. Node 1 moves, so way 100's blob is
        // rewritten only for that; way 101 is modified; way 102's blob misses
        // the way diff and holds no stale way, so a refresh scan passes it
        // through. Node 6 is deleted inside the second node blob's range (a
        // false positive), and the third node blob is decompressed only for
        // its coordinates. Four blobs pass through.
        let low = dir.path().join("low.osm.pbf");
        write_low_base(
            &low,
            &[1, 3, 5, 7, 9, 11],
            2,
            &[(100, vec![1, 3]), (101, vec![5, 7]), (102, vec![9, 11])],
            &[(500, 100)],
        );
        let low_osc = dir.path().join("low.osc.gz");
        write_osc_body(
            &low_osc,
            &format!(
                "<modify><node id=\"1\" lat=\"1.5\" lon=\"2.5\" version=\"2\"/>{}</modify>\
                 <delete><node id=\"6\" version=\"2\"/></delete>",
                osc_way(101, 2, &[5, 7]),
            ),
        );
        // Splice: the three node blobs and three way blobs are candidates,
        // two node blobs false positives (the refresh-clean way blob is not
        // counted as one). Owned: also the relation blob, and four blobs
        // leave as the frames read.
        let figures = merge_on_both_routes(
            &low,
            &low_osc,
            &low_options(2),
            "locations",
            [[6, 6, 2, 0], [7, 7, 2, 4]],
        );
        assert_eq!(figure(&figures, "blobs_rewritten"), 3, "{figures:?}");
        assert_eq!(figure(&figures, "blobs_passthrough"), 4, "{figures:?}");
        assert_eq!(figure(&figures, "blobs_index_hit"), 4, "{figures:?}");
        assert_eq!(figure(&figures, "loc_refresh_only_blobs"), 1, "{figures:?}");
        assert_eq!(
            figure(&figures, "loc_refresh_blobs_scanned"),
            2,
            "ways 100 and 102 are scanned: {figures:?}"
        );
    }

    /// A way blob outside every OSC range used to be spliced unread. Under a
    /// refresh it is searched, and a malformed coordinate array in it (one
    /// latitude for two refs) fails the run on both routes: nothing is
    /// published and no scratch is left behind.
    #[test]
    fn a_malformed_way_in_an_untouched_blob_fails_the_run_cleanly() {
        fn varint(mut v: u64, out: &mut Vec<u8>) {
            loop {
                #[allow(clippy::cast_possible_truncation)]
                let byte = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(byte);
                    break;
                }
                out.push(byte | 0x80);
            }
        }
        fn len_field(field: u32, payload: &[u8], out: &mut Vec<u8>) {
            varint(u64::from((field << 3) | 2), out);
            varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }
        // Way 300: refs 1, 2 (zigzag deltas 2, 2), one latitude, two
        // longitudes.
        let mut way = vec![0x08];
        varint(300, &mut way);
        len_field(8, &[2, 2], &mut way);
        len_field(9, &[2], &mut way);
        len_field(10, &[2, 2], &mut way);
        let mut group = Vec::new();
        len_field(3, &way, &mut group);
        let mut way_block = Vec::new();
        len_field(1, &[0x0A, 0x00], &mut way_block);
        len_field(2, &group, &mut way_block);

        for owned in [false, true] {
            let dir = crate::common::test_scratch();
            let base = dir.path().join("base.osm.pbf");
            let osc = dir.path().join("diff.osc.gz");
            let output = dir.path().join("output.osm.pbf");
            let mut writer = PbfWriter::new(
                File::create(&base).expect("create base"),
                Compression::default(),
            );
            let header = block_builder::HeaderBuilder::new()
                .sorted()
                .optional_feature("LocationsOnWays")
                .build()
                .expect("build header");
            writer.write_header(&header).expect("write header");
            let mut bb = BlockBuilder::new();
            for id in 1..=4 {
                let (lat, lon) = coord(id);
                bb.add_node(id, lat, lon, std::iter::empty(), None);
            }
            writer
                .write_primitive_block(bb.take().expect("take").expect("node block"))
                .expect("node blob");
            writer.write_primitive_block(&way_block).expect("way blob");
            writer.flush().expect("flush base");
            write_osc_body(
                &osc,
                "<modify><node id=\"1\" lat=\"5.0\" lon=\"6.0\" version=\"2\"/></modify>",
            );

            let before = common::snapshot_dir(dir.path());
            let (result, _) = run_low(&base, &osc, &output, 2, owned, |_| {});
            let err = result
                .err()
                .unwrap_or_else(|| panic!("owned={owned}: must fail"));
            assert!(
                err.to_string().contains("node-location array"),
                "owned={owned}: {err}"
            );
            assert!(
                !output.exists(),
                "owned={owned}: a failed run must publish nothing"
            );
            common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
        }
    }

    /// Run the ordinary shape with `nodes` single-node blobs and way 101
    /// modified onto `refs`, armed by `arm`, and require it to fail with
    /// `expect` in its message, publish nothing and leave no scratch.
    fn assert_barrier_failure(
        label: &str,
        nodes: i64,
        refs: &[i64],
        jobs: usize,
        owned: bool,
        arm: impl FnOnce(&pbfhogg::test_hooks::FaultPlan),
        expect: &str,
    ) -> pbfhogg::host::Host {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");
        write_low_base(
            &base,
            &(1..=nodes).collect::<Vec<_>>(),
            1,
            &[(100, vec![1, 2]), (101, vec![3, 4])],
            &[],
        );
        write_osc_body(&osc, &format!("<modify>{}</modify>", osc_way(101, 2, refs)));
        let before = common::snapshot_dir(dir.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_low(&base, &osc, &output, jobs, owned, arm)
        }));
        let (result, host) = result.unwrap_or_else(|_| panic!("{label}: merge unwound"));
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{label}: merge must fail"));
        assert!(
            err.to_string().contains(expect),
            "{label}: expected {expect:?}, got: {err}"
        );
        assert!(
            !output.exists(),
            "{label}: a failed run must publish nothing"
        );
        common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
        assert!(
            !host
                .invocation
                .faults()
                .apply_changes_hook_timed_out
                .load(std::sync::atomic::Ordering::SeqCst),
            "{label}: a hook wait expired, so the failure did not land where intended"
        );
        host
    }

    /// A worker that panics while the scanner is parked in its barrier wait
    /// (the node phase can never complete, since that worker's item never
    /// arrives) fails the run with the panic's own message instead of
    /// hanging scanner, drain and the other workers. With a missing node in
    /// the change too, the panic still wins: the barrier where the refusal
    /// would be taken is never reached.
    #[test]
    fn a_worker_panic_during_the_barrier_wait_fails_the_run() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                for refs in [&[1, 4][..], &[1, 999]] {
                    let label = format!("jobs={jobs} owned={owned} refs={refs:?}");
                    let host = assert_barrier_failure(
                        &label,
                        8,
                        refs,
                        jobs,
                        owned,
                        |f| {
                            f.apply_changes_panic_at_blob_seq.store(1, SeqCst);
                            f.apply_changes_panic_waits_for_barrier_wait
                                .store(true, SeqCst);
                        },
                        "panic_at_blob_seq=1 triggered",
                    );
                    assert!(
                        host.invocation
                            .faults()
                            .apply_changes_scanner_in_barrier_wait
                            .load(SeqCst),
                        "{label}: the scanner must have reached its barrier wait"
                    );
                }
            }
        }
    }

    /// A worker that panics while the scanner is blocked sending a candidate
    /// on the full candidate channel (more node blobs than the channel and
    /// the parked workers hold) fails the run instead of leaving the
    /// scanner blocked.
    #[test]
    fn a_worker_panic_while_the_scanner_is_blocked_dispatching_fails_the_run() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
                let host = assert_barrier_failure(
                    &label,
                    120,
                    &[1, 4],
                    jobs,
                    owned,
                    |f| {
                        f.apply_changes_panic_at_blob_seq.store(0, SeqCst);
                        f.apply_changes_hold_workers_until_dispatch_blocked
                            .store(true, SeqCst);
                    },
                    "panic_at_blob_seq=0 triggered",
                );
                assert!(
                    host.invocation
                        .faults()
                        .apply_changes_dispatch_blocked
                        .load(SeqCst),
                    "{label}: the scanner must have blocked on a full candidate channel"
                );
            }
        }
    }

    /// Cancelling while the scanner waits for the barrier (one worker parked
    /// on a node blob, so the barrier cannot fire) returns `Cancelled`,
    /// publishes nothing and joins every thread, a change with a missing
    /// node included (the refusal is never reached).
    #[test]
    fn cancelling_during_the_barrier_wait_stops_the_run() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                for refs in [&[2, 3][..], &[2, 999]] {
                    let label = format!("jobs={jobs} owned={owned} refs={refs:?}");
                    let dir = crate::common::test_scratch();
                    let base = dir.path().join("base.osm.pbf");
                    let osc = dir.path().join("diff.osc.gz");
                    let output = dir.path().join("output.osm.pbf");
                    write_low_base(
                        &base,
                        &(1..=8).collect::<Vec<_>>(),
                        1,
                        &[(100, vec![1, 2])],
                        &[],
                    );
                    write_osc_body(&osc, &format!("<modify>{}</modify>", osc_way(100, 2, refs)));
                    let host = crate::common::test_host();
                    let faults = host.invocation.faults();
                    faults.apply_changes_force_owned_route.store(owned, SeqCst);
                    faults.apply_changes_park_at_blob_seq.store(1, SeqCst);
                    let before = common::snapshot_dir(dir.path());
                    let result = std::thread::scope(|scope| {
                        let run = scope.spawn(|| {
                            merge(
                                &base,
                                &osc,
                                common::out(&output),
                                &low_options(jobs),
                                &pbfhogg::HeaderOverrides::default(),
                                &host,
                            )
                        });
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        while !faults.apply_changes_scanner_in_barrier_wait.load(SeqCst) {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "{label}: the scanner never reached its barrier wait"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        host.invocation.cancel();
                        run.join().expect("merge must not unwind")
                    });
                    let err = result
                        .err()
                        .unwrap_or_else(|| panic!("{label}: merge must fail"));
                    assert!(
                        matches!(err.kind(), pbfhogg::ErrorKind::Cancelled),
                        "{label}: expected Cancelled, got: {err:?}"
                    );
                    assert!(
                        !output.exists(),
                        "{label}: a cancelled run must publish nothing"
                    );
                    common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
                    assert!(
                        !faults.apply_changes_hook_timed_out.load(SeqCst),
                        "{label}: the parked worker must see the cancellation"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // The missing-node refusal at the barrier
    // ---------------------------------------------------------------------

    /// A change whose ways reference nodes neither file supplies, with the
    /// distinct count the refusal must report and the number of node blobs
    /// in its base (the only blobs a refusal at the barrier lets a worker
    /// see).
    struct RefusalShape {
        name: &'static str,
        write: fn(&Path, &Path),
        missing: u64,
        node_blobs: u64,
    }

    const REFUSAL_SHAPES: &[RefusalShape] = &[
        // Nodes, ways and a relation. Two missing IDs, 999 used twice:
        // the count is distinct nodes, not references.
        RefusalShape {
            name: "ordinary",
            write: |base, osc| {
                write_low_base(
                    base,
                    &(1..=12).collect::<Vec<_>>(),
                    2,
                    &[
                        (100, vec![1, 2]),
                        (101, vec![3, 4]),
                        (102, vec![5, 6]),
                        (103, vec![7, 8]),
                    ],
                    &[(500, 100)],
                );
                write_osc_body(
                    osc,
                    &format!(
                        "<modify>{}{}</modify><create>{}</create>",
                        osc_node(1, 2),
                        osc_way(101, 2, &[1, 999, 5, 999]),
                        osc_way(200, 1, &[6, 998, 7]),
                    ),
                );
            },
            missing: 2,
            node_blobs: 6,
        },
        // A node the change deletes, though the base holds it.
        RefusalShape {
            name: "deleted",
            write: |base, osc| {
                write_low_base(
                    base,
                    &(1..=6).collect::<Vec<_>>(),
                    2,
                    &[(100, vec![1, 2]), (101, vec![3, 4])],
                    &[],
                );
                write_osc_body(
                    osc,
                    &format!(
                        "<delete><node id=\"5\" version=\"2\"/></delete><modify>{}</modify>",
                        osc_way(101, 2, &[3, 5]),
                    ),
                );
            },
            missing: 1,
            node_blobs: 3,
        },
        // No base blobs: the boundary is the empty one, at end of input.
        RefusalShape {
            name: "empty",
            write: |base, osc| {
                write_low_base(base, &[], 1, &[], &[]);
                write_osc_body(
                    osc,
                    &format!(
                        "<create>{}{}</create>",
                        osc_node(1, 1),
                        osc_way(10, 1, &[1, 2, 2]),
                    ),
                );
            },
            missing: 1,
            node_blobs: 0,
        },
        // Nodes only: the boundary is sent at end of input and the scanner
        // is in its barrier wait when the drain refuses.
        RefusalShape {
            name: "node-only",
            write: |base, osc| {
                write_low_base(base, &(1..=6).collect::<Vec<_>>(), 2, &[], &[]);
                write_osc_body(
                    osc,
                    &format!("<create>{}</create>", osc_way(10, 1, &[2, 7, 7, 8])),
                );
            },
            missing: 2,
            node_blobs: 3,
        },
        // Ways only: the empty boundary, sent at the first way.
        RefusalShape {
            name: "way-only",
            write: |base, osc| {
                write_low_base(base, &[], 1, &[(100, vec![1, 2]), (101, vec![3, 4])], &[]);
                write_osc_body(
                    osc,
                    &format!(
                        "<create>{}</create><modify>{}</modify>",
                        osc_node(1, 1),
                        osc_way(101, 2, &[1, 3]),
                    ),
                );
            },
            missing: 1,
            node_blobs: 0,
        },
    ];

    /// Require `result` to be the barrier refusal for `missing` distinct
    /// nodes.
    fn assert_refusal(label: &str, result: pbfhogg::Result<()>, missing: u64) {
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{label}: merge must refuse"));
        assert!(
            matches!(
                err.kind(),
                pbfhogg::ErrorKind::MissingNodeLocations {
                    count,
                    counted: pbfhogg::Unresolved::ChangedWayNodes,
                } if *count == missing
            ),
            "{label}: expected the refusal for {missing} distinct nodes, got: {err:?}"
        );
        assert!(
            err.to_string().contains(&format!(
                "could not be resolved for {missing} distinct nodes"
            )),
            "{label}: the message must say what it counts: {err}"
        );
    }

    /// Every refusal shape, at every worker count, on both routes, with the
    /// boundary on time and held past the last node item: the run refuses
    /// with the distinct count, publishes nothing, leaves no scratch, and
    /// refuses at the barrier: the map is never published, the drain
    /// handles nothing past the node phase (no way or relation item, no
    /// trailing create), and the workers process exactly the node blobs.
    #[test]
    fn missing_nodes_are_refused_at_the_barrier_however_it_lands() {
        use std::sync::atomic::Ordering::SeqCst;
        for shape in REFUSAL_SHAPES {
            let dir = crate::common::test_scratch();
            let base = dir.path().join("base.osm.pbf");
            let osc = dir.path().join("diff.osc.gz");
            let output = dir.path().join("output.osm.pbf");
            (shape.write)(&base, &osc);
            let before = common::snapshot_dir(dir.path());
            for &jobs in JOBS {
                for owned in [false, true] {
                    for late in [false, true] {
                        let label = format!("{} jobs={jobs} owned={owned} late={late}", shape.name);
                        let (result, host) = run_low(&base, &osc, &output, jobs, owned, |f| {
                            f.apply_changes_late_boundary.store(late, SeqCst);
                        });
                        assert_refusal(&label, result, shape.missing);
                        assert!(
                            !output.exists(),
                            "{label}: a refused run must publish nothing"
                        );
                        common::assert_scratch_unchanged(
                            &before,
                            &common::snapshot_dir(dir.path()),
                        );
                        let faults = host.invocation.faults();
                        assert!(
                            !faults.apply_changes_hook_timed_out.load(SeqCst),
                            "{label}: a hook wait expired"
                        );
                        if late {
                            assert_eq!(
                                faults.apply_changes_late_boundaries_seen.load(SeqCst),
                                1,
                                "{label}: the boundary must arrive after the last node item"
                            );
                        }
                        assert_eq!(
                            faults.apply_changes_worker_counter("merge_streaming_blobs_processed"),
                            Some(shape.node_blobs),
                            "{label}: no way or relation blob may reach a worker"
                        );
                        assert!(
                            !faults.apply_changes_drain_past_nodes.load(SeqCst),
                            "{label}: the drain must refuse before writing any way, \
                             relation or trailing create"
                        );
                        assert!(
                            !faults.apply_changes_barrier_signalled.load(SeqCst),
                            "{label}: the refusal must come before the barrier signal is sent"
                        );
                    }
                }
            }
        }
    }

    /// `ignore_missing_nodes` keeps its meaning: the same change publishes,
    /// every unresolved reference written as `(0, 0)`, and the stats count
    /// the distinct missing nodes.
    #[test]
    fn ignored_missing_nodes_still_publish_placeholders() {
        let shape = &REFUSAL_SHAPES[0];
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");
        (shape.write)(&base, &osc);
        for owned in [false, true] {
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .apply_changes_force_owned_route
                .store(owned, std::sync::atomic::Ordering::SeqCst);
            let stats = merge(
                &base,
                &osc,
                common::out(&output),
                &MergeOptions {
                    ignore_missing_nodes: true,
                    ..low_options(2)
                },
                &pbfhogg::HeaderOverrides::default(),
                &host,
            )
            .unwrap_or_else(|e| panic!("owned={owned}: {e}"));
            assert_eq!(stats.loc_missing, shape.missing, "owned={owned}");
            let ways = way_locations(&output);
            let way = |id: i64| {
                ways.iter()
                    .find(|(w, _)| *w == id)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| panic!("owned={owned}: way {id} missing"))
            };
            assert_eq!(
                way(101),
                vec![coord(1), (0, 0), coord(5), (0, 0)],
                "owned={owned}"
            );
            assert_eq!(way(200), vec![coord(6), (0, 0), coord(7)], "owned={owned}");
            std::fs::remove_file(&output).expect("remove output");
        }
    }

    /// A base whose header claims sorted order but holds node, way, node,
    /// way blobs: the scanner sends the boundary at the first way, walks the
    /// late node after it, and is held at that node's barrier poll until the
    /// drain has signalled, while the drain holds its signal until the
    /// scanner is there, so that poll (not the first way's) takes it.
    /// The scanner must record it, keep the second way held, skip its
    /// end-of-input wait (a signal taken and not recorded would leave it
    /// waiting there for good) and release both ways, after which the
    /// drain's order guard refuses the late node.
    #[test]
    fn a_signal_taken_after_a_late_node_still_releases_the_held_ways() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");
        {
            let file = File::create(&base).expect("create base");
            let mut writer = PbfWriter::new(file, Compression::default());
            let header = block_builder::HeaderBuilder::new()
                .sorted()
                .optional_feature("LocationsOnWays")
                .build()
                .expect("build header");
            writer.write_header(&header).expect("write header");
            let mut bb = BlockBuilder::new();
            let mut flush = |bb: &mut BlockBuilder| {
                let bytes = bb.take().expect("take").expect("non-empty block");
                writer.write_primitive_block(bytes).expect("write block");
            };
            for id in [1, 2] {
                let (lat, lon) = coord(id);
                bb.add_node(id, lat, lon, std::iter::empty(), None);
            }
            flush(&mut bb);
            bb.add_way_with_locations(
                10,
                [("highway", "road")],
                &[1, 2],
                &[coord(1), coord(2)],
                None,
            );
            flush(&mut bb);
            let (lat, lon) = coord(5);
            bb.add_node(5, lat, lon, std::iter::empty(), None);
            flush(&mut bb);
            bb.add_way_with_locations(
                11,
                [("highway", "road")],
                &[1, 5],
                &[coord(1), coord(5)],
                None,
            );
            flush(&mut bb);
            writer.flush().expect("flush base");
        }
        write_osc_body(
            &osc,
            &format!("<modify>{}</modify>", osc_way(10, 2, &[2, 1])),
        );
        let before = common::snapshot_dir(dir.path());
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
                let (result, host) = run_low(&base, &osc, &output, jobs, owned, |f| {
                    f.apply_changes_node_poll_waits_for_signal
                        .store(true, SeqCst);
                });
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{label}: the late node must be refused"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::Unsorted { .. }),
                    "{label}: expected the order guard, got: {err:?}"
                );
                let faults = host.invocation.faults();
                assert!(
                    !faults.apply_changes_hook_timed_out.load(SeqCst),
                    "{label}: the drain never signalled while the scanner was held"
                );
                assert!(
                    faults.apply_changes_node_poll_took_signal.load(SeqCst),
                    "{label}: the poll after the late node must take the signal"
                );
                assert!(
                    !faults.apply_changes_scanner_in_barrier_wait.load(SeqCst),
                    "{label}: a recorded signal must skip the end-of-input wait"
                );
                assert!(
                    faults.apply_changes_drain_past_nodes.load(SeqCst),
                    "{label}: the held ways must be released to the drain"
                );
                assert!(
                    !output.exists(),
                    "{label}: a refused run must publish nothing"
                );
                common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
            }
        }
    }

    /// A refusal while the scanner is still walking held way and relation
    /// headers stops the walk at its next barrier poll instead of at end of
    /// input: the scanner is held at its first poll until the drain has
    /// returned, and must leave through the closed channel, never reaching
    /// its end-of-input wait.
    #[test]
    fn a_refusal_stops_the_scanner_before_end_of_input() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        let output = dir.path().join("output.osm.pbf");
        let ways: Vec<(i64, Vec<i64>)> = (0..8).map(|i| (100 + i, vec![1 + i, 2 + i])).collect();
        write_low_base(
            &base,
            &(1..=10).collect::<Vec<_>>(),
            2,
            &ways,
            &[(500, 100)],
        );
        write_osc_body(
            &osc,
            &format!("<modify>{}</modify>", osc_way(103, 2, &[4, 999])),
        );
        let before = common::snapshot_dir(dir.path());
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
                let (result, host) = run_low(&base, &osc, &output, jobs, owned, |f| {
                    f.apply_changes_scanner_waits_for_drain_exit
                        .store(true, SeqCst);
                });
                assert_refusal(&label, result, 1);
                assert!(
                    !output.exists(),
                    "{label}: a refused run must publish nothing"
                );
                common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
                let faults = host.invocation.faults();
                assert!(
                    !faults.apply_changes_hook_timed_out.load(SeqCst),
                    "{label}: the drain never returned while the scanner was held"
                );
                assert!(
                    faults.apply_changes_scanner_stopped_before_eof.load(SeqCst),
                    "{label}: the scanner must stop at the closed barrier channel"
                );
                assert!(
                    !faults.apply_changes_scanner_in_barrier_wait.load(SeqCst),
                    "{label}: the scanner must not walk on to its end-of-input wait"
                );
            }
        }
    }

    /// A producer failure published, or a cancellation requested, after the
    /// drain's per-receive checks but before the refusal is decided
    /// outranks the refusal.
    #[test]
    fn a_failure_or_cancellation_at_the_refusal_outranks_it() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned} failure");
                assert_barrier_failure(
                    &label,
                    8,
                    &[1, 999],
                    jobs,
                    owned,
                    |f| f.apply_changes_fail_at_refusal.store(true, SeqCst),
                    "producer failure at the barrier refusal",
                );
                let label = format!("jobs={jobs} owned={owned} cancel");
                let dir = crate::common::test_scratch();
                let base = dir.path().join("base.osm.pbf");
                let osc = dir.path().join("diff.osc.gz");
                let output = dir.path().join("output.osm.pbf");
                (REFUSAL_SHAPES[0].write)(&base, &osc);
                let before = common::snapshot_dir(dir.path());
                let (result, _) = run_low(&base, &osc, &output, jobs, owned, |f| {
                    f.apply_changes_cancel_at_refusal.store(true, SeqCst);
                });
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{label}: merge must fail"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::Cancelled),
                    "{label}: expected Cancelled, got: {err:?}"
                );
                assert!(
                    !output.exists(),
                    "{label}: a cancelled run must publish nothing"
                );
                common::assert_scratch_unchanged(&before, &common::snapshot_dir(dir.path()));
            }
        }
    }

    const REPLACEMENT_OSC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="7" lat="0.0000007" lon="0.0000007" version="2"/>
    <node id="23" lat="0.0000023" lon="0.0000023" version="2"/>
  </modify>
</osmChange>"#;

    /// A base renamed over mid-run: every later open of the base comes from
    /// the pin taken at the start, so on a host that reopens through
    /// `/proc/self/fd` the run reads the file it started on and writes what
    /// an unreplaced run writes, and where opens go through the path the
    /// run fails with `InputReplaced` and publishes nothing.
    ///
    /// On the splice route the base is opened four times after the pin: the
    /// front read, the splice source, then the scanner's walker and the
    /// worker pool's descriptor from their own threads. The replacement
    /// fires right after the splice source, after the OSC parse and before
    /// either producer opens, so both producer opens follow it and both of
    /// their failure paths run in the forced mode. It does not replace the
    /// base while blobs are in flight.
    #[test]
    fn base_replaced_after_the_splice_open_is_never_read() {
        let dir = crate::common::test_scratch();
        let base = dir.path().join("base.osm.pbf");
        let osc = dir.path().join("diff.osc.gz");
        // Different IDs and blob layout: a run that read any of it would
        // write different bytes, or fail.
        write_multi_block_test_pbf(&base, &generate_nodes(40, 1), &[], &[], 5);
        write_osc(&osc, REPLACEMENT_OSC);
        let opts = MergeOptions {
            write: WriteOptions {
                // A splice-capable backend: the base's untouched blobs are
                // copied from the splice source, the fourth derived open.
                backend: pbfhogg::write::OutputBackend::Parallel,
                ..WriteOptions::default()
            },
            parallelism: pbfhogg::Parallelism::from_jobs(2),
            ..MergeOptions::default()
        };
        let run = |host: &pbfhogg::host::Host, output: &Path| {
            merge(
                &base,
                &osc,
                common::out(output),
                &opts,
                &pbfhogg::HeaderOverrides::default(),
                host,
            )
        };

        let reference = dir.path().join("reference.osm.pbf");
        run(&crate::common::test_host(), &reference).expect("unreplaced run");
        let expected = std::fs::read(&reference).expect("read reference output");

        for force_path in [true, false] {
            let replacement = dir.path().join("replacement.osm.pbf");
            write_multi_block_test_pbf(&replacement, &generate_nodes(60, 1), &[], &[], 7);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, std::sync::atomic::Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(&base, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.invocation
                .faults()
                .arm_input_replacement(&base, &replacement, 2);
            let output = dir.path().join(format!("output-{force_path}.osm.pbf"));
            let before = common::snapshot_dir(dir.path());
            let result = run(&host, &output);

            let report = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(report.fired, "the replacement must fire: {report:?}");
            assert_eq!(report.attempts, 4, "{report:?}");
            assert_eq!(report.attempts_after_fired, 2, "{report:?}");

            if procfs {
                result.expect("a procfs-pinned run is unaffected by the replacement");
                assert_eq!(report.verified, 4, "{report:?}");
                assert_eq!(
                    std::fs::read(&output).expect("read output"),
                    expected,
                    "the run must read only the base it pinned"
                );
                std::fs::remove_file(&output).expect("remove output");
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is
                // a Unix guarantee.
                let err = result.err().expect("a path-reopened run must refuse");
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{err:?}"
                );
                assert_eq!(report.refused, 2, "{report:?}");
                assert!(!output.exists(), "a refused run must publish nothing");
                // The replacement file was renamed over the base, so it is
                // the one path the run itself did not account for.
                let mut after = common::snapshot_dir(dir.path());
                after.insert(std::path::PathBuf::from("replacement.osm.pbf"));
                common::assert_scratch_unchanged(&before, &after);
            }
            // Restore the original base for the next mode.
            write_multi_block_test_pbf(&base, &generate_nodes(40, 1), &[], &[], 5);
        }
    }
}
