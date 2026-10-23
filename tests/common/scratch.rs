//! Scratch allocation shared by unit and integration test harnesses.

/// Random names isolate tests and processes; TempDir cleans up on unwinding.
pub fn test_scratch() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/test-scratch");
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}
