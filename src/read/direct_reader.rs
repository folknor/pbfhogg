//! O_DIRECT file readers with aligned buffering.
//!
//! [`DirectReader`] maintains an aligned internal buffer and reads in
//! aligned chunks via `pread(2)`, at the alignment `crate::direct_io`
//! discovered for the file. This bypasses the kernel page cache entirely,
//! preventing cache pollution during planet-scale (80 GB+) PBF reads.
//!
//! [`DirectPositional`] is its positional twin: an exact read of any byte
//! range at any offset, served from aligned `pread(2)` calls over the range's
//! enclosing aligned span ([`read_aligned`]). It is the transport of the
//! direct header walk (`HeaderWalker::open_direct`).

use std::alloc::{self, Layout};
use std::cell::RefCell;
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

    /// Empty the buffer and return the whole capacity as a read target. Its
    /// address is aligned to the file's alignment and its length is a
    /// multiple of it, so a `pread` into it at an aligned offset is one
    /// `O_DIRECT` allows.
    fn clear_for_fill(&mut self) -> &mut [u8] {
        self.pos = 0;
        self.len = 0;
        // Safety: the allocation is `capacity` zeroed (so initialised) bytes,
        // exclusively owned through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity()) }
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
/// `pread(2)` when exhausted. The reads are positional, so the descriptor's
/// own file offset is never used or moved.
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
    /// Where `fill` stops without a syscall: `file_len` for a regular file,
    /// whose last read there would only re-read the final partial block to
    /// learn it is at the end, and unbounded otherwise (a non-regular
    /// file's metadata length says nothing about how much it serves). Block
    /// devices are not supported: input pinning refuses them.
    read_limit: u64,
    /// The file offset just past the data in `buf`, possibly unaligned after
    /// a short transfer. Advanced by every successful fill and set by every
    /// skip that leaves the buffer, so the logical position is
    /// `file_pos - buf.remaining()`.
    file_pos: u64,
}

