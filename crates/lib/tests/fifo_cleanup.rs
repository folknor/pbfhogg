//! Own process: BrokenPipe proves the reader was closed only when no
//! concurrent fork can retain a copy before exec closes CLOEXEC descriptors.
//! The shared all binaries launch children, so they cannot host this assertion.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, reason = "test failures")]

#[path = "common/mod.rs"]
mod common;
#[path = "common/fifo_capture.rs"]
mod fifo_capture;

mod tests {
    use crate::fifo_capture::FifoCapture;
    use std::io::{self, Write};

    #[test]
    fn native_path_and_unwind_cleanup() {
        use std::os::unix::ffi::OsStringExt;
        let scratch = crate::common::test_scratch();
        let dir = scratch
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'f', 0xff]));
        std::fs::create_dir(&dir).expect("native directory");
        let capture = FifoCapture::new(&dir);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&capture.path)
            .expect("producer")
            .write_all(b"line\n")
            .expect("write");
        assert_eq!(capture.finish(), "line\n");
        std::fs::remove_file(dir.join("sidecar.fifo")).expect("remove FIFO");
        let capture = FifoCapture::new(&dir);
        let mut producer = std::fs::OpenOptions::new()
            .write(true)
            .open(&capture.path)
            .expect("producer stays open during cleanup");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _capture = capture;
            std::panic::resume_unwind(Box::new("deliberate unwind"));
        }));
        assert!(result.is_err());
        assert_eq!(
            producer
                .write_all(b"after cleanup")
                .expect_err("reader closed")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
