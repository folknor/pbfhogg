//! Parser robustness and semantic codec properties, entirely in memory.
//!
//! Three properties run 64 cases each (192 total) with a fixed seed and explicit runner
//! settings. No PROPTEST_* setting changes coverage, shrinking or replay.
//! Persistence stays disabled; promote failures to named regression tests.

use std::io::Cursor;

use pbfhogg::block_builder::{BlockBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, Info, MemberId, PrimitiveBlock};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestCaseResult, TestRunner};

// Do not use Config::default or proptest!: both consult the environment.
// Spelling every field also makes new upstream settings a review decision.
fn property_config() -> Config {
    Config {
        cases: 64,
        max_local_rejects: 65_536,
        max_global_rejects: 1024,
        max_flat_map_regens: 1_000_000,
        failure_persistence: None,
        source_file: None,
        test_name: None,
        fork: false,
        timeout: 0,
        max_shrink_time: 0,
        max_shrink_iters: 256,
        max_default_size_range: 100,
        result_cache: proptest::test_runner::noop_result_cache,
        verbose: 0,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x7062_6668_6f67_6701),
        _non_exhaustive: (),
    }
}

fn run_property<S: Strategy>(strategy: S, check: impl Fn(S::Value) -> TestCaseResult) {
    TestRunner::new(property_config())
        .run(&strategy, check)
        .expect("property failed; promote the minimized input to a named regression");
}

#[test]
fn resolved_runner_settings_and_case_count() {
    let runner = TestRunner::new(property_config());
    let config = runner.config();
    assert_eq!(config.cases, 64);
    assert_eq!(config.rng_seed, RngSeed::Fixed(0x7062_6668_6f67_6701));
    assert_eq!(config.rng_algorithm, RngAlgorithm::ChaCha);
    assert!(config.failure_persistence.is_none());
    assert!(!config.fork);
    assert_eq!(config.timeout, 0);
    assert_eq!(config.max_shrink_iters, 256);
    assert_eq!(config.max_local_rejects, 65_536);
    assert_eq!(config.max_global_rejects, 1024);
    assert_eq!(config.max_flat_map_regens, 1_000_000);
    assert_eq!(config.max_default_size_range, 100);
    assert_eq!(config.max_shrink_time, 0);
    assert_eq!(config.verbose, 0);
    let calls = std::cell::Cell::new(0);
    run_property(Just(()), |()| {
        calls.set(calls.get() + 1);
        Ok(())
    });
    assert_eq!(calls.get(), 64);
    let sequences = [
        std::cell::RefCell::new(Vec::new()),
        std::cell::RefCell::new(Vec::new()),
    ];
    for sequence in &sequences {
        run_property(any::<u64>(), |value| {
            sequence.borrow_mut().push(value);
            Ok(())
        });
    }
    assert_eq!(*sequences[0].borrow(), *sequences[1].borrow());
    let failure = TestRunner::new(property_config()).run(&Just(()), |()| {
        Err(proptest::test_runner::TestCaseError::fail(
            "runner control failure",
        ))
    });
    assert!(matches!(
        failure,
        Err(proptest::test_runner::TestError::Fail(_, ()))
    ));
}

#[test]
fn primitive_block_from_arbitrary_bytes_never_panics() {
    run_property(proptest::collection::vec(any::<u8>(), 0..2048), |bytes| {
        drop(PrimitiveBlock::from_vec(bytes));
        Ok(())
    });
}

// Every successful frame consumes at least its four-byte length prefix.
// Errors must end the walk; reaching a cap alone is not evidence of EOF.
fn assert_reader_terminates(bytes: &[u8]) -> bool {
    let mut reader = BlobReader::new(Cursor::new(bytes));
    for _ in 0..=bytes.len() / 4 {
        match reader.next() {
            None => return false,
            Some(Err(_)) => {
                assert!(reader.next().is_none(), "reader resumed after refusal");
                return true;
            }
            Some(Ok(_)) => {}
        }
    }
    panic!("reader did not terminate within the framing bound");
}

#[test]
fn blob_reader_arbitrary_bytes_never_panics_and_terminates() {
    run_property(proptest::collection::vec(any::<u8>(), 0..4096), |bytes| {
        assert_reader_terminates(&bytes);
        Ok(())
    });
}

