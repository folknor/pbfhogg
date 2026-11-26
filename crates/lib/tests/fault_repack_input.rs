//! repack against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it. With a
//! stable scratch reopen name a replaced input does not affect the run,
//! and where opens go through the path the run fails with
//! `InputReplaced` and publishes nothing. Each route is checked with the
//! replacement armed after each of its opens in turn, the last included.
//! All three opens are taken on the calling thread, one at a time, so the
//! counts are exact.
//!
//! The opens, in order, on every route: the indexdata probe, the header
//! reader, then the schedule walk, whose descriptor every per-kind pool
//! preads. A forced unindexed input, direct reads, a `LocationsOnWays`
//! input and a kind with no blobs change none of them.
//!
//! The replacement differs from the input everywhere a reading open could
//! see it: it carries no indexdata (a probe of it refuses a run that is not
//! forced), the opposite `LocationsOnWays` declaration (a header read of it
//! changes the output header) and other elements (a walk of it changes the
//! output).

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::read::input::InputSource;
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

use crate::common::{out, out_replace};

/// Every route opens the input three times after the pin.
const OPENS: u64 = 3;

/// The element cap. Input blobs of 25 elements split into two full output
/// blocks framed by the worker and a trailing 5 that the merge thread
/// coalesces across input blobs, so both output paths carry elements.
const CAP: usize = 10;

/// Elements per input blob.
const INPUT_BLOCK: usize = 25;

#[derive(Clone, Copy, Debug)]
struct Route {
    /// The input carries indexdata; without it the run is forced.
    indexed: bool,
    /// The header declares `LocationsOnWays` and the ways carry inline
    /// coordinates.
    low: bool,
    /// The input holds relations; without them the relation schedule is
    /// empty.
    relations: bool,
    direct: bool,
}

impl Route {
    const PLAIN: Self = Self {
        indexed: true,
        low: false,
        relations: true,
        direct: false,
    };
}

/// One element as the output must carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Node {
        id: i64,
        lat: i32,
        lon: i32,
        tags: Vec<(String, String)>,
    },
    Way {
        id: i64,
        refs: Vec<i64>,
        locations: Vec<(i32, i32)>,
        tags: Vec<(String, String)>,
    },
    Relation {
        id: i64,
        members: Vec<i64>,
        tags: Vec<(String, String)>,
    },
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
    low: bool,
}

impl Spec {
    fn input(route: Route) -> Self {
        Self {
            first_node: 1,
            nodes: 60,
            first_way: 1_001,
            ways: 30,
            first_relation: 2_001,
            relations: if route.relations { 3 } else { 0 },
            value: "original",
            low: route.low,
        }
    }

    /// Other IDs, other values, the opposite `LocationsOnWays` declaration.
    fn replacement(route: Route) -> Self {
        Self {
            first_node: 501,
            nodes: 40,
            first_way: 5_001,
            ways: 20,
            first_relation: 6_001,
            relations: 2,
            value: "replacement",
            low: !route.low,
        }
    }

    fn coord(id: i64) -> (i32, i32) {
        let c = i32::try_from(id).expect("small id") * 1_000;
        (c, c + 7)
    }

    fn items(&self) -> Vec<Item> {
        let tag = |k: &str| vec![(k.to_string(), self.value.to_string())];
        let mut items = Vec::new();
        for i in 0..self.nodes {
            let id = self.first_node + i64::try_from(i).expect("small");
            let (lat, lon) = Self::coord(id);
            items.push(Item::Node {
                id,
                lat,
                lon,
                tags: tag("n"),
            });
        }
        for i in 0..self.ways {
            let id = self.first_way + i64::try_from(i).expect("small");
            let first = self.first_node + i64::try_from(i).expect("small");
            let refs = vec![first, first + 1, first + 2];
            let locations = if self.low {
                refs.iter().map(|&r| Self::coord(r)).collect()
            } else {
                Vec::new()
            };
            items.push(Item::Way {
                id,
                refs,
                locations,
                tags: tag("w"),
            });
        }
        for i in 0..self.relations {
            let id = self.first_relation + i64::try_from(i).expect("small");
            items.push(Item::Relation {
                id,
                members: vec![self.first_way + i64::try_from(i).expect("small")],
                tags: tag("r"),
            });
        }
        items
    }
}

/// Write `spec` sorted, `INPUT_BLOCK` elements per blob, one kind per blob.
fn write_fixture(path: &Path, spec: &Spec, indexed: bool) {
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = HeaderBuilder::new().sorted();
    if spec.low {
        header = header.optional_feature("LocationsOnWays");
    }
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
    let items = spec.items();
    let mut kind_of_last: Option<std::mem::Discriminant<Item>> = None;
    let mut in_block = 0;
    for item in &items {
        let kind = std::mem::discriminant(item);
        if kind_of_last.is_some_and(|k| k != kind) || in_block == INPUT_BLOCK {
            flush(&mut bb, &mut writer);
            in_block = 0;
        }
        kind_of_last = Some(kind);
        in_block += 1;
        match item {
            Item::Node { id, lat, lon, tags } => {
                bb.add_node(
                    *id,
                    *lat,
                    *lon,
                    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    None,
                );
            }
            Item::Way {
                id,
                refs,
                locations,
                tags,
            } => {
                let tags = tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
                if locations.is_empty() {
                    bb.add_way(*id, tags, refs, None);
                } else {
                    bb.add_way_with_locations(*id, tags, refs, locations, None);
                }
            }
            Item::Relation { id, members, tags } => {
                let members: Vec<MemberData<'_>> = members
                    .iter()
                    .map(|&m| MemberData {
                        id: pbfhogg::MemberId::Way(m),
                        role: "outer",
                    })
                    .collect();
                bb.add_relation(
                    *id,
                    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    &members,
                    None,
                );
            }
        }
    }
    flush(&mut bb, &mut writer);
    writer.flush().expect("flush fixture");
}

