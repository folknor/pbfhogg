//! build-geocode-index against an input renamed over mid-build, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the build's pin is derived from it, so with
//! a stable scratch reopen name (`reopen_cell::Scenario`, injected through
//! the builder's caller-pin input) a replaced input does not affect the build, and where opens go through the path the build fails
//! with `InputReplaced` and publishes nothing. The replacement is armed
//! after each open in turn, the last included. Every open is taken on the
//! calling thread, between passes, so the counts are exact.
//!
//! The opens, in order: the indexdata probe and the replication header read
//! (both `O_DIRECT` under direct reads), then the schedule walk, whose one
//! description the unindexed classify, pass 1, pass 1.5 and pass 2 share,
//! then the whole-file digest's own description (opened on the calling
//! thread, read on its worker).
//!
//! The replacement differs in what every open feeds the index: its header
//! carries other replication values, its boundary relation another name and
//! its ways and nodes other names and coordinates, so a build that read any
//! open's worth of it publishes other bytes, or fails.

use crate::pinned_cell::{self, Cell, Judged};

use std::path::{Path, PathBuf};

use pbfhogg::MemberId;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::geocode_index::builder::{BuildConfig, BuildStats, build_geocode_index, index_output};
use pbfhogg::writer::{Compression, PbfWriter};

use crate::common::snapshot_dir;

/// Every build opens the input this many times, on every route.
const OPENS: u64 = 4;

/// What one fixture puts in the index.
struct Fixture {
    sequence: i64,
    timestamp: i64,
    /// Added to every latitude, in decimicrodegrees.
    lat_shift: i32,
    street: &'static str,
    house_number: &'static str,
    country: &'static str,
}

const INPUT: Fixture = Fixture {
    sequence: 4_242,
    timestamp: 1_700_000_000,
    lat_shift: 0,
    street: "Pinned Street",
    house_number: "10",
    country: "Pinnedland",
};

/// Another place, other names, another replication position.
const REPLACEMENT: Fixture = Fixture {
    sequence: 9_999,
    timestamp: 1_800_000_000,
    lat_shift: 100_000_000,
    street: "Other Street",
    house_number: "99",
    country: "Otherland",
};

