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
//!
//! The transport rule lives here too. A pool that reads an [`InputFile`]
//! reads it through [`ReadBuf::read_input_at`], which honours the input's
//! transport: a buffered input is pread into the plain buffer, an input
//! opened `O_DIRECT` (`InputFile::direct`) is read in aligned spans into an
//! aligned scratch the buffer makes on first use, and the requested bytes
//! are borrowed from it where they landed. Both allocations follow the same
//! retention line, each charged and released on its own. A direct read's
//! bytes leave by [`ReadBuf::take`] as a copy into a fresh `Vec` (the
//! aligned allocation cannot be handed on as one), leaving the scratch for
//! the next read.

use crate::read::header_walker::Positional;
use crate::read::input::InputFile;
use crate::tunables::READ_BUF_MAX_RETAINED_CAPACITY;

/// One worker's read buffer; see the module docs for its growth, retention
/// and transport rules.
#[derive(Default)]
pub(crate) struct ReadBuf {
    buf: Vec<u8>,
    /// The aligned scratch direct inputs are read through, made on the first
    /// direct read (and remade when an input's alignment differs).
    #[cfg(feature = "linux-direct-io")]
    scratch: Option<crate::read::direct_reader::AlignedScratch>,
    /// Where the last read's bytes are: `None` in `buf`, `Some(range)` in
    /// `scratch`. Reset at the start of every read, so a failed read, a
    /// switch of transport or a `take` never leaves stale bytes visible.
    #[cfg(feature = "linux-direct-io")]
    direct: Option<std::ops::Range<usize>>,
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
        #[cfg(feature = "linux-direct-io")]
        {
            self.direct = None;
        }
        if len > self.buf.capacity() {
            // Free the old buffer before allocating its replacement, so the
            // two are never held at once.
            drop(std::mem::take(&mut self.buf));
            self.buf = Vec::with_capacity(len);
        }
        self.buf.resize(len, 0);
        if let Err(e) = src.pread_exact(&mut self.buf, offset) {
            self.buf.clear();
            return Err(e);
        }
        Ok(&self.buf)
    }

    /// Read exactly `len` bytes at `offset` of `input` in the input's own
    /// transport (see the module docs) and return them; `UnexpectedEof` when
    /// the file ends first.
    pub(crate) fn read_input_at(
        &mut self,
        input: &InputFile,
        offset: u64,
        len: usize,
    ) -> std::io::Result<&[u8]> {
        #[cfg(feature = "linux-direct-io")]
        if let Some(align) = input.direct_align() {
            use crate::read::direct_reader::{AlignedScratch, read_aligned_in};
            use std::os::unix::fs::FileExt as _;
            self.direct = None;
            // The plain buffer holds no current bytes from here on.
            self.buf.clear();
            let scratch = match &mut self.scratch {
                Some(s) if s.align() == align => s,
                slot => slot.insert(AlignedScratch::new(align)),
            };
            let file = input.file();
            read_aligned_in(len, offset, scratch, |dst, at| file.read_at(dst, at))?;
            // `read_aligned_in` leaves the bytes at the offset's distance
            // past its aligned block (`align` is a power of two).
            #[allow(clippy::cast_possible_truncation)]
            let lead = (offset & (align as u64 - 1)) as usize;
            self.direct = Some(lead..lead + len);
            return Ok(self.bytes());
        }
        self.read_exact_at(&**input.file(), offset, len)
    }

    /// The bytes of the last read (empty after a failed read or a `take`).
    pub(crate) fn bytes(&self) -> &[u8] {
        #[cfg(feature = "linux-direct-io")]
        if let (Some(range), Some(scratch)) = (&self.direct, &self.scratch) {
            return scratch.held_bytes(range.clone());
        }
        &self.buf
    }

    /// The capacity held across both allocations (the aligned one with its
    /// padding), for a caller's own peak sample.
    pub(crate) fn capacity(&self) -> usize {
        #[cfg(feature = "linux-direct-io")]
        {
            self.buf.capacity()
                + self
                    .scratch
                    .as_ref()
                    .map_or(0, crate::read::direct_reader::AlignedScratch::held)
        }
        #[cfg(not(feature = "linux-direct-io"))]
        {
            self.buf.capacity()
        }
    }

    /// Move the last read's bytes out whole (a frame handed on to a writer
    /// or a drain), leaving the buffer empty. Not a release: a buffered
    /// read's allocation lives on downstream, so nothing is counted. A direct
    /// read's bytes are copied into a fresh `Vec` of their exact length; the
    /// scratch stays, under the retention line.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        #[cfg(feature = "linux-direct-io")]
        if self.direct.is_some() {
            let out = self.bytes().to_vec();
            self.direct = None;
            return out;
        }
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
    /// Each allocation (the plain buffer, the aligned scratch with its
    /// padding) is tested against the line and counted on its own; returns
    /// whether either was released.
    pub(crate) fn release_past(
        &mut self,
        limit: usize,
        invocation: &crate::host::Recorder,
    ) -> bool {
        let mut released = false;
        let capacity = self.buf.capacity();
        if capacity > limit {
            self.buf = Vec::new();
            invocation
                .metrics()
                .decompress
                .record_read_buf_release(capacity);
            released = true;
        }
        #[cfg(feature = "linux-direct-io")]
        if let Some(scratch) = &mut self.scratch
            && scratch.release_past(limit, invocation)
        {
            self.direct = None;
            released = true;
        }
        released
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

    /// A direct input is read through the aligned scratch: unaligned ranges
    /// return exactly their bytes, a following buffered read shows its own,
    /// `take` copies the span out and leaves nothing visible, and the
    /// scratch is charged and released on its own.
    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn a_direct_input_reads_through_the_aligned_scratch() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("in.bin");
        let len = 3 * crate::direct_io::DIO_ALIGN_FLOOR + 123;
        let bytes: Vec<u8> = (0..len).map(byte).collect();
        std::fs::write(&path, &bytes).unwrap();
        // Only a typed direct-io refusal skips (announced, and failed where
        // the gate requires the backend).
        if !crate::test_support::direct_reads_supported(dir.path(), "the direct ReadBuf reads") {
            return;
        }
        let direct = crate::read::input::InputFile::direct_for_tests(&path).unwrap();
        let buffered = crate::read::input::InputFile::open(path.as_path()).unwrap();
        let mut read = ReadBuf::default();

        assert_eq!(
            read.read_input_at(&direct, 7, 1000).unwrap(),
            &bytes[7..1007]
        );
        assert_eq!(read.bytes(), &bytes[7..1007]);
        assert!(read.capacity() >= 1000);
        assert_eq!(read.read_input_at(&direct, 0, 0).unwrap(), &[] as &[u8]);
        // Ends at EOF inside an aligned block.
        assert_eq!(
            read.read_input_at(&direct, 100, len - 100).unwrap(),
            &bytes[100..]
        );
        let err = read.read_input_at(&direct, 1, len).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            read.bytes().is_empty(),
            "a failed read leaves no bytes visible"
        );

        read.read_input_at(&direct, 5, 50).unwrap();
        assert_eq!(read.take(), &bytes[5..55]);
        assert!(
            read.bytes().is_empty(),
            "a taken direct read leaves nothing"
        );

        assert_eq!(read.read_input_at(&buffered, 9, 20).unwrap(), &bytes[9..29]);
        read.read_input_at(&direct, 11, 20).unwrap();
        assert_eq!(read.bytes(), &bytes[11..31]);
        assert_eq!(
            read.read_input_at(&buffered, 13, 20).unwrap(),
            &bytes[13..33]
        );

        let call = crate::host::Call::detached();
        assert!(read.release_past(0, &call));
        assert_eq!(read.capacity(), 0);
        assert_eq!(
            call.metrics().decompress.read_buf_releases.load(Relaxed),
            2,
            "each allocation is released and counted on its own"
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