/// Tags in order, duplicate keys allowed: the codec keeps both.
type Tags = Vec<(String, String)>;
/// Version, timestamp in seconds (the builder's unit), changeset, uid,
/// user, visible.
type Meta = (i32, i64, i64, i32, String, bool);

#[derive(Clone, Debug)]
struct CodecCase {
    id: i64,
    lat: i32,
    lon: i32,
    tags: Tags,
    meta: Option<Meta>,
    refs: Vec<i64>,
    members: Vec<(u8, i64, String)>,
}

fn codec_cases() -> impl Strategy<Value = CodecCase> {
    (
        -1_000_000i64..1_000_000,
        -900_000_000i32..=900_000_000,
        -1_800_000_000i32..=1_800_000_000,
        // Short keys, so duplicates and unsorted orders occur.
        proptest::collection::vec(("[a-c]{1,2}", ".{0,16}"), 0..5),
        proptest::option::of((
            1i32..20,
            0i64..2_000_000_000,
            0i64..1_000_000,
            0i32..100_000,
            ".{0,12}",
            any::<bool>(),
        )),
        proptest::collection::vec(-1_000_000i64..1_000_000, 0..12),
        proptest::collection::vec((0u8..3, -1_000_000i64..1_000_000, ".{0,12}"), 0..12),
    )
        .prop_map(|(id, lat, lon, tags, meta, refs, members)| CodecCase {
            id,
            lat,
            lon,
            tags,
            meta,
            refs,
            members,
        })
}

fn member_id(kind: u8, id: i64) -> MemberId {
    match kind {
        0 => MemberId::Node(id),
        1 => MemberId::Way(id),
        2 => MemberId::Relation(id),
        _ => panic!("invalid generated member kind"),
    }
}

fn encode(cases: &[CodecCase]) -> Vec<u8> {
    let mut writer = PbfWriter::new(Vec::new(), Compression::None);
    let header = pbfhogg::write::header_builder::HeaderBuilder::new()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    for kind in 0..3 {
        let mut builder = BlockBuilder::new();
        for case in cases {
            let meta = case.meta.as_ref().map(|m| Metadata {
                version: m.0,
                timestamp: m.1,
                changeset: m.2,
                uid: m.3,
                user: &m.4,
                visible: m.5,
            });
            let members: Vec<_> = case
                .members
                .iter()
                .map(|(kind, id, role)| MemberData {
                    id: member_id(*kind, *id),
                    role,
                })
                .collect();
            let tags = case.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
            match kind {
                0 => builder.add_node(case.id, case.lat, case.lon, tags, meta.as_ref()),
                1 => builder.add_way(case.id, tags, &case.refs, meta.as_ref()),
                _ => builder.add_relation(case.id, tags, &members, meta.as_ref()),
            }
        }
        writer
            .write_primitive_block(
                builder
                    .take()
                    .expect("build block")
                    .expect("nonempty block"),
            )
            .expect("write block");
    }
    writer.into_inner().expect("finish memory fixture")
}

/// The generated metadata is all six fields or none, so an `Info` carrying
/// only some of them is a codec failure here (a fixture-shape assertion, not
/// a metadata validity rule). Timestamps compare in milliseconds against
/// the seconds the builder took, so no sub-second residue is truncated away.
fn info_meta(info: &Info<'_>) -> Option<(i32, i64, i64, i32, String, bool)> {
    let fields = (
        info.version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user(),
    );
    match fields {
        (None, None, None, None, None) => None,
        (Some(v), Some(ms), Some(cs), Some(uid), Some(user)) => Some((
            v,
            ms,
            cs,
            uid,
            user.expect("user string").to_owned(),
            info.visible(),
        )),
        _ => panic!("partial Info for a complete or absent generated one"),
    }
}

/// A case's metadata in [`info_meta`]'s form.
fn expected_meta(meta: Option<&Meta>) -> Option<(i32, i64, i64, i32, String, bool)> {
    meta.map(|m| (m.0, m.1 * 1000, m.2, m.3, m.4.clone(), m.5))
}

fn checked<'a, E: std::fmt::Display>(
    tags: impl Iterator<Item = Result<(&'a str, &'a str), E>>,
) -> Tags {
    crate::common::checked_tags(tags, "codec case")
}

