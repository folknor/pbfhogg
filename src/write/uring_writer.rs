//! io_uring writer thread with registered buffers for pipelined PBF output.
//!
//! Replaces the synchronous `writer_thread` in [`super::writer`] with an
//! io_uring submission loop. Uses `O_DIRECT` + `WriteFixed` with pre-registered
//! page-aligned buffers for maximum throughput when the pipeline is I/O-bound
//! (e.g. `Compression::None` on erofs).
//!
//! Data is accumulated into 256 KB registered buffers (same strategy as
//! [`super::direct_writer::DirectWriter`]). When a buffer fills, a `WriteFixed`
//! SQE is submitted. CQEs are reaped to recycle buffer indices via a free-list.

use crate::write::writer::{PipelineItem, PipelinePayload, WRITE_AHEAD};
use io_uring::opcode;
use io_uring::types::Fixed;
use io_uring::IoUring;
use std::alloc::{self, Layout};
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender};

/// Page size for O_DIRECT alignment.
const PAGE_SIZE: usize = 4096;

/// Size of each registered buffer (256 KB, matches DirectWriter / BufWriter).
const BUF_SIZE: usize = 256 * 1024;

/// Number of registered buffers. 64 × 256 KB = 16 MB total.
/// Charged against `RLIMIT_MEMLOCK`.
const NUM_BUFS: u16 = 64;

// ---------------------------------------------------------------------------
// AlignedBufferPool — contiguous page-aligned allocation with free-list
// ---------------------------------------------------------------------------

/// A pool of page-aligned buffers for io_uring registered I/O.
///
/// Allocates a single contiguous block of `count × buf_size` bytes, page-aligned.
/// Each buffer is addressed by index (0..count). A free-list tracks which indices
/// are available for use.
struct AlignedBufferPool {
    base: NonNull<u8>,
    layout: Layout,
    count: u16,
    buf_size: usize,
    free: VecDeque<u16>,
}

// Safety: AlignedBufferPool is a plain owned byte array with no shared references.
// It is only accessed by the owning writer thread.
unsafe impl Send for AlignedBufferPool {}

impl AlignedBufferPool {
    /// Allocate `count` page-aligned buffers of `buf_size` bytes each.
    fn new(count: u16, buf_size: usize) -> io::Result<Self> {
        let total = buf_size * count as usize;
        let layout = Layout::from_size_align(total, PAGE_SIZE)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // Safety: layout has non-zero size (BUF_SIZE > 0, NUM_BUFS > 0).
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let base = NonNull::new(ptr)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "aligned alloc failed"))?;
        let mut free = VecDeque::with_capacity(count as usize);
        for i in 0..count {
            free.push_back(i);
        }
        Ok(Self { base, layout, count, buf_size, free })
    }

    /// Pointer to the start of buffer at `index`.
    fn buf_ptr(&self, index: u16) -> *mut u8 {
        debug_assert!((index) < self.count);
        // Safety: index < count, so offset is within the allocation.
        unsafe { self.base.as_ptr().add(index as usize * self.buf_size) }
    }

    /// Build the iovec array for `register_buffers`.
    fn iovecs(&self) -> Vec<libc::iovec> {
        (0..self.count)
            .map(|i| libc::iovec {
                iov_base: self.buf_ptr(i).cast::<libc::c_void>(),
                iov_len: self.buf_size,
            })
            .collect()
    }

    /// Acquire a free buffer index. Returns `None` if all buffers are in-flight.
    fn acquire(&mut self) -> Option<u16> {
        self.free.pop_front()
    }

    /// Return a buffer index to the free-list.
    fn release(&mut self, index: u16) {
        self.free.push_back(index);
    }
}

impl Drop for AlignedBufferPool {
    fn drop(&mut self) {
        // Safety: base was allocated with this layout in `new`.
        unsafe { alloc::dealloc(self.base.as_ptr(), self.layout) };
    }
}

// ---------------------------------------------------------------------------
// UringState — io_uring ring + buffered accumulation
// ---------------------------------------------------------------------------

/// Manages the io_uring ring and buffered write accumulation.
///
/// Data is written via [`write`](Self::write), which copies bytes into the
/// current registered buffer. When the buffer fills (256 KB), it is submitted
/// as a `WriteFixed` SQE. CQEs are reaped to recycle buffer indices.
struct UringState {
    ring: IoUring,
    pool: AlignedBufferPool,
    /// Index of the buffer currently being filled (`None` = need to acquire).
    current_buf: Option<u16>,
    /// Bytes filled in the current buffer.
    current_len: usize,
    /// Physical write offset (always advances by page-aligned amounts).
    write_offset: u64,
    /// Logical file size (actual data bytes, for ftruncate).
    logical_size: u64,
    /// Number of submitted but not yet completed SQEs.
    in_flight: u32,
}

