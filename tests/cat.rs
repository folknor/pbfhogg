//! Integration tests for the cat command.

mod common;

use common::{read_all_elements_with_coords, write_test_pbf, TestNode, TestWay, TestRelation, TestMember};
use pbfhogg::cat::{cat, CleanAttrs};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn cat_passthrough_buffered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let stats = cat(
        &[input.as_path()],
        &output,
        None,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat");

    assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

    let contents = read_all_elements_with_coords(&output);
    assert_eq!(contents.nodes.len(), 2);
    assert_eq!(contents.ways.len(), 1);
    assert_eq!(contents.relations.len(), 1);

    // Verify element data preserved
    assert_eq!(contents.nodes[0].0, 1);
    assert_eq!(contents.nodes[1].0, 2);
    assert_eq!(contents.ways[0].0, 10);
    assert_eq!(contents.relations[0].0, 20);
}

// ---------------------------------------------------------------------------
// O_DIRECT helper
// ---------------------------------------------------------------------------

/// Check if an error is EINVAL (O_DIRECT not supported on this filesystem).
#[cfg(feature = "linux-direct-io")]
fn is_einval(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        return io_err.raw_os_error() == Some(libc::EINVAL);
    }
    if let Some(pbf_err) = err.downcast_ref::<pbfhogg::Error>() {
        if let pbfhogg::ErrorKind::Io(io_err) = pbf_err.kind() {
            return io_err.raw_os_error() == Some(libc::EINVAL);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
#[test]
fn cat_passthrough_direct_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let result = cat(
        &[input.as_path()],
        &output,
        None,
        &CleanAttrs::default(),
        Compression::default(),
        true,
        true,
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(stats) => {
            assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

            let contents = read_all_elements_with_coords(&output);
            assert_eq!(contents.nodes.len(), 2);
            assert_eq!(contents.ways.len(), 1);
            assert_eq!(contents.relations.len(), 1);

            // Verify element data preserved
            assert_eq!(contents.nodes[0].0, 1);
            assert_eq!(contents.nodes[1].0, 2);
            assert_eq!(contents.ways[0].0, 10);
            assert_eq!(contents.relations[0].0, 20);
        }
        Err(e) if is_einval(&*e) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
            return;
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}
