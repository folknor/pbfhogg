//! build-geocode-index against an input renamed over mid-build, through the
//! fault plan on the run's token (`arm_input_replacement`, see
//! `pbfhogg::test_hooks`).
//!
//! Every open of the input after the build's pin is derived from it, so on a
//! host that reopens through `/proc/self/fd` a replaced input does not
//! affect the build, and where opens go through the path the build fails
//! with `InputReplaced` and publishes nothing. The replacement is armed
//! after each open in turn, the last included. Every open is taken on the
//! calling thread, between passes, so the counts are exact.
//!
//! The opens, in order: the indexdata probe, the replication header read,
//! pass 1's relation scan (all three `O_DIRECT` under direct reads), then
//! the schedule walk, whose one description the unindexed classify, pass
//! 1.5 and pass 2 share.
//!
//! The replacement differs in what every open feeds the index: its header
//! carries other replication values, its boundary relation another name and
//! its ways and nodes other names and coordinates, so a build that read any
//! open's worth of it publishes other bytes, or fails.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pbfhogg::MemberId;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::geocode_index::builder::{BuildConfig, BuildStats, build_geocode_index, index_output};
use pbfhogg::read::input::PinnedInput;
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
fn run(
    route: Route,
    input: &Path,
    index_dir: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<BuildStats> {
    let output = index_output(index_dir, pbfhogg::Overwrite::Replace, &host.diagnostics)?;
    let scratch = crate::common::scratch_in(index_dir.parent().expect("index dir has a parent"));
    build_geocode_index(
        &BuildConfig {
            input_path: input.to_path_buf(),
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

/// The index built from `fixture`, checked by query.
fn assert_index_of(index_dir: &Path, fixture: &Fixture, cell: &str) {
    let reader = pbfhogg::geocode_index::reader::Reader::open(index_dir).expect("open index");
    assert_eq!(
        reader.replication_sequence(),
        u32::try_from(fixture.sequence).expect("small sequence"),
        "{cell}"
    );
    assert_eq!(
        reader.replication_timestamp(),
        u64::try_from(fixture.timestamp).expect("positive timestamp"),
        "{cell}"
    );
    let lat = 55.700_05 + f64::from(fixture.lat_shift) * 1e-7;
    let result = reader.query(lat, 12.5005).expect("valid query");
    let address = result.address.expect("an address at the address node");
    assert_eq!(
        (address.house_number, address.street),
        (fixture.house_number, fixture.street),
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

/// What [`reference`] found.
enum Reference {
    /// Every file of the unreplaced build's index.
    Index(std::collections::BTreeMap<PathBuf, Vec<u8>>),
    /// The host cannot run the route.
    Skipped,
}

/// The unreplaced build's index, its open count and content checked: armed
/// past the last open, the replacement never fires and only counts. Skipped
/// when the build failed and `skippable` accepts its error; any other
/// failure panics.
fn reference(
    route: Route,
    input: &Path,
    dir: &Path,
    skippable: impl Fn(&pbfhogg::Error) -> bool,
) -> Reference {
    let index_dir = dir.join("reference-index");
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .arm_input_replacement(input, &dir.join("unused.osm.pbf"), OPENS + 1);
    match run(route, input, &index_dir, &host) {
        Ok(_) => {}
        Err(e) if skippable(&e) => {
            crate::common::skip(format!("{route:?}: {e}"));
            return Reference::Skipped;
        }
        Err(e) => panic!("{route:?}: unreplaced build: {e}"),
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
    assert_index_of(&index_dir, &INPUT, &format!("{route:?}, unreplaced"));
    let files = index_files(&index_dir);
    std::fs::remove_dir_all(&index_dir).expect("remove reference index");
    Reference::Index(files)
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

/// Run `route` with the replacement armed after each open in turn, in both
/// reopen modes, against the unreplaced `expected`.
fn check_replacements(
    route: Route,
    dir: &Path,
    input: &Path,
    expected: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
) {
    let replacement = dir.join("replacement.osm.pbf");
    let index_dir = dir.join("index");
    for after in 1..=OPENS {
        for force_path in [true, false] {
            seed_old_index(route, dir, &index_dir);
            let old = index_files(&index_dir);
            assert_ne!(
                &old, expected,
                "the old index must differ from the reference"
            );
            write_fixture(input, &INPUT, route.indexdata);
            write_fixture(&replacement, &REPLACEMENT, route.indexdata);
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
            let result = run(route, input, &index_dir, &host);

            let cell = format!("{route:?}, replaced after open {after}, procfs {procfs}");
            let report = host
                .cancel
                .faults()
                .input_replacement()
                .expect("armed replacement");
            assert!(
                report.fired,
                "{cell}: the replacement must fire: {report:?}"
            );

            // After the last open nothing opens again, so the build reads
            // only what it already holds, in either mode.
            if procfs || after == OPENS {
                result.unwrap_or_else(|e| {
                    panic!("{cell}: the build is unaffected by the replacement: {e}")
                });
                assert_eq!(report.attempts, OPENS, "{cell}: {report:?}");
                assert_eq!(report.verified, OPENS, "{cell}: {report:?}");
                assert_eq!(report.refused, 0, "{cell}: {report:?}");
                assert_eq!(
                    report.attempts_after_fired,
                    OPENS - after,
                    "{cell}: {report:?}"
                );
                assert_index_of(&index_dir, &INPUT, &cell);
                assert_eq!(
                    &index_files(&index_dir),
                    expected,
                    "{cell}: the index must hold only what the pinned input yields"
                );
            } else if cfg!(unix) {
                // Off Unix a pin verifies nothing; the identity refusal is
                // a Unix guarantee.
                let err = result
                    .err()
                    .unwrap_or_else(|| panic!("{cell}: must refuse"));
                assert!(
                    matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                    "{cell}: {err:?}"
                );
                assert_eq!(err.source_path(), Some(input), "{cell}: {err:?}");
                assert_eq!(report.refused, 1, "{cell}: {report:?}");
                assert_eq!(report.verified, after, "{cell}: {report:?}");
                assert_eq!(report.attempts, after + 1, "{cell}: {report:?}");
                assert_eq!(report.attempts_after_fired, 1, "{cell}: {report:?}");
                assert_eq!(
                    index_files(&index_dir),
                    old,
                    "{cell}: a refused build must leave the existing index as it was"
                );
                // The replacement was renamed over the input, so it is the
                // one path the build itself did not account for.
                let mut after_run = snapshot_dir(dir);
                after_run.insert(PathBuf::from("replacement.osm.pbf"));
                crate::common::assert_scratch_unchanged(&before, &after_run);
            }
            std::fs::remove_dir_all(&index_dir).expect("remove index");
        }
    }
}

fn check_route(route: Route, skippable: impl Fn(&pbfhogg::Error) -> bool) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input, &INPUT, route.indexdata);
    let Reference::Index(expected) = reference(route, &input, dir.path(), skippable) else {
        return;
    };
    check_replacements(route, dir.path(), &input, &expected);
}

#[test]
fn indexed_build_reads_only_the_pinned_input() {
    check_route(
        Route {
            indexdata: true,
            direct: false,
        },
        |_| false,
    );
}

/// `--allow-unindexed`: the walk decodes the unindexed blobs through its
/// own description, and the open count is unchanged.
#[test]
fn unindexed_build_reads_only_the_pinned_input() {
    check_route(
        Route {
            indexdata: false,
            direct: false,
        },
        |_| false,
    );
}

/// The probe, the header read and pass 1 open `O_DIRECT` when asked to,
/// through the pin's own direct open; the walk stays buffered.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_build_reads_only_the_pinned_input() {
    // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
    check_route(
        Route {
            indexdata: true,
            direct: true,
        },
        |e| crate::common::is_einval(e),
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