/// The raw positional read a [`DirectReader`] fill issues on its file. The
/// reader's internals take it as a parameter so tests can inject any
/// sequence of transfers.
fn pread_file(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt as _;
    file.read_at(buf, offset)
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
    /// Discards any buffered bytes that overlap the skip range. A target past
    /// the buffer at an aligned offset is checked against the length captured
    /// at open and left for the next read. An unaligned one cannot start a
    /// direct read, so the buffer is refilled from the aligned offset below it
    /// and the read position lands at the target inside it; a target past the
    /// end of the file fails there.
    pub(crate) fn skip(&mut self, n: u64) -> io::Result<()> {
        self.skip_with(n, &mut pread_file)
    }

    fn skip_with<P>(&mut self, n: u64, pread: &mut P) -> io::Result<()>
    where
        P: FnMut(&File, &mut [u8], u64) -> io::Result<usize>,
    {
        let past_end = || io::Error::new(io::ErrorKind::UnexpectedEof, "skip past end of file");
        let buffered = self.buf.remaining() as u64;
        if n <= buffered {
            #[allow(clippy::cast_possible_truncation)] // n <= buffered <= buf capacity (usize)
            self.buf.consume(n as usize);
            return Ok(());
        }
        // `file_pos` is the byte immediately past the end of the data in
        // `self.buf`, so the target is `past_buf` beyond it.
        let past_buf = n - buffered;
        let target = self.file_pos + past_buf;
        let aligned = target & !(self.align as u64 - 1);
        self.buf.clear_for_fill();

        if aligned == target {
            // Validate the logical target now; the next read starts there.
            if target > self.file_len {
                self.eof = true;
                return Err(past_end());
            }
            self.file_pos = target;
            return Ok(());
        }

        // Fill from the aligned offset below the target until the buffer
        // reaches it. A short transfer is rebased by `fill_with`, not taken
        // for the end of the file, so this ends when the buffer covers the
        // target or a fill delivers nothing new.
        self.file_pos = aligned;
        while self.file_pos < target {
            match self.fill_with(pread) {
                Ok(0) => {
                    self.eof = true;
                    return Err(past_end());
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        // The buffer starts at or below the target (each fill starts at or
        // below the `file_pos` it was called with) and ends at `file_pos`.
        #[allow(clippy::cast_possible_truncation)] // <= buffered bytes (usize)
        let beyond = (self.file_pos - target) as usize;
        self.buf.consume(self.buf.remaining() - beyond);
        Ok(())
    }

    /// Refill the buffer from `file_pos`, returning the new bytes it holds
    /// (0 at the end of the file). At or past `read_limit` it returns 0
    /// without a syscall.
    ///
    /// `O_DIRECT` reads must start at an aligned offset, and `file_pos` is
    /// unaligned after a short transfer whose count is not a multiple of the
    /// alignment: a filesystem whose real alignment is finer than `align`,
    /// or a regular file's final partial block. So the read starts at the
    /// aligned offset at or below `file_pos`, re-reading at most one partial
    /// block, and the read position skips the bytes already served, as
    /// [`read_aligned`] does. A transfer that adds nothing past `file_pos` is
    /// the end of the file (a file that shrank after open reads as one that
    /// ends there).
    fn fill_with<P>(&mut self, pread: &mut P) -> io::Result<usize>
    where
        P: FnMut(&File, &mut [u8], u64) -> io::Result<usize>,
    {
        let dst = self.buf.clear_for_fill();
        if self.file_pos >= self.read_limit {
            return Ok(0);
        }
        let base = self.file_pos & !(self.align as u64 - 1);
        #[allow(clippy::cast_possible_truncation)] // file_pos - base < align, a usize
        let lead = (self.file_pos - base) as usize;
        let cap = dst.len();
        let n = pread(&self.file, dst, base)?.min(cap);
        if n <= lead {
            return Ok(0);
        }
        self.buf.len = n;
        self.buf.pos = lead;
        self.file_pos = base + n as u64;
        Ok(n - lead)
    }

    fn read_with<P>(&mut self, out: &mut [u8], pread: &mut P) -> io::Result<usize>
    where
        P: FnMut(&File, &mut [u8], u64) -> io::Result<usize>,
    {
        if self.buf.remaining() == 0 {
            if self.eof {
                return Ok(0);
            }
            if self.fill_with(pread)? == 0 {
                self.eof = true;
                return Ok(0);
            }
        }
        let n = out.len().min(self.buf.remaining());
        // Safety: read_ptr() is valid for remaining() bytes, and `out` for
        // `n <= out.len()`; the buffer and `out` are distinct allocations.
        unsafe {
            std::ptr::copy_nonoverlapping(self.buf.read_ptr(), out.as_mut_ptr(), n);
        }
        self.buf.consume(n);
        Ok(n)
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
            file_pos: 0,
        })
    }
}

impl Read for DirectReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.read_with(out, &mut pread_file)
    }
}

// ---------------------------------------------------------------------------
// DirectPositional - exact positional reads through O_DIRECT
// ---------------------------------------------------------------------------

/// A growable aligned byte buffer, zeroed on every growth so the whole of it
/// can be handed to `pread` as an initialised slice. Never shrinks.
pub(crate) struct AlignedScratch {
    align: usize,
    alloc: Option<(NonNull<u8>, Layout)>,
}

impl AlignedScratch {
    /// An empty buffer whose allocations are aligned to `align`, a power of
    /// two.
    pub(crate) fn new(align: usize) -> Self {
        Self { align, alloc: None }
    }

    /// The alignment its allocations keep.
    pub(crate) fn align(&self) -> usize {
        self.align
    }

    /// The first `len` bytes, growing the allocation to hold them.
    fn slice_mut(&mut self, len: usize) -> io::Result<&mut [u8]> {
        let held = self.alloc.map_or(0, |(_, layout)| layout.size());
        if held < len {
            let (ptr, layout) = crate::direct_io::alloc_aligned_zeroed(len, self.align)?;
            if let Some((old, old_layout)) = self.alloc.replace((ptr, layout)) {
                // Safety: allocated with this layout by `alloc_aligned_zeroed`.
                unsafe { alloc::dealloc(old.as_ptr(), old_layout) };
            }
        }
        match self.alloc {
            // Safety: the allocation holds at least `len` zeroed (so
            // initialised) bytes, exclusively owned through `&mut self`.
            Some((ptr, _)) => Ok(unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), len) }),
            // Only `len == 0` leaves nothing allocated.
            None => Ok(&mut []),
        }
    }
}

impl Drop for AlignedScratch {
    fn drop(&mut self) {
        if let Some((ptr, layout)) = self.alloc.take() {
            // Safety: allocated with this layout by `alloc_aligned_zeroed`.
            unsafe { alloc::dealloc(ptr.as_ptr(), layout) };
        }
    }
}