impl UringState {
    /// Append `data` to the output.
    ///
    /// Accumulates into the current registered buffer. When the buffer fills,
    /// submits a `WriteFixed` SQE. Handles any data size by splitting across
    /// multiple buffer fills.
    #[allow(clippy::cast_possible_truncation)] // data.len() as u64: usize fits u64
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.logical_size += data.len() as u64;
        let mut offset = 0;
        while offset < data.len() {
            // Ensure we have a current buffer.
            if self.current_buf.is_none() {
                let idx = self.acquire_buffer()?;
                self.current_buf = Some(idx);
                self.current_len = 0;
            }
            // Safety: current_buf is Some — we just ensured it above.
            let buf_idx = match self.current_buf {
                Some(idx) => idx,
                None => return Err(io::Error::other("no current buffer")),
            };

            let remaining = self.pool.buf_size - self.current_len;
            let chunk = remaining.min(data.len() - offset);

            // Copy chunk into the current registered buffer.
            // Safety: buf_idx is valid (acquired from pool), offset is within
            // the buffer allocation, and the buffer is not in-flight.
            unsafe {
                let dst = self.pool.buf_ptr(buf_idx).add(self.current_len);
                std::ptr::copy_nonoverlapping(data.as_ptr().add(offset), dst, chunk);
            }
            self.current_len += chunk;
            offset += chunk;

            // If buffer is full, submit it.
            if self.current_len == self.pool.buf_size {
                self.submit_current()?;
            }
        }
        Ok(())
    }

    /// Submit the current buffer as a `WriteFixed` SQE.
    #[allow(clippy::cast_possible_truncation)] // aligned_len as u32/u64, buf_idx as u64: bounded by BUF_SIZE/NUM_BUFS
    fn submit_current(&mut self) -> io::Result<()> {
        let buf_idx = match self.current_buf.take() {
            Some(idx) => idx,
            None => return Ok(()),
        };
        if self.current_len == 0 {
            self.pool.release(buf_idx);
            return Ok(());
        }

        // Pad to page boundary for O_DIRECT. Zero-fill the padding region
        // (buffer memory may have stale data from a previous use).
        let aligned_len = round_up_to_page(self.current_len);
        if aligned_len > self.current_len {
            // Safety: buf_idx is valid and not in-flight, padding is within
            // the buffer allocation (aligned_len <= buf_size).
            unsafe {
                let dst = self.pool.buf_ptr(buf_idx).add(self.current_len);
                std::ptr::write_bytes(dst, 0, aligned_len - self.current_len);
            }
        }

        let ptr = self.pool.buf_ptr(buf_idx).cast_const();
        let sqe = opcode::WriteFixed::new(
            Fixed(0), // registered fd index 0
            ptr,
            aligned_len as u32,
            buf_idx,
        )
        .offset(self.write_offset)
        .build()
        // Pack buf_idx (low 16 bits) and expected length (upper 48 bits) into user_data
        // so reap_cqes can detect short writes.
        .user_data((aligned_len as u64) << 16 | buf_idx as u64);

        // Safety: the SQE references a registered buffer (buf_idx) and a
        // registered fd (index 0). The buffer will not be touched until the
        // CQE is reaped (enforced by the free-list protocol).
        unsafe {
            self.ring
                .submission()
                .push(&sqe)
                .map_err(|_| io::Error::other("io_uring submission queue full"))?;
        }
        self.ring
            .submitter()
            .submit()
            .map_err(|e| io::Error::new(e.kind(), format!("io_uring submit failed: {e}")))?;

        self.write_offset += aligned_len as u64;
        self.in_flight += 1;
        self.current_len = 0;

        Ok(())
    }

    /// Acquire a free buffer, reaping CQEs if necessary.
    fn acquire_buffer(&mut self) -> io::Result<u16> {
        // Fast path: free buffer available.
        if let Some(idx) = self.pool.acquire() {
            return Ok(idx);
        }
        // Slow path: all buffers in-flight, wait for at least one completion.
        self.reap_cqes(true)?;
        self.pool
            .acquire()
            .ok_or_else(|| io::Error::other("no free buffers after reap"))
    }

    /// Reap completed CQEs and recycle buffer indices.
    ///
    /// If `wait` is true, blocks until at least one CQE is available.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // ud as u16, result as u32: io_uring CQE fields
    fn reap_cqes(&mut self, wait: bool) -> io::Result<()> {
        if self.in_flight == 0 {
            return Ok(());
        }
        if wait {
            self.ring
                .submitter()
                .submit_and_wait(1)
                .map_err(|e| {
                    io::Error::new(e.kind(), format!("io_uring submit_and_wait failed: {e}"))
                })?;
        }
        for cqe in self.ring.completion() {
            let ud = cqe.user_data();
            let buf_idx = ud as u16;
            let expected_len = (ud >> 16) as u32;
            let result = cqe.result();
            self.in_flight -= 1;
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            if (result as u32) != expected_len {
                return Err(io::Error::other(format!(
                    "io_uring short write: expected {expected_len} bytes, got {result}"
                )));
            }
            self.pool.release(buf_idx);
        }
        Ok(())
    }

    /// Drain all in-flight SQEs.
    fn drain(&mut self) -> io::Result<()> {
        while self.in_flight > 0 {
            self.reap_cqes(true)?;
        }
        Ok(())
    }

    /// Flush the current partial buffer, drain all in-flight writes,
    /// truncate to logical size, and sync to disk.
    fn flush_final(&mut self, file: &File) -> io::Result<()> {
        self.submit_current()?;
        self.drain()?;
        file.set_len(self.logical_size)?;
        file.sync_all()?;
        Ok(())
    }
}

