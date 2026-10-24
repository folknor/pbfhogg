//! O_DIRECT file writer with aligned buffering.
//!
//! [`DirectWriter`] maintains an aligned internal buffer and flushes in
//! aligned chunks via `libc::write`, at the alignment `crate::direct_io`
//! discovered for the file. On final flush, writes the padded
//! tail and calls `ftruncate` to trim to the actual byte count. This bypasses
//! the kernel page cache entirely, preventing cache pollution during
//! planet-scale (80 GB+) PBF writes.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use super::IO_BUF_BYTES;
use super::copy_range::{retry_eintr, syscall_result};
use crate::direct_io::{alloc_aligned_zeroed, round_up};

// ---------------------------------------------------------------------------
// AlignedBuffer - aligned heap allocation
// ---------------------------------------------------------------------------

/// An aligned byte buffer for O_DIRECT I/O.
struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    len: usize,
}

// Safety: AlignedBuffer is a plain owned byte array with no shared references.
// It is only accessed by the owning DirectWriter.
unsafe impl Send for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocate a new buffer of `capacity` bytes aligned to `align`.
    fn new(capacity: usize, align: usize) -> io::Result<Self> {
        let (ptr, layout) = alloc_aligned_zeroed(capacity, align)?;
        Ok(Self {
            ptr,
            layout,
            len: 0,
        })
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
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                data.len(),
            );
        }
        self.len += data.len();
    }

    /// Zero-fill from `self.len` up to `new_len`. Used for padding the final
    /// aligned block.
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
/// All writes to the underlying file descriptor are aligned in address,
/// offset, and size to the file's direct I/O alignment. The final partial
/// block is zero-padded and written, then `ftruncate` trims the file to the
/// actual byte count.
pub struct DirectWriter {
    file: File,
    buf: AlignedBuffer,
    /// The direct I/O alignment `crate::direct_io` discovered for `file`: a
    /// power of two dividing the buffer capacity.
    align: usize,
    /// Total logical bytes written (for ftruncate at the end).
    logical_size: u64,
    /// Whether flush_final has already been called.
    flushed: bool,
}

impl DirectWriter {
    /// Open a file for writing with `O_DIRECT`.
    ///
    /// A filesystem that refuses `O_DIRECT` at open fails with
    /// `ErrorKind::PlatformUnsupported` (feature `direct-io`).
    /// A file whose reported alignment the buffers cannot keep is refused the
    /// same way.
    pub fn create(path: &Path) -> crate::Result<Self> {
        let (file, align) = crate::direct_io::create_for_write(path)?;
        let buf = AlignedBuffer::new(IO_BUF_BYTES, align)?;
        Ok(Self {
            file,
            buf,
            align,
            logical_size: 0,
            flushed: false,
        })
    }

    /// Write the full aligned buffer to the fd, handling partial writes.
    fn flush_buf(&mut self) -> io::Result<()> {
        let mut written = 0;
        let total = self.buf.len;
        let fd = self.file.as_raw_fd();
        while written < total {
            // A signal delivered mid-write is not a write failure: the shared
            // `EINTR` rule in `copy_range` retries it.
            let n = retry_eintr(|| {
                // Safety: the range lies inside the buffer and the fd is open
                // for writing. The first write is aligned (buffer address,
                // fd offset and `total` are multiples of `align`, the last
                // ensured by callers).
                syscall_result(unsafe {
                    libc::write(fd, self.buf.as_ptr().add(written).cast(), total - written)
                })
            })?;
            if n == 0 {
                // `write_all`'s rule: a write that takes nothing would loop
                // here forever.
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            written += n;
            // A short write that stopped off an aligned boundary makes the
            // retry of the remainder misaligned. It is still issued, on
            // purpose: a filesystem with a buffered fallback for misaligned
            // direct writes (btrfs) completes it correctly, and a strict one
            // refuses it with `EINVAL`, which surfaces as this flush's error.
            // Refusing here instead would fail the first case for nothing.
        }
        self.buf.clear();
        Ok(())
    }

    /// Final flush: pad the remaining data to an aligned boundary, write it,
    /// then truncate the file to the actual logical size.
    fn flush_final(&mut self) -> io::Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;

        if self.buf.len > 0 {
            // Round up to the next aligned boundary.
            let aligned = round_up(self.buf.len, self.align);
            self.buf.zero_pad_to(aligned);
            self.flush_buf()?;
        }

        // Trim the file to the actual byte count (remove zero-padding).
        self.file.set_len(self.logical_size)?;
        Ok(())
    }

