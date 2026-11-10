//! Scratch directories for this crate's unit tests.
//!
//! Random, RAII-cleaned directories under the workspace's gitignored
//! `data/test-scratch`, never the system temp directory. The integration
//! harnesses have their own copy in `crates/lib/tests/common/scratch.rs`; this crate's
//! unit tests cannot mount it, so they share this one.

/// The workspace root: the nearest ancestor of `CARGO_MANIFEST_DIR` that
/// holds `Cargo.lock`, as `crates/lib/tests/common/scratch.rs` finds it. Never a fixed
/// `..`, which would silently name a different directory if the package
/// moved deeper.
pub(crate) fn workspace_root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .expect("a Cargo.lock above the package manifest marks the workspace root")
}

/// A fresh scratch directory; the caller keeps the guard alive for the
/// fixture's lifetime, and dropping it removes the directory.
#[allow(clippy::disallowed_methods)] // fixture directories in test scratch, not operator input
pub(crate) fn scratch() -> tempfile::TempDir {
    let root = workspace_root().join("data/test-scratch");
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}