fn write_input(path: &Path, route: Route) {
    write_fixture(path, &Spec::input(route), route.indexed);
}

fn write_replacement(path: &Path, route: Route) {
    write_fixture(path, &Spec::replacement(route), false);
}

/// Every element of the file at `path` in file order, and the element count
/// of each data blob.
fn read_items(path: &Path) -> (Vec<Item>, Vec<usize>) {
    type Checked<'a> = dyn Iterator<Item = (&'a str, &'a str)> + 'a;
    let tags =
        |it: &mut Checked<'_>| -> Vec<(String, String)> { crate::common::checked_tags(it, "item") };
    let mut items = Vec::new();
    let mut blobs = Vec::new();
    for blob in BlobReader::from_path(path).expect("open output") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        let before = items.len();
        for element in block.elements() {
            items.push(match element {
                Element::DenseNode(n) => Item::Node {
                    id: n.id(),
                    lat: n.decimicro_lat().expect("representable coordinate"),
                    lon: n.decimicro_lon().expect("representable coordinate"),
                    tags: tags(&mut n.tags()),
                },
                Element::Node(n) => Item::Node {
                    id: n.id(),
                    lat: n.decimicro_lat().expect("representable coordinate"),
                    lon: n.decimicro_lon().expect("representable coordinate"),
                    tags: tags(&mut n.tags()),
                },
                Element::Way(w) => Item::Way {
                    id: w.id(),
                    refs: w.refs().collect(),
                    locations: w
                        .node_locations()
                        .map(|l| {
                            (
                                l.decimicro_lat().expect("representable coordinate"),
                                l.decimicro_lon().expect("representable coordinate"),
                            )
                        })
                        .collect(),
                    tags: tags(&mut w.tags()),
                },
                Element::Relation(r) => Item::Relation {
                    id: r.id(),
                    members: r.members().map(|m| m.id.id()).collect(),
                    tags: tags(&mut r.tags()),
                },
                _ => continue,
            });
        }
        blobs.push(items.len() - before);
    }
    (items, blobs)
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
    pbfhogg::repack::repack(
        input,
        output,
        std::num::NonZeroUsize::new(CAP).expect("non-zero cap"),
        &write_options(route),
        &pbfhogg::HeaderOverrides::default(),
        host,
    )
    .map(drop)
}

/// What a refused run leaves at an existing destination.
const SENTINEL: &[u8] = b"an existing destination";

/// Check `route` in every read mode through [`pinned_cell::check`], into
/// an absent destination and over an existing one: the reference with its
/// exact transports (the probe and the header reader direct on a direct
/// run, the walk buffered, following the repack row of
/// `crates/cli/src/io_reach.rs`), then the replacement armed after each
/// open in turn, in both reopen modes.
fn check_route(route: Route) {
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
                OPENS,
                "DDB",
            )
            .scratch(dir.path());
            pinned_cell::check(
                &cell,
                |host| run(route, &input, out(&reference), host),
                |()| {
                    let (items, blobs) = read_items(&reference);
                    assert_eq!(
                        items,
                        Spec::input(route).items(),
                        "{route:?}: the unreplaced output must carry every input element, in order"
                    );
                    assert!(
                        blobs.iter().all(|&n| (1..=CAP).contains(&n)),
                        "{route:?}: every output blob holds 1 to {CAP} elements: {blobs:?}"
                    );
                    assert_eq!(
                        crate::common::read_header(&reference).has_locations_on_ways(),
                        route.low,
                        "{route:?}: the output header follows the input's"
                    );
                    pinned_cell::take(&reference)
                },
                |_, _| {
                    write_input(&input, route);
                    write_replacement(&replacement, route);
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
fn an_indexed_input_reads_only_the_pinned_input() {
    check_route(Route::PLAIN);
}

/// Forced on an input without indexdata: every blob is replicated into all
/// three schedules, and the probe, header read and walk still open once
/// each.
#[test]
fn a_forced_unindexed_input_reads_only_the_pinned_input() {
    check_route(Route {
        indexed: false,
        ..Route::PLAIN
    });
}

/// Inline way coordinates ride both output paths; the replacement declares
/// no `LocationsOnWays`, so a header read of it changes the output.
#[test]
fn a_locations_on_ways_input_reads_only_the_pinned_input() {
    check_route(Route {
        low: true,
        ..Route::PLAIN
    });
}

/// No relations: the relation schedule is empty, and the opens are the
/// same.
#[test]
fn an_absent_kind_reads_only_the_pinned_input() {
    check_route(Route {
        relations: false,
        ..Route::PLAIN
    });
}

/// The input is pinned, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer), and nothing is published.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let output = dir.path().join("out.osm.pbf");
    for route in [
        Route::PLAIN,
        Route {
            indexed: false,
            ..Route::PLAIN
        },
    ] {
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
