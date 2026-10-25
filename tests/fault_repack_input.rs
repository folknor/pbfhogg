//! repack against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! Every open of the input after the run's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the run, and where opens go through the path the run fails with
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

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::read::input::{InputSource, PinnedInput};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};

use crate::common::{out, out_replace, snapshot_dir};

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
    let tags = |it: &mut dyn Iterator<Item = (&str, &str)>| -> Vec<(String, String)> {
        it.map(|(k, v)| (k.to_string(), v.to_string())).collect()
    };
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
                    lat: n.decimicro_lat(),
                    lon: n.decimicro_lon(),
                    tags: tags(&mut n.tags()),
                },
                Element::Node(n) => Item::Node {
                    id: n.id(),
                    lat: n.decimicro_lat(),
                    lon: n.decimicro_lon(),
                    tags: tags(&mut n.tags()),
                },
                Element::Way(w) => Item::Way {
                    id: w.id(),
                    refs: w.refs().collect(),
                    locations: w
                        .node_locations()
                        .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
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
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), OPENS + 1);
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
    assert_eq!(counted.attempts, OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.verified, OPENS, "{route:?}: {counted:?}");
    assert_eq!(counted.refused, 0, "{route:?}: {counted:?}");

    let (items, blobs) = read_items(&output);
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
        crate::common::read_header(&output).has_locations_on_ways(),
        route.low,
        "{route:?}: the output header follows the input's"
    );
    let bytes = std::fs::read(&output).expect("read reference output");
    std::fs::remove_file(&output).expect("remove reference output");
    Reference::Bytes(bytes)
}

/// What [`reference`] found.
enum Reference {
    Bytes(Vec<u8>),
    /// The host cannot run the route.
    Skipped,
}

/// Run `route` with the replacement armed after each of its opens in turn,
/// in both reopen modes, into an absent destination and over an existing
/// one, against the unreplaced `expected` bytes.
fn check_replacements(route: Route, dir: &Path, input: &Path, expected: &[u8]) {
    let replacement = dir.join("replacement.osm.pbf");
    let output = dir.join("out.osm.pbf");
    const SENTINEL: &[u8] = b"an existing destination";
    for after in 1..=OPENS {
        for force_path in [true, false] {
            for existing in [false, true] {
                write_input(input, route);
                write_replacement(&replacement, route);
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
                if procfs || after == OPENS {
                    result.unwrap_or_else(|e| {
                        panic!("{cell}: the run is unaffected by the replacement: {e}")
                    });
                    assert_eq!(report.attempts, OPENS, "{cell}: {report:?}");
                    assert_eq!(report.verified, OPENS, "{cell}: {report:?}");
                    assert_eq!(report.refused, 0, "{cell}: {report:?}");
                    assert_eq!(
                        report.attempts_after_fired,
                        OPENS - after,
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

fn check_route(route: Route) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    let Reference::Bytes(expected) = reference(route, &input, dir.path(), |_| false) else {
        unreachable!("no failure is skippable here");
    };
    check_replacements(route, dir.path(), &input, &expected);
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

/// The probe and the header reader open `O_DIRECT` when asked to, through
/// the pin's own direct open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_reads_follow_the_pinned_input() {
    let route = Route {
        direct: true,
        ..Route::PLAIN
    };
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input, route);
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    let Reference::Bytes(expected) =
        reference(route, &input, dir.path(), |e| crate::common::is_einval(e))
    else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

/// A caller's pin is the run's pin: replaced between the caller's pin and
/// the call, the input is the file the caller pinned (procfs) or the first
/// open is refused and nothing is published (path reopen). An entry point
/// that pinned the path afresh would repack the replacement instead.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let route = Route::PLAIN;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    for force_path in [true, false] {
        write_input(&input, route);
        write_replacement(&replacement, route);
        let host = crate::common::test_host();
        host.cancel
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        std::fs::rename(&replacement, &input).expect("replace the input");
        let result = run(route, &pin, out(&output), &host);
        let cell = format!("procfs {procfs}");
        if procfs {
            result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            assert_eq!(
                read_items(&output).0,
                Spec::input(route).items(),
                "{cell}: the output must come from the pinned file"
            );
            // The replacement declares `LocationsOnWays`; a header read of it
            // would carry the declaration into the output.
            assert_eq!(
                crate::common::read_header(&output).has_locations_on_ways(),
                route.low,
                "{cell}: the output header must come from the pinned file"
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