/// Fill `out` exactly with the bytes at `offset`, through `pread` calls that
/// each keep `align` (a power of two) in file offset, buffer address and
/// length, as `O_DIRECT` requires.
///
/// The bytes are read over the enclosing aligned span into `scratch` (whose
/// allocations are aligned to `align`) and only the requested part is copied
/// out. The read stops as soon as the requested bytes are present: the
/// span's padding exists to make the calls legal, so a range ending at the
/// file's end inside an aligned block is a complete read. A short transfer is
/// not taken for the end of the file: the next call is rebased to the aligned
/// offset at or below the bytes still missing, re-reading at most one partial
/// block, so a filesystem whose real alignment is finer than `align` and
/// returns a count that is not a multiple of it is still read to the end.
/// A call that delivers nothing, or nothing past what is already held, ends
/// the read with `UnexpectedEof` (the kind `FileExt::read_exact_at` gives a
/// short file); because every other call adds bytes, the loop terminates.
/// `Interrupted` is retried. `pread(buf, at)` is the raw positional read,
/// injected so tests can drive any sequence of transfers.
pub(crate) fn read_aligned(
    out: &mut [u8],
    offset: u64,
    align: usize,
    scratch: &mut AlignedScratch,
    pread: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
) -> io::Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    let read = read_aligned_in(out.len(), offset, align, scratch, pread)?;
    out.copy_from_slice(read);
    Ok(())
}

/// [`read_aligned`] without the copy out: the `len` bytes at `offset`,
/// borrowed from `scratch` where the aligned read left them. Same reads,
/// same termination, same errors.
pub(crate) fn read_aligned_in(
    len: usize,
    offset: u64,
    align: usize,
    scratch: &mut AlignedScratch,
    mut pread: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
) -> io::Result<&[u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    debug_assert!(align.is_power_of_two() && align == scratch.align);
    let overflow = || io::Error::new(io::ErrorKind::InvalidInput, "direct read range overflows");
    let align64 = u64::try_from(align).map_err(|_| overflow())?;
    let start = offset & !(align64 - 1);
    // offset - start < align, a usize.
    let lead = usize::try_from(offset - start).map_err(|_| overflow())?;
    let need = lead.checked_add(len).ok_or_else(overflow)?;
    let span = need
        .checked_add(align - 1)
        .map(|n| n & !(align - 1))
        .ok_or_else(overflow)?;
    start
        .checked_add(u64::try_from(span).map_err(|_| overflow())?)
        .ok_or_else(overflow)?;
    let buf = scratch.slice_mut(span)?;
    let mut have = 0_usize;
    while have < need {
        let base = have & !(align - 1);
        // base < span, so start + base was checked above.
        let at = start + base as u64;
        let n = match pread(&mut buf[base..span], at) {
            Ok(n) => n.min(span - base),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 || base + n <= have {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "direct read ended short of the requested bytes",
            ));
        }
        have = base + n;
    }
    Ok(&buf[lead..need])
}

/// An `O_DIRECT` file read by position: any byte range at any offset, read
/// through [`read_aligned`] at the file's direct I/O alignment. The aligned
/// scratch it reads into sits behind a `RefCell`, so a read takes `&self` as
/// a buffered `pread` does; the price is that this type, and anything sharing
/// it through an `Arc`, stays on the thread that made it.
pub(crate) struct DirectPositional {
    file: File,
    align: usize,
    scratch: RefCell<AlignedScratch>,
}

impl DirectPositional {
    /// `file`, opened `O_DIRECT` by `crate::direct_io` with the alignment
    /// `align` it reported.
    pub(crate) fn new(file: File, align: usize) -> Self {
        Self {
            file,
            align,
            scratch: RefCell::new(AlignedScratch::new(align)),
        }
    }

