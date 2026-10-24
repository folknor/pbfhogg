//! Scratch allocation shared by unit and integration test harnesses.

/// Random names isolate tests and processes; TempDir cleans up on unwinding.
///
/// Rooted at the workspace's `data/test-scratch`, whichever package mounts
/// this file. The workspace root is the nearest ancestor of
/// `CARGO_MANIFEST_DIR` that holds `Cargo.lock`.
pub fn test_scratch() -> tempfile::TempDir {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .expect("a Cargo.lock above the package manifest marks the workspace root");
    let root = workspace.join("data/test-scratch");
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}
