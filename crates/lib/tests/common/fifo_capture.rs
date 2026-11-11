//! Owned FIFO capture with a wakeable reader and joined cleanup.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub struct FifoCapture {
    pub path: PathBuf,
    wake: UnixStream,
    reader: Option<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
}

impl FifoCapture {
    #[allow(clippy::disallowed_methods)] // FIFO owned by test scratch
    pub fn new(dir: &Path) -> Self {
        // FIFO creation has one owner, common::make_fifo; this file owns draining.
        let path = crate::common::make_fifo(dir, "sidecar.fifo");
        // Holding both ends prevents EOF/HUP spinning before a producer opens.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("open FIFO");
        #[cfg(target_os = "linux")]
        {
            // SAFETY: file owns this descriptor. A host refusal is harmless;
            // capture still drains concurrently at the default pipe capacity.
            let _size = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETPIPE_SZ, 1 << 20) };
        }
        let (wake, shutdown) = UnixStream::pair().expect("shutdown channel");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let mut fds = [
                    libc::pollfd {
                        fd: file.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: shutdown.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: fds is a live array of two descriptors owned above.
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                for fd in &fds {
                    if fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        return Err(io::Error::other("FIFO capture poll failed"));
                    }
                }
                loop {
                    match file.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                }
                if fds[1].revents != 0 {
                    return Ok(bytes);
                }
            }
        });
        Self {
            path,
            wake,
            reader: Some(reader),
        }
    }

    fn stop(&mut self) -> io::Result<Vec<u8>> {
        let Some(reader) = self.reader.take() else {
            return Ok(Vec::new());
        };
        // Closing the channel also wakes poll if the explicit signal fails.
        let signal = self.wake.write_all(&[1]);
        // A failed shutdown only means the peer is already gone.
        drop(self.wake.shutdown(std::net::Shutdown::Both));
        let result = reader
            .join()
            .map_err(|_| io::Error::other("FIFO reader panicked"))?;
        signal?;
        result
    }

    /// Producers must have finished writing before capture is stopped.
    pub fn finish(mut self) -> String {
        String::from_utf8(self.stop().expect("drain FIFO")).expect("UTF-8 stream")
    }
}

impl Drop for FifoCapture {
    fn drop(&mut self) {
        let result = self.stop();
        if !std::thread::panicking() {
            result.expect("join FIFO reader");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
