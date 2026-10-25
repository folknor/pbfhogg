//! degrade against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! degrade picks its route from the flags alone, before it opens anything.
//!
//! The decode path (`unsort`, `unsort_intra`, `strip_locations`, `drop_ids`)
//! pins the input and opens it three times through the pin, in order, on
//! the calling thread: the indexdata probe, the header reader, then the
//! schedule walk whose descriptor the drop selection and every per-kind
//! pool pread. On a host that reopens through `/proc/self/fd` a replaced
//! input does not affect the run; where opens go through the path the run
//! fails with `InputReplaced` and publishes nothing. Each decode route is
//! checked with the replacement armed after each open in turn.
//!
//! The header-only passthrough opens the input once and reads the header
//! and every data frame from that one reader, so it takes no pin of its
//! own: a path is opened plainly (a FIFO serves it), and a caller's pin is
//! opened as the file it pinned. Through a caller's pin the open count is
//! observable: one open, after which a replacement changes nothing, in
//! either reopen mode.
//!
//! The replacement differs from the input everywhere a reading open could
//! see it: it carries no indexdata (a probe of it refuses a run that is not
//! forced), another `source` and bbox in its header (a header read of it
//! changes the output header) and other elements (a walk of it changes the
//! output).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::degrade::{DegradeFlags, DropSpec};
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

use crate::common::{out, out_replace, snapshot_dir};

/// Opens of the input on the decode path, after the pin.
const DECODE_OPENS: u64 = 3;

/// The element cap: input blobs of 25 elements split into two full output
/// blocks framed by the worker and a trailing 5 the merge thread coalesces,
/// and each unsort swap has more than one block of its kind to land in.
const CAP: usize = 10;

/// Elements per input blob.
const INPUT_BLOCK: usize = 25;

/// Elements `drop_ids` removes.
const DROP: u64 = 5;

#[derive(Clone, Copy, Debug)]
struct Route {
    flags: DegradeFlags,
    /// The input carries indexdata; without it a decode run is forced.
    indexed: bool,
    direct: bool,
}

impl Route {
    /// Which path `flags` take is up to degrade; the route lists say which
    /// each is meant to exercise.
    fn of(flags: DegradeFlags) -> Self {
        Self {
            flags,
            indexed: true,
            direct: false,
        }
    }

    fn strip_locations() -> Self {
        Self::of(DegradeFlags {
            strip_locations: true,
            ..DegradeFlags::default()
        })
    }

    fn strip_indexdata() -> Self {
        Self::of(DegradeFlags {
            strip_indexdata: true,
            ..DegradeFlags::default()
        })
    }

    fn dropped(self) -> usize {
        if self.flags.drop_ids.is_some() {
            usize::try_from(DROP).expect("small")
        } else {
            0
        }
    }
}

/// Every decode route: each decode flag on its own, a forced unindexed
/// input, and `drop_ids` with a header-only strip riding along.
fn decode_routes() -> Vec<Route> {
    let drop = Some(DropSpec { n: DROP, seed: 42 });
    vec![
        Route::strip_locations(),
        Route::of(DegradeFlags {
            unsort: true,
            ..DegradeFlags::default()
        }),
        Route::of(DegradeFlags {
            unsort_intra: true,
            ..DegradeFlags::default()
        }),
        Route::of(DegradeFlags {
            drop_ids: drop,
            ..DegradeFlags::default()
        }),
        Route::of(DegradeFlags {
            drop_ids: drop,
            strip_tagdata: true,
            strip_bbox: true,
            ..DegradeFlags::default()
        }),
        Route {
            indexed: false,
            ..Route::strip_locations()
        },
    ]
}

/// Every header-only rewrite, alone, plus the compatible pairs.
fn passthrough_routes() -> Vec<Route> {
    let flags = DegradeFlags::default();
    vec![
        Route::strip_indexdata(),
        Route::of(DegradeFlags {
            strip_tagdata: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            corrupt_tagdata: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            corrupt_indexdata: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            corrupt_indexdata_kind: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            strip_bbox: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            strip_indexdata: true,
            strip_tagdata: true,
            strip_bbox: true,
            ..flags
        }),
        Route::of(DegradeFlags {
            corrupt_indexdata: true,
            corrupt_indexdata_kind: true,
            corrupt_tagdata: true,
            ..flags
        }),
    ]
}

/// What a fixture holds.
struct Spec {
    first_node: i64,
    nodes: usize,
    first_way: i64,
    ways: usize,
    first_relation: i64,
    relations: usize,
    value: &'static str,
    source: &'static str,
    bbox: f64,
}

impl Spec {
    fn input() -> Self {
        Self {
            first_node: 1,
            nodes: 60,
            first_way: 1_001,
            ways: 30,
            first_relation: 2_001,
            relations: 3,
            value: "original",
            source: "the pinned input",
            bbox: 1.0,
        }
    }

