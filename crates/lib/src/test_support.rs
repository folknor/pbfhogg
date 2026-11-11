//! Skip support for the library's unit tests, which cannot reach the
//! integration harness in `crates/lib/tests/common`.
//!
//! A unit test that cannot run on this host announces it through the same
//! `skip` the integration tests use (`crates/lib/tests/common/skip.rs`, mounted by
//! `#[path]` as `path_guard` mounts the shared scratch allocator), and skips
//! only on the typed refusal of the feature it needs: a `PlatformUnsupported`
//! labelled for another feature, or any other error, fails the test.

#[path = "../tests/common/skip.rs"]
mod shared_skip;

pub(crate) use shared_skip::skip;

/// Whether `err` is the typed O_DIRECT setup refusal, the one error an
/// O_DIRECT test may skip on.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn is_direct_io_refusal(err: &crate::Error) -> bool {
    matches!(
        err.kind(),
        crate::ErrorKind::PlatformUnsupported {
            feature: "direct-io",
            ..
        }
    )
}

/// Whether files in `dir` open `O_DIRECT`. A typed direct-io refusal is the
/// one `false`, announced as a skip of `portion` (the caller drops that
/// portion or returns); any other failure fails the test.
#[cfg(feature = "linux-direct-io")]
pub(crate) fn direct_reads_supported(dir: &std::path::Path, portion: &str) -> bool {
    let probe = dir.join("direct-probe.bin");
    std::fs::write(&probe, [0_u8; 16]).unwrap_or_else(|e| panic!("write probe: {e}"));
    match crate::direct_io::open_for_read(&probe) {
        Ok(_) => true,
        Err(e) if is_direct_io_refusal(&e) => {
            skip(format!("{portion}: O_DIRECT not supported: {e}"));
            false
        }
        Err(e) => panic!("{e}"),
    }
}

/// Build several data blobs without reading a checkout fixture. The returned
/// offset is inside the final payload, recorded while writing rather than
/// discovered through the reader under test.
#[allow(clippy::unwrap_used)]
pub(crate) fn multi_blob_pbf(path: &std::path::Path) -> usize {
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};
    let mut bytes = Vec::new();
    let mut final_frame = 0;
    {
        let mut writer = PbfWriter::new(&mut bytes, Compression::None);
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        writer.flush().unwrap();
    }
    for id in 1..=3 {
        final_frame = bytes.len();
        let mut block = BlockBuilder::new();
        block.add_node(id, 100, 200, std::iter::empty::<(&str, &str)>(), None);
        let payload = block.take().unwrap().unwrap();
        let mut writer = PbfWriter::new(&mut bytes, Compression::None);
        writer.write_primitive_block(payload).unwrap();
        writer.flush().unwrap();
    }
    let header_len =
        u32::from_be_bytes(bytes[final_frame..final_frame + 4].try_into().unwrap()) as usize;
    let cut = final_frame + 4 + header_len + 3;
    assert!(cut < bytes.len());
    std::fs::write(path, bytes).unwrap();
    cut
}
