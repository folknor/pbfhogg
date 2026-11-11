//! Controls for the shared element oracle (`common::model` and
//! `read_normalized`) and for the fixture writers' framing.
//!
//! The oracle must tell apart outputs that differ by one metadata claim, one
//! millisecond, one duplicate or reordered tag, or one unknown member kind,
//! and must refuse a string index it cannot resolve rather than read it as
//! empty. Inputs are hand-encoded, since `BlockBuilder` writes none of plain
//! nodes, partial `Info`, sub-second timestamps or bad indexes.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use crate::common::adversarial::{
    decompress_blob, locate_blobs_strict, without_fields, write_varint,
};
use crate::common::groups::write_unindexed_blocks;
use crate::common::model::{DecodedPbf, MemberKind, NormalizedMeta, decode_path};
use crate::common::{
    FixtureFraming, TestMeta, encode_fixture_blocks, frame_fixture_blocks, generate_nodes,
    generate_relations, generate_ways, read_all_elements_with_coords, read_normalized,
    write_indexed_pbf, write_multi_block_non_indexed_pbf, write_multi_block_test_pbf,
    write_test_pbf, write_test_pbf_non_indexed, write_test_pbf_sorted,
};

// ---------------------------------------------------------------------------
// Hand encoding
// ---------------------------------------------------------------------------

fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)).cast_unsigned()
}

#[derive(Default)]
struct Msg(Vec<u8>);