/// Round `n` up to the next multiple of `PAGE_SIZE`.
const fn round_up_to_page(n: usize) -> usize {
    (n + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

// ---------------------------------------------------------------------------
// CopyRange handling
// ---------------------------------------------------------------------------

/// Handle a `CopyRange` payload by reading from the input fd and writing
/// through the ring.
///
/// Reads via `pread(2)` into a heap buffer, then writes through
/// [`UringState::write`]. This replaces `copy_file_range` for the io_uring path
/// — the output fd is `O_DIRECT` and managed by io_uring, so `copy_file_range`
/// cannot be used.
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn handle_copy_range_uring(
    state: &mut UringState,
    in_fd: std::os::unix::io::RawFd,
    mut src_offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    let chunk_size = state.pool.buf_size.min(remaining as usize);
    let mut read_buf = vec![0u8; chunk_size];
    while remaining > 0 {
        let to_read = read_buf.len().min(remaining as usize);
        let n = pread_full(in_fd, &mut read_buf[..to_read], src_offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pread returned 0 during copy_range",
            ));
        }
        state.write(&read_buf[..n])?;
        src_offset += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}

/// `pread(2)` wrapper that handles partial reads.
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn pread_full(
    fd: std::os::unix::io::RawFd,
    buf: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        // Safety: fd is valid and open, buf is a valid mutable slice.
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr().add(total).cast::<libc::c_void>(),
                buf.len() - total,
                offset as i64 + total as i64,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break; // EOF
        }
        total += n.cast_unsigned();
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Writer thread entry point
// ---------------------------------------------------------------------------

/// io_uring writer thread: receives framed blobs via channel and writes them
/// to disk using io_uring `WriteFixed` with registered page-aligned buffers.
///
/// Opens the output file with `O_DIRECT`, creates an io_uring instance,
/// registers the fd and buffer pool. Init errors are sent back via `init_tx`
/// so the constructor can propagate them immediately.
#[allow(clippy::needless_pass_by_value)] // Thread entry point — owned values required for move closure.
pub(crate) fn uring_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<io::Result<()>>,
) -> io::Result<()> {
    uring_init_and_run(&rx, &path, &framed_header, &init_tx)
}

