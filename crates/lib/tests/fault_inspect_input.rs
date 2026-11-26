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

use crate::pinned_cell::{self, Cell, Judged};
use crate::reopen_cell;

use std::path::Path;

use pbfhogg::inspect::InspectOptions;
use pbfhogg::read::input::InputSource;

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

fn ways(node_start: i64) -> Vec<TestWay<'static>> {
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

fn relations() -> Vec<TestRelation<'static>> {
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

impl Route {
    /// How many times the cell opens the input through a pin: the pinning
    /// routes count their own opens; the decode-only route takes no pin,
    /// so its cell hands it one and sees its one open.
    fn opens(self) -> u64 {
        match (self.decode_only, self.indexed) {
            (true, _) | (false, true) => 1,
            (false, false) => 2,
        }
    }

    /// The transport of each open on a direct run, following the inspect
    /// row of `crates/cli/src/io_reach.rs`: the index scan's header walk
    /// buffered whatever the flag, the full decode (the fallback's second
    /// open, or the decode-only route's only one) direct.
    fn transports(self) -> &'static str {
        match (self.decode_only, self.indexed) {
            (true, _) => "D",
            (false, true) => "B",
            (false, false) => "BD",
        }
    }
}

/// Check `route` in every read mode through [`pinned_cell::check`]: the
/// reference report with its exact transports, then the replacement armed
/// after each open in turn, in both reopen modes. The decode-only route's
/// one open is its last, so a replacement after it changes nothing, file
/// size included, in either reopen mode.
fn check_route(route: Route) {
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        write_input(&input, route.indexed);
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            route.opens(),
            route.transports(),
        );
        pinned_cell::check(
            &cell,
            |host| {
                if route.decode_only {
                    let pin = pbfhogg::read::input::PinnedInput::pin(&input, host)?;
                    run_inspect(route, &pin, host)
                } else {
                    run_inspect(route, &input, host)
                }
            },
            |report| {
                assert_eq!(report, expected(route, &input), "{route:?}: the report");
                report
            },
            |_, _| {
                write_input(&input, route.indexed);
                write_replacement(&replacement, route.indexed);
            },
            |(), step| run_inspect(route, step.pin, step.host),
            |expected, (), step, judged| {
                if let Judged::Unaffected(got) = judged {
                    assert_eq!(
                        &got, expected,
                        "{}: the report must be of the pinned input",
                        step.what
                    );
                }
            },
        );
    }
}

/// Indexed input: the header walk answers the whole report, one open.
#[test]
fn the_index_only_scan_reads_only_the_pinned_input() {
    check_route(Route {
        indexed: true,
        decode_only: false,
        direct: false,
    });
}

/// Unindexed input: the walk finds a blob without indexdata and the full
/// decode reopens from the start. Replaced between the two, the decode
/// refuses (path reopen) or reads the pinned file (stable reopen name), file size
/// included.
#[test]
fn the_full_decode_fallback_reads_only_the_pinned_input() {
    check_route(Route {
        indexed: false,
        decode_only: false,
        direct: false,
    });
}

/// A decode-only run takes no pin of its own: given a path it opens the
/// path (no pinned open is counted). Given a caller's pin it opens through
/// it, once.
#[test]
fn a_decode_only_run_opens_once_through_a_caller_pin() {
    for indexed in [true, false] {
        let route = Route {
            indexed,
            decode_only: true,
            direct: false,
        };
        check_route(route);
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        write_input(&input, indexed);
        let host = crate::common::test_host();
        host.invocation.faults().arm_input_replacement(
            &input,
            &dir.path().join("unused.osm.pbf"),
            1,
        );
        let got = run_inspect(route, &input, &host).expect("decode-only run on the path");
        assert_eq!(got, expected(route, &input), "{route:?}");
        let counted = host
            .invocation
            .faults()
            .input_replacement()
            .expect("armed replacement");
        assert_eq!(counted.attempts, 0, "{route:?}: {counted:?}");
    }
}

/// A caller's pin is the run's pin on the pinning route: replaced between
/// the caller's pin and the call, the report is of the file the caller
/// pinned (stable reopen name) or the first open is refused (path reopen). An entry
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
        for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
            write_input(&input, indexed);
            let expected = expected(route, &input);
            write_replacement(&replacement, indexed);
            let host = crate::common::test_host();
            scenario.run(&input, &host, |pin, scenario| {
                std::fs::rename(&replacement, &input).expect("replace the input");
                let result = run_inspect(route, pin, &host);
                let cell = format!("{route:?}, scenario {scenario:?}");
                match scenario {
                    reopen_cell::Scenario::Pinned => {
                        let got = result.unwrap_or_else(|e| panic!("{cell}: {e}"));
                        assert_eq!(
                            got, expected,
                            "{cell}: the report must be of the pinned file"
                        );
                    }
                    reopen_cell::Scenario::Path => {
                        let err = result
                            .err()
                            .unwrap_or_else(|| panic!("{cell}: must refuse"));
                        assert!(
                            matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                            "{cell}: {err:?}"
                        );
                    }
                }
            });
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

/// Feed `bytes` into the FIFO at `path`, under a hang bound the caller
/// arms first. Whether the run opened it; a reader that stops early closes
/// its end, so a short write is the reader's business, not the feeder's.
#[cfg(unix)]
fn feed_fifo(path: &Path, bytes: Vec<u8>) -> crate::common::FifoFeeder {
    crate::common::FifoFeeder::start(path, bytes)
}

