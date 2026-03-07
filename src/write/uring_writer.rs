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

use crate::write::writer::{PipelineItem, PipelinePayload, WRITE_AHEAD};
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

/// Registered file descriptor index for the output file.
const OUT_FD_IDX: u32 = 0;
/// Registered file descriptor index for the input file (CopyRange passthrough).
#[cfg(feature = "linux-direct-io")]
const IN_FD_IDX: u32 = 1;

/// User data flag: bit 15 set means this CQE is from a ReadFixed SQE.
/// Since `NUM_BUFS` is 64 (uses bits 0-5), bit 15 is always free.
const READ_FLAG: u64 = 1 << 15;

/// Extract the buffer index from user_data (bits 0-5, masking out READ_FLAG).
const fn ud_buf_idx(ud: u64) -> u16 {
    (ud & 0x3F) as u16
}

/// Extract the expected length from user_data (bits 16+).
const fn ud_expected_len(ud: u64) -> u32 {
    (ud >> 16) as u32
}

/// Check whether this CQE is from a ReadFixed SQE.
const fn ud_is_read(ud: u64) -> bool {
    ud & READ_FLAG != 0
}

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
    /// Whether the input fd has been registered at `IN_FD_IDX` (one-time, on first CopyRange).
    #[cfg(feature = "linux-direct-io")]
    input_fd_registered: bool,
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
    /// Does **not** update `logical_size` — the caller is responsible.
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
        self.ring
            .submitter()
            .submit()
            .map_err(|e| io::Error::new(e.kind(), format!("io_uring submit failed: {e}")))?;

        self.write_offset += aligned_len as u64;
        self.in_flight += 1;

        Ok(())
    }

    /// Register the input file descriptor at `IN_FD_IDX` for `ReadFixed` CopyRange.
    ///
    /// Only registers on the first call; subsequent calls are no-ops.
    /// Drains all in-flight SQEs first — the kernel may hold references to the
    /// file table during I/O processing. The stall is bounded by the header
    /// write (1 partial buffer, <1ms for merge).
    #[cfg(feature = "linux-direct-io")]
    fn register_input_fd(&mut self, in_fd: std::os::unix::io::RawFd) -> io::Result<()> {
        if self.input_fd_registered {
            return Ok(());
        }
        self.drain()?;
        self.ring
            .submitter()
            .register_files_update(IN_FD_IDX, &[in_fd])
            .map_err(|e| io::Error::new(e.kind(), format!("register_files_update failed: {e}")))?;
        self.input_fd_registered = true;
        Ok(())
    }

    /// Submit a linked `ReadFixed` → `WriteFixed` pair for zero-copy CopyRange.
    ///
    /// Reads `len` bytes from the registered input fd at `src_offset` into
    /// registered buffer `buf_idx`, then writes from that buffer to the output.
    /// The two SQEs are linked: if the read fails, the write is canceled.
    #[cfg(feature = "linux-direct-io")]
    #[allow(clippy::cast_possible_truncation)]
    fn submit_copy_chain(
        &mut self,
        src_offset: u64,
        buf_idx: u16,
        len: usize,
    ) -> io::Result<()> {
        let aligned_len = round_up_to_page(len);
        let buf_ptr = self.pool.buf_ptr(buf_idx);

        // Pre-zero the O_DIRECT padding region. ReadFixed writes [0..len);
        // the trailing [len..aligned_len) must be zero for the WriteFixed.
        if aligned_len > len {
            unsafe {
                std::ptr::write_bytes(buf_ptr.add(len), 0, aligned_len - len);
            }
        }

        // ReadFixed SQE — linked to the next SQE via IO_LINK.
        let read_sqe = opcode::ReadFixed::new(
            Fixed(IN_FD_IDX),
            buf_ptr,
            len as u32,
            buf_idx,
        )
        .offset(src_offset)
        .build()
        .flags(io_uring::squeue::Flags::IO_LINK)
        .user_data((len as u64) << 16 | buf_idx as u64 | READ_FLAG);

        // WriteFixed SQE — executed only if the read succeeds.
        let write_sqe = opcode::WriteFixed::new(
            Fixed(OUT_FD_IDX),
            buf_ptr.cast_const(),
            aligned_len as u32,
            buf_idx,
        )
        .offset(self.write_offset)
        .build()
        .user_data((aligned_len as u64) << 16 | buf_idx as u64);

        // Safety: both SQEs reference registered buffers and registered fds.
        // The buffer will not be touched until both CQEs are reaped.
        self.push_sqe_pair(&read_sqe, &write_sqe)?;
        self.ring
            .submitter()
            .submit()
            .map_err(|e| io::Error::new(e.kind(), format!("io_uring submit failed: {e}")))?;

        self.write_offset += aligned_len as u64;
        self.in_flight += 2; // Both SQEs produce CQEs

        Ok(())
    }

    /// Acquire a free buffer, reaping CQEs if necessary.
    ///
    /// With linked ReadFixed+WriteFixed chains, a single `reap_cqes` call may
    /// only return the ReadFixed CQE (which doesn't release the buffer). We
    /// loop until a WriteFixed CQE releases one.
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
        // SQ full — wait for the kernel to drain entries, then retry.
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

    /// Push a linked SQE pair (ReadFixed + WriteFixed) to the submission queue.
    ///
    /// Both SQEs must be pushed in the same `SubmissionQueue` scope to ensure
    /// the IO_LINK chain is visible to the kernel atomically.
    fn push_sqe_pair(
        &mut self,
        first: &io_uring::squeue::Entry,
        second: &io_uring::squeue::Entry,
    ) -> io::Result<()> {
        // Ensure at least 2 SQ slots are free before pushing either SQE.
        loop {
            {
                let sq = self.ring.submission();
                if sq.capacity() - sq.len() >= 2 {
                    break;
                }
                // Drop without pushing — publishes unchanged tail (no-op).
            }
            self.ring
                .submitter()
                .squeue_wait()
                .map_err(|e| io::Error::new(e.kind(), format!("squeue_wait failed: {e}")))?;
        }
        // Safety: SQEs reference registered buffers/fds, validated by callers.
        // Space was verified above; only this thread pushes, so it can't shrink.
        unsafe {
            let mut sq = self.ring.submission();
            sq.push(first)
                .map_err(|_| io::Error::other("io_uring SQ full (read)"))?;
            sq.push(second)
                .map_err(|_| io::Error::other("io_uring SQ full (write)"))?;
        }
        Ok(())
    }

    /// Reap completed CQEs and recycle buffer indices.
    ///
    /// If `wait` is true, blocks until at least one CQE is available.
    ///
    /// Handles both write-only CQEs and linked ReadFixed+WriteFixed pairs:
    /// - Write CQEs (bit 15 clear): validate result and release the buffer.
    /// - Read CQEs (bit 15 set): validate result but do NOT release — the
    ///   linked WriteFixed still references the buffer.
    /// - Canceled write CQEs (`-ECANCELED`): the linked read already failed
    ///   and released the buffer; skip without double-releasing.
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
            let buf_idx = ud_buf_idx(ud);
            let expected_len = ud_expected_len(ud);
            let result = cqe.result();
            self.in_flight -= 1;

            if ud_is_read(ud) {
                // Read CQE from a linked ReadFixed+WriteFixed chain.
                // Don't release — the linked WriteFixed still needs this buffer.
                if result < 0 {
                    // Read failed. Release buffer here; the linked write CQE
                    // will arrive as -ECANCELED and skip release.
                    self.pool.release(buf_idx);
                    return Err(io::Error::from_raw_os_error(-result));
                }
                if (result as u32) != expected_len {
                    self.pool.release(buf_idx);
                    return Err(io::Error::other(format!(
                        "io_uring short read: expected {expected_len}, got {result}"
                    )));
                }
            } else {
                // Write CQE — either standalone WriteFixed or from a linked chain.
                if result == -(libc::ECANCELED as i32) {
                    // Write was canceled because the linked read failed.
                    // Buffer was already released in the read CQE error path.
                    continue;
                }
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

/// Handle a `CopyRange` payload using linked `ReadFixed` → `WriteFixed` SQEs.
///
/// Registers the input fd on first use, then submits linked SQE pairs that
/// read from the input file directly into registered buffers and write them
/// to the output — fully async, no syscalls in userspace beyond `io_uring_enter`.
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn handle_copy_range_uring(
    state: &mut UringState,
    in_fd: std::os::unix::io::RawFd,
    mut src_offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    // Register the input fd if this is the first CopyRange.
    state.register_input_fd(in_fd)?;
    // Flush the accumulation buffer to maintain byte ordering.
    state.submit_current()?;
    // Track logical size upfront since we bypass write() which normally does it.
    state.logical_size += remaining;

    while remaining > 0 {
        let chunk_len = state.pool.buf_size.min(remaining as usize);
        let buf_idx = state.acquire_buffer()?;

        state.submit_copy_chain(src_offset, buf_idx, chunk_len)?;
        src_offset += chunk_len as u64;
        remaining -= chunk_len as u64;
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
#[allow(clippy::needless_pass_by_value)] // Thread entry point — owned values required for move closure.
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
    // Ring depth: 2× buffer count for linked ReadFixed+WriteFixed pairs, plus
    // headroom for standalone writes.
    let ring_depth = u32::from(NUM_BUFS) * 4;
    let ring = builder
        .build(ring_depth)
        .map_err(|e| io::Error::new(e.kind(), format!("io_uring creation failed: {e}")))?;

    // Step 2b: Probe supported opcodes for clear error messages on old kernels.
    // WriteFixed requires Linux 5.1+, ReadFixed requires Linux 5.1+.
    {
        let mut probe = io_uring::Probe::new();
        if ring.submitter().register_probe(&mut probe).is_ok() {
            if !probe.is_supported(opcode::WriteFixed::CODE) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "kernel does not support io_uring WriteFixed (requires Linux 5.1+)",
                ));
            }
            if !probe.is_supported(opcode::ReadFixed::CODE) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "kernel does not support io_uring ReadFixed (requires Linux 5.1+)",
                ));
            }
        }
    }

    // Step 3: Open file with O_DIRECT.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;

    // Step 4: Register fds — 2 slots: [0]=output, [1]=placeholder for input (CopyRange).
    // Slot 1 is filled lazily via register_files_update on first CopyRange.
    ring.submitter()
        .register_files(&[file.as_raw_fd(), -1])
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
        #[cfg(feature = "linux-direct-io")]
        input_fd_registered: false,
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
    let mut pending: ReorderBuffer<PipelinePayload> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);

    for item in rx {
        pending.push(item.seq, item.data);

        // Drain consecutive ready items from the front.
        while let Some(payload) = pending.pop_ready() {
            match payload {
                PipelinePayload::Bytes(result) => {
                    state.write(&result?)?;
                }
                PipelinePayload::ByteChunks(chunks) => {
                    for chunk in &chunks {
                        state.write(chunk)?;
                    }
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

    #[test]
    fn user_data_encoding() {
        // Write CQE: buf_idx=42, aligned_len=256KB
        let write_ud = (262_144u64 << 16) | 42u64;
        assert!(!ud_is_read(write_ud));
        assert_eq!(ud_buf_idx(write_ud), 42);
        assert_eq!(ud_expected_len(write_ud), 262_144);

        // Read CQE: buf_idx=7, len=55000
        let read_ud = (55_000u64 << 16) | 7u64 | READ_FLAG;
        assert!(ud_is_read(read_ud));
        assert_eq!(ud_buf_idx(read_ud), 7);
        assert_eq!(ud_expected_len(read_ud), 55_000);

        // Edge case: buf_idx=63 (max with NUM_BUFS=64)
        let max_ud = (4096u64 << 16) | 63u64;
        assert_eq!(ud_buf_idx(max_ud), 63);
        assert!(!ud_is_read(max_ud));

        let max_read_ud = (4096u64 << 16) | 63u64 | READ_FLAG;
        assert_eq!(ud_buf_idx(max_read_ud), 63);
        assert!(ud_is_read(max_read_ud));
    }
}