/// Initialize the io_uring ring and run the writer loop.
///
/// Separated from `uring_writer_thread` to keep the init error signaling
/// outside the main logic.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn uring_init_and_run(
    rx: &Receiver<PipelineItem>,
    path: &std::path::Path,
    framed_header: &[u8],
    init_tx: &SyncSender<io::Result<()>>,
) -> io::Result<()> {
    // Step 1: Allocate buffer pool.
    let pool = AlignedBufferPool::new(NUM_BUFS, BUF_SIZE)?;
    let iovecs = pool.iovecs();

    // Step 2: Create io_uring ring.
    let ring = IoUring::builder()
        .setup_clamp()
        .build(u32::from(NUM_BUFS))
        .map_err(|e| io::Error::new(e.kind(), format!("io_uring creation failed: {e}")))?;

    // Step 3: Open file with O_DIRECT.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;

    // Step 4: Register fd.
    ring.submitter()
        .register_files(&[file.as_raw_fd()])
        .map_err(|e| io::Error::new(e.kind(), format!("register_files failed: {e}")))?;

    // Step 5: Register buffers (this is where RLIMIT_MEMLOCK bites).
    // Safety: iovecs point into the AlignedBufferPool allocation which
    // lives for the duration of this function.
    unsafe { ring.submitter().register_buffers(&iovecs) }.map_err(|e| {
        let os_err = e.raw_os_error();
        if os_err == Some(libc::ENOMEM) || os_err == Some(libc::EPERM) {
            io::Error::new(
                e.kind(),
                format!(
                    "register_buffers failed: RLIMIT_MEMLOCK too low \
                     (need {} MB, try `ulimit -l unlimited`): {e}",
                    (NUM_BUFS as usize * BUF_SIZE) / (1024 * 1024)
                ),
            )
        } else {
            io::Error::new(e.kind(), format!("register_buffers failed: {e}"))
        }
    })?;

    // Step 6: Signal successful init.
    init_tx
        .send(Ok(()))
        .map_err(|_| io::Error::other("constructor dropped before init completed"))?;

    let mut state = UringState {
        ring,
        pool,
        current_buf: None,
        current_len: 0,
        write_offset: 0,
        logical_size: 0,
        in_flight: 0,
    };

    // Step 7: Write header.
    state.write(framed_header)?;

    // Step 8: Main reorder + write loop (same as writer_thread in writer.rs).
    uring_main_loop(rx, &mut state)?;

    // Step 9: Flush and finalize.
    state.flush_final(&file)?;

    Ok(())
}

/// Reorder and write loop — identical reorder logic to
/// [`super::writer::writer_thread`], but dispatches to [`UringState::write`].
fn uring_main_loop(
    rx: &Receiver<PipelineItem>,
    state: &mut UringState,
) -> io::Result<()> {
    let mut next_seq: usize = 0;
    let mut pending: VecDeque<Option<PipelinePayload>> =
        VecDeque::with_capacity(WRITE_AHEAD);

    for item in rx {
        let slot_idx = item.seq - next_seq;
        if slot_idx >= pending.len() {
            pending.resize_with(slot_idx + 1, || None);
        }
        pending[slot_idx] = Some(item.data);

        // Drain consecutive ready items from the front.
        loop {
            let front_is_filled = pending.front().is_some_and(Option::is_some);
            if !front_is_filled {
                break;
            }
            #[allow(clippy::unwrap_used)]
            let payload = pending.pop_front().unwrap().unwrap();
            next_seq += 1;
            match payload {
                PipelinePayload::Bytes(result) => {
                    state.write(&result?)?;
                }
                #[cfg(feature = "linux-direct-io")]
                PipelinePayload::CopyRange { in_fd, offset, len } => {
                    handle_copy_range_uring(state, in_fd, offset, len)?;
                }
            }
        }

        // Opportunistically reap CQEs without blocking to recycle buffers.
        state.reap_cqes(false)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer_pool_basics() {
        let mut pool = AlignedBufferPool::new(4, PAGE_SIZE * 2).unwrap();
        // Verify page alignment.
        assert_eq!(pool.buf_ptr(0) as usize % PAGE_SIZE, 0);
        assert_eq!(pool.buf_ptr(1) as usize % PAGE_SIZE, 0);
        // Buffers are distinct.
        let diff = unsafe { pool.buf_ptr(1).offset_from(pool.buf_ptr(0)) };
        assert_eq!(diff, (PAGE_SIZE * 2).cast_signed());
        // Free-list.
        assert_eq!(pool.acquire(), Some(0));
        assert_eq!(pool.acquire(), Some(1));
        assert_eq!(pool.acquire(), Some(2));
        assert_eq!(pool.acquire(), Some(3));
        assert_eq!(pool.acquire(), None);
        pool.release(1);
        assert_eq!(pool.acquire(), Some(1));
    }

    #[test]
    fn round_up_to_page_correctness() {
        assert_eq!(round_up_to_page(0), 0);
        assert_eq!(round_up_to_page(1), PAGE_SIZE);
        assert_eq!(round_up_to_page(PAGE_SIZE), PAGE_SIZE);
        assert_eq!(round_up_to_page(PAGE_SIZE + 1), PAGE_SIZE * 2);
    }
}