/// One node blob (two street nodes, an address node, a square's four
/// corners), one way blob (the street and the square's ring) and one
/// relation blob (an admin level 2 boundary over the ring), with or
/// without indexdata.
fn write_fixture(path: &Path, fixture: &Fixture, indexdata: bool) {
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new()
        .sorted()
        .replication_sequence_number(fixture.sequence)
        .replication_timestamp(fixture.timestamp)
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    let mut write = |bb: &mut BlockBuilder| {
        let bytes = bb.take().expect("take block").expect("non-empty block");
        if indexdata {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    };

    let shift = fixture.lat_shift;
    let mut bb = BlockBuilder::new();
    let none = std::iter::empty::<(&str, &str)>;
    bb.add_node(1, 557_000_000 + shift, 125_000_000, none(), None);
    bb.add_node(2, 557_000_000 + shift, 125_010_000, none(), None);
    bb.add_node(
        3,
        557_000_500 + shift,
        125_005_000,
        [
            ("addr:housenumber", fixture.house_number),
            ("addr:street", fixture.street),
        ],
        None,
    );
    bb.add_node(10, 556_990_000 + shift, 124_990_000, none(), None);
    bb.add_node(11, 556_990_000 + shift, 125_020_000, none(), None);
    bb.add_node(12, 557_010_000 + shift, 125_020_000, none(), None);
    bb.add_node(13, 557_010_000 + shift, 124_990_000, none(), None);
    write(&mut bb);

    bb.add_way(
        20,
        [("highway", "residential"), ("name", fixture.street)],
        &[1, 2],
        None,
    );
    bb.add_way(30, none(), &[10, 11, 12, 13, 10], None);
    write(&mut bb);

    bb.add_relation(
        40,
        [
            ("type", "boundary"),
            ("boundary", "administrative"),
            ("admin_level", "2"),
            ("name", fixture.country),
        ],
        &[MemberData {
            id: MemberId::Way(30),
            role: "outer",
        }],
        None,
    );
    write(&mut bb);
    drop(write);
    writer.flush().expect("flush fixture");
}

#[derive(Clone, Copy, Debug)]
struct Route {
    indexdata: bool,
    direct: bool,
}

/// Build the index of `input` into `index_dir`, replacing what is there.
fn run<'a>(
    route: Route,
    input: impl Into<pbfhogg::read::input::InputSource<'a>>,
    index_dir: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<BuildStats> {
    let output = index_output(index_dir, pbfhogg::Overwrite::Replace, &host.diagnostics)?;
    let scratch = crate::common::scratch_in(index_dir.parent().expect("index dir has a parent"));
    build_geocode_index(
        input,
        &BuildConfig {
            allow_unindexed: !route.indexdata,
            direct_io: route.direct,
            ..BuildConfig::default()
        },
        output,
        host,
        scratch,
    )
}

/// Every file of an index directory, by name.
fn index_files(index_dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(index_dir)
        .expect("list index")
        .map(|entry| {
            let entry = entry.expect("index entry");
            assert!(
                entry.file_type().expect("entry type").is_file(),
                "{:?}",
                entry.path()
            );
            (
                PathBuf::from(entry.file_name()),
                std::fs::read(entry.path()).expect("read index file"),
            )
        })
        .collect()
}

/// The whole-file XXH3-128 and size of the file at `path`.
fn identity_of(path: &Path) -> (u128, u64) {
    let bytes = std::fs::read(path).expect("read input");
    (xxhash_rust::xxh3::xxh3_128(&bytes), bytes.len() as u64)
}

/// The index built from `fixture`, whose input file had `identity`, checked
/// by its header and by query.
fn assert_index_of(index_dir: &Path, fixture: &Fixture, identity: (u128, u64), cell: &str) {
    let reader = pbfhogg::geocode_index::reader::Reader::open(index_dir).expect("open index");
    assert_eq!(
        (reader.input_xxh3_128(), reader.input_bytes()),
        identity,
        "{cell}: the header records the pinned input's digest"
    );
    assert_eq!(
        reader.replication_sequence(),
        Some(fixture.sequence),
        "{cell}"
    );
    assert_eq!(
        reader.replication_timestamp(),
        Some(fixture.timestamp),
        "{cell}"
    );
    let lat = 55.700_05 + f64::from(fixture.lat_shift) * 1e-7;
    let result = reader.query(lat, 12.5005).expect("valid query");
    let address = result.address.expect("an address at the address node");
    assert_eq!(
        (address.house_number, address.street),
        (fixture.house_number, Some(fixture.street)),
        "{cell}"
    );
    assert_eq!(
        result.street.expect("a street beside the address").name,
        fixture.street,
        "{cell}"
    );
    let admin: Vec<&str> = result.admin.iter().map(|a| a.name).collect();
    assert_eq!(admin, [fixture.country], "{cell}");
}

/// Put the index of [`REPLACEMENT`] at `index_dir`: a refused build must
/// leave it as it was, and a build that went through replaces it with the
/// reference's bytes, which differ from it.
fn seed_old_index(route: Route, dir: &Path, index_dir: &Path) {
    let old_input = dir.join("old.osm.pbf");
    write_fixture(&old_input, &REPLACEMENT, route.indexdata);
    run(route, &old_input, index_dir, &crate::common::test_host()).expect("build the old index");
    std::fs::remove_file(&old_input).expect("remove old input");
}

type Index = std::collections::BTreeMap<PathBuf, Vec<u8>>;

/// Check `route` in every read mode through [`pinned_cell::check`]: the
/// reference build with its exact transports (the probe and the
/// replication header read direct on a direct run, the schedule walk and
/// the digest's description buffered, following the build-geocode-index
/// row of `crates/cli/src/io_reach.rs`), then the replacement armed after
/// each open in turn, in both reopen modes, over an existing index.
fn check_route(route: Route) {
    for &direct in pinned_cell::modes() {
        let route = Route { direct, ..route };
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let reference_dir = dir.path().join("reference-index");
        let index_dir = dir.path().join("index");
        write_fixture(&input, &INPUT, route.indexdata);
        let cell = Cell::new(
            format!("{route:?}"),
            direct,
            &input,
            &replacement,
            OPENS,
            "DDBB",
        )
        .scratch(dir.path());
        pinned_cell::check(
            &cell,
            |host| run(route, &input, &reference_dir, host),
            |_| {
                assert_index_of(
                    &reference_dir,
                    &INPUT,
                    identity_of(&input),
                    &format!("{route:?}, unreplaced"),
                );
                let files = index_files(&reference_dir);
                std::fs::remove_dir_all(&reference_dir).expect("remove reference index");
                files
            },
            |_, _| {
                seed_old_index(route, dir.path(), &index_dir);
                let old = index_files(&index_dir);
                write_fixture(&input, &INPUT, route.indexdata);
                let identity = identity_of(&input);
                write_fixture(&replacement, &REPLACEMENT, route.indexdata);
                assert_ne!(identity_of(&replacement), identity);
                (old, identity)
            },
            |_, step| run(route, step.pin, &index_dir, step.host),
            |expected: &Index, (old, identity), step, judged| {
                assert_ne!(
                    old, expected,
                    "the old index must differ from the reference"
                );
                match judged {
                    Judged::Unaffected(_) => {
                        assert_index_of(&index_dir, &INPUT, *identity, &step.what);
                        assert_eq!(
                            &index_files(&index_dir),
                            expected,
                            "{}: the index must hold only what the pinned input yields",
                            step.what
                        );
                    }
                    Judged::Refused => assert_eq!(
                        &index_files(&index_dir),
                        old,
                        "{}: a refused build must leave the existing index as it was",
                        step.what
                    ),
                }
                std::fs::remove_dir_all(&index_dir).expect("remove index");
            },
        );
    }
}

#[test]
fn indexed_build_reads_only_the_pinned_input() {
    check_route(Route {
        indexdata: true,
        direct: false,
    });
}

/// `--allow-unindexed`: the walk decodes the unindexed blobs through its
/// own description, and the open count is unchanged.
#[test]
fn unindexed_build_reads_only_the_pinned_input() {
    check_route(Route {
        indexdata: false,
        direct: false,
    });
}

/// The index carries the input header's replication values verbatim: a
/// negative timestamp stays negative instead of wrapping to a huge unsigned
/// one, and a sequence beyond u32 is kept rather than refused.
#[test]
fn replication_values_outside_unsigned_ranges_are_kept_verbatim() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let fixture = Fixture {
        sequence: 5_000_000_000,
        timestamp: -1,
        ..INPUT
    };
    write_fixture(&input, &fixture, true);
    let index_dir = dir.path().join("index");
    let route = Route {
        indexdata: true,
        direct: false,
    };
    run(route, &input, &index_dir, &crate::common::test_host()).expect("build");
    assert_index_of(
        &index_dir,
        &fixture,
        identity_of(&input),
        "signed replication",
    );
}

/// The build pins, so a FIFO is refused up front rather than opened (a
/// blocking open of a FIFO waits for a writer), and nothing is published.
#[cfg(unix)]
#[test]
fn a_fifo_input_is_refused() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let index_dir = dir.path().join("index");
    let before = snapshot_dir(dir.path());
    let err = run(
        Route {
            indexdata: true,
            direct: false,
        },
        &fifo,
        &index_dir,
        &crate::common::test_host(),
    )
    .err()
    .unwrap_or_else(|| panic!("a FIFO must be refused"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err:?}"
    );
    assert!(
        err.report().contains("not a regular file"),
        "{}",
        err.report()
    );
    assert!(!index_dir.exists(), "a refused build publishes nothing");
    crate::common::assert_scratch_unchanged(&before, &snapshot_dir(dir.path()));
}