    /// Fill `buf` exactly with the bytes at `offset`; `UnexpectedEof` when
    /// the file ends first.
    pub(crate) fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        let mut scratch = self.scratch.borrow_mut();
        read_aligned(buf, offset, self.align, &mut scratch, |dst, at| {
            self.file.read_at(dst, at)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ALIGN: usize = crate::direct_io::DIO_ALIGN_FLOOR;

    /// A file of `len` bytes, byte `i` being `i % 251`, read by a fake
    /// `pread` that asserts every call is aligned and serves at most
    /// `chunks[k]` bytes on call `k` (the last entry repeating), or
    /// `Interrupted` for a zero entry.
    fn fake_read(
        len: usize,
        chunks: &[usize],
        offset: u64,
        want: usize,
    ) -> (io::Result<Vec<u8>>, usize) {
        let file: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
        let mut scratch = AlignedScratch::new(ALIGN);
        let mut out = vec![0xEE_u8; want];
        let mut calls = 0_usize;
        let result = read_aligned(&mut out, offset, ALIGN, &mut scratch, |dst, at| {
            assert_eq!(at % ALIGN as u64, 0, "unaligned offset {at}");
            assert_eq!(dst.as_ptr() as usize % ALIGN, 0, "unaligned buffer");
            assert_eq!(dst.len() % ALIGN, 0, "unaligned length {}", dst.len());
            let chunk = chunks[calls.min(chunks.len() - 1)];
            calls += 1;
            if chunk == 0 {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let at = usize::try_from(at).unwrap();
            let avail = file.len().saturating_sub(at);
            let n = dst.len().min(avail).min(chunk);
            if n > 0 {
                dst[..n].copy_from_slice(&file[at..at + n]);
            }
            Ok(n)
        });
        let expect = (offset..offset + want as u64)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect::<Vec<_>>();
        (
            result.map(|()| {
                assert_eq!(out, expect, "wrong bytes copied out");
                out
            }),
            calls,
        )
    }

    #[test]
    fn a_read_returns_exactly_the_requested_bytes() {
        let big = usize::MAX;
        // Zero length: no call at all.
        assert_eq!(fake_read(100, &[big], 7, 0).1, 0);
        for (offset, want) in [
            (0, 1),
            (1, 10),
            (ALIGN as u64 - 1, 2),
            (ALIGN as u64, ALIGN),
            (100, 3 * ALIGN + 17),
        ] {
            let (r, calls) = fake_read(8 * ALIGN, &[big], offset, want);
            r.unwrap_or_else(|e| panic!("{offset}+{want}: {e}"));
            assert_eq!(calls, 1, "{offset}+{want}");
        }
    }

    #[test]
    fn a_range_ending_at_the_file_end_inside_a_block_is_complete() {
        let len = 2 * ALIGN + 300;
        // Ends exactly at EOF, mid block; and an exact aligned EOF.
        fake_read(len, &[usize::MAX], 50, len - 50).0.unwrap();
        fake_read(2 * ALIGN, &[usize::MAX], 10, 2 * ALIGN - 10)
            .0
            .unwrap();
    }

    #[test]
    fn a_range_past_the_file_end_is_unexpected_eof() {
        let len = 2 * ALIGN + 300;
        for (offset, want) in [(0, len + 1), (len as u64, 1), (len as u64 + 5000, 4)] {
            let err = fake_read(len, &[usize::MAX], offset, want).0.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{offset}+{want}");
        }
    }

    #[test]
    fn short_transfers_are_rebased_and_retried() {
        let len = 8 * ALIGN;
        // Aligned partial transfers, unaligned ones (a filesystem whose real
        // alignment is finer), interruptions, and one that already covers
        // the request before the padding is read.
        for chunks in [
            &[ALIGN][..],
            &[512, usize::MAX][..],
            &[ALIGN + 100, ALIGN][..],
            &[0, 0, 700, usize::MAX][..],
            &[300, 0, ALIGN][..],
        ] {
            fake_read(len, chunks, 123, 3 * ALIGN)
                .0
                .unwrap_or_else(|e| panic!("{chunks:?}: {e}"));
        }
        // The first transfer covers the requested bytes: no second call for
        // the rest of the span.
        let (r, calls) = fake_read(len, &[ALIGN, 0], 10, 100);
        r.unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_range_whose_span_overflows_is_refused_before_any_read() {
        let mut scratch = AlignedScratch::new(ALIGN);
        for offset in [u64::MAX - 1, u64::MAX - ALIGN as u64 + 2] {
            let mut out = [0_u8; 4];
            let err = read_aligned(&mut out, offset, ALIGN, &mut scratch, |_, _| {
                panic!("a read was issued for an overflowing range")
            })
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{offset}");
        }
    }

    #[test]
    fn a_transfer_that_adds_nothing_ends_the_read() {
        // Every call returns only bytes already held: no progress, so the
        // read fails rather than looping.
        let (r, calls) = fake_read(8 * ALIGN, &[ALIGN + 10, 10], 0, 3 * ALIGN);
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(calls, 2);
    }

    #[test]
    fn a_real_direct_file_reads_like_a_buffered_one() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("positional.bin");
        let bytes: Vec<u8> = (0..3 * ALIGN + 777)
            .map(|i| u8::try_from(i % 253).unwrap())
            .collect();
        std::fs::write(&path, &bytes).unwrap();
        // The typed direct-io refusal skips, announced; any other failure,
        // a refusal labelled for another feature included, fails.
        let (file, align) = match crate::direct_io::open_for_read(&path) {
            Ok(opened) => opened,
            Err(e) if crate::test_support::is_direct_io_refusal(&e) => {
                crate::test_support::skip(format!("O_DIRECT not supported: {e}"));
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let reader = DirectPositional::new(file, align);
        for (offset, want) in [(0, 1), (5, 4000), (4095, 2), (100, bytes.len() - 100)] {
            let mut out = vec![0_u8; want];
            reader.read_exact_at(&mut out, offset as u64).unwrap();
            assert_eq!(out, bytes[offset..offset + want], "{offset}+{want}");
        }
        let mut out = vec![0_u8; 10];
        let err = reader
            .read_exact_at(&mut out, bytes.len() as u64 - 5)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // A file truncated after open fails the bytes it lost.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(ALIGN as u64)
            .unwrap();
        let mut out = vec![0_u8; 10];
        let err = reader
            .read_exact_at(&mut out, ALIGN as u64 + 1)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A file of bytes `i % 251` read by a fake `pread` that asserts every
    /// call is aligned and serves at most `chunks[k]` bytes on call `k` (the
    /// last entry repeating), or `Interrupted` for a zero entry.
    struct FakeFile {
        data: Vec<u8>,
        chunks: Vec<usize>,
        calls: usize,
    }

    impl FakeFile {
        fn pread(&mut self, dst: &mut [u8], at: u64) -> io::Result<usize> {
            assert_eq!(at % ALIGN as u64, 0, "unaligned offset {at}");
            assert_eq!(dst.as_ptr() as usize % ALIGN, 0, "unaligned buffer");
            assert_eq!(dst.len() % ALIGN, 0, "unaligned length {}", dst.len());
            let chunk = self.chunks[self.calls.min(self.chunks.len() - 1)];
            self.calls += 1;
            if chunk == 0 {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let at =
                usize::try_from(at).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let avail = self.data.len().saturating_sub(at);
            let n = dst.len().min(avail).min(chunk);
            if n > 0 {
                dst[..n].copy_from_slice(&self.data[at..at + n]);
            }
            Ok(n)
        }
    }

    /// A `DirectReader` whose fills are served by a [`FakeFile`] of `len`
    /// bytes. The reader is opened over a real scratch file of the same
    /// length (buffered: the fake ignores it), so the length and regular-file
    /// limits are the ones `from_opened` takes from the metadata.
    struct Harness {
        _dir: tempfile::TempDir,
        reader: DirectReader,
        fake: FakeFile,
    }

    impl Harness {
        fn new(len: usize, chunks: &[usize]) -> Self {
            let dir = crate::path_guard::tests::scratch();
            let path = dir.path().join("sequential.bin");
            let data: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
            std::fs::write(&path, &data).unwrap();
            let file = File::open(&path).unwrap();
            let reader = DirectReader::from_opened(file, ALIGN, &path).unwrap();
            Self {
                _dir: dir,
                reader,
                fake: FakeFile {
                    data,
                    chunks: chunks.to_vec(),
                    calls: 0,
                },
            }
        }

        /// One `read` call; `Interrupted` is returned as is.
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let fake = &mut self.fake;
            self.reader
                .read_with(out, &mut |_: &File, dst: &mut [u8], at| fake.pread(dst, at))
        }

        /// Read exactly `want` bytes, retrying `Interrupted`.
        fn read_exact(&mut self, want: usize) -> io::Result<Vec<u8>> {
            let mut out = vec![0_u8; want];
            let mut have = 0;
            while have < want {
                match self.read(&mut out[have..]) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => have += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(out)
        }

        /// Everything left, in reads of at most `step` bytes.
        fn read_to_end(&mut self, step: usize) -> Vec<u8> {
            let mut all = Vec::new();
            let mut out = vec![0_u8; step];
            loop {
                match self.read(&mut out) {
                    Ok(0) => return all,
                    Ok(n) => all.extend_from_slice(&out[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => panic!("{e}"),
                }
            }
        }

        fn skip(&mut self, n: u64) -> io::Result<()> {
            let fake = &mut self.fake;
            self.reader
                .skip_with(n, &mut |_: &File, dst: &mut [u8], at| fake.pread(dst, at))
        }

        fn expect(&self, from: usize, len: usize) -> Vec<u8> {
            self.fake.data[from..from + len].to_vec()
        }
    }

    #[test]
    fn a_sequential_read_rebases_after_unaligned_short_transfers() {
        // Spans several buffer fills and ends mid block.
        let len = 3 * IO_BUF_BYTES + 777;
        for chunks in [
            &[usize::MAX][..],
            // Short transfers whose counts are not multiples of ALIGN, as a
            // filesystem whose real alignment is finer returns; the last
            // leaves every fill unaligned.
            &[512, usize::MAX][..],
            &[ALIGN + 100, usize::MAX][..],
            &[IO_BUF_BYTES - 1, 3 * ALIGN + 1][..],
            // Interruptions between and before short transfers.
            &[0, 700, 0, 0, usize::MAX][..],
        ] {
            let mut h = Harness::new(len, chunks);
            let all = h.read_to_end(10_000);
            assert!(all == h.fake.data, "{chunks:?}: wrong bytes");
            // At the end it stays at the end, without another transfer.
            let calls = h.fake.calls;
            assert_eq!(h.read(&mut [0_u8; 8]).unwrap(), 0, "{chunks:?}");
            assert_eq!(h.fake.calls, calls, "{chunks:?}");
        }
    }

    #[test]
    fn a_transfer_adding_nothing_past_the_position_is_the_end() {
        // The second transfer, rebased to ALIGN, returns no byte past the
        // 100 already served from that block: a file that shrank reads as
        // one ending there rather than looping.
        let mut h = Harness::new(8 * ALIGN, &[ALIGN + 100, 50]);
        assert_eq!(h.read_to_end(1 << 20), h.expect(0, ALIGN + 100));
        assert_eq!(h.fake.calls, 2);
    }

    #[test]
    fn skips_land_on_their_target_after_short_transfers() {
        let len = 3 * IO_BUF_BYTES + 777;
        for chunks in [
            &[usize::MAX][..],
            &[512, usize::MAX][..],
            &[2 * ALIGN + 1][..],
            &[0, ALIGN + 100, 0, 2 * ALIGN + 1][..],
            // The first skip's fill is interrupted, then stops 148 bytes
            // short of its target inside the block: it is refilled, not
            // taken for the end.
            &[usize::MAX, 0, 300, usize::MAX][..],
        ] {
            let mut h = Harness::new(len, chunks);
            let mut pos = 0_usize;
            // `(target, read)`: within the buffer, to an unaligned target
            // past it, to an aligned one, and to an unaligned one again.
            for (target, read) in [
                (0, 10),
                (15, 100),
                (IO_BUF_BYTES + 448, 2 * ALIGN),
                (70 * ALIGN, 1),
                (70 * ALIGN + IO_BUF_BYTES + 3, 5000),
            ] {
                let skip = u64::try_from(target - pos).unwrap();
                h.skip(skip)
                    .unwrap_or_else(|e| panic!("{chunks:?} to {target}: {e}"));
                let got = h.read_exact(read).unwrap();
                assert!(got == h.expect(target, read), "{chunks:?} at {target}");
                pos = target + read;
            }
            // To the file's end, unaligned: fine, and the next read is EOF.
            h.skip(u64::try_from(len - pos).unwrap()).unwrap();
            assert_eq!(h.read_to_end(64), Vec::<u8>::new(), "{chunks:?}");
        }
    }

    #[test]
    fn a_skip_past_the_end_is_unexpected_eof() {
        let len = 2 * IO_BUF_BYTES + 300;
        // Unaligned past the end, aligned past the end, unaligned inside the
        // final partial block's padding, and one byte past.
        for target in [len as u64 + 5000, 4 * IO_BUF_BYTES as u64, len as u64 + 1] {
            for chunks in [&[usize::MAX][..], &[2 * ALIGN + 1][..]] {
                let mut h = Harness::new(len, chunks);
                h.read_exact(10).unwrap();
                let err = h.skip(target - 10).unwrap_err();
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::UnexpectedEof,
                    "{target} {chunks:?}"
                );
            }
        }
    }

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
