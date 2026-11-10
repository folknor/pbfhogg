//! A worker's compressed-frame read buffer and its retention rule.
//!
//! Every buffer that holds an input frame (or a blob's compressed payload)
//! from one blob to the next is a [`ReadBuf`]: the pread workers of the
//! classify pools, the altw stages, extract, multi-extract, renumber,
//! geocode pass 1.5, apply-changes, the overlap sweeps and the header-walk
//! consumers. Two rules hold for all of them.
//!
//! Growth is exact. A read larger than the capacity frees the old buffer and
//! allocates the frame's size, never doubling: the read overwrites every
//! byte, so there is nothing to copy, and the buffer never holds more than
//! the largest frame it read. `Vec::resize`'s amortized growth kept up to
//! nearly twice that, which also pushed ordinary frames past a retention
//! line they do not reach themselves.
//!
//! Retention follows `READ_BUF_MAX_RETAINED_CAPACITY` (crates/lib/src/tunables.rs,
//! which records why that line and what the stricter scratch limit cost in
//! its place). A holder calls [`ReadBuf::release_outlier`] once per blob,
//! after the last read of the bytes and after any capacity sampling; an
//! allocation past the line is freed and counted in
//! `pipeline_read_buf_releases` (one per allocation freed, with its capacity
//! in `pipeline_read_buf_release_capacity_bytes`: invocation totals, not
//! blobs, and not memory returned to the OS), so one large frame does not
//! pin its size in the holder for the rest of the phase. Like the decode
//! buffers' limit, this bounds what is kept between frames, not what a read
//! holds: a worker reading a frame at the size cap holds that much either
//! way. apply-changes' base-read buffer is the one holder on a stricter line,
//! its own measured choice ([`ReadBuf::release_past`]).
//!
//! A buffer whose bytes move on whole ([`ReadBuf::take`], a raw frame handed
//! to the writer) keeps nothing, so nothing is released or counted.

use crate::read::header_walker::Positional;
use crate::tunables::READ_BUF_MAX_RETAINED_CAPACITY;

/// One worker's read buffer; see the module docs for its growth and
/// retention rules.
#[derive(Default)]
pub(crate) struct ReadBuf {
    buf: Vec<u8>,
}

impl ReadBuf {
    /// Read exactly `len` bytes at `offset` from `src` and return them,
    /// growing the buffer exactly when `len` exceeds its capacity. A read
    /// within the capacity zero fills only the part beyond the previous
    /// read's length.
    pub(crate) fn read_exact_at<P: Positional + ?Sized>(
        &mut self,
        src: &P,
        offset: u64,
        len: usize,
    ) -> std::io::Result<&[u8]> {
        if len > self.buf.capacity() {
            // Free the old buffer before allocating its replacement, so the
            // two are never held at once.
            drop(std::mem::take(&mut self.buf));
            self.buf = Vec::with_capacity(len);
        }
        self.buf.resize(len, 0);
        src.pread_exact(&mut self.buf, offset)?;
        Ok(&self.buf)
    }

    /// The bytes of the last read.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The capacity held, for a caller's own peak sample.
    pub(crate) fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Move the last read's bytes out whole (a frame handed on to a writer
    /// or a drain), leaving the buffer empty. Not a release: the allocation
    /// lives on downstream, so nothing is counted.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// Free the buffer if a frame grew it past `READ_BUF_MAX_RETAINED_CAPACITY`,
    /// counted in `pipeline_read_buf_releases`. Call once per blob, after the
    /// last read of its bytes and after any capacity sampling. Returns
    /// whether it released.
    pub(crate) fn release_outlier(&mut self, invocation: &crate::host::Recorder) -> bool {
        self.release_past(READ_BUF_MAX_RETAINED_CAPACITY, invocation)
    }

    /// [`release_outlier`](Self::release_outlier) at `limit`, for a holder
    /// whose own measurement chose a different line; counted the same way.
    pub(crate) fn release_past(
        &mut self,
        limit: usize,
        invocation: &crate::host::Recorder,
    ) -> bool {
        let capacity = self.buf.capacity();
        if capacity > limit {
            self.buf = Vec::new();
            invocation
                .metrics()
                .decompress
                .record_read_buf_release(capacity);
            return true;
        }
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::ReadBuf;
    use crate::tunables::READ_BUF_MAX_RETAINED_CAPACITY as LIMIT;
    use std::sync::atomic::Ordering::Relaxed;

    /// A positional source serving `len` bytes of a fixed pattern.
    struct Pattern(usize);

    impl crate::read::header_walker::Positional for Pattern {
        fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start.saturating_add(buf.len()) > self.0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            for (i, b) in buf.iter_mut().enumerate() {
                *b = byte(start + i);
            }
            Ok(())
        }

        fn read_cached(&self, _buf: &mut [u8], _offset: u64) -> crate::read::header_walker::Cached {
            crate::read::header_walker::Cached::Unsupported
        }

        fn will_need(&self, _offset: u64, _len: u64, _invocation: &crate::host::Invocation) -> u64 {
            0
        }
    }

    /// The pattern's byte at offset `i`.
    fn byte(i: usize) -> u8 {
        u8::try_from(i % 251).unwrap()
    }

    #[test]
    fn grows_exactly_and_only_on_a_larger_frame() {
        let src = Pattern(1 << 20);
        let mut read = ReadBuf::default();
        assert_eq!(read.read_exact_at(&src, 3, 1000).unwrap()[0], 3);
        assert_eq!(read.capacity(), 1000);
        // A smaller frame reuses the buffer and returns only its bytes.
        assert_eq!(read.read_exact_at(&src, 10, 400).unwrap().len(), 400);
        assert_eq!(read.capacity(), 1000);
        // A larger one grows to exactly its size, not a doubling.
        let bytes = read.read_exact_at(&src, 0, 1001).unwrap();
        assert_eq!(bytes.len(), 1001);
        assert_eq!(bytes[1000], byte(1000));
        assert_eq!(read.capacity(), 1001);
        assert_eq!(read.bytes().len(), 1001);
    }

    #[test]
    fn a_buffer_past_the_limit_is_released_and_counted() {
        let call = crate::host::Call::detached();
        let invocation = &call;
        let releases = || {
            invocation
                .metrics()
                .decompress
                .read_buf_releases
                .load(Relaxed)
        };
        let src = Pattern(LIMIT + 2);

        let mut read = ReadBuf::default();
        read.read_exact_at(&src, 0, LIMIT).unwrap();
        assert!(!read.release_outlier(invocation), "at the limit is kept");
        assert_eq!(read.capacity(), LIMIT);
        assert_eq!(releases(), 0);

        read.read_exact_at(&src, 0, LIMIT + 1).unwrap();
        assert!(read.release_outlier(invocation));
        assert_eq!(read.capacity(), 0);
        assert_eq!(releases(), 1);
        assert_eq!(
            invocation
                .metrics()
                .decompress
                .read_buf_release_capacity_bytes
                .load(Relaxed),
            (LIMIT + 1) as u64
        );
    }

    #[test]
    fn a_taken_frame_leaves_nothing_and_counts_nothing() {
        let call = crate::host::Call::detached();
        let src = Pattern(LIMIT + 1);
        let mut read = ReadBuf::default();
        read.read_exact_at(&src, 0, LIMIT + 1).unwrap();
        let frame = read.take();
        assert_eq!(frame.len(), LIMIT + 1);
        assert_eq!(read.capacity(), 0);
        assert!(!read.release_outlier(&call));
        assert_eq!(call.metrics().decompress.read_buf_releases.load(Relaxed), 0);
    }
}