/// The case's metadata as the independent decoder must read it back: a
/// dense slot claims `visible` always; the writer omits a plain `visible`
/// true, which a plain `Info` then does not claim.
fn expected_model(case: &CodecCase, plain: bool) -> Option<crate::common::NormalizedMeta> {
    case.meta.as_ref().map(|m| crate::common::NormalizedMeta {
        version: Some(m.0),
        milli_timestamp: Some(m.1 * 1000),
        changeset: Some(m.2),
        uid: Some(m.3),
        user: Some(m.4.clone()),
        visible: (!plain || !m.5).then_some(m.5),
    })
}

/// The writer half checked without pbfhogg's reader: the independent decode
/// of `bytes` is exactly the cases, nodes then ways then relations.
fn assert_model_roundtrip(cases: &[CodecCase], bytes: &[u8]) {
    use crate::common::{MemberKind, NormalizedMember};
    let decoded = crate::common::model::decode_bytes(bytes);
    assert!(decoded.header);
    let nodes: Vec<_> = decoded
        .nodes
        .iter()
        .map(|n| {
            (
                n.id,
                n.decimicro_lat(),
                n.decimicro_lon(),
                n.tags.clone(),
                n.meta.clone(),
            )
        })
        .collect();
    let want: Vec<_> = cases
        .iter()
        .map(|c| (c.id, c.lat, c.lon, c.tags.clone(), expected_model(c, false)))
        .collect();
    assert_eq!(nodes, want, "nodes");
    for n in &decoded.nodes {
        assert_eq!(n.nano_lat % 100, 0);
        assert_eq!(n.nano_lon % 100, 0);
    }
    let ways: Vec<_> = decoded
        .ways
        .iter()
        .map(|w| {
            (
                w.id,
                w.refs.clone(),
                w.locations.clone(),
                w.tags.clone(),
                w.meta.clone(),
            )
        })
        .collect();
    let want: Vec<_> = cases
        .iter()
        .map(|c| {
            (
                c.id,
                c.refs.clone(),
                None,
                c.tags.clone(),
                expected_model(c, true),
            )
        })
        .collect();
    assert_eq!(ways, want, "ways");
    let relations: Vec<_> = decoded
        .relations
        .iter()
        .map(|r| (r.id, r.members.clone(), r.tags.clone(), r.meta.clone()))
        .collect();
    let want: Vec<_> = cases
        .iter()
        .map(|c| {
            let members = c
                .members
                .iter()
                .map(|(k, id, role)| NormalizedMember {
                    member_type: match k {
                        0 => MemberKind::Node,
                        1 => MemberKind::Way,
                        _ => MemberKind::Relation,
                    },
                    ref_id: *id,
                    role: role.clone(),
                })
                .collect();
            (c.id, members, c.tags.clone(), expected_model(c, true))
        })
        .collect();
    assert_eq!(relations, want, "relations");
}

fn assert_roundtrip(cases: &[CodecCase], bytes: &[u8]) {
    assert_model_roundtrip(cases, bytes);
    let mut counts = [0; 3];
    for blob in BlobReader::new(Cursor::new(bytes)) {
        let blob = blob.expect("frame");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                let kind = match &element {
                    Element::Node(_) | Element::DenseNode(_) => 0,
                    Element::Way(_) => 1,
                    Element::Relation(_) => 2,
                    // `Element` is non_exhaustive outside the crate.
                    _ => panic!("unexpected element kind"),
                };
                let case = &cases[counts[kind]];
                let (id, tags, meta) = match element {
                    Element::DenseNode(node) => {
                        counts[0] += 1;
                        assert_eq!(
                            (node.decimicro_lat(), node.decimicro_lon()),
                            (case.lat, case.lon)
                        );
                        let meta = node.info().map(|i| {
                            (
                                i.version(),
                                i.milli_timestamp(),
                                i.changeset(),
                                i.uid(),
                                i.user().expect("dense user").to_owned(),
                                i.visible(),
                            )
                        });
                        (node.id(), checked(node.tags_checked()), meta)
                    }
                    Element::Node(node) => {
                        counts[0] += 1;
                        assert_eq!(
                            (node.decimicro_lat(), node.decimicro_lon()),
                            (case.lat, case.lon)
                        );
                        (
                            node.id(),
                            checked(node.tags_checked()),
                            info_meta(&node.info()),
                        )
                    }
                    Element::Way(way) => {
                        counts[1] += 1;
                        assert_eq!(way.refs().collect::<Vec<_>>(), case.refs);
                        (
                            way.id(),
                            checked(way.tags_checked()),
                            info_meta(&way.info()),
                        )
                    }
                    Element::Relation(relation) => {
                        counts[2] += 1;
                        let members: Vec<_> = relation
                            .members()
                            .map(|m| (m.id, m.role().expect("role").to_owned()))
                            .collect();
                        let expected: Vec<_> = case
                            .members
                            .iter()
                            .map(|(k, id, r)| (member_id(*k, *id), r.clone()))
                            .collect();
                        assert_eq!(members, expected);
                        (
                            relation.id(),
                            checked(relation.tags_checked()),
                            info_meta(&relation.info()),
                        )
                    }
                    _ => panic!("unexpected element kind"),
                };
                assert_eq!(id, case.id);
                assert_eq!(tags, case.tags);
                assert_eq!(meta, expected_meta(case.meta.as_ref()));
            }
        }
    }
    assert!(
        counts == [cases.len(); 3],
        "element counts by kind {counts:?} for {} cases, first {:?}",
        cases.len(),
        cases.first()
    );
}

