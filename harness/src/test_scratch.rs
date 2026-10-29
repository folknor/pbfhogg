//! Scratch for this crate's unit tests: random, RAII-cleaned directories
//! under the workspace's gitignored `data/test-scratch`, as
//! `cli/src/test_scratch.rs` gives the CLI's.

/// A fresh scratch directory; dropping the guard removes it.
#[allow(clippy::disallowed_methods)] // fixture directories in test scratch
pub(crate) fn scratch() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/test-scratch");
    std::fs::create_dir_all(&root).expect("create test scratch root");
    tempfile::TempDir::new_in(root).expect("create test scratch directory")
}

/// A marker FIFO in fresh scratch, with a read end held open (never read)
/// so the write end opens. Keep all three alive together.
pub(crate) fn marker_fifo() -> (tempfile::TempDir, pbfhogg::debug::MarkerFifo, std::fs::File) {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = scratch();
    let path = dir.path().join("fifo");
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    #[allow(clippy::disallowed_methods)]
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .expect("open the read end");
    let fifo = pbfhogg::debug::MarkerFifo::open(&path).expect("open the FIFO");
    (dir, fifo, reader)
}