impl Msg {
    fn varint(mut self, field: u32, v: u64) -> Self {
        write_varint(&mut self.0, u64::from(field << 3));
        write_varint(&mut self.0, v);
        self
    }
    fn bytes(mut self, field: u32, b: &[u8]) -> Self {
        write_varint(&mut self.0, u64::from((field << 3) | 2));
        write_varint(&mut self.0, b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    fn packed(self, field: u32, values: &[u64]) -> Self {
        let mut p = Vec::new();
        for &v in values {
            write_varint(&mut p, v);
        }
        self.bytes(field, &p)
    }
    fn delta(self, field: u32, values: &[i64]) -> Self {
        let mut prev = 0i64;
        let d: Vec<u64> = values
            .iter()
            .map(|&v| {
                let out = zz(v.wrapping_sub(prev));
                prev = v;
                out
            })
            .collect();
        self.packed(field, &d)
    }
}

/// A plain `Info`, each field written only when `Some`.
#[derive(Clone, Copy, Default)]
struct Info {
    version: Option<i32>,
    timestamp: Option<i64>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user_sid: Option<u32>,
    visible: Option<bool>,
}

impl Info {
    fn full() -> Self {
        Self {
            version: Some(3),
            timestamp: Some(1_700_000_000_000),
            changeset: Some(42),
            uid: Some(7),
            user_sid: Some(1),
            visible: Some(true),
        }
    }

    fn encode(self) -> Vec<u8> {
        let mut m = Msg::default();
        if let Some(v) = self.version {
            m = m.varint(1, i64::from(v).cast_unsigned());
        }
        if let Some(v) = self.timestamp {
            m = m.varint(2, v.cast_unsigned());
        }
        if let Some(v) = self.changeset {
            m = m.varint(3, v.cast_unsigned());
        }
        if let Some(v) = self.uid {
            m = m.varint(4, i64::from(v).cast_unsigned());
        }
        if let Some(v) = self.user_sid {
            m = m.varint(5, u64::from(v));
        }
        if let Some(v) = self.visible {
            m = m.varint(6, u64::from(v));
        }
        m.0
    }
}

/// String table shared by every hand block. Index 1 is the user name.
const STRINGS: [&str; 6] = ["", "mapper", "highway", "primary", "name", "outer"];

/// A block of `groups` with date granularity 1, so a timestamp is in
/// milliseconds on the wire.
fn block(groups: &[Vec<u8>]) -> Vec<u8> {
    let mut table = Msg::default();
    for s in STRINGS {
        table = table.bytes(1, s.as_bytes());
    }
    let mut m = Msg::default().bytes(1, &table.0);
    for g in groups {
        m = m.bytes(2, g);
    }
    m.varint(18, 1).0
}

fn plain_node(id: i64, tags: &[(u64, u64)], info: Option<Info>) -> Vec<u8> {
    let mut n = Msg::default()
        .varint(1, zz(id))
        .packed(2, &tags.iter().map(|t| t.0).collect::<Vec<_>>())
        .packed(3, &tags.iter().map(|t| t.1).collect::<Vec<_>>());
    if let Some(info) = info {
        n = n.bytes(4, &info.encode());
    }
    let n = n.varint(8, zz(550_000_000)).varint(9, zz(120_000_000));
    Msg::default().bytes(1, &n.0).0
}

fn way(id: i64, tags: &[(u64, u64)], info: Option<Info>) -> Vec<u8> {
    let mut w = Msg::default()
        .varint(1, id.cast_unsigned())
        .packed(2, &tags.iter().map(|t| t.0).collect::<Vec<_>>())
        .packed(3, &tags.iter().map(|t| t.1).collect::<Vec<_>>());
    if let Some(info) = info {
        w = w.bytes(4, &info.encode());
    }
    let w = w.delta(8, &[1, 2]);
    Msg::default().bytes(3, &w.0).0
}

/// A relation whose one member has wire type `kind` and role index `role`.
fn relation(id: i64, kind: u64, role: u64) -> Vec<u8> {
    let r = Msg::default()
        .varint(1, id.cast_unsigned())
        .packed(8, &[role])
        .delta(9, &[10])
        .packed(10, &[kind]);
    Msg::default().bytes(4, &r.0).0
}

/// One dense node with every column, values as given.
fn dense_node(id: i64, version: i32, uid: i32, user_sid: i32, visible: bool) -> Vec<u8> {
    let info = Msg::default()
        .packed(1, &[i64::from(version).cast_unsigned()])
        .delta(2, &[1_700_000_000_000])
        .delta(3, &[42])
        .delta(4, &[i64::from(uid)])
        .delta(5, &[i64::from(user_sid)])
        .packed(6, &[u64::from(visible)]);
    let d = Msg::default()
        .delta(1, &[id])
        .bytes(5, &info.0)
        .delta(8, &[550_000_000])
        .delta(9, &[120_000_000]);
    Msg::default().bytes(2, &d.0).0
}

fn write(dir: &Path, name: &str, groups: &[Vec<u8>]) -> std::path::PathBuf {
    let path = dir.join(name);
    write_unindexed_blocks(&path, &[block(groups)]);
    path
}

fn refused(path: &Path) -> bool {
    catch_unwind(AssertUnwindSafe(|| decode_path(path))).is_err()
}

fn way_meta(d: &DecodedPbf) -> Option<NormalizedMeta> {
    d.ways[0].meta.clone()
}

// ---------------------------------------------------------------------------
// Counterexamples
// ---------------------------------------------------------------------------

/// [`Info::full`] with one field changed, removed or made explicit.
fn one_field_variants() -> Vec<(&'static str, Info)> {
    let full = Info::full();
    vec![
        (
            "version",
            Info {
                version: Some(4),
                ..full
            },
        ),
        (
            "one millisecond",
            Info {
                timestamp: Some(1_700_000_000_001),
                ..full
            },
        ),
        (
            "changeset",
            Info {
                changeset: Some(43),
                ..full
            },
        ),
        (
            "uid",
            Info {
                uid: Some(8),
                ..full
            },
        ),
        (
            "user",
            Info {
                user_sid: Some(2),
                ..full
            },
        ),
        (
            "visible false",
            Info {
                visible: Some(false),
                ..full
            },
        ),
        (
            "visible absent",
            Info {
                visible: None,
                ..full
            },
        ),
        (
            "timestamp absent",
            Info {
                timestamp: None,
                ..full
            },
        ),
        (
            "changeset absent",
            Info {
                changeset: None,
                ..full
            },
        ),
        ("uid absent", Info { uid: None, ..full }),
        (
            "user absent",
            Info {
                user_sid: None,
                ..full
            },
        ),
        (
            "uid zero",
            Info {
                uid: Some(0),
                ..full
            },
        ),
    ]
}

/// Every metadata field, changed by one unit or one millisecond, made absent,
/// or (for `visible`) made explicit, changes the decoded element; and the
/// production reader agrees with each decode.
#[test]
fn one_metadata_field_or_millisecond_differs() {
    let dir = crate::common::test_scratch();
    let base = write(dir.path(), "base.pbf", &[way(1, &[], Some(Info::full()))]);
    let base_meta = way_meta(&read_normalized_decoded(&base));
    assert_eq!(
        base_meta,
        Some(NormalizedMeta {
            version: Some(3),
            milli_timestamp: Some(1_700_000_000_000),
            changeset: Some(42),
            uid: Some(7),
            user: Some("mapper".to_owned()),
            visible: Some(true),
        })
    );
    let full = Info::full();
    for (what, info) in one_field_variants() {
        let path = write(dir.path(), "variant.pbf", &[way(1, &[], Some(info))]);
        let meta = way_meta(&read_normalized_decoded(&path));
        assert_ne!(meta, base_meta, "{what} must change the decoded claims");
    }

    // An unversioned plain Info keeps its other fields: the version alone is
    // no claim.
    let unversioned = write(
        dir.path(),
        "unversioned.pbf",
        &[way(
            1,
            &[],
            Some(Info {
                version: None,
                ..full
            }),
        )],
    );
    let meta = way_meta(&read_normalized_decoded(&unversioned)).expect("fields still claimed");
    assert_eq!(meta.version, None);
    assert_eq!(meta.uid, Some(7));
}

/// The two encodings claim alike where both can say it, and differ where
/// the dense one cannot: a versionless dense slot's zero uid is padding,
/// a plain Info's present zero is a claim.
#[test]
fn plain_and_dense_presence_rules() {
    let dir = crate::common::test_scratch();
    let plain = write(
        dir.path(),
        "plain.pbf",
        &[plain_node(1, &[], Some(Info::full()))],
    );
    let dense = write(dir.path(), "dense.pbf", &[dense_node(1, 3, 7, 1, true)]);
    assert_eq!(
        read_normalized_decoded(&plain).nodes,
        read_normalized_decoded(&dense).nodes,
        "complete metadata claims alike in both encodings"
    );

    let plain_zero = write(
        dir.path(),
        "plain_zero.pbf",
        &[plain_node(
            1,
            &[],
            Some(Info {
                version: Some(0),
                uid: Some(0),
                ..Info::default()
            }),
        )],
    );
    let dense_zero = write(
        dir.path(),
        "dense_zero.pbf",
        &[dense_node(1, 0, 0, 0, true)],
    );
    let plain_meta = read_normalized_decoded(&plain_zero).nodes[0].meta.clone();
    let dense_meta = read_normalized_decoded(&dense_zero).nodes[0].meta.clone();
    assert_eq!(
        plain_meta,
        Some(NormalizedMeta {
            uid: Some(0),
            ..NormalizedMeta::default()
        })
    );
    // The dense slot still claims its non-zero timestamp and changeset, but
    // not the zero uid, the empty user or `visible` true.
    assert_eq!(
        dense_meta,
        Some(NormalizedMeta {
            milli_timestamp: Some(1_700_000_000_000),
            changeset: Some(42),
            ..NormalizedMeta::default()
        })
    );
}

/// Tags compare in stored order, duplicate keys included.
#[test]
fn duplicate_and_reordered_tags_differ() {
    let dir = crate::common::test_scratch();
    let decode = |name: &str, tags: &[(u64, u64)]| {
        read_normalized_decoded(&write(dir.path(), name, &[way(1, tags, None)])).ways[0]
            .tags
            .clone()
    };
    let one = decode("one.pbf", &[(2, 3)]);
    let twice = decode("twice.pbf", &[(2, 3), (2, 3)]);
    assert_eq!(twice.len(), 2, "a duplicate key is kept");
    assert_ne!(one, twice);
    let ab = decode("ab.pbf", &[(2, 3), (4, 1)]);
    let ba = decode("ba.pbf", &[(4, 1), (2, 3)]);
    assert_ne!(ab, ba, "tag order is content");
}

/// An unknown member kind keeps its wire value.
#[test]
fn unknown_member_kinds_keep_their_value() {
    let dir = crate::common::test_scratch();
    let three = decode_path(&write(dir.path(), "three.pbf", &[relation(1, 3, 5)]));
    let four = decode_path(&write(dir.path(), "four.pbf", &[relation(1, 4, 5)]));
    assert_eq!(
        three.relations[0].members[0].member_type,
        MemberKind::Unknown(3)
    );
    assert_ne!(three.relations, four.relations);
    let known = decode_path(&write(dir.path(), "known.pbf", &[relation(1, 1, 5)]));
    assert_eq!(known.relations[0].members[0].member_type, "way");
    assert_eq!(known.relations[0].members[0].role, "outer");
}

/// A string index past the table is refused, never read as empty: tag
/// keys and values, roles, plain and dense users. The production-reader
/// helpers refuse a bad tag and role too.
#[test]
fn unresolvable_strings_are_refused() {
    let dir = crate::common::test_scratch();
    let past = STRINGS.len() as u64;
    let bad_tag = write(dir.path(), "tag.pbf", &[way(1, &[(past, 3)], None)]);
    let bad_value = write(dir.path(), "value.pbf", &[way(1, &[(2, past)], None)]);
    let bad_role = write(dir.path(), "role.pbf", &[relation(1, 1, past)]);
    let bad_user = write(
        dir.path(),
        "user.pbf",
        &[way(
            1,
            &[],
            Some(Info {
                user_sid: Some(u32::try_from(past).expect("small")),
                ..Info::full()
            }),
        )],
    );
    let bad_dense_user = write(
        dir.path(),
        "dense_user.pbf",
        &[dense_node(
            1,
            3,
            7,
            i32::try_from(past).expect("small"),
            true,
        )],
    );
    for path in [&bad_tag, &bad_value, &bad_role, &bad_user, &bad_dense_user] {
        assert!(refused(path), "{} must be refused", path.display());
    }
    for path in [&bad_tag, &bad_role] {
        assert!(
            catch_unwind(AssertUnwindSafe(|| read_all_elements_with_coords(path))).is_err(),
            "the production-reader helper must refuse {}",
            path.display()
        );
    }
}

/// `read_normalized`, keeping the independent decode's file order for the
/// single-element inputs above.
fn read_normalized_decoded(path: &Path) -> DecodedPbf {
    let n = read_normalized(path);
    DecodedPbf {
        header: true,
        nodes: n.nodes,
        ways: n.ways,
        relations: n.relations,
    }
}

// ---------------------------------------------------------------------------
// Fixture framing
// ---------------------------------------------------------------------------

/// Each frame's BlobHeader with indexdata (2) and tagdata (4) removed, and
/// its decompressed payload.
fn frames(path: &Path) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let pbf = std::fs::read(path).expect("read fixture");
    let strict = locate_blobs_strict(&pbf).expect("complete framing");
    strict
        .frames
        .into_iter()
        .map(|(loc, kind)| {
            let header = &pbf[loc.header_start..loc.header_end];
            let payload =
                decompress_blob(&pbf[loc.blob_start..loc.blob_end]).expect("decompress blob");
            (kind, without_fields(header, &[2, 3, 4]), payload)
        })
        .collect()
}

fn has_index_fields(path: &Path) -> Vec<bool> {
    let pbf = std::fs::read(path).expect("read fixture");
    locate_blobs_strict(&pbf)
        .expect("complete framing")
        .frames
        .into_iter()
        .filter(|(_, kind)| kind == b"OSMData")
        .map(|(loc, _)| {
            let header = &pbf[loc.header_start..loc.header_end];
            without_fields(header, &[2, 4]).len() != header.len()
        })
        .collect()
}

/// Fixture writers that differ only in framing write the same blocks: every
/// frame of a pair has the same kind, the same payload bytes and the same
/// BlobHeader once indexdata, tagdata and datasize are set aside; the
/// indexed side carries the index fields on every data blob and the
/// non-indexed side on none.
#[test]
fn paired_fixtures_differ_only_in_their_envelope() {
    let dir = crate::common::test_scratch();
    let mut nodes = generate_nodes(250, 1);
    for n in nodes.iter_mut().step_by(3) {
        n.meta = Some(TestMeta::default());
        n.tags.push(("amenity", "bench"));
    }
    let ways = generate_ways(30, 1, 4, 1);
    let relations = generate_relations(5, 1, 2, 1);

    let sorted = dir.path().join("sorted.pbf");
    let unsorted = dir.path().join("unsorted.pbf");
    let non_indexed = dir.path().join("non_indexed.pbf");
    let pipelined = dir.path().join("pipelined.pbf");
    write_test_pbf_sorted(&sorted, &nodes, &ways, &relations);
    write_test_pbf(&unsorted, &nodes, &ways, &relations);
    write_test_pbf_non_indexed(&non_indexed, &nodes, &ways, &relations);
    write_indexed_pbf(&pipelined, &nodes, &ways, &relations);

    let multi = dir.path().join("multi.pbf");
    let multi_non_indexed = dir.path().join("multi_non_indexed.pbf");
    write_multi_block_test_pbf(&multi, &nodes, &ways, &relations, 100);
    write_multi_block_non_indexed_pbf(&multi_non_indexed, &nodes, &ways, &relations, 100);

    assert_eq!(frames(&sorted), frames(&non_indexed));
    assert_eq!(frames(&sorted), frames(&pipelined));
    assert_eq!(frames(&multi), frames(&multi_non_indexed));
    assert!(
        frames(&multi).len() > frames(&sorted).len(),
        "the override splits blocks"
    );

    // The sort flag is the header's alone.
    let (s, u) = (frames(&sorted), frames(&unsorted));
    assert_eq!(s[1..], u[1..], "data frames do not depend on the sort flag");
    assert_ne!(s[0], u[0], "the header frame carries the sort flag");

    for path in [&sorted, &pipelined, &multi] {
        assert!(
            has_index_fields(path).iter().all(|&x| x),
            "{}",
            path.display()
        );
    }
    for path in [&non_indexed, &multi_non_indexed] {
        assert!(
            has_index_fields(path).iter().all(|&x| !x),
            "{}",
            path.display()
        );
    }

    // The writers are the encoder and the framing step, nothing else.
    let direct = dir.path().join("direct.pbf");
    frame_fixture_blocks(
        &direct,
        &encode_fixture_blocks(&nodes, &ways, &relations, Some(100)),
        FixtureFraming::NON_INDEXED,
    );
    assert_eq!(
        std::fs::read(&direct).expect("direct"),
        std::fs::read(&multi_non_indexed).expect("multi non-indexed")
    );
}
