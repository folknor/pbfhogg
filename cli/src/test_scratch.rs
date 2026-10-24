//! Scratch directories for this crate's unit tests.
//!
//! Random, RAII-cleaned directories under the workspace's gitignored
//! `data/test-scratch`, never the system temp directory. The integration
//! harnesses have their own copy in `tests/common/scratch.rs`; this crate's
//! unit tests cannot mount it, so they share this one.

/// A fresh scratch directory; the caller keeps the guard alive for the
/// fixture's lifetime, and dropping it removes the directory.
#[allow(clippy::disallowed_methods)] // fixture directories in test scratch, not operator input
pub(crate) fn scratch() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/test-scratch");
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}
