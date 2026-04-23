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

use std::collections::VecDeque;

use crate::write::pipeline::{OutputChunk, PipelineItem, WRITE_AHEAD};
use io_uring::opcode;
use io_uring::types::Fixed;
use io_uring::IoUring;
use crate::reorder_buffer::ReorderBuffer;
use std::alloc::{self, Layout};
use std::fs::File;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::atomic::Ordering::Relaxed;

use super::{PAGE_SIZE, alloc_page_aligned};
use super::metrics::WRITER_METRICS;

fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Registered file descriptor index for the output file.
const OUT_FD_IDX: u32 = 0;

/// Extract the buffer index from user_data (bits 0-5).
const fn ud_buf_idx(ud: u64) -> u16 {
    (ud & 0x3F) as u16
}

/// Extract the expected length from user_data (bits 16+).
#[allow(clippy::cast_possible_truncation)] // upper 48 bits, max 256KB << 16 fits u32
const fn ud_expected_len(ud: u64) -> u32 {
    (ud >> 16) as u32
}

/// Size of each registered buffer (256 KB, matches DirectWriter / BufWriter).
const BUF_SIZE: usize = 256 * 1024;

/// Number of registered buffers. 64 × 256 KB = 16 MB total.
/// Charged against `RLIMIT_MEMLOCK`.
const NUM_BUFS: u16 = 64;

