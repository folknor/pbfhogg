//! inspect against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Each library entry decides for itself whether it pins:
//!
//! - `inspect::inspect` pins when its index-only scan may run, since a data
//!   blob without indexdata sends it back to the start for a full decode:
//!   one open (the header walk) on indexed input, two (the walk, then the
//!   decode) on unindexed input. A run that only decodes (`--extended`,
//!   `--locations`, `--distrust-metadata`) opens once and takes no pin, so a
//!   path is opened plainly and a FIFO serves it; through a caller's pin its
//!   one open is observable.
//! - `node_stats` pins and opens twice: the indexdata probe, then the
//!   schedule walk whose descriptor the classify pool preads.
//! - `inspect_header` opens once and takes no pin; the file size comes from
//!   that open's descriptor.
//!
//! Every open is taken on the calling thread, one at a time, so the counts
//! are exact. The replacement differs from the input in its element counts,
//! node coordinates and size, so a read of it shows in any report.

use std::path::Path;
use std::sync::atomic::Ordering;

use pbfhogg::inspect::InspectOptions;
use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{TestNode, TestRelation, TestWay, generate_nodes};

/// The report fields every inspect route fills.
const KEYS: &[&str] = &[
    "file.size",
    "indexed",
    "blocks.total",
    "elements.nodes",
    "elements.ways",
    "elements.relations",
];

fn ways(node_start: i64) -> Vec<TestWay> {
    vec![
        TestWay {
            id: 10,
            refs: vec![node_start, node_start + 1, node_start + 2],
            tags: vec![("highway", "residential")],
            meta: None,
        },
        TestWay {
            id: 11,
            refs: vec![node_start + 1, node_start + 3],
            tags: vec![],
            meta: None,
        },
    ]
}

fn relations() -> Vec<TestRelation> {
    vec![TestRelation {
        id: 100,
        members: vec![],
        tags: vec![("type", "route")],
        meta: None,
    }]
}

/// The input: 30 nodes from ID 1, two ways, one relation.
fn write_input(path: &Path, indexed: bool) {
    write_fixture(path, indexed, &generate_nodes(30, 1), &ways(1));
}

/// The replacement: 50 nodes from ID 1001 (other coordinates), two ways.
fn write_replacement(path: &Path, indexed: bool) {
    write_fixture(path, indexed, &generate_nodes(50, 1001), &ways(1001));
}

fn write_fixture(path: &Path, indexed: bool, nodes: &[TestNode], ways: &[TestWay]) {
    if indexed {
        crate::common::write_indexed_pbf(path, nodes, ways, &relations());
    } else {
        crate::common::write_test_pbf_non_indexed(path, nodes, ways, &relations());
    }
}

fn size_of(path: &Path) -> String {
    std::fs::metadata(path)
        .expect("stat fixture")
        .len()
        .to_string()
}

// ---------------------------------------------------------------------------
// inspect::inspect
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Route {
    /// Every data blob of the input carries indexdata.
    indexed: bool,
    /// `extended`: the full decode is the only read.
    decode_only: bool,
    direct: bool,
}

impl Route {
    /// How many times the route opens the input through a pin: the pinning
    /// routes count their own opens, the decode-only route takes no pin.
    fn pinned_opens(self) -> u64 {
        match (self.decode_only, self.indexed) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        }
    }

    fn options(self) -> InspectOptions {
        InspectOptions {
            show_blocks: false,
            show_id_ranges: true,
            show_locations: false,
            extended: self.decode_only,
            distrust_metadata: false,
            direct_io: self.direct,
        }
    }
}

fn run_inspect<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<Vec<Option<String>>> {
    let report = pbfhogg::inspect::inspect(input, &route.options(), host)?;
    Ok(KEYS.iter().map(|key| report.get_value(key)).collect())
}

/// What the unreplaced run of `route` on `input` reports.
fn expected(route: Route, input: &Path) -> Vec<Option<String>> {
    // One block per kind, whichever writer framed them.
    vec![
        Some(size_of(input)),
        Some(route.indexed.to_string()),
        Some("3".to_string()),
        Some("30".to_string()),
        Some("2".to_string()),
        Some("1".to_string()),
    ]
}

