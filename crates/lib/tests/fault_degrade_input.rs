//! degrade against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! degrade picks its route from `DegradeFlags::reencode` alone, before it
//! opens anything.
//!
//! The decode path (a `Reencode`: with an unsort mode, `strip_locations`,
//! `drop_ids`, or none of them) pins the input and opens it three times
//! through the pin, in order, on the calling thread: the indexdata probe, the header reader, then the
//! schedule walk whose descriptor the drop selection and every per-kind
//! pool pread. With an injected stable scratch reopen name a replaced
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

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::degrade::{DegradeFlags, DropSpec, Reencode, Unsort};
use pbfhogg::read::input::InputSource;
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

use crate::common::{out, out_replace};

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

    /// A re-encode at [`CAP`] carrying `reencode`'s transformations.
    fn reencode(reencode: Reencode) -> Self {
        Self::of(DegradeFlags {
            reencode: Some(Reencode {
                block_cap: std::num::NonZeroUsize::new(CAP).expect("non-zero cap"),
                ..reencode
            }),
            ..DegradeFlags::default()
        })
    }

    fn strip_locations() -> Self {
        Self::reencode(Reencode {
            strip_locations: true,
            ..Reencode::default()
        })
    }

    fn strip_indexdata() -> Self {
        Self::of(DegradeFlags {
            strip_indexdata: true,
            ..DegradeFlags::default()
        })
    }

    fn dropped(self) -> usize {
        if self.flags.reencode.and_then(|r| r.drop_ids).is_some() {
            usize::try_from(DROP).expect("small")
        } else {
            0
        }
    }
}

