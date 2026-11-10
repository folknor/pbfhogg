//! time-filter against an input renamed over mid-run, through the fault plan
//! on the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn, the last included.
//! Every open is taken on the calling thread, one at a time, so the counts
//! are exact.
//!
//! The opens, in order: the header reader, which decides the path from the
//! header's `HistoricalInformation`; then, on the snapshot path only, the
//! schedule walk, whose descriptor the classify pool preads. The history
//! path reads everything through the header reader. Direct reads change
//! neither count.
//!
//! The replacement differs from the input everywhere a reading open could
//! see it: it declares the opposite `HistoricalInformation` (a header read of
//! it picks the other path) and carries other IDs (a walk of it changes the
//! output).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{Element, ElementReader, MemberId};

use crate::common::{out, out_replace, snapshot_dir};

const CUTOFF: i64 = 250;

#[derive(Clone, Copy, Debug)]
struct Route {
    /// The input declares `HistoricalInformation`.
    history: bool,
    direct: bool,
}

impl Route {
    /// How many times the route opens the input after the pin.
    fn opens(self) -> u64 {
        if self.history { 1 } else { 2 }
    }
}

/// One fixture element: kind, ID, version, timestamp, visible.
type Version = (char, i64, i32, i64, bool);

/// The history input's blobs. Node 2 and way 11 straddle a blob boundary
/// with versions on both sides of the cutoff; node 3 is deleted at the
/// cutoff, node 4 has no version at or before it, and way 10 and relation
/// 100 have a later version past it.
const HISTORY: &[&[Version]] = &[
    &[
        ('n', 1, 1, 100, true),
        ('n', 2, 1, 100, true),
        ('n', 2, 2, 200, true),
    ],
    &[
        ('n', 2, 3, 300, true),
        ('n', 3, 1, 100, true),
        ('n', 3, 2, 150, false),
        ('n', 4, 1, 400, true),
    ],
    &[
        ('w', 10, 1, 100, true),
        ('w', 10, 2, 300, true),
        ('w', 11, 1, 200, true),
    ],
    &[('w', 11, 2, 240, true)],
    &[('r', 100, 1, 100, true), ('r', 100, 2, 260, true)],
];

/// The snapshot input's blobs: each object once. Node 2 and way 11 are past
/// the cutoff and node 3 is deleted.
const SNAPSHOT: &[&[Version]] = &[
    &[('n', 1, 1, 100, true), ('n', 2, 1, 300, true)],
    &[('n', 3, 2, 100, false), ('n', 4, 1, 200, true)],
    &[('w', 10, 1, 100, true)],
    &[('w', 11, 1, 300, true)],
    &[('r', 100, 1, 100, true)],
];

/// Added to every ID of a replacement, so any element read from it shows.
const REPLACEMENT_SHIFT: i64 = 500;

/// An output element: kind, ID, version.
type Written = (char, i64, i32);

/// What the unreplaced run of `route` writes, from the fixture.
fn expected(route: Route) -> Vec<Written> {
    if route.history {
        vec![
            ('n', 1, 1),
            ('n', 2, 2),
            ('w', 10, 1),
            ('w', 11, 2),
            ('r', 100, 1),
        ]
    } else {
        vec![('n', 1, 1), ('n', 4, 1), ('w', 10, 1), ('r', 100, 1)]
    }
}

