//! Unit-test fixtures over the shared scratch owner, the dev-only
//! `pbfhogg-test-scratch` crate.

/// A marker FIFO in fresh scratch, with a read end held open (never read)
/// so the write end opens. Keep all three alive together.
pub(crate) fn marker_fifo() -> (
    pbfhogg_test_scratch::TestScratch,
    pbfhogg::debug::MarkerFifo,
    std::fs::File,
) {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = pbfhogg_test_scratch::scratch();
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