/// The unreplaced run's report, with its pinned-open count checked: armed
/// past the last open, the replacement never fires and only counts. `None`
/// when the run failed and `skippable` accepts its error.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Option<Vec<Option<String>>> {
    let opens = route.pinned_opens();
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), opens + 1);
    let report = match run_inspect(route, input, &host) {
        Ok(report) => report,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return None;
        }
        Err(e) => panic!("{route:?}: unreplaced run: {e}"),
    };
    let Some(counted) = host.invocation.faults().input_replacement() else {
        panic!("{route:?}: the replacement was armed");
    };
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_eq!(report, expected(route, input), "{route:?}: the report");
    Some(report)
}

/// Run `route` with the replacement armed after each of its pinned opens in
/// turn, in both reopen modes, against the unreplaced report.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[Option<String>]) {
    let opens = route.pinned_opens();
    let replacement = dir.join("replacement.osm.pbf");
    for after in 1..=opens {
        for force_path in [true, false] {
            write_input(input, route.indexed);
            write_replacement(&replacement, route.indexed);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = PinnedInput::pin(input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.invocation
                .faults()
                .arm_input_replacement(input, &replacement, after);
            let result = run_inspect(route, input, &host);

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
            if procfs || after == opens {
                let got = result.unwrap_or_else(|e| {
                    panic!("{cell}: the run is unaffected by the replacement: {e}")
                });
                assert_eq!(report.attempts, opens, "{cell}: {report:?}");
                assert_eq!(report.verified, opens, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    opens - after,
                    "{cell}: {report:?}"
                );
                assert_eq!(
                    got, expected,
                    "{cell}: the report must be of the pinned input"
                );
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is a
                // Unix guarantee.
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
            }
        }
    }
}

fn check_route(route: Route, skippable: impl Fn(&pbfhogg::Error) -> bool) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route.indexed);
    let Some(expected) = reference(route, &input, dir.path(), skippable) else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// Indexed input: the header walk answers the whole report, one open.
#[test]
fn the_index_only_scan_reads_only_the_pinned_input() {
    check_route(
        Route {
            indexed: true,
            decode_only: false,
            direct: false,
        },
        |_| false,
    );
}

/// Unindexed input: the walk finds a blob without indexdata and the full
/// decode reopens from the start. Replaced between the two, the decode
/// refuses (path reopen) or reads the pinned file (procfs), file size
/// included.
#[test]
fn the_full_decode_fallback_reads_only_the_pinned_input() {
    check_route(
        Route {
            indexed: false,
            decode_only: false,
            direct: false,
        },
        |_| false,
    );
}

/// The fallback decode opens `O_DIRECT` when asked to, through the pin's own
/// direct open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn the_direct_fallback_reads_only_the_pinned_input() {
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    check_route(
        Route {
            indexed: false,
            decode_only: false,
            direct: true,
        },
        |e| crate::common::is_einval(e),
    );
}

/// A decode-only run takes no pin of its own: given a path it opens the path
/// (no pinned open is counted). Given a caller's pin it opens through it,
/// once, and a replacement after that open changes nothing, file size
/// included, in either reopen mode.
#[test]
fn a_decode_only_run_opens_once_through_a_caller_pin() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for indexed in [true, false] {
        let route = Route {
            indexed,
            decode_only: true,
            direct: false,
        };
        write_input(&input, indexed);
        let Some(expected) = reference(route, &input, dir.path(), |_| false) else {
            unreachable!("no failure is skippable here");
        };
        for force_path in [true, false] {
            write_input(&input, indexed);
            write_replacement(&replacement, indexed);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            host.invocation
                .faults()
                .arm_input_replacement(&input, &replacement, 1);
            let got = run_inspect(route, &pin, &host)
                .unwrap_or_else(|e| panic!("{route:?}, force_path {force_path}: {e}"));
            let report = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(report.fired, "{route:?}: {report:?}");
            assert_eq!(report.attempts, 1, "{route:?}: {report:?}");
            assert_eq!(report.attempts_after_fired, 0, "{route:?}: {report:?}");
            assert_eq!(got, expected, "{route:?}, force_path {force_path}");
        }
    }
}

/// A caller's pin is the run's pin on the pinning route: replaced between
/// the caller's pin and the call, the report is of the file the caller
/// pinned (procfs) or the first open is refused (path reopen). An entry
/// that pinned the path afresh would report the replacement.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for indexed in [true, false] {
        let route = Route {
            indexed,
            decode_only: false,
            direct: false,
        };
        for force_path in [true, false] {
            write_input(&input, indexed);
            let expected = expected(route, &input);
            write_replacement(&replacement, indexed);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let pin = PinnedInput::pin(&input, &host).expect("caller pin");
            let procfs = pin.reopens_through_procfs();
            std::fs::rename(&replacement, &input).expect("replace the input");
            let result = run_inspect(route, &pin, &host);
            let cell = format!("{route:?}, procfs {procfs}");
            if procfs {
                let got = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                assert_eq!(
                    got, expected,
                    "{cell}: the report must be of the pinned file"
                );
            } else if cfg!(unix) {
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
            }
        }
    }
}

