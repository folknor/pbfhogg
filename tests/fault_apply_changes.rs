//! Fault-injection tests for `apply_changes::merge`, through the fault plan
//! on the run's token (see `pbfhogg::test_hooks`): worker panic recovery
//! (`apply_changes_panic_at_blob_seq`), a base renamed over mid-run
//! (`arm_input_replacement`), and the `--locations-on-ways` node-to-way
//! barrier under deterministic orderings: a node boundary held until after
//! the last node item, a worker panic while the scanner is in its barrier
//! wait or blocked dispatching, and a cancellation during the barrier wait,
//! on both output routes. A drain-side output failure releasing blocked
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
        host.cancel
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
        let faults = host.cancel.faults();
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
                        let faults = host.cancel.faults();
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

    /// Run the ordinary shape with `nodes` single-node blobs, armed by
    /// `arm`, and require it to fail with `expect` in its message, publish
    /// nothing and leave no scratch.
    fn assert_barrier_failure(
        label: &str,
        nodes: i64,
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
        write_osc_body(
            &osc,
            &format!("<modify>{}</modify>", osc_way(101, 2, &[1, 4])),
        );
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
                .cancel
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
    /// hanging scanner, drain and the other workers.
    #[test]
    fn a_worker_panic_during_the_barrier_wait_fails_the_run() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
                let host = assert_barrier_failure(
                    &label,
                    8,
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
                    host.cancel
                        .faults()
                        .apply_changes_scanner_in_barrier_wait
                        .load(SeqCst),
                    "{label}: the scanner must have reached its barrier wait"
                );
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
                    host.cancel
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
    /// publishes nothing and joins every thread.
    #[test]
    fn cancelling_during_the_barrier_wait_stops_the_run() {
        use std::sync::atomic::Ordering::SeqCst;
        for &jobs in JOBS {
            for owned in [false, true] {
                let label = format!("jobs={jobs} owned={owned}");
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
                write_osc_body(
                    &osc,
                    &format!("<modify>{}</modify>", osc_way(100, 2, &[2, 3])),
                );
                let host = crate::common::test_host();
                let faults = host.cancel.faults();
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
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !faults.apply_changes_scanner_in_barrier_wait.load(SeqCst) {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{label}: the scanner never reached its barrier wait"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    host.cancel.cancel();
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
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, std::sync::atomic::Ordering::Relaxed);
            let procfs = pbfhogg::read::input::PinnedInput::pin(&base, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.cancel
                .faults()
                .arm_input_replacement(&base, &replacement, 2);
            let output = dir.path().join(format!("output-{force_path}.osm.pbf"));
            let before = common::snapshot_dir(dir.path());
            let result = run(&host, &output);

            let report = host
                .cancel
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
