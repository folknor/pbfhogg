//! O_DIRECT file writer with page-aligned buffering.
//!
//! [`DirectWriter`] maintains a page-aligned internal buffer and flushes in
//! page-aligned chunks via `libc::write`. On final flush, writes the padded
//! tail and calls `ftruncate` to trim to the actual byte count. This bypasses
//! the kernel page cache entirely, preventing cache pollution during
//! planet-scale (80 GB+) PBF writes.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

/// Page size for alignment. 4096 is universally safe across Linux filesystems.
const PAGE_SIZE: usize = 4096;

/// Internal buffer capacity. Matches the 256 KB `BufWriter` capacity used
/// elsewhere. Must be a multiple of `PAGE_SIZE`.
const BUF_CAPACITY: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// AlignedBuffer — page-aligned heap allocation
// ---------------------------------------------------------------------------

/// A page-aligned byte buffer for O_DIRECT I/O.
struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    len: usize,
}

// Safety: AlignedBuffer is a plain owned byte array with no shared references.
// It is only accessed by the owning DirectWriter.
unsafe impl Send for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocate a new buffer of `capacity` bytes aligned to `PAGE_SIZE`.
    fn new(capacity: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(capacity, PAGE_SIZE)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // Safety: layout has non-zero size (BUF_CAPACITY > 0).
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "aligned alloc failed"))?;
        Ok(Self { ptr, layout, len: 0 })
    }

    fn capacity(&self) -> usize {
        self.layout.size()
    }

    fn remaining(&self) -> usize {
        self.capacity() - self.len
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Append `data` to the buffer. Caller must ensure `data.len() <= remaining()`.
    fn extend(&mut self, data: &[u8]) {
        debug_assert!(data.len() <= self.remaining());
        // Safety: we verified there is room, and ptr + len is within the allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.as_ptr().add(self.len), data.len());
        }
        self.len += data.len();
    }

    /// Zero-fill from `self.len` up to `new_len`. Used for padding the final page.
    fn zero_pad_to(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.capacity());
        if new_len > self.len {
            // Safety: within allocation bounds.
            unsafe {
                std::ptr::write_bytes(self.ptr.as_ptr().add(self.len), 0, new_len - self.len);
            }
            self.len = new_len;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // Safety: ptr was allocated with this layout in `new`.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// ---------------------------------------------------------------------------
// DirectWriter
// ---------------------------------------------------------------------------

/// A file writer that bypasses the page cache using `O_DIRECT`.
///
/// All writes to the underlying file descriptor are page-aligned in address,
/// offset, and size. The final partial page is zero-padded and written, then
/// `ftruncate` trims the file to the actual byte count.
pub struct DirectWriter {
    file: File,
    buf: AlignedBuffer,
    /// Total logical bytes written (for ftruncate at the end).
    logical_size: u64,
    /// Whether flush_final has already been called.
    flushed: bool,
}

impl DirectWriter {
    /// Open a file for writing with `O_DIRECT`.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)?;
        let buf = AlignedBuffer::new(BUF_CAPACITY)?;
        Ok(Self {
            file,
            buf,
            logical_size: 0,
            flushed: false,
        })
    }

    /// Write the full aligned buffer to the fd, handling partial writes.
    fn flush_buf(&mut self) -> io::Result<()> {
        let mut written = 0;
        let total = self.buf.len;
        while written < total {
            // Safety: buf.as_ptr() is page-aligned, total is a multiple of
            // PAGE_SIZE (ensured by callers), and the fd is open for writing.
            let n = unsafe {
                libc::write(
                    self.file.as_raw_fd(),
                    self.buf.as_ptr().add(written).cast(),
                    total - written,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            written += n.cast_unsigned();
        }
        self.buf.clear();
        Ok(())
    }

    /// Final flush: pad the remaining data to a page boundary, write it,
    /// then truncate the file to the actual logical size.
    fn flush_final(&mut self) -> io::Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;

        if self.buf.len > 0 {
            // Round up to the next page boundary.
            let aligned = (self.buf.len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            self.buf.zero_pad_to(aligned);
            self.flush_buf()?;
        }

        // Trim the file to the actual byte count (remove zero-padding).
        self.file.set_len(self.logical_size)?;
        Ok(())
    }
}

impl Write for DirectWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.logical_size += data.len() as u64;
        let mut offset = 0;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(self.buf.remaining());
            self.buf.extend(&data[offset..offset + chunk]);
            offset += chunk;
            if self.buf.remaining() == 0 {
                self.flush_buf()?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_final()
    }
}

impl Drop for DirectWriter {
    fn drop(&mut self) {
        // Best-effort flush — errors can't be propagated from Drop.
        // Callers should call flush() explicitly to get errors.
        drop(self.flush_final());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer_basics() {
        let mut buf = AlignedBuffer::new(PAGE_SIZE * 4).unwrap();
        assert_eq!(buf.len, 0);
        assert_eq!(buf.capacity(), PAGE_SIZE * 4);
        assert_eq!(buf.remaining(), PAGE_SIZE * 4);
        // Verify page alignment.
        assert_eq!(buf.as_ptr() as usize % PAGE_SIZE, 0);

        buf.extend(&[1, 2, 3, 4, 5]);
        assert_eq!(buf.len, 5);
        assert_eq!(buf.remaining(), PAGE_SIZE * 4 - 5);

        buf.zero_pad_to(PAGE_SIZE);
        assert_eq!(buf.len, PAGE_SIZE);

        buf.clear();
        assert_eq!(buf.len, 0);
    }
}