/// The pinning route refuses a FIFO up front rather than opening it (a
/// blocking open of a FIFO waits for a writer, and none is attached here).
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_where_the_scan_may_read_twice() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let route = Route {
        indexed: true,
        decode_only: false,
        direct: false,
    };
    let err = run_inspect(route, &fifo, &crate::common::test_host())
        .err()
        .unwrap_or_else(|| panic!("a FIFO must be refused"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

/// Feed `bytes` into the FIFO at `path` from a thread. The writer side
/// opens non-blocking and retries until a reader has the FIFO open, so a
/// run that never opens it leaves the writer to give up (false) rather than
/// hang the test.
#[cfg(unix)]
fn feed_fifo(path: &Path, bytes: Vec<u8>) -> std::thread::JoinHandle<bool> {
    let path = path.to_path_buf();
    std::thread::spawn(move || -> bool {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut file = loop {
            // ENXIO until a reader has the FIFO open.
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => break file,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        };
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is open for both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL");
        // SAFETY: as above.
        assert!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } >= 0,
            "F_SETFL"
        );
        // A reader that stops early closes its end: a short write is the
        // reader's business, not this feeder's.
        match file.write_all(&bytes) {
            Ok(()) | Err(_) => {}
        }
        true
    })
}

/// The decode-only route reads its input once, so a FIFO serves it. Its
/// file size is the FIFO's `st_size`, zero, as a stat of the path gave.
#[cfg(unix)]
#[test]
fn a_decode_only_run_reads_a_fifo() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, true);
    let route = Route {
        indexed: true,
        decode_only: true,
        direct: false,
    };
    let mut expected = expected(route, &input);
    expected[0] = Some("0".to_string());
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let feeder = feed_fifo(&fifo, std::fs::read(&input).expect("read input"));
    let result = run_inspect(route, &fifo, &crate::common::test_host());
    let fed = feeder.join().expect("feeder thread");
    let got = result.unwrap_or_else(|e| panic!("a FIFO serves the decode-only scan: {e}"));
    assert!(fed, "the run must have opened the FIFO");
    assert_eq!(got, expected);
}

// ---------------------------------------------------------------------------
// node_stats
// ---------------------------------------------------------------------------

/// Opens of the input by `node_stats`, after the pin: the probe, the walk.
const NODE_STATS_OPENS: u64 = 2;

/// The node count, coordinate ranges and FOR block count of a report.
fn run_node_stats<'a>(
    input: impl Into<InputSource<'a>>,
    direct: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<NodeStats> {
    let report = pbfhogg::inspect::node_stats::node_stats(
        input,
        direct,
        false,
        pbfhogg::Parallelism::from_jobs(1),
        host,
    )?;
    Ok(NodeStats {
        node_count: report.node_count,
        ranges: [
            report.min_lat,
            report.max_lat,
            report.min_lon,
            report.max_lon,
        ],
        for_blocks: report.lat_stats.total_blocks,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct NodeStats {
    node_count: u64,
    ranges: [i32; 4],
    for_blocks: u64,
}

fn check_node_stats(direct: bool, skippable: impl Fn(&pbfhogg::Error) -> bool) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    write_input(&input, true);

    let host = crate::common::test_host();
    host.invocation.faults().arm_input_replacement(
        &input,
        &dir.path().join("unused.osm.pbf"),
        NODE_STATS_OPENS + 1,
    );
    let expected = match run_node_stats(&input, direct, &host) {
        Ok(stats) => stats,
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("node_stats direct {direct}: {e}"));
            return;
        }
        Err(e) => panic!("node_stats direct {direct}: unreplaced run: {e}"),
    };
    let counted = host
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{counted:?}");
    assert_eq!(counted.attempts, NODE_STATS_OPENS, "{counted:?}");
    assert_eq!(counted.verified, NODE_STATS_OPENS, "{counted:?}");
    assert_eq!(expected.node_count, 30, "the unreplaced node count");

    for after in 1..=NODE_STATS_OPENS {
        for force_path in [true, false] {
            write_input(&input, true);
            write_replacement(&replacement, true);
            let host = crate::common::test_host();
            host.invocation
                .faults()
                .force_pin_path_reopen
                .store(force_path, Ordering::Relaxed);
            let procfs = PinnedInput::pin(&input, &host)
                .expect("probe pin")
                .reopens_through_procfs();
            host.invocation
                .faults()
                .arm_input_replacement(&input, &replacement, after);
            let result = run_node_stats(&input, direct, &host);
            let cell = format!("direct {direct}, replaced after open {after}, procfs {procfs}");
            let report = host
                .invocation
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(report.fired, "{cell}: {report:?}");
            if procfs || after == NODE_STATS_OPENS {
                let got = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                assert_eq!(report.attempts, NODE_STATS_OPENS, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    NODE_STATS_OPENS - after,
                    "{cell}: {report:?}"
                );
                assert_eq!(
                    got, expected,
                    "{cell}: the stats must be of the pinned input"
                );
            } else if cfg!(unix) {
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
                assert_eq!(report.refused, 1, "{cell}: {report:?}");
                assert_eq!(report.attempts, after + 1, "{cell}: {report:?}");
            }
        }
    }
}

