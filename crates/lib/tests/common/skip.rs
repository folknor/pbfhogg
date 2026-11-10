//! The one skip report, shared by every harness: integration tests reach it
//! as `common::skip`, and the library's unit tests as `test_support::skip`,
//! which mounts this file by `#[path]` so a unit test announces its skip the
//! same way without linking the integration harness.

/// Report a test (or the named portion of one) that cannot run on this host.
/// The caller returns or drops that portion itself.
pub fn skip(reason: impl std::fmt::Display) {
    eprintln!("Skipping: {reason}");
}