/// Write the history or snapshot fixture, sorted, with its IDs shifted by
/// `shift`.
fn write_fixture(path: &Path, history: bool, shift: i64) {
    let blobs = if history { HISTORY } else { SNAPSHOT };
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = HeaderBuilder::new().sorted();
    if history {
        header = header.historical();
    }
    writer
        .write_header(&header.build().expect("build header"))
        .expect("write header");
    let mut bb = BlockBuilder::new();
    for blob in blobs {
        for &(kind, id, version, timestamp, visible) in *blob {
            let id = id + shift;
            let meta = Metadata {
                version,
                timestamp,
                changeset: 7,
                uid: 3,
                user: "u",
                visible,
            };
            let tags = [("name", "x")];
            match kind {
                'n' => bb.add_node(id, 10, 10, tags, Some(&meta)),
                'w' => bb.add_way(id, tags, &[1 + shift, 2 + shift], Some(&meta)),
                _ => bb.add_relation(
                    id,
                    tags,
                    &[MemberData {
                        id: MemberId::Way(10 + shift),
                        role: "outer",
                    }],
                    Some(&meta),
                ),
            }
        }
        let bytes = bb.take().expect("take").expect("a non-empty blob");
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush fixture");
}

fn write_input(path: &Path, route: Route) {
    write_fixture(path, route.history, 0);
}

/// The opposite `HistoricalInformation`, other IDs.
fn write_replacement(path: &Path, route: Route) {
    write_fixture(path, !route.history, REPLACEMENT_SHIFT);
}

/// Every element of the file at `path`, in file order.
fn read_written(path: &Path) -> Vec<Written> {
    let mut written = Vec::new();
    ElementReader::open(path, false)
        .expect("open output")
        .for_each(|element| {
            written.push(match element {
                Element::DenseNode(n) => ('n', n.id(), n.info().expect("info").version()),
                Element::Node(n) => ('n', n.id(), n.info().version().expect("version")),
                Element::Way(w) => ('w', w.id(), w.info().version().expect("version")),
                Element::Relation(r) => ('r', r.id(), r.info().version().expect("version")),
                other => panic!("unexpected element {other:?}"),
            });
        })
        .expect("read output");
    written
}

fn run<'a>(
    route: Route,
    input: impl Into<InputSource<'a>>,
    output: pbfhogg::Output,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let write = WriteOptions {
        direct_reads: route.direct,
        ..WriteOptions::default()
    };
    pbfhogg::time_filter::time_filter(
        input,
        output,
        CUTOFF,
        &write,
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// What [`reference`] found.
enum Reference {
    Bytes(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced run's output bytes, with its open count and its content
/// checked: armed past the last open, the replacement never fires and only
/// counts. Skipped when the run failed and `skippable` accepts its error;
/// any other failure panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let opens = route.opens();
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), opens + 1);
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
        .invocation
        .faults()
        .input_replacement()
        .expect("armed replacement");
    assert!(!counted.fired, "{route:?}: {counted:?}");
    assert_eq!(counted.attempts, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, opens, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");
    assert_eq!(
        read_written(&output),
        expected(route),
        "{route:?}: the unreplaced output"
    );
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");
    Reference::Bytes(bytes)
}

/// Run `route` with the replacement armed after each of its opens in turn,
/// in both reopen modes, into an absent destination and over an existing
/// one, against the unreplaced `expected` bytes.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[u8]) {
    const SENTINEL: &[u8] = b"an existing destination";
    let opens = route.opens();
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    for after in 1..=opens {
        for force_path in [true, false] {
            for existing in [false, true] {
                write_input(input, route);
                write_replacement(&replacement, route);
                if existing {
                    std::fs::write(&output, SENTINEL).expect("write destination");
                }
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
                    .invocation
                    .faults()
                    .input_replacement()
                    .expect("armed replacement");
                assert!(
                    report.fired,
                    "{cell}: the replacement must fire: {report:?}"
                );

                // After the last open nothing opens again, so the run reads
                // only what it already holds, in either mode.
                if procfs || after == opens {
                    result.unwrap_or_else(|e| {
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

fn check_route(route: Route, skippable: impl Fn(&pbfhogg::Error) -> bool) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Bytes(expected) = reference(route, &input, dir.path(), skippable) else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// The snapshot path's two opens: replaced after the header read, the walk
/// refuses (path reopen) or reads the pinned file (procfs).
#[test]
fn a_snapshot_input_reads_only_the_pinned_input() {
    check_route(
        Route {
            history: false,
            direct: false,
        },
        |_| false,
    );
}

/// The history path's one open: nothing opens after it, so a replacement
/// never reaches the run.
#[test]
fn a_history_input_reads_only_the_pinned_input() {
    check_route(
        Route {
            history: true,
            direct: false,
        },
        |_| false,
    );
}

/// The header reader opens `O_DIRECT` when asked to, through the pin's own
/// direct open; the snapshot walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    for history in [false, true] {
        // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
        check_route(
            Route {
                history,
                direct: true,
            },
            |e| crate::common::is_einval(e),
        );
    }
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (procfs) or the first
/// open is refused and nothing is published (path reopen). An entry point
/// that pinned the path afresh would read the replacement's header, take the
/// other path, and write the replacement's IDs.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    for history in [false, true] {
        let route = Route {
            history,
            direct: false,
        };
        for force_path in [true, false] {
            write_input(&input, route);
            write_replacement(&replacement, route);
            let host = crate::common::test_host();
            host.invocation
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
                assert_eq!(
                    read_written(&output),
                    expected(route),
                    "{cell}: the output must come from the pinned file"
                );
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

/// The input is pinned before its header is read, so a FIFO is refused up
/// front, whichever path its bytes would have taken, rather than opened (a
/// blocking open of a FIFO waits for a writer, and none is attached here),
/// and nothing is published.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");
    let route = Route {
        history: true,
        direct: false,
    };
    let err = run(route, &fifo, out(&output), &crate::common::test_host())
        .err()
        .unwrap_or_else(|| panic!("a FIFO must be refused"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("not a regular file"), "{err}");
    assert!(!output.exists(), "nothing is published");
}