/// Replaced between the probe and the walk, the walk refuses (path reopen)
/// or reads the pinned file (procfs).
#[test]
fn node_stats_reads_only_the_pinned_input() {
    check_node_stats(false, |_| false);
}

/// The probe opens `O_DIRECT` when asked to, through the pin.
#[cfg(feature = "linux-direct-io")]
#[test]
fn node_stats_direct_reads_follow_the_pinned_input() {
    check_node_stats(true, |e| crate::common::is_einval(e));
}

/// A caller's pin is used as it is: replaced before the call, the stats are
/// of the pinned file (procfs) or the probe is refused (path reopen).
#[test]
fn node_stats_uses_a_caller_pin_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for force_path in [true, false] {
        write_input(&input, true);
        let expected =
            run_node_stats(&input, false, &crate::common::test_host()).expect("unreplaced run");
        write_replacement(&replacement, true);
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        std::fs::rename(&replacement, &input).expect("replace the input");
        let result = run_node_stats(&pin, false, &host);
        if procfs {
            assert_eq!(result.expect("procfs run"), expected);
        } else if cfg!(unix) {
            let err = result.expect_err("must refuse");
            assert!(
                matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                "{err:?}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn node_stats_refuses_a_fifo() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let err = run_node_stats(&fifo, false, &crate::common::test_host())
        .err()
        .unwrap_or_else(|| panic!("a FIFO must be refused"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

// ---------------------------------------------------------------------------
// inspect_header
// ---------------------------------------------------------------------------

/// The header read and the size come from one open: through a caller's pin
/// that open is counted once, and a replacement after it changes neither,
/// in either reopen mode. A stat of the path would report the
/// replacement's size.
#[test]
fn inspect_header_takes_the_size_from_its_one_open() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for force_path in [true, false] {
        write_input(&input, true);
        write_replacement(&replacement, true);
        let size = std::fs::metadata(&input).expect("stat input").len();
        assert_ne!(
            size,
            std::fs::metadata(&replacement)
                .expect("stat replacement")
                .len(),
            "the sizes must differ for the check to mean anything"
        );
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        host.invocation
            .faults()
            .arm_input_replacement(&input, &replacement, 1);
        let read = pbfhogg::inspect::inspect_header(&pin, false, &host)
            .unwrap_or_else(|e| panic!("force_path {force_path}: {e}"));
        let report = host
            .invocation
            .faults()
            .input_replacement()
            .expect("armed replacement");
        assert!(report.fired, "{report:?}");
        assert_eq!(report.attempts, 1, "{report:?}");
        assert_eq!(read.file_size, size, "force_path {force_path}");
        assert!(read.header.is_sorted());
    }
}

/// The header read takes no pin, so a FIFO serves it; its size is the
/// FIFO's `st_size`, zero.
#[cfg(unix)]
#[test]
fn inspect_header_reads_a_fifo() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, true);
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let feeder = feed_fifo(&fifo, std::fs::read(&input).expect("read input"));
    let result = pbfhogg::inspect::inspect_header(&fifo, false, &crate::common::test_host());
    let fed = feeder.join().expect("feeder thread");
    let read = result.unwrap_or_else(|e| panic!("a FIFO serves the header read: {e}"));
    assert!(fed, "the run must have opened the FIFO");
    assert_eq!(read.file_size, 0);
    assert!(read.header.is_sorted());
}
