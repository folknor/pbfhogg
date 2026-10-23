//! Scratch allocation shared by unit and integration test harnesses.

use std::path::{Path, PathBuf};

pub(super) fn path_override(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The test data root: `PBFHOGG_TEST_DATA_DIR` when set, else the
/// repository's `data` directory. Dataset fallbacks and the default scratch
/// root both hang off this one answer.
pub(super) fn test_data_dir() -> PathBuf {
    path_override("PBFHOGG_TEST_DATA_DIR")
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("data"))
}

/// Random names isolate tests and processes; TempDir cleans up on unwinding.
pub fn test_scratch() -> tempfile::TempDir {
    let root = path_override("PBFHOGG_TEST_SCRATCH_DIR")
        .unwrap_or_else(|| test_data_dir().join("test-scratch"));
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}