#[test]
fn semantic_codec_roundtrips() {
    run_property(proptest::collection::vec(codec_cases(), 1..8), |cases| {
        assert_roundtrip(&cases, &encode(&cases));
        Ok(())
    });
}

#[test]
fn codec_strategy_control_cases() {
    // Force the semantic endpoints instead of trusting random selection to
    // include empty collections, all member types, metadata and both signs.
    for id in [-1, 0, 1] {
        for populated in [false, true] {
            // Includes the all-default id-0 way and relation: no tags, refs,
            // members or metadata, which the writer must still emit.
            let case = CodecCase {
                id,
                lat: -900_000_000,
                lon: 1_800_000_000,
                tags: if populated {
                    vec![
                        ("name".into(), "\u{00e9}\n".into()),
                        ("a".into(), String::new()),
                        ("name".into(), "second".into()),
                    ]
                } else {
                    Vec::new()
                },
                meta: populated.then(|| (2, 123, 456, 7, "user".into(), false)),
                refs: if populated { vec![1, -1, 1, 0] } else { vec![] },
                members: if populated {
                    vec![
                        (0, -1, String::new()),
                        (1, 2, "outer".into()),
                        (2, 0, "inner".into()),
                    ]
                } else {
                    vec![]
                },
            };
            let cases = [case.clone(), case];
            assert_roundtrip(&cases, &encode(&cases));
        }
    }
}

#[test]
fn truncated_fixture_terminates_and_refuses_incomplete_frames() {
    let case = CodecCase {
        id: 1,
        lat: 2,
        lon: 3,
        tags: Vec::new(),
        meta: None,
        refs: vec![1],
        members: vec![],
    };
    let bytes = encode(&[case]);
    let mut frame_start = 0;
    let mut checked = 0;
    while frame_start < bytes.len() {
        let header_len = u32::from_be_bytes(
            bytes[frame_start..frame_start + 4]
                .try_into()
                .expect("prefix"),
        ) as usize;
        // The complete reader gives the frame's end without duplicating the
        // BlobHeader protobuf decoder in the oracle.
        let cursor = Cursor::new(&bytes[frame_start..]);
        let mut reader = BlobReader::new_seekable(cursor).expect("seekable memory reader");
        reader.next().expect("frame").expect("valid frame");
        let frame_len = usize::try_from(
            reader
                .seek_raw(std::io::SeekFrom::Current(0))
                .expect("frame end"),
        )
        .expect("small memory fixture");
        for offset in 0..frame_len {
            let refused = assert_reader_terminates(&bytes[..frame_start + offset]);
            // EOF on a frame boundary or in its short length prefix is
            // deliberately tolerated. Once the prefix is complete, a short
            // header or body must be refused.
            assert_eq!(
                refused,
                offset >= 4,
                "cut at {frame_start}+{offset}, header {header_len}"
            );
            checked += 1;
        }
        frame_start += frame_len;
    }
    assert_eq!(checked, bytes.len());
    assert!(!assert_reader_terminates(&bytes));
}
