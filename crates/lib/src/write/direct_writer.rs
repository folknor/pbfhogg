//! O_DIRECT file writer with aligned buffering.
//!
//! [`DirectWriter`] maintains an aligned internal buffer and flushes in
//! aligned chunks via `write(2)`, at the alignment `crate::direct_io`
//! discovered for the file. On final flush, writes the padded
//! tail and calls `ftruncate` to trim to the actual byte count. This bypasses
//! the kernel page cache entirely, preventing cache pollution during
//! planet-scale (80 GB+) PBF writes.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::ptr::NonNull;

use super::IO_BUF_BYTES;
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

    /// The whole allocation. The only unsafe view of it: every other access
    /// indexes this slice, so the bounds check is the slice's own, in
    /// release builds too, whatever `len` says.
    fn whole(&self) -> &[u8] {
        // Safety: the allocation is `capacity` zeroed (so initialised) bytes,
        // owned by `self`, and `&self` excludes a writer.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.capacity()) }
    }

    /// [`whole`](Self::whole), mutably.
    fn whole_mut(&mut self) -> &mut [u8] {
        // Safety: as `whole`, and `&mut self` makes the borrow exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity()) }
    }

    /// The `len` bytes held.
    fn as_slice(&self) -> &[u8] {
        &self.whole()[..self.len]
    }

    /// Append `data` to the buffer. The caller splits its writes at
    /// [`remaining`](Self::remaining); data past it panics on the slice
    /// bound rather than writing past the allocation.
    fn extend(&mut self, data: &[u8]) {
        let len = self.len;
        self.whole_mut()[len..][..data.len()].copy_from_slice(data);
        self.len += data.len();
    }

    /// Zero-fill from `self.len` up to `new_len`. Used for padding the final
    /// aligned block; a `new_len` past the capacity panics on the slice
    /// bound.
    fn zero_pad_to(&mut self, new_len: usize) {
        if new_len > self.len {
            let len = self.len;
            self.whole_mut()[len..new_len].fill(0);
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
    /// Bytes accepted by `write` (copied into the buffer), the length the
    /// file is truncated to at the end. Counted per copied chunk, never for
    /// bytes a failed call did not take.
    logical_size: u64,
    state: State,
}

/// Where a [`DirectWriter`] stands. Fail-stop: an I/O error is final.
enum State {
    /// Accepting writes.
    Open,
    /// `flush` padded, wrote and truncated the file successfully.
    Finished,
    /// A `write(2)` or `ftruncate` failed. What reached the file is unknown:
    /// `write_all` may have written a prefix of the buffer and advanced the
    /// fd offset before failing, and an `O_DIRECT` retry from there would be
    /// misaligned or duplicate bytes. So nothing is retried; every later
    /// `write` and `flush` reports the original cause, and `Drop` leaves the
    /// file alone, since a `set_len` to the accepted size could zero-extend
    /// over bytes that never reached the disk.
    Failed(io::ErrorKind, String),
}

impl DirectWriter {
    /// Open a file for writing with `O_DIRECT`.
    ///
    /// A filesystem that refuses `O_DIRECT` at open fails with
    /// `ErrorKind::PlatformUnsupported` (feature `direct-io`).
    /// A file whose reported alignment the buffers cannot keep is refused the
    /// same way.
    pub fn create(path: &Path) -> crate::Result<Self> {
        Self::create_in(path, None)
    }

    pub(crate) fn create_in(
        path: &Path,
        invocation: Option<&crate::host::Recorder>,
    ) -> crate::Result<Self> {
        let (file, align) = crate::direct_io::create_for_write(path, invocation)?;
        let buf = AlignedBuffer::new(IO_BUF_BYTES, align)?;
        Ok(Self {
            file,
            buf,
            align,
            logical_size: 0,
            state: State::Open,
        })
    }

    /// The error a call on a writer in `state` returns, if any.
    fn refusal(&self) -> Option<io::Error> {
        match &self.state {
            State::Open => None,
            // A call-order error, carried through the `Write` signature whole.
            State::Finished => Some(
                crate::Error::invalid_argument(
                    "DirectWriter: write after flush; the file has already been \
                     padded and truncated to its final size",
                )
                .into(),
            ),
            State::Failed(kind, cause) => Some(io::Error::new(
                *kind,
                format!("DirectWriter: an earlier write failed and the writer is stopped: {cause}"),
            )),
        }
    }

    /// Run `step`, moving to `Failed` if it errors.
    fn fail_stop<T>(&mut self, step: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        step(self).inspect_err(|e| self.state = State::Failed(e.kind(), e.to_string()))
    }

    /// Write the full aligned buffer to the fd, handling partial writes.
    ///
    /// std's `write_all` is the loop: each call is one `write(2)` straight
    /// from the aligned buffer (no copy), `Interrupted` is retried, and a
    /// write that takes nothing is `WriteZero`. The first write is aligned
    /// (buffer address, fd offset and length are multiples of `align`, the
    /// last ensured by callers). A short write that stopped off an aligned
    /// boundary makes the retry of the remainder misaligned. It is still
    /// issued, on purpose: a filesystem with a buffered fallback for
    /// misaligned direct writes (btrfs) completes it correctly, and a strict
    /// one refuses it with `EINVAL`, which surfaces as this flush's error.
    /// Refusing here instead would fail the first case for nothing.
    fn flush_buf(&mut self) -> io::Result<()> {
        (&self.file).write_all(self.buf.as_slice())?;
        self.buf.clear();
        Ok(())
    }

    /// Final flush: pad the remaining data to an aligned boundary, write it,
    /// then truncate the file to the actual logical size. `Finished` is
    /// recorded only once both steps succeeded, so a repeated flush is a
    /// no-op only after a real success; a failure is sticky.
    fn flush_final(&mut self) -> io::Result<()> {
        match self.state {
            State::Open => {}
            State::Finished => return Ok(()),
            State::Failed(..) => {
                if let Some(err) = self.refusal() {
                    return Err(err);
                }
            }
        }
        self.fail_stop(|w| {
            if w.buf.len > 0 {
                // Round up to the next aligned boundary.
                let aligned = round_up(w.buf.len, w.align);
                w.buf.zero_pad_to(aligned);
                w.flush_buf()?;
            }
            // Trim the file to the actual byte count (remove zero-padding).
            w.file.set_len(w.logical_size)
        })?;
        self.state = State::Finished;
        Ok(())
    }

    /// Fsync the underlying file to ensure durability, through the
    /// writer-metrics funnel of `metrics`.
    pub(crate) fn sync_all(&self, metrics: &super::metrics::WriterMetrics) -> io::Result<()> {
        metrics.sync_recorded(&self.file)
    }
}

impl Write for DirectWriter {
    /// Accepts at most one buffer's worth per call, so an `Err` always
    /// means this call took nothing and `Ok(n)` means exactly `n` bytes were
    /// copied and counted. A full buffer is written at the start of the next
    /// call (or by `flush`), never after the copy, so its error cannot follow
    /// an acceptance.
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // After a successful `flush` the fd offset is past zero padding that
        // the truncation removed, so anything written afterwards would land
        // after a mid-file zero gap - readers see nulls where a BlobHeader
        // belongs. After a failure the file content is unknown. Refuse both.
        if let Some(err) = self.refusal() {
            return Err(err);
        }
        if data.is_empty() {
            return Ok(0);
        }
        if self.buf.remaining() == 0 {
            self.fail_stop(Self::flush_buf)?;
        }
        let chunk = data.len().min(self.buf.remaining());
        self.buf.extend(&data[..chunk]);
        self.logical_size += chunk as u64;
        Ok(chunk)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_final()
    }
}