    /// Other IDs, other values, another header.
    fn replacement() -> Self {
        Self {
            first_node: 501,
            nodes: 40,
            first_way: 5_001,
            ways: 20,
            first_relation: 6_001,
            relations: 2,
            value: "replacement",
            source: "the replacement",
            bbox: 2.0,
        }
    }

    /// `(kind, id)` of every element, in file order.
    fn ids(&self) -> Vec<(u8, i64)> {
        let range =
            |first: i64, n: usize| (0..n).map(move |i| first + i64::try_from(i).expect("small"));
        range(self.first_node, self.nodes)
            .map(|id| (0, id))
            .chain(range(self.first_way, self.ways).map(|id| (1, id)))
            .chain(range(self.first_relation, self.relations).map(|id| (2, id)))
            .collect()
    }
}

/// Write `spec` sorted, `INPUT_BLOCK` elements per blob, one kind per blob.
fn write_fixture(path: &Path, spec: &Spec, indexed: bool) {
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new()
        .sorted()
        .source(spec.source)
        .bbox(-spec.bbox, -spec.bbox, spec.bbox, spec.bbox);
    writer
        .write_header(&header.build().expect("build header"))
        .expect("write header");
    let mut bb = BlockBuilder::new();
    let flush = |bb: &mut BlockBuilder, writer: &mut PbfWriter<_>| {
        if let Some(bytes) = bb.take().expect("take") {
            if indexed {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
    };
    let tags = |k: &'static str| [(k, spec.value)];
    let mut last_kind = None;
    let mut in_block = 0;
    for (kind, id) in spec.ids() {
        if last_kind.is_some_and(|k| k != kind) || in_block == INPUT_BLOCK {
            flush(&mut bb, &mut writer);
            in_block = 0;
        }
        last_kind = Some(kind);
        in_block += 1;
        let small = i32::try_from(id).expect("small id");
        match kind {
            0 => bb.add_node(id, small * 1_000, small * 1_000 + 7, tags("n"), None),
            1 => {
                let first = spec.first_node + (id - spec.first_way);
                bb.add_way(id, tags("w"), &[first, first + 1, first + 2], None);
            }
            _ => {
                let member = [MemberData {
                    id: pbfhogg::MemberId::Way(spec.first_way + (id - spec.first_relation)),
                    role: "outer",
                }];
                bb.add_relation(id, tags("r"), &member, None);
            }
        }
    }
    flush(&mut bb, &mut writer);
    writer.flush().expect("flush fixture");
}

fn write_input(path: &Path, route: Route) {
    write_fixture(path, &Spec::input(), route.indexed);
}

fn write_replacement(path: &Path) {
    write_fixture(path, &Spec::replacement(), false);
}

/// `(kind, id)` of every element of the file at `path`, sorted.
fn read_ids(path: &Path) -> Vec<(u8, i64)> {
    let mut ids = Vec::new();
    for blob in BlobReader::from_path(path).expect("open output") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        for element in block.elements() {
            ids.push(match element {
                Element::DenseNode(n) => (0, n.id()),
                Element::Node(n) => (0, n.id()),
                Element::Way(w) => (1, w.id()),
                Element::Relation(r) => (2, r.id()),
                _ => continue,
            });
        }
    }
    ids.sort_unstable();
    ids
}

/// The unreplaced input's elements must all reach the output, but those
/// `drop_ids` removed, and the output header must be the input's.
fn assert_from_input(route: Route, output: &Path, cell: &str) {
    let ids = read_ids(output);
    let input = Spec::input().ids();
    assert_eq!(
        ids.len(),
        input.len() - route.dropped(),
        "{cell}: the output must carry every input element but those dropped"
    );
    assert!(
        ids.iter().all(|id| input.contains(id)),
        "{cell}: every output element must come from the input"
    );
    assert_eq!(
        crate::common::read_header(output).source(),
        Some(Spec::input().source),
        "{cell}: the output header must come from the input"
    );
}

fn write_options(route: Route) -> WriteOptions {
    WriteOptions {
        force: !route.indexed,
        direct_reads: route.direct,
        ..WriteOptions::default()
    }
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    output: pbfhogg::Output,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::degrade::degrade(
        input,
        output,
        route.flags,
        std::num::NonZeroUsize::new(CAP).expect("non-zero cap"),
        &write_options(route),
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// The unreplaced decode run's output bytes, with its open count and its
/// content checked: armed past the last open, the replacement never fires
/// and only counts. Skipped when the run failed and `skippable` accepts its
/// error; any other failure panics.
fn decode_reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let host = crate::common::test_host();
    host.cancel.faults().arm_input_replacement(
        input,
        &dir.join("unused.osm.pbf"),
        DECODE_OPENS + 1,
    );
    let output = dir.join("reference.osm.pbf");
    match run(route, input, out(&output), &host) {
        Ok(()) => {}
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    }
    let counted = host
        .cancel
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, DECODE_OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, DECODE_OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    Reference::of(route, &output)
}

/// The unreplaced output of any route, read from the path with no fault
/// armed. Skipped as [`decode_reference`].
fn plain_reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let output = dir.join("reference.osm.pbf");
    match run(route, input, out(&output), &crate::common::test_host()) {
        Ok(()) => Reference::of(route, &output),
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            Reference::Skipped
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    }
}

/// What a reference run found.
enum Reference {
    Bytes(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

impl Reference {
    /// The reference output's bytes, its content checked, the file removed.
    fn of(route: Route, output: &Path) -> Self {
        assert_from_input(route, output, &format!("{route:?}"));
        let bytes = std::fs::read(output).expect("read reference output");
        std::fs::remove_file(output).expect("remove reference output");
        Self::Bytes(bytes)
    }
}

/// Run a decode `route` with the replacement armed after each of its opens
/// in turn, in both reopen modes, into an absent destination and over an
/// existing one, against the unreplaced `expected` bytes.
fn check_decode_replacements(route: Route, dir: &Path, input: &Path, expected: &[u8]) {
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    const SENTINEL: &[u8] = b"an existing destination";
    for after in 1..=DECODE_OPENS {
        for force_path in [true, false] {
            for existing in [false, true] {
                write_input(input, route);
                write_replacement(&replacement);
                if existing {
                    std::fs::write(&output, SENTINEL).expect("write destination");
                }
                let host = crate::common::test_host();
                host.cancel
                    .faults()
                    .force_pin_path_reopen
                    .store(force_path, Ordering::Relaxed);
                let procfs = PinnedInput::pin(input, &host)
                    .expect("probe pin")
                    .reopens_through_procfs();
                host.cancel
                    .faults()
                    .arm_input_replacement(input, &replacement, after);
                let before = snapshot_dir(dir);
                let staged = if existing {
                    out_replace(&output)
                } else {
                    out(&output)
                };
                let result = run(route, input, staged, &host);

                let cell = format!(
                    "{route:?}, replaced after open {after}, procfs {procfs}, existing {existing}"
                );
                let report = host
                    .cancel
                    .faults()
                    .input_replacement()
                    .expect("armed replacement");
                assert!(
                    report.fired,
                    "{cell}: the replacement must fire: {report:?}"
                );

                // After the last open nothing opens again, so the run reads
                // only what it already holds, in either mode.
                if procfs || after == DECODE_OPENS {
                    result.unwrap_or_else(|e| {
                        panic!("{cell}: the run is unaffected by the replacement: {e}")
                    });
                    assert_eq!(report.attempts, DECODE_OPENS, "{cell}: {report:?}");
                    assert_eq!(report.verified, DECODE_OPENS, "{cell}: {report:?}");
                    assert_eq!(report.refused, 0, "{cell}: {report:?}");
                    assert_eq!(
                        report.attempts_after_fired,
                        DECODE_OPENS - after,
                        "{cell}: {report:?}"
                    );
                    assert!(
                        std::fs::read(&output).expect("read output") == expected,
                        "{cell}: the run must read only the input it pinned"
                    );
                    std::fs::remove_file(&output).expect("remove output");
                } else if cfg!(unix) {
                    // Off Unix a pin verifies nothing; the identity refusal
                    // is a Unix guarantee.
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
                    if existing {
                        assert_eq!(
                            std::fs::read(&output).expect("read destination"),
                            SENTINEL,
                            "{cell}: a refused run must leave the destination as it was"
                        );
                        std::fs::remove_file(&output).expect("remove destination");
                    } else {
                        assert!(
                            !output.exists(),
                            "{cell}: a refused run must publish nothing"
                        );
                    }
                    // The replacement was renamed over the input, so it is
                    // the one path the run itself did not account for.
                    let mut after_run = snapshot_dir(dir);
                    after_run.insert(PathBuf::from("replacement.osm.pbf"));
                    if existing {
                        after_run.insert(PathBuf::from("out.osm.pbf"));
                    }
                    crate::common::assert_scratch_unchanged(&before, &after_run);
                }
            }
        }
    }
}

fn check_decode_route(route: Route) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Bytes(expected) = decode_reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_decode_replacements(route, dir.path(), &input, &expected);
}

#[test]
fn every_decode_route_reads_only_the_pinned_input() {
    for route in decode_routes() {
        check_decode_route(route);
    }
}

/// The probe and the header reader open `O_DIRECT` when asked to, through
/// the pin's own direct open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_decode_reads_follow_the_pinned_input() {
    let route = Route {
        direct: true,
        ..Route::strip_locations()
    };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    let Reference::Bytes(expected) =
        decode_reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
    else {
        return;
    };
    check_decode_replacements(route, dir.path(), &input, &expected);
}

/// The passthrough through a caller's pin, replaced right after its first
/// open: that open is the only one, so the run reads the pinned file to
/// the end and succeeds in both reopen modes. A second open of the input,
/// header and frames read apart, would be counted, and refused under a
/// path reopen.
fn check_passthrough_route(route: Route) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_input(&input, route);
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    let Reference::Bytes(expected) = plain_reference(route, &input, dir.path(), |e| {
        route.direct && crate::common::is_einval(e)
    }) else {
        return;
    };
    for force_path in [true, false] {
        write_input(&input, route);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        host.cancel
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        host.cancel
            .faults()
            .arm_input_replacement(&input, &replacement, 1);
        let cell = format!("{route:?}, procfs {procfs}");
        run(route, &pin, out(&output), &host)
            .unwrap_or_else(|e| panic!("{cell}: the run is unaffected by the replacement: {e}"));
        let report = host
            .cancel
            .faults()
            .input_replacement()
            .expect("armed replacement");
        assert!(
            report.fired,
            "{cell}: the replacement must fire: {report:?}"
        );
        assert_eq!(report.attempts, 1, "{cell}: one open: {report:?}");
        assert_eq!(report.verified, 1, "{cell}: {report:?}");
        assert_eq!(report.attempts_after_fired, 0, "{cell}: {report:?}");
        assert!(
            std::fs::read(&output).expect("read output") == expected,
            "{cell}: the run must read only the file it opened"
        );
        std::fs::remove_file(&output).expect("remove output");
    }
}