/// Every decode route: a re-encode with no transformation of its own, each
/// decode transformation on its own, a forced unindexed input, and
/// `drop_ids` with header-only strips riding along.
fn decode_routes() -> Vec<Route> {
    let drop = Some(DropSpec { n: DROP, seed: 42 });
    let dropping = Route::reencode(Reencode {
        drop_ids: drop,
        ..Reencode::default()
    });
    vec![
        Route::reencode(Reencode::default()),
        Route::strip_locations(),
        Route::reencode(Reencode {
            unsort: Some(Unsort::CrossBlob),
            ..Reencode::default()
        }),
        Route::reencode(Reencode {
            unsort: Some(Unsort::IntraBlob),
            ..Reencode::default()
        }),
        dropping,
        Route::of(DegradeFlags {
            strip_tagdata: true,
            strip_bbox: true,
            ..dropping.flags
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
        &write_options(route),
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// What a refused run leaves at an existing destination.
const SENTINEL: &[u8] = b"an existing destination";

/// The reference output's bytes, its content checked, the file removed.
fn reference_bytes(route: Route, output: &Path) -> Vec<u8> {
    assert_from_input(route, output, &format!("{route:?}"));
    pinned_cell::take(output)
}

/// Check a decode `route` in every read mode through
/// [`pinned_cell::check`], into an absent destination and over an existing
/// one: the reference with its exact transports (the probe and the header
/// reader direct on a direct run, the walk buffered, following the degrade
/// row of `crates/cli/src/io_reach.rs`), then the replacement armed after
/// each open in turn, in both reopen modes.
fn check_decode_route(route: Route) {
    for &direct in pinned_cell::modes() {
        for existing in [false, true] {
            let route = Route { direct, ..route };
            let dir = crate::common::test_scratch();
            let input = dir.path().join("in.osm.pbf");
            let replacement = dir.path().join("replacement.osm.pbf");
            let reference = dir.path().join("reference.osm.pbf");
            let output = dir.path().join("out.osm.pbf");
            write_input(&input, route);
            let cell = Cell::new(
                format!("{route:?}, existing {existing}"),
                direct,
                &input,
                &replacement,
                DECODE_OPENS,
                "DDB",
            )
            .scratch(dir.path());
            pinned_cell::check(
                &cell,
                |host| run(route, &input, out(&reference), host),
                |()| reference_bytes(route, &reference),
                |_, _| {
                    write_input(&input, route);
                    write_replacement(&replacement);
                    if existing {
                        std::fs::write(&output, SENTINEL).expect("write destination");
                    }
                },
                |(), step| {
                    let staged = if existing {
                        out_replace(&output)
                    } else {
                        out(&output)
                    };
                    run(route, step.pin, staged, step.host)
                },
                |expected, (), step, judged| match judged {
                    Judged::Unaffected(()) => assert!(
                        pinned_cell::take(&output) == *expected,
                        "{}: the run must read only the input it pinned",
                        step.what
                    ),
                    Judged::Refused if existing => assert_eq!(
                        pinned_cell::take(&output),
                        SENTINEL,
                        "{}: a refused run must leave the destination as it was",
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
    }
}

#[test]
fn every_decode_route_reads_only_the_pinned_input() {
    for route in decode_routes() {
        check_decode_route(route);
    }
}

/// The passthrough through a caller's pin, in every read mode, through
/// [`pinned_cell::check`]: its reference pins the input itself, as the
/// passthrough takes no pin of its own, and sees one open, direct on a
/// direct run (the raw frames row of `crates/cli/src/io_reach.rs`). That
/// open is the last, so replaced right after it the run reads the pinned
/// file to the end and succeeds in both reopen modes. A second open of the
/// input, header and frames read apart, would be counted, and refused under
/// a path reopen.
fn check_passthrough_route(route: Route) {
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        write_input(&input, route);
        let cell = Cell::new(format!("{route:?}"), direct, &input, &replacement, 1, "D");
        pinned_cell::check(
            &cell,
            |host| {
                let pin = pbfhogg::read::input::PinnedInput::pin(&input, host)?;
                run(route, &pin, out(&output), host)
            },
            |()| {
                let bytes = reference_bytes(route, &output);
                // The path entry, which opens the path plainly, writes the
                // same bytes as the caller-pin entry the cell counts.
                let plain = dir.path().join("plain.osm.pbf");
                run(route, &input, out(&plain), &crate::common::test_host())
                    .unwrap_or_else(|e| panic!("{route:?}: run on the path: {e}"));
                assert!(
                    pinned_cell::take(&plain) == bytes,
                    "{route:?}: the path entry must write what the pinned entry does"
                );
                bytes
            },
            |_, _| {
                write_input(&input, route);
                write_replacement(&replacement);
            },
            |(), step| run(route, step.pin, out(&output), step.host),
            |expected, (), step, judged| {
                // Only the caller-pin arm (replaced before the call) can
                // refuse: after the one open nothing opens again.
                if matches!(judged, Judged::Refused) {
                    assert_eq!(step.after, 0, "{}: the one open is the last", step.what);
                    assert!(
                        !output.exists(),
                        "{}: a refused run must publish nothing",
                        step.what
                    );
                    return;
                }
                assert!(
                    pinned_cell::take(&output) == *expected,
                    "{}: the run must read only the file it opened",
                    step.what
                );
            },
        );
    }
}

#[test]
fn every_passthrough_route_reads_one_file_through_one_open() {
    for route in passthrough_routes() {
        check_passthrough_route(route);
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
/// The feeder (`common::FifoFeeder`) gives up if the run never opens the
/// FIFO; the hang bound covers a run that opens it and stops reading.
#[cfg(unix)]
#[test]
fn the_passthrough_reads_a_fifo() {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        pbfhogg::TEST_HANG_BOUND,
        pbfhogg::TEST_HANG_REPORT_GRACE,
        "the FIFO passthrough or its feeder did not end",
    );
    let route = Route::strip_indexdata();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let unreplaced = dir.path().join("unreplaced.osm.pbf");
    run(route, &input, out(&unreplaced), &crate::common::test_host())
        .unwrap_or_else(|e| panic!("{route:?}: unreplaced run: {e}"));
    let expected = reference_bytes(route, &unreplaced);
    let bytes = std::fs::read(&input).expect("read input");
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");

    let feeder = crate::common::FifoFeeder::start(&fifo, bytes);
    let result = run(route, &fifo, out(&output), &crate::common::test_host());
    let fed = feeder.finish();
    result.unwrap_or_else(|e| panic!("a FIFO serves the passthrough: {e}"));
    match fed {
        crate::common::Fed::Unfed => panic!("the run must have opened the FIFO"),
        crate::common::Fed::OpenFailed(e) => panic!("the feeder could not open the FIFO: {e}"),
        crate::common::Fed::Wrote(written) => {
            written.unwrap_or_else(|e| panic!("the passthrough read the whole FIFO: {e}"));
        }
    }
    assert!(
        std::fs::read(&output).expect("read output") == expected,
        "the output must equal the run on the file the FIFO carried"
    );
}