// ---------------------------------------------------------------------------
// AlignedBufferPool - contiguous page-aligned allocation with free-list
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
        let (base, layout) = alloc_page_aligned(total)?;
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
// UringState - io_uring ring + buffered accumulation
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
    ///
    /// Intentionally has no `Drop` impl that calls `file.set_len`.
    /// `logical_size` is only consumed by `flush_final`, which is
    /// called on the explicit success path. On any error, the surrounding
    /// `uring_main_loop` propagates via `?` before `flush_final` runs,
    /// so a partially-inflated `logical_size` cannot reach a `set_len`
    /// call and extend the file past real bytes with kernel zeroes.
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
            // Safety: current_buf is Some - we just ensured it above.
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

    /// Submit the current accumulation buffer as a `WriteFixed` SQE.
    fn submit_current(&mut self) -> io::Result<()> {
        let buf_idx = match self.current_buf.take() {
            Some(idx) => idx,
            None => return Ok(()),
        };
        let len = self.current_len;
        self.current_len = 0;
        self.submit_buffer(buf_idx, len)
    }

    /// Submit a registered buffer as a `WriteFixed` SQE.
    ///
    /// Handles O_DIRECT page-alignment padding, advances `write_offset` and
    /// `in_flight`. Releases the buffer immediately if `data_len` is zero.
    /// Does **not** update `logical_size` - the caller is responsible.
    #[allow(clippy::cast_possible_truncation)] // aligned_len as u32/u64, buf_idx as u64: bounded by BUF_SIZE/NUM_BUFS
    fn submit_buffer(&mut self, buf_idx: u16, data_len: usize) -> io::Result<()> {
        if data_len == 0 {
            self.pool.release(buf_idx);
            return Ok(());
        }

        // Pad to page boundary for O_DIRECT. Zero-fill the padding region
        // (buffer memory may have stale data from a previous use).
        let aligned_len = round_up_to_page(data_len);
        if aligned_len > data_len {
            // Safety: buf_idx is valid and not in-flight, padding is within
            // the buffer allocation (aligned_len <= buf_size).
            unsafe {
                let dst = self.pool.buf_ptr(buf_idx).add(data_len);
                std::ptr::write_bytes(dst, 0, aligned_len - data_len);
            }
        }

        let ptr = self.pool.buf_ptr(buf_idx).cast_const();
        let sqe = opcode::WriteFixed::new(
            Fixed(OUT_FD_IDX),
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
        // registered fd (OUT_FD_IDX). The buffer will not be touched until the
        // CQE is reaped (enforced by the free-list protocol).
        self.push_sqe(&sqe)?;
        let t_submit = std::time::Instant::now();
        self.ring
            .submitter()
            .submit()
            .map_err(|e| io::Error::new(e.kind(), format!("io_uring submit failed: {e}")))?;
        WRITER_METRICS.uring_submit_calls.fetch_add(1, Relaxed);
        WRITER_METRICS
            .uring_submit_ns
            .fetch_add(elapsed_ns_u64(t_submit), Relaxed);

        self.write_offset += aligned_len as u64;
        self.in_flight += 1;

        Ok(())
    }

    /// Acquire a free buffer, reaping CQEs if necessary.
    fn acquire_buffer(&mut self) -> io::Result<u16> {
        // Fast path: free buffer available.
        if let Some(idx) = self.pool.acquire() {
            return Ok(idx);
        }
        // Slow path: all buffers in-flight, wait until a buffer is freed.
        loop {
            if self.in_flight == 0 {
                return Err(io::Error::other("no free buffers and nothing in-flight"));
            }
            self.reap_cqes(true)?;
            if let Some(idx) = self.pool.acquire() {
                return Ok(idx);
            }
        }
    }

    /// Push a single SQE to the submission queue, waiting for SQ space if full.
    fn push_sqe(&mut self, sqe: &io_uring::squeue::Entry) -> io::Result<()> {
        // Safety: SQE references registered buffers/fds, validated by callers.
        unsafe {
            if self.ring.submission().push(sqe).is_ok() {
                return Ok(());
            }
        }
        // SQ full - wait for the kernel to drain entries, then retry.
        self.ring
            .submitter()
            .squeue_wait()
            .map_err(|e| io::Error::new(e.kind(), format!("squeue_wait failed: {e}")))?;
        unsafe {
            self.ring
                .submission()
                .push(sqe)
                .map_err(|_| io::Error::other("io_uring SQ still full after squeue_wait"))?;
        }
        Ok(())
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
            let t_wait = std::time::Instant::now();
            self.ring
                .submitter()
                .submit_and_wait(1)
                .map_err(|e| {
                    io::Error::new(e.kind(), format!("io_uring submit_and_wait failed: {e}"))
                })?;
            let elapsed = elapsed_ns_u64(t_wait);
            WRITER_METRICS.uring_submit_and_wait_calls.fetch_add(1, Relaxed);
            WRITER_METRICS
                .uring_submit_and_wait_ns
                .fetch_add(elapsed, Relaxed);
            WRITER_METRICS.uring_cq_wait_ns.fetch_add(elapsed, Relaxed);
        }
        for cqe in self.ring.completion() {
            let ud = cqe.user_data();
            let buf_idx = ud_buf_idx(ud);
            let expected_len = ud_expected_len(ud);
            let result = cqe.result();
            self.in_flight -= 1;

            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            if result.cast_unsigned() != expected_len {
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
        let t_sync = std::time::Instant::now();
        file.sync_all()?;
        WRITER_METRICS
            .sync_all_ns
            .fetch_add(elapsed_ns_u64(t_sync), Relaxed);
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

/// Handle a `CopyRange` payload by `pread`ing input bytes directly into the
/// current registered buffer, letting the normal accumulator submit full
/// buffers via `submit_current` (always page-aligned, always safe).
///
/// The previous design used linked `ReadFixed` → `WriteFixed` SQE chains to
/// keep the data path fully async, but that submitted a `WriteFixed` whose
/// length was `round_up_to_page(copy_len)` on every CopyRange call. When
/// `copy_len` was not a multiple of `PAGE_SIZE`, the write padded out to the
/// next page boundary on disk while `logical_size` tracked only the real
/// data bytes. The result: mid-stream zero-filled gaps in the output that
/// `set_len(logical_size)` could not remove (the gaps were inside the file,
/// not at the end). Readers saw `OSMHeader` followed by nulls where the next
/// `OSMData` blob header should have been.
///
/// The buffered path below preserves the writer's core invariant: mid-stream
/// `WriteFixed` SQEs are always `BUF_SIZE` (page-aligned) because
/// `submit_current` only fires when `current_len == BUF_SIZE`; only the
/// final flush_final SQE is partial, and its trailing zero-padding is then
/// truncated by `set_len`.
///
/// The `pread` writes directly into the registered buffer at `current_len`,
/// so there is no user-space bounce buffer and no extra copy beyond what
/// the kernel's read path already performs.
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn handle_copy_range_uring(
    state: &mut UringState,
    in_fd: std::os::unix::io::RawFd,
    mut src_offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    // Track logical size upfront since we bypass write() which normally does it.
    state.logical_size += remaining;

    while remaining > 0 {
        if state.current_buf.is_none() {
            let idx = state.acquire_buffer()?;
            state.current_buf = Some(idx);
            state.current_len = 0;
        }
        let buf_idx = match state.current_buf {
            Some(idx) => idx,
            None => return Err(io::Error::other("no current buffer")),
        };

        let space = state.pool.buf_size - state.current_len;
        let chunk = space.min(remaining as usize);
        // Safety: buf_idx is valid (acquired from pool), current_len is
        // within BUF_SIZE, chunk <= remaining space, so dst is within the
        // buffer allocation.
        let dst = unsafe { state.pool.buf_ptr(buf_idx).add(state.current_len) };

        // Safety: dst points into a valid registered buffer of size BUF_SIZE,
        // chunk <= remaining space in that buffer, and in_fd is a valid
        // readable file descriptor passed in from the caller.
        let n = unsafe {
            libc::pread(
                in_fd,
                dst.cast::<libc::c_void>(),
                chunk,
                src_offset as i64,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n.cast_unsigned() != chunk {
            return Err(io::Error::other(format!(
                "io_uring CopyRange pread short: expected {chunk}, got {n}"
            )));
        }

        state.current_len += chunk;
        src_offset += chunk as u64;
        remaining -= chunk as u64;

        if state.current_len == state.pool.buf_size {
            state.submit_current()?;
        }
    }
    Ok(())
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
#[allow(clippy::needless_pass_by_value)] // Thread entry point - owned values required for move closure.
pub(crate) fn uring_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<io::Result<()>>,
) -> io::Result<()> {
    let result = uring_init_and_run(&rx, &path, &framed_header, &init_tx);
    if let Err(ref e) = result {
        eprintln!("[uring_writer] error: {e}");
    }
    result
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
    let mut builder = IoUring::builder();
    builder.setup_clamp();
    // Ring depth: enough headroom so one `WriteFixed` can be in flight per
    // registered buffer without the submission queue stalling, plus slack
    // for bursty submission.
    let ring_depth = u32::from(NUM_BUFS) * 2;
    let ring = builder
        .build(ring_depth)
        .map_err(|e| io::Error::new(e.kind(), format!("io_uring creation failed: {e}")))?;

    // Step 2b: Probe supported opcodes for clear error messages on old kernels.
    // WriteFixed requires Linux 5.1+.
    {
        let mut probe = io_uring::Probe::new();
        if ring.submitter().register_probe(&mut probe).is_ok()
            && !probe.is_supported(opcode::WriteFixed::CODE)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel does not support io_uring WriteFixed (requires Linux 5.1+)",
            ));
        }
    }

    // Step 3: Open file with O_DIRECT.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;

    // Step 4: Register the output fd so `WriteFixed` can reference it by slot.
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

/// Reorder and write loop using the shared sequence-number reorder buffer,
/// then dispatches to [`UringState::write`].
fn uring_main_loop(
    rx: &Receiver<PipelineItem>,
    state: &mut UringState,
) -> io::Result<()> {
    let mut pending: ReorderBuffer<io::Result<OutputChunk>> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);

    loop {
        let t_recv = std::time::Instant::now();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS
            .recv_wait_ns
            .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            match chunk {
                OutputChunk::Framed(parts) => {
                    // io_uring backend flattens at the backend boundary
                    // (registered-buffer copy is backend-local).
                    let bytes = parts.into_vec();
                    let len = bytes.len() as u64;
                    let t_write = std::time::Instant::now();
                    state.write(&bytes)?;
                    WRITER_METRICS
                        .write_ns
                        .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                }
                OutputChunk::Raw(bytes) => {
                    let len = bytes.len() as u64;
                    let t_write = std::time::Instant::now();
                    state.write(&bytes)?;
                    WRITER_METRICS
                        .write_ns
                        .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                }
                OutputChunk::RawChunks(chunks) => {
                    let total_bytes: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
                    let t_write = std::time::Instant::now();
                    for chunk in &chunks {
                        state.write(chunk)?;
                    }
                    WRITER_METRICS
                        .write_ns
                        .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                    WRITER_METRICS.bytes_written.fetch_add(total_bytes, Relaxed);
                }
                #[cfg(feature = "linux-direct-io")]
                OutputChunk::CopyRange { in_fd, offset, len } => {
                    let t_write = std::time::Instant::now();
                    handle_copy_range_uring(state, in_fd, offset, len)?;
                    WRITER_METRICS
                        .write_ns
                        .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
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

    #[test]
    fn user_data_encoding() {
        // Write CQE: buf_idx=42, aligned_len=256KB
        let write_ud = (262_144u64 << 16) | 42u64;
        assert_eq!(ud_buf_idx(write_ud), 42);
        assert_eq!(ud_expected_len(write_ud), 262_144);

        // Edge case: buf_idx=63 (max with NUM_BUFS=64)
        let max_ud = (4096u64 << 16) | 63u64;
        assert_eq!(ud_buf_idx(max_ud), 63);
        assert_eq!(ud_expected_len(max_ud), 4096);
    }
}