impl Drop for DirectWriter {
    fn drop(&mut self) {
        #[cfg(test)]
        let settled = !matches!(self.state, State::Open);
        // Best-effort cleanup still finalizes an open file (the error that
        // abandoned it was someone else's), but only an explicit flush can
        // report its I/O error. A failed writer is left as it is; see
        // `State::Failed`. The crate's own unit-test builds assert when a
        // non-panicking path skipped the explicit flush; see `PbfWriter`'s
        // `Drop` for why it is not every debug build.
        if matches!(self.state, State::Open) {
            drop(self.flush_final());
        }
        #[cfg(test)]
        debug_assert!(
            std::thread::panicking() || settled,
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
        assert_eq!(buf.as_slice().as_ptr() as usize % ALIGN, 0);

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
            state: State::Open,
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

    /// A writer over a file opened read-only: every `write(2)` fails.
    fn failing_writer(dir: &std::path::Path) -> DirectWriter {
        let path = dir.join("ro.bin");
        File::create(&path).unwrap();
        DirectWriter {
            file: File::open(&path).unwrap(),
            buf: AlignedBuffer::new(IO_BUF_BYTES, DIO_ALIGN_FLOOR).unwrap(),
            align: DIO_ALIGN_FLOOR,
            logical_size: 0,
            state: State::Open,
        }
    }

    /// A failed final flush stays failed: a retry must not report success.
    #[test]
    fn a_failed_flush_is_sticky() {
        let dir = crate::path_guard::tests::scratch();
        let mut w = failing_writer(dir.path());
        w.write_all(b"tail").unwrap();
        let first = w.flush().expect_err("the tail write must fail");
        let again = w.flush().expect_err("a retried flush must not succeed");
        assert_eq!(again.kind(), first.kind());
        assert!(
            again.to_string().contains(&first.to_string()),
            "got: {again}"
        );
        assert!(w.write(b"more").is_err(), "a failed writer refuses writes");
    }

    /// A failing buffer flush inside `write_all` counts only the bytes
    /// actually accepted, and the failing call takes nothing.
    #[test]
    fn a_failed_write_counts_only_accepted_bytes() {
        let dir = crate::path_guard::tests::scratch();
        let mut w = failing_writer(dir.path());
        let data = vec![7u8; IO_BUF_BYTES * 2];
        assert_eq!(w.write(&data).unwrap(), IO_BUF_BYTES);
        assert!(w.write(&data[IO_BUF_BYTES..]).is_err());
        assert_eq!(w.logical_size, IO_BUF_BYTES as u64);
        assert!(w.write_all(&data).is_err());
        assert_eq!(w.logical_size, IO_BUF_BYTES as u64);
        assert!(w.flush().is_err());
    }
}