#[test]
fn every_passthrough_route_reads_one_file_through_one_open() {
    for route in passthrough_routes() {
        check_passthrough_route(route);
    }
}

/// The passthrough's one open is `O_DIRECT` when asked to, and still the
/// only one.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_passthrough_reads_one_file_through_one_open() {
    check_passthrough_route(Route {
        direct: true,
        ..Route::strip_indexdata()
    });
}

/// A caller's pin is the run's pin on either route: replaced between the
/// caller's pin and the call, the input is the file the caller pinned
/// (procfs) or the first open is refused and nothing is published (path
/// reopen). An entry point that pinned or opened the path afresh would
/// degrade the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    for route in [Route::strip_locations(), Route::strip_indexdata()] {
        for force_path in [true, false] {
            write_input(&input, route);
            write_replacement(&replacement);
            let host = crate::common::test_host();
            host.cancel
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &input).expect("replace the input");
            let result = run(route, &pin, out(&output), &host);
            let cell = format!("{route:?}, procfs {procfs}");
            if procfs {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                assert_from_input(route, &output, &cell);
                std::fs::remove_file(&output).expect("remove output");
            } else if cfg!(unix) {
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
                assert!(
                    !output.exists(),
                    "{cell}: a refused run must publish nothing"
                );
            }
        }
    }
}