#[cfg(unix)]
fn fifo_bound() -> pbfhogg_test_scratch::HangBound {
    pbfhogg_test_scratch::HangBound::arm(
        pbfhogg::TEST_HANG_BOUND,
        pbfhogg::TEST_HANG_REPORT_GRACE,
        "the FIFO run or its feeder did not end",
    )
}

/// The decode-only route reads its input once, so a FIFO serves it. Its
/// file size is the FIFO's `st_size`, zero, as a stat of the path gave.
#[cfg(unix)]
#[test]
fn a_decode_only_run_reads_a_fifo() {
    let _bound = fifo_bound();
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
    let fed = matches!(feeder.finish(), crate::common::Fed::Wrote(_));
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

/// Check `node_stats` in every read mode through [`pinned_cell::check`]:
/// the reference with its exact transports (the probe direct on a direct
/// run, the classify walk buffered, following the `--nodes` part of the
/// inspect row of `crates/cli/src/io_reach.rs`), then the replacement
/// armed after each open in turn, in both reopen modes. Replaced between
/// the probe and the walk, the walk refuses (path reopen) or reads the
/// pinned file (stable reopen name).
#[test]
fn node_stats_reads_only_the_pinned_input() {
    for &direct in pinned_cell::modes() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        write_input(&input, true);
        let cell = Cell::new(
            "node_stats",
            direct,
            &input,
            &replacement,
            NODE_STATS_OPENS,
            "DB",
        );
        pinned_cell::check(
            &cell,
            |host| run_node_stats(&input, direct, host),
            |stats| {
                assert_eq!(stats.node_count, 30, "the unreplaced node count");
                stats
            },
            |_, _| {
                write_input(&input, true);
                write_replacement(&replacement, true);
            },
            |(), step| run_node_stats(step.pin, direct, step.host),
            |expected, (), step, judged| {
                if let Judged::Unaffected(got) = judged {
                    assert_eq!(
                        &got, expected,
                        "{}: the stats must be of the pinned input",
                        step.what
                    );
                }
            },
        );
    }
}

/// A caller's pin is used as it is: replaced before the call, the stats are
/// of the pinned file (stable reopen name) or the probe is refused (path reopen).
#[test]
fn node_stats_uses_a_caller_pin_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for scenario in [reopen_cell::Scenario::Pinned, reopen_cell::Scenario::Path] {
        write_input(&input, true);
        let expected =
            run_node_stats(&input, false, &crate::common::test_host()).expect("unreplaced run");
        write_replacement(&replacement, true);
        let host = crate::common::test_host();
        scenario.run(&input, &host, |pin, scenario| {
            std::fs::rename(&replacement, &input).expect("replace the input");
            let result = run_node_stats(pin, false, &host);
            match scenario {
                reopen_cell::Scenario::Pinned => {
                    assert_eq!(result.expect("pinned run"), expected);
                }
                reopen_cell::Scenario::Path => {
                    let err = result.expect_err("must refuse");
                    assert!(
                        matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                        "{err:?}"
                    );
                }
            }
        });
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

/// The size `inspect_header` reports through `pin`, its header checked.
fn inspect_header_size(
    pin: &pbfhogg::read::input::PinnedInput,
    direct: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<u64> {
    pbfhogg::inspect::inspect_header(pin, direct, host).map(|read| {
        assert!(read.header.is_sorted());
        read.file_size
    })
}

/// The header read and the size come from one open, through a caller's
/// pin, in every read mode through [`pinned_cell::check`]: that open is
/// counted once, direct on a direct run (the header probe of the inspect
/// row of `crates/cli/src/io_reach.rs`), and a replacement after it changes
/// neither, in either reopen mode. A stat of the path would report the
/// replacement's size.
#[test]
fn inspect_header_takes_the_size_from_its_one_open() {
    for &direct in pinned_cell::modes() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
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
        let header = |source: &pbfhogg::read::input::PinnedInput, host: &pbfhogg::host::Host| {
            inspect_header_size(source, direct, host)
        };
        let cell = Cell::new("inspect_header", direct, &input, &replacement, 1, "D");
        pinned_cell::check(
            &cell,
            |host| header(&pbfhogg::read::input::PinnedInput::pin(&input, host)?, host),
            |file_size| {
                assert_eq!(file_size, size, "the unreplaced size");
                file_size
            },
            |_, _| {
                write_input(&input, true);
                write_replacement(&replacement, true);
            },
            |(), step| header(step.pin, step.host),
            |expected, (), step, judged| {
                let Judged::Unaffected(file_size) = judged else {
                    panic!("{}: the one open is the last", step.what);
                };
                assert_eq!(file_size, *expected, "{}", step.what);
            },
        );
    }
}

/// The header read takes no pin, so a FIFO serves it; its size is the
/// FIFO's `st_size`, zero.
#[cfg(unix)]
#[test]
fn inspect_header_reads_a_fifo() {
    let _bound = fifo_bound();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, true);
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let feeder = feed_fifo(&fifo, std::fs::read(&input).expect("read input"));
    let result = pbfhogg::inspect::inspect_header(&fifo, false, &crate::common::test_host());
    let fed = matches!(feeder.finish(), crate::common::Fed::Wrote(_));
    let read = result.unwrap_or_else(|e| panic!("a FIFO serves the header read: {e}"));
    assert!(fed, "the run must have opened the FIFO");
    assert_eq!(read.file_size, 0);
    assert!(read.header.is_sorted());
}
