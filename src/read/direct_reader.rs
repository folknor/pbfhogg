//! O_DIRECT file reader with aligned buffering.
//!
//! [`DirectReader`] maintains an aligned internal buffer and reads in
//! aligned chunks via `read(2)`, at the alignment `crate::direct_io`
//! discovered for the file. This bypasses the kernel page cache entirely,
//! preventing cache pollution during planet-scale (80 GB+) PBF reads.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use crate::error::Result;
use crate::write::IO_BUF_BYTES;

// ---------------------------------------------------------------------------
// AlignedReadBuffer - aligned heap allocation for reads
// ---------------------------------------------------------------------------

/// An aligned byte buffer for O_DIRECT reads.
struct AlignedReadBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Number of valid bytes in the buffer (from the last read).
    len: usize,
    /// Current read position within the buffer.
    pos: usize,
}

// Safety: AlignedReadBuffer is a plain owned byte array with no shared references.
// It is only accessed by the owning DirectReader.
unsafe impl Send for AlignedReadBuffer {}

impl AlignedReadBuffer {
    /// Allocate a new buffer of `capacity` bytes aligned to `align`.
    ///
    /// Zeroed, like the write-side buffers, so the whole capacity can be
    /// handed to `read` as an initialised slice. Paid once per reader.
    fn new(capacity: usize, align: usize) -> Result<Self> {
        let (ptr, layout) = crate::direct_io::alloc_aligned_zeroed(capacity, align)?;
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

    /// Fill the buffer with one `read(2)` of its whole capacity (std's
    /// `File::read`, which is that one call). Returns the number of bytes
    /// read (0 on EOF); an interrupted read is returned as `Interrupted`.
    ///
    /// The read is aligned: the buffer address is aligned to the file's
    /// alignment, the capacity is a multiple of it, and the fd offset is
    /// aligned too (`DirectReader::fill` never reads past a regular file's
    /// final short read, and `skip` seeks to aligned offsets only).
    fn fill_from(&mut self, mut file: &File) -> io::Result<usize> {
        self.pos = 0;
        self.len = 0;
        // Safety: the allocation is `capacity` zeroed (so initialised) bytes,
        // exclusively owned through `&mut self`.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity()) };
        self.len = file.read(dst)?;
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
/// All reads from the underlying file descriptor use an aligned buffer,
/// aligned sizes and aligned offsets, at the file's direct I/O alignment.
/// Data is served from the internal buffer, which is refilled via
/// `read(2)` when exhausted.
pub struct DirectReader {
    file: File,
    buf: AlignedReadBuffer,
    /// The direct I/O alignment `crate::direct_io` discovered for `file`: a
    /// power of two dividing the buffer capacity.
    align: usize,
    /// Whether we've hit EOF on the underlying fd.
    eof: bool,
    /// File length captured once at open. Input files are read-only for the
    /// reader's lifetime, so `skip` validates an aligned target against this
    /// instead of paying an `fstat` per skip.
    file_len: u64,
    /// Where `fill` stops instead of issuing a read at the unaligned offset
    /// the final short read left: `file_len` for a regular file, unbounded
    /// otherwise, because a block device's metadata length is zero whatever
    /// its capacity and its reads must still reach the device.
    read_limit: u64,
    /// The fd's file offset: the byte just past the data in `buf`. Advanced
    /// by every successful fill and set by every successful seek, so the
    /// logical position is `fd_pos - buf.remaining()`.
    fd_pos: u64,
}

impl DirectReader {
    /// Return the raw file descriptor for `copy_file_range`.
    pub(crate) fn raw_fd(&self) -> std::os::unix::io::RawFd {
        self.file.as_raw_fd()
    }

    /// The metadata of the opened file (an `fstat` of this reader's fd).
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    /// Skip `n` bytes without materializing data.
    ///
    /// Discards any buffered bytes that overlap the skip range, then seeks
    /// the fd. The seek target is aligned down to a page boundary, and the
    /// buffer is refilled so the read position lands at the correct logical
    /// offset. This is necessary because O_DIRECT requires page-aligned
    /// file offsets for `read()`.
    pub(crate) fn skip(&mut self, n: u64) -> io::Result<()> {
        let buffered = self.buf.remaining() as u64;
        if n <= buffered {
            #[allow(clippy::cast_possible_truncation)] // n <= buffered <= buf capacity (usize)
            self.buf.consume(n as usize);
            return Ok(());
        }
        // Consume remaining buffer, then seek past the rest.
        let past_buf = n - buffered;
        #[allow(clippy::cast_possible_truncation)] // buffered <= buf capacity (usize)
        self.buf.consume(buffered as usize);

        // `fd_pos` is the byte immediately past the end of the data in
        // `self.buf`. After the `consume(buffered)` above the buffer is
        // logically at `fd_pos - 0`, so adding `past_buf` gives the
        // absolute target.
        use std::io::{Seek, SeekFrom};
        let target = self.fd_pos + past_buf;
        let aligned = target & !(self.align as u64 - 1);
        self.file.seek(SeekFrom::Start(aligned))?;
        self.fd_pos = aligned;

        if aligned < target {
            // Refill buffer from the aligned position, then skip the gap.
            // gap < align <= buffer capacity, so it always fits in usize.
            #[allow(clippy::cast_possible_truncation)]
            let gap = (target - aligned) as usize;
            let filled = self.fill()?;
            if filled < gap {
                self.eof = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "skip past end of file",
                ));
            }
            self.buf.consume(gap);
        } else {
            // Already aligned. Seeking can succeed beyond EOF, so validate
            // the logical target before clearing the buffer.
            if target > self.file_len {
                self.eof = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "skip past end of file",
                ));
            }
            // Clear the buffer so the next read refills at the target.
            self.buf.pos = 0;
            self.buf.len = 0;
        }
        Ok(())
    }

    /// Refill the buffer from the fd, tracking the fd offset. At or past
    /// `read_limit` it returns 0 without a syscall: the final short read left
    /// the fd at an unaligned offset, and a direct read there is not one the
    /// alignment contract allows. A short read before the limit is not
    /// guessed to be EOF; the next fill issues its read as before.
    fn fill(&mut self) -> io::Result<usize> {
        if self.fd_pos >= self.read_limit {
            self.buf.pos = 0;
            self.buf.len = 0;
            return Ok(0);
        }
        let filled = self.buf.fill_from(&self.file)?;
        self.fd_pos += filled as u64;
        Ok(filled)
    }

    /// Open a file for reading with `O_DIRECT`.
    ///
    /// A filesystem that refuses `O_DIRECT` at open, or whose reported
    /// alignment the buffers cannot keep, fails with
    /// `ErrorKind::PlatformUnsupported` (feature `direct-io`).
    pub fn open(path: &Path) -> Result<Self> {
        // Ordinary failures name the path in the caller (`FileReader::direct`),
        // as the input helpers do; a platform refusal already names it.
        let (file, align) = crate::direct_io::open_for_read(path)?;
        Self::from_opened(file, align, path)
    }

    /// A reader over `file`, already opened `O_DIRECT` by `crate::direct_io`
    /// with the alignment `align` it reported; `path` names it in a failure.
    pub(crate) fn from_opened(file: File, align: usize, path: &Path) -> Result<Self> {
        let metadata = file.metadata().map_err(|e| {
            crate::Error::from(e).context(format!("failed to stat {}", path.display()))
        })?;
        let file_len = metadata.len();
        let read_limit = if metadata.is_file() {
            file_len
        } else {
            u64::MAX
        };
        let buf = AlignedReadBuffer::new(IO_BUF_BYTES, align)?;
        Ok(Self {
            file,
            buf,
            align,
            eof: false,
            file_len,
            read_limit,
            fd_pos: 0,
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

        // Buffer exhausted - refill from fd.
        if self.eof {
            return Ok(0);
        }

        let filled = self.fill()?;
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

    #[test]
    fn aligned_read_buffer_basics() {
        // Above the floor, so the test shows the buffer honours the alignment
        // it is handed rather than a fixed page size.
        const ALIGN: usize = crate::direct_io::DIO_ALIGN_FLOOR * 4;
        let buf = AlignedReadBuffer::new(ALIGN * 4, ALIGN).unwrap();
        assert_eq!(buf.len, 0);
        assert_eq!(buf.pos, 0);
        assert_eq!(buf.capacity(), ALIGN * 4);
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.ptr.as_ptr() as usize % ALIGN, 0);
    }
}
