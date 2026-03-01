//! O_DIRECT file reader with page-aligned buffering.
//!
//! [`DirectReader`] maintains a page-aligned internal buffer and reads in
//! page-aligned chunks via `libc::read`. This bypasses the kernel page cache
//! entirely, preventing cache pollution during planet-scale (80 GB+) PBF reads.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

/// Page size for alignment. 4096 is universally safe across Linux filesystems.
const PAGE_SIZE: usize = 4096;

/// Internal buffer capacity. Matches the 256 KB `BufReader` capacity used
/// elsewhere. Must be a multiple of `PAGE_SIZE`.
const BUF_CAPACITY: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// AlignedReadBuffer — page-aligned heap allocation for reads
// ---------------------------------------------------------------------------

/// A page-aligned byte buffer for O_DIRECT reads.
struct AlignedReadBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Number of valid bytes in the buffer (from the last `libc::read`).
    len: usize,
    /// Current read position within the buffer.
    pos: usize,
}

// Safety: AlignedReadBuffer is a plain owned byte array with no shared references.
// It is only accessed by the owning DirectReader.
unsafe impl Send for AlignedReadBuffer {}

impl AlignedReadBuffer {
    /// Allocate a new buffer of `capacity` bytes aligned to `PAGE_SIZE`.
    fn new(capacity: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(capacity, PAGE_SIZE)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // Safety: layout has non-zero size (BUF_CAPACITY > 0).
        let ptr = unsafe { alloc::alloc(layout) };
        let ptr = NonNull::new(ptr)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "aligned alloc failed"))?;
        Ok(Self {
            ptr,
            layout,
            len: 0,
            pos: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.layout.size()
    }

    /// Unread bytes remaining in the buffer.
    fn remaining(&self) -> usize {
        self.len - self.pos
    }

    /// Pointer to the current read position.
    fn read_ptr(&self) -> *const u8 {
        // Safety: pos <= len <= capacity, so ptr + pos is within the allocation.
        unsafe { self.ptr.as_ptr().add(self.pos) }
    }

    /// Advance the read position by `n` bytes.
    fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.remaining());
        self.pos += n;
    }

    /// Fill the buffer from a file descriptor via `libc::read`.
    /// Returns the number of bytes read (0 on EOF).
    fn fill_from(&mut self, fd: i32) -> io::Result<usize> {
        self.pos = 0;
        self.len = 0;
        // Safety: ptr is page-aligned, capacity is a multiple of PAGE_SIZE,
        // and fd is open with O_DIRECT.
        let n = unsafe {
            libc::read(fd, self.ptr.as_ptr().cast(), self.capacity())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        self.len = n.cast_unsigned();
        Ok(self.len)
    }
}

impl Drop for AlignedReadBuffer {
    fn drop(&mut self) {
        // Safety: ptr was allocated with this layout in `new`.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// ---------------------------------------------------------------------------
// DirectReader
// ---------------------------------------------------------------------------

/// A file reader that bypasses the page cache using `O_DIRECT`.
///
/// All reads from the underlying file descriptor use a page-aligned buffer
/// and page-aligned sizes. Data is served from the internal buffer, which
/// is refilled via `libc::read` when exhausted.
pub struct DirectReader {
    file: File,
    buf: AlignedReadBuffer,
    /// Whether we've hit EOF on the underlying fd.
    eof: bool,
}

impl DirectReader {
    /// Return the raw file descriptor for `copy_file_range`.
    pub(crate) fn raw_fd(&self) -> std::os::unix::io::RawFd {
        self.file.as_raw_fd()
    }

    /// Skip `n` bytes without materializing data.
    ///
    /// Discards any buffered bytes that overlap the skip range, then seeks
    /// the fd past the remainder.
    pub(crate) fn skip(&mut self, n: u64) -> io::Result<()> {
        let buffered = self.buf.remaining() as u64;
        if n <= buffered {
            self.buf.consume(n as usize);
            return Ok(());
        }
        // Consume remaining buffer, then lseek past the rest.
        let past_buf = n - buffered;
        self.buf.consume(buffered as usize);
        use std::io::{Seek, SeekFrom};
        self.file.seek(SeekFrom::Current(past_buf as i64))?;
        Ok(())
    }

    /// Open a file for reading with `O_DIRECT`.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)?;
        let buf = AlignedReadBuffer::new(BUF_CAPACITY)?;
        Ok(Self {
            file,
            buf,
            eof: false,
        })
    }
}

impl Read for DirectReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Serve from internal buffer if possible.
        if self.buf.remaining() > 0 {
            let n = out.len().min(self.buf.remaining());
            // Safety: read_ptr() is valid for remaining() bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(self.buf.read_ptr(), out.as_mut_ptr(), n);
            }
            self.buf.consume(n);
            return Ok(n);
        }

        // Buffer exhausted — refill from fd.
        if self.eof {
            return Ok(0);
        }

        let filled = self.buf.fill_from(self.file.as_raw_fd())?;
        if filled == 0 {
            self.eof = true;
            return Ok(0);
        }

        // Now serve from the freshly filled buffer.
        let n = out.len().min(self.buf.remaining());
        unsafe {
            std::ptr::copy_nonoverlapping(self.buf.read_ptr(), out.as_mut_ptr(), n);
        }
        self.buf.consume(n);
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn aligned_read_buffer_basics() {
        let buf = AlignedReadBuffer::new(PAGE_SIZE * 4).unwrap();
        assert_eq!(buf.len, 0);
        assert_eq!(buf.pos, 0);
        assert_eq!(buf.capacity(), PAGE_SIZE * 4);
        assert_eq!(buf.remaining(), 0);
        // Verify page alignment.
        assert_eq!(buf.ptr.as_ptr() as usize % PAGE_SIZE, 0);
    }

    #[test]
    fn direct_reader_reads_file() {
        // Write a test file with known content.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("bench-tmp");
        drop(std::fs::create_dir_all(&dir));
        let path = dir.join("direct_reader_test.bin");

        let data: Vec<u8> = (0..10000u32).flat_map(u32::to_le_bytes).collect();
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&data).unwrap();
        }

        // Read back via DirectReader.
        let result = DirectReader::open(&path);
        drop(std::fs::remove_file(&path));
        drop(std::fs::remove_dir(&dir));

        match result {
            Ok(mut reader) => {
                let mut buf = Vec::new();
                reader.read_to_end(&mut buf).unwrap();
                assert_eq!(buf, data);
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                eprintln!("Skipping direct_reader test: O_DIRECT not supported (EINVAL)");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
