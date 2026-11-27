//! Extraction refusal tests use the production writer cleanup path.

use std::path::Path;

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::extract::{
    ExtractStrategy, Polygon, PolygonRings, Region, extract, extract_multi, parse_bbox,
};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{
    Element, ErrorKind, HeaderOverrides, MemberId, OutputSet, Overwrite, PrimitiveBlock,
};
use protohoggr::encode_bytes_field_always;

use crate::common::adversarial::{
    blob_indexdata, locate_blobs, mutate_blob_payload, read_bytes_field, without_fields,
    write_varint,
};
use crate::common::{out, test_host, test_scratch};

const STRATEGIES: [ExtractStrategy; 3] = [
    ExtractStrategy::Simple,
    ExtractStrategy::CompleteWays,
    ExtractStrategy::Smart,
];

fn valid_blocks() -> [Vec<u8>; 3] {
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 0, 0, [("name", "fixture")], None);
    // Spanning the region boundary makes simple extract rebuild the blob.
    builder.add_node(2, 20_000_000, 20_000_000, std::iter::empty(), None);
    let nodes = builder
        .take()
        .expect("encode nodes")
        .expect("node block")
        .to_vec();
    builder.add_way(10, [("name", "fixture")], &[1], None);
    let ways = builder
        .take()
        .expect("encode ways")
        .expect("way block")
        .to_vec();
    builder.add_relation(
        20,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(10),
            role: "",
        }],
        None,
    );
    let relations = builder
        .take()
        .expect("encode relations")
        .expect("relation block")
        .to_vec();
    [nodes, ways, relations]
}

fn indexed_pbf(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut pbf = Vec::new();
    let mut writer = PbfWriter::new(&mut pbf, Compression::None);
    writer
        .write_header(&HeaderBuilder::new().sorted().build().expect("build header"))
        .expect("write header");
    for block in blocks {
        writer
            .write_primitive_block(block)
            .expect("write indexed fixture block");
    }
    writer.flush().expect("flush fixture");
    drop(writer);

    let frames = locate_blobs(&pbf);
    assert_eq!(frames.len(), blocks.len() + 1);
    for frame in &frames[1..] {
        assert!(
            blob_indexdata(&pbf[frame.header_start..frame.header_end]).is_some(),
            "every data blob must carry indexdata"
        );
    }
    pbf
}

/// Each fixture blob contains one group with one element message. Replace
/// a field while leaving IDs, coordinates, and dependency arrays intact.
fn replace_record_field(
    payload: &[u8],
    record_field: u32,
    field: u32,
    replacement: &[u8],
) -> Vec<u8> {
    let group = read_bytes_field(payload, 2).expect("fixture group");
    let record = read_bytes_field(&group, u64::from(record_field)).expect("fixture record");
    let mut record = without_fields(&record, &[u64::from(field)]);
    encode_bytes_field_always(&mut record, field, replacement);
    let mut group = without_fields(&group, &[u64::from(record_field)]);
    encode_bytes_field_always(&mut group, record_field, &record);
    let mut payload = without_fields(payload, &[2]);
    encode_bytes_field_always(&mut payload, 2, &group);
    payload
}

fn assert_refusal<T>(result: pbfhogg::Result<T>, expected: &ErrorKind) {
    let error = result
        .map(drop)
        .expect_err("extract must refuse the fixture");
    match (expected, error.kind()) {
        (ErrorKind::WireFormat { msg: expected }, ErrorKind::WireFormat { msg }) => {
            assert_eq!(msg, expected);
        }
        (
            ErrorKind::StringtableIndexOutOfBounds { index: expected },
            ErrorKind::StringtableIndexOutOfBounds { index },
        ) => assert_eq!(index, expected),
        _ => panic!("expected {expected:?}, got {}", error.report()),
    }
}

