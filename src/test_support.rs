//! Skip support for the library's unit tests, which cannot reach the
//! integration harness in `tests/common`.
//!
//! A unit test that cannot run on this host announces it through the same
//! `skip` the integration tests use (`tests/common/skip.rs`, mounted by
//! `#[path]` as `path_guard` mounts the shared scratch allocator), and skips
//! only on the typed refusal of the feature it needs: a `PlatformUnsupported`
//! labelled for another feature, or any other error, fails the test.

#[path = "../tests/common/skip.rs"]
mod shared_skip;

pub(crate) use shared_skip::skip;

/// Whether `err` is the typed O_DIRECT setup refusal, the one error an
/// O_DIRECT test may skip on.
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
