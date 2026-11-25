//! A block no cut fits is refused where its input is known.
//!
//! A zlib blob may carry a payload of exactly the 32 MiB raw limit, which an
//! uncompressed blob cannot hold: its body adds a field head under the same
//! cap. A re-encoding command writing that blob's element uncompressed
//! builds a block the oversize cut has no plan for. The refusal must be the
//! writer's typed size error, located at the input blob, raised where the
//! block is built, not an unlocated failure of a later write or the flush.

use std::path::Path;

use pbfhogg::tags_filter::{TagsFilterOptions, tags_filter};
use pbfhogg::write::WriteOptions;

/// `MAX_BLOB_MESSAGE_SIZE`, the inclusive payload limit.
const RAW_LIMIT: usize = 32 << 20;

/// A sorted zlib file of one data blob: node 1 tagged `v=a...`, its payload
/// exactly [`RAW_LIMIT`] bytes.
fn write_input_at_the_limit(path: &Path) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};
    let encode = |len: usize| {
        let mut bb = BlockBuilder::new();
        let v = "a".repeat(len);
        bb.add_node(1, 0, 0, [("v", v.as_str())], None);
        bb.take()
            .expect("encode")
            .expect("non-empty block")
            .to_vec()
    };
    let probe = encode(RAW_LIMIT - 1024).len();
    let payload = encode(RAW_LIMIT - 1024 + (RAW_LIMIT - probe));
    assert_eq!(
        payload.len(),
        RAW_LIMIT,
        "fixture: the payload is not at the limit"
    );
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
        .expect("write header");
    writer.write_primitive_block(&payload).expect("write block");
    writer.flush().expect("flush fixture");
}

/// tags-filter re-encodes each blob's matches on the decode worker; its
/// refusal names the input, the blob ordinal and its frame offset.
#[test]
fn tags_filter_refuses_an_uncuttable_block_at_its_input_blob() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_input_at_the_limit(&input);
    let expressions = vec![String::from("n/v")];
    let opts = TagsFilterOptions {
        expression_strs: &expressions,
        add_referenced: None,
        invert: false,
        write: WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..WriteOptions::default()
        },
        distrust_metadata: false,
    };
    let host = crate::common::test_host();
    let result = tags_filter(
        input.as_path(),
        crate::common::out(&output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &host,
    );
    let err = match result {
        Ok(_) => panic!("tags-filter framed a block past the uncompressed limit"),
        Err(err) => err,
    };
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::BlobTooLarge { .. }),
        "not the size refusal: {}",
        err.report()
    );
    assert_eq!(err.source_path(), Some(input.as_path()), "{}", err.report());
    assert_eq!(err.blob_index(), Some(1), "{}", err.report());
    assert!(err.byte_offset().is_some(), "{}", err.report());
    assert!(!output.exists());
}