fn assert_extract_refusal(input: &Path, region: &Region, expected: &ErrorKind) {
    let dir = input.parent().expect("fixture directory");
    for strategy in STRATEGIES {
        let single = dir.join(format!("single-{strategy:?}.pbf"));
        let multi = [dir.join(format!("multi-{strategy:?}.pbf"))];
        let clean = CleanAttrs::default();
        let write = WriteOptions::default();
        let overrides = HeaderOverrides::default();
        assert_refusal(
            extract(
                input,
                out(&single),
                region,
                strategy,
                false,
                &clean,
                &write,
                &overrides,
                &test_host(),
            ),
            expected,
        );
        let outputs = OutputSet::files(
            &multi,
            Overwrite::Refuse,
            &pbfhogg::host::Diagnostics::discard(),
        )
        .expect("stage multi output");
        assert_refusal(
            extract_multi(
                input,
                std::slice::from_ref(region),
                outputs,
                strategy,
                false,
                &clean,
                &write,
                &overrides,
                &test_host(),
            ),
            expected,
        );
        assert!(!single.exists() && !multi[0].exists());
        assert_eq!(
            dir.read_dir().expect("inspect fixture directory").count(),
            1,
            "refusal removes staging for {strategy:?}"
        );
    }
}

#[test]
fn extract_refuses_malformed_retained_fields() {
    let scratch = test_scratch();
    let input = scratch.path().join("retained.pbf");
    let region = Region::Bbox(parse_bbox("-1,-1,1,1").expect("parse bbox"));
    let blocks = valid_blocks();
    let pbf = indexed_pbf(&blocks);
    let faults: [(usize, u32, u32, &[u8]); 4] = [
        (0, 2, 10, &[1, 127, 0, 0]), // DenseNodes.keys_vals
        (1, 3, 4, &[0x08]),          // Way.info with a truncated version
        (2, 4, 8, &[127]),           // Relation.roles_sid
        (2, 4, 3, &[127]),           // Relation.vals for the type tag
    ];
    for (block, record, field, replacement) in faults {
        let payload = replace_record_field(&blocks[block], record, field, replacement);
        let full = PrimitiveBlock::from_vec(payload.clone())
            .and_then(|block| {
                // Full construction checks the other three fields. A relation
                // role is only resolved when the full decoder re-encodes it.
                if let Some(Element::Relation(relation)) = block.elements().next() {
                    relation.members_into(&mut Vec::new())?;
                }
                Ok(())
            })
            .expect_err("full decode or retained field resolution must refuse the fixture");
        match (block, full.kind()) {
            (1, ErrorKind::WireFormat { .. }) => {}
            (0 | 2, ErrorKind::StringtableIndexOutOfBounds { index: 127 }) => {}
            _ => panic!(
                "unexpected full-decoder error for block {block}: {}",
                full.report()
            ),
        }
        let malformed = mutate_blob_payload(&pbf, block + 1, |bytes| *bytes = payload);
        std::fs::write(&input, malformed).expect("write malformed fixture");
        assert_extract_refusal(&input, &region, full.kind());
    }
}

#[test]
fn extract_classification_refuses_a_wrapping_dense_coordinate() {
    let scratch = test_scratch();
    let input = scratch.path().join("wrapping-dense.pbf");
    let [mut nodes, ..] = valid_blocks();
    // This narrows to 0 with a cast. Index the fixture before damaging
    // its tags so it carries an exact node claim without a spatial bbox.
    write_varint(&mut nodes, 19 << 3);
    write_varint(&mut nodes, 429_496_729_600);
    let pbf = indexed_pbf(&[nodes]);
    let malformed = mutate_blob_payload(&pbf, 1, |bytes| {
        // If classification accepted the wrapped value, the writer would
        // report this tag's invalid index before reaching the coordinate.
        *bytes = replace_record_field(bytes, 2, 10, &[1, 127, 0, 0]);
    });
    std::fs::write(&input, malformed).expect("write wrapping-coordinate fixture");
    let regions = [
        Region::Bbox(parse_bbox("-1,-1,1,1").expect("parse bbox")),
        Region::Polygon(
            Polygon::new(&[vec![PolygonRings {
                exterior: vec![
                    (-1.0, -1.0),
                    (1.0, -1.0),
                    (1.0, 1.0),
                    (-1.0, 1.0),
                    (-1.0, -1.0),
                ],
                holes: Vec::new(),
            }]])
            .expect("build polygon"),
        ),
    ];
    let expected = ErrorKind::WireFormat {
        msg: "node coordinate does not fit in an i32 decimicrodegree; \
              the blob's granularity, offset, or deltas are corrupt",
    };
    for region in &regions {
        assert_extract_refusal(&input, region, &expected);
    }
}