/// The decode path pins its input, so a FIFO is refused up front rather
/// than opened (a blocking open of a FIFO waits for a writer), and nothing
/// is published.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused_on_the_decode_path() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");
    for route in decode_routes() {
        let err = run(route, &fifo, out(&output), &crate::common::test_host())
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
        assert!(!output.exists(), "{route:?}: nothing is published");
    }
}

/// The passthrough reads its input in one sequential pass, so a FIFO
/// serves it: the output equals the run on the file the FIFO carried.
///
/// The writer side opens non-blocking and retries until the run has opened
/// the read side, so a run that never opens the FIFO leaves the writer to
/// give up rather than hang the test.
#[cfg(unix)]
#[test]
fn the_passthrough_reads_a_fifo() {
    let route = Route::strip_indexdata();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Bytes(expected) = plain_reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    let bytes = std::fs::read(&input).expect("read input");
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");

    let writer_path = fifo.clone();
    let feeder = std::thread::spawn(move || -> bool {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut file = loop {
            // ENXIO until a reader has the FIFO open.
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&writer_path)
            {
                Ok(file) => break file,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        };
        // Back to blocking writes now that the reader is there.
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: `fd` is open for both calls.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0, "F_GETFL");
            // SAFETY: as above.
            assert!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } >= 0,
                "F_SETFL"
            );
        }
        file.write_all(&bytes).expect("feed the FIFO");
        true
    });

    let result = run(route, &fifo, out(&output), &crate::common::test_host());
    let fed = feeder.join().expect("feeder thread");
    result.unwrap_or_else(|e| panic!("a FIFO serves the passthrough: {e}"));
    assert!(fed, "the run must have opened the FIFO");
    assert!(
        std::fs::read(&output).expect("read output") == expected,
        "the output must equal the run on the file the FIFO carried"
    );
}