    /// Fsync the underlying file to ensure durability. The caller records it
    /// through the writer-metrics funnel.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

impl Write for DirectWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // `flush` is final for this type: it pads the buffer to an aligned
        // block, writes it, and `ftruncate`s the file back to the logical
        // size.
        // The fd offset is then past zero padding that the truncation just
        // removed, so anything written afterwards would land after a
        // mid-file zero gap - readers see nulls where a BlobHeader belongs -
        // and the final partial buffer would be dropped silently, because
        // `flush_final` early-returns on the latch and `Drop` calls the same
        // latched function. Refuse instead of producing that file.
        if self.flushed {
            // A call-order error, carried through the `Write` signature whole.
            return Err(crate::Error::invalid_argument(
                "DirectWriter: write after flush; the file has already been \
                 padded and truncated to its final size",
            )
            .into());
        }
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
        #[cfg(test)]
        let flushed = self.flushed;
        // Best-effort cleanup still finalizes the file, but only an explicit
        // flush can report its I/O error. An error bail-out reaching here is
        // expected (the primary error dominates). The crate's own unit-test
        // builds assert when a non-panicking path skipped the explicit flush;
        // see `PbfWriter`'s `Drop` for why it is not every debug build.
        drop(self.flush_final());
        #[cfg(test)]
        debug_assert!(
            std::thread::panicking() || flushed,
            "DirectWriter dropped before flush(); call flush() to observe deferred errors"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::direct_io::DIO_ALIGN_FLOOR;

    #[test]
    fn aligned_buffer_basics() {
        const ALIGN: usize = DIO_ALIGN_FLOOR;
        let mut buf = AlignedBuffer::new(ALIGN * 4, ALIGN).unwrap();
        assert_eq!(buf.len, 0);
        assert_eq!(buf.capacity(), ALIGN * 4);
        assert_eq!(buf.remaining(), ALIGN * 4);
        assert_eq!(buf.as_ptr() as usize % ALIGN, 0);

        buf.extend(&[1, 2, 3, 4, 5]);
        assert_eq!(buf.len, 5);
        assert_eq!(buf.remaining(), ALIGN * 4 - 5);

        buf.zero_pad_to(ALIGN);
        assert_eq!(buf.len, ALIGN);

        buf.clear();
        assert_eq!(buf.len, 0);
    }

    /// `flush` is final: a later `write` must be refused, not accepted.
    ///
    /// `flush_final` pads to a page, writes, `ftruncate`s to the logical
    /// size, and latches. Nothing stopped a further `write`, and the result
    /// was a file with a mid-file zero gap plus a silently dropped final
    /// partial buffer - `flush_final` early-returns on the latch and `Drop`
    /// calls the same latched function. `DirectWriter` is `pub` and
    /// implements `Write`, so this is contract-conforming use.
    ///
    /// The file is created without `O_DIRECT` involvement in the assertion
    /// itself; only the refusal is under test, so this runs anywhere.
    #[test]
    fn write_after_flush_is_refused() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("direct.bin");
        let mut w = DirectWriter {
            file: File::create(&path).unwrap(),
            buf: AlignedBuffer::new(IO_BUF_BYTES, DIO_ALIGN_FLOOR).unwrap(),
            align: DIO_ALIGN_FLOOR,
            logical_size: 0,
            flushed: false,
        };
        w.write_all(b"first").unwrap();
        w.flush().unwrap();
        let err = w
            .write(b"second")
            .expect_err("a write after the final flush must be refused");
        assert!(err.to_string().contains("write after flush"), "got: {err}");
        drop(w);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"first",
            "the refused bytes must not have reached the file, and the \
             zero padding must still have been truncated away"
        );
    }
}
