//! io_uring writer thread with registered buffers for pipelined PBF output.
//!
//! Replaces the buffered sink used by
//! [`super::pipeline::writer_thread`] with an io_uring submission loop. Uses
//! `O_DIRECT` + `WriteFixed` with pre-registered
//! aligned buffers for maximum throughput when the pipeline is I/O-bound
//! (e.g. `Compression::None` on erofs).
//!
//! Data is accumulated into 256 KB registered buffers (same strategy as
//! [`super::direct_writer::DirectWriter`]). When a buffer fills, a `WriteFixed`
//! SQE is submitted. CQEs are reaped to recycle buffer indices via a free-list.

use super::IO_BUF_BYTES;

use std::collections::VecDeque;

use crate::error::{Error, Result};
use crate::reorder_buffer::ReorderBuffer;
use crate::tunables::{NUM_BUFS, WRITE_AHEAD};
use crate::write::pipeline::{OutputChunk, PipelineItem, SeqTotal, check_stream_complete};
use io_uring::IoUring;
use io_uring::opcode;
use io_uring::types::Fixed;
use std::alloc::{self, Layout};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, SyncSender};

use super::metrics::WRITER_METRICS;
use crate::direct_io::{alloc_aligned_zeroed, round_up};

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

// `NUM_BUFS` (in `crate::tunables`) is asserted there to fit the 6-bit mask in
// [`ud_buf_idx`]; widening the pool means widening that mask.

// ---------------------------------------------------------------------------
// AlignedBufferPool - contiguous aligned allocation with free-list
// ---------------------------------------------------------------------------

/// A pool of aligned buffers for io_uring registered I/O.
///
/// Allocates a single contiguous block of `count × buf_size` bytes, aligned
/// to the output file's direct I/O alignment. `buf_size` is a multiple of
/// that alignment, so every buffer starts aligned too. Each buffer is
/// addressed by index (0..count). A free-list tracks which indices are
/// available for use.
struct AlignedBufferPool {
    base: NonNull<u8>,
    layout: Layout,
    count: u16,
    buf_size: usize,
    /// The direct I/O alignment `crate::direct_io` discovered for the output
    /// file: a power of two dividing `buf_size`. Every `WriteFixed` address,
    /// offset and length is a multiple of it.
    align: usize,
    free: VecDeque<u16>,
}

// Safety: AlignedBufferPool is a plain owned byte array with no shared references.
// It is only accessed by the owning writer thread.
unsafe impl Send for AlignedBufferPool {}

impl AlignedBufferPool {
    /// Allocate `count` buffers of `buf_size` bytes each, aligned to `align`
    /// (a power of two dividing `buf_size`).
    fn new(count: u16, buf_size: usize, align: usize) -> Result<Self> {
        if !align.is_power_of_two() || !buf_size.is_multiple_of(align) {
            return Err(Error::internal(
                "io_uring buffer size is not a multiple of the direct I/O alignment",
            ));
        }
        let total = buf_size * count as usize;
        let (base, layout) = alloc_aligned_zeroed(total, align)?;
        let mut free = VecDeque::with_capacity(count as usize);
        for i in 0..count {
            free.push_back(i);
        }
        Ok(Self {
            base,
            layout,
            count,
            buf_size,
            align,
            free,
        })
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
    /// Physical write offset (always advances by multiples of the alignment).
    write_offset: u64,
    /// Bytes accepted into registered buffers, excluding alignment padding.
    /// Updated only by `accept_buffered` after bytes have been copied/read.
    /// Completion errors still abort before `flush_final` can truncate.
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
    fn write(&mut self, data: &[u8]) -> Result<()> {
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
                None => return Err(Error::internal("no current buffer")),
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
            self.accept_buffered(chunk);
            offset += chunk;

            // If buffer is full, submit it.
            if self.current_len == self.pool.buf_size {
                self.submit_current()?;
            }
        }
        Ok(())
    }

    /// Account for initialized payload bytes in the current buffer.
    /// Both framed writes and CopyRange reads use this accounting protocol.
    #[allow(clippy::cast_possible_truncation)] // bytes as u64: usize fits u64
    fn accept_buffered(&mut self, bytes: usize) {
        self.current_len += bytes;
        self.logical_size += bytes as u64;
    }

    /// Submit the current accumulation buffer as a `WriteFixed` SQE.
    fn submit_current(&mut self) -> Result<()> {
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
    /// Handles O_DIRECT alignment padding, advances `write_offset` and
    /// `in_flight`. Releases the buffer immediately if `data_len` is zero.
    /// Does **not** update `logical_size`: `accept_buffered` already counted
    /// payload bytes, and alignment padding must never contribute.
    #[allow(clippy::cast_possible_truncation)] // aligned_len as u32/u64, buf_idx as u64: bounded by IO_BUF_BYTES/NUM_BUFS
    fn submit_buffer(&mut self, buf_idx: u16, data_len: usize) -> Result<()> {
        if data_len == 0 {
            self.pool.release(buf_idx);
            return Ok(());
        }

        // Pad to the file's alignment for O_DIRECT. Zero-fill the padding
        // region (buffer memory may have stale data from a previous use).
        let aligned_len = round_up(data_len, self.pool.align);
        if aligned_len > data_len {
            // Safety: buf_idx is valid and not in-flight, padding is within
            // the buffer allocation (aligned_len <= buf_size).
            unsafe {
                let dst = self.pool.buf_ptr(buf_idx).add(data_len);
                std::ptr::write_bytes(dst, 0, aligned_len - data_len);
            }
        }

        let ptr = self.pool.buf_ptr(buf_idx).cast_const();
        let sqe = opcode::WriteFixed::new(Fixed(OUT_FD_IDX), ptr, aligned_len as u32, buf_idx)
            .offset(self.write_offset)
            .build()
            // Pack buf_idx (low 16 bits) and expected length (upper 48 bits) into user_data
            // so reap_cqes can detect short writes.
            .user_data((aligned_len as u64) << 16 | buf_idx as u64);

        // Safety: the SQE references a registered buffer (buf_idx) and a
        // registered fd (OUT_FD_IDX). The buffer will not be touched until the
        // CQE is reaped (enforced by the free-list protocol).
        self.push_sqe(&sqe)?;
        let t_submit = crate::counters::Stopwatch::start();
        self.ring
            .submitter()
            .submit()
            .map_err(|e| Error::from(e).context("io_uring submit failed"))?;
        WRITER_METRICS.uring_submit_calls.fetch_add(1, Relaxed);
        t_submit.record_ns(&WRITER_METRICS.uring_submit_ns);

        self.write_offset += aligned_len as u64;
        self.in_flight += 1;

        Ok(())
    }

    /// Acquire a free buffer, reaping CQEs if necessary.
    fn acquire_buffer(&mut self) -> Result<u16> {
        // Fast path: free buffer available.
        if let Some(idx) = self.pool.acquire() {
            return Ok(idx);
        }
        // Slow path: all buffers in-flight, wait until a buffer is freed.
        loop {
            if self.in_flight == 0 {
                return Err(Error::internal("no free buffers and nothing in-flight"));
            }
            self.reap_cqes(true)?;
            if let Some(idx) = self.pool.acquire() {
                return Ok(idx);
            }
        }
    }

    /// Push a single SQE to the submission queue, waiting for SQ space if full.
    fn push_sqe(&mut self, sqe: &io_uring::squeue::Entry) -> Result<()> {
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
            .map_err(|e| Error::from(e).context("squeue_wait failed"))?;
        unsafe {
            self.ring
                .submission()
                .push(sqe)
                .map_err(|_| Error::internal("io_uring SQ still full after squeue_wait"))?;
        }
        Ok(())
    }

    /// Reap completed CQEs and recycle buffer indices.
    ///
    /// If `wait` is true, blocks until at least one CQE is available.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // ud as u16, result as u32: io_uring CQE fields
    fn reap_cqes(&mut self, wait: bool) -> Result<()> {
        if self.in_flight == 0 {
            return Ok(());
        }
        if wait {
            let t_wait = crate::counters::Stopwatch::start();
            self.ring
                .submitter()
                .submit_and_wait(1)
                .map_err(|e| Error::from(e).context("io_uring submit_and_wait failed"))?;
            let elapsed = t_wait.elapsed_ns();
            WRITER_METRICS
                .uring_submit_and_wait_calls
                .fetch_add(1, Relaxed);
            WRITER_METRICS
                .uring_submit_and_wait_ns
                .fetch_add(elapsed, Relaxed);
        }
        for cqe in self.ring.completion() {
            let ud = cqe.user_data();
            let buf_idx = ud_buf_idx(ud);
            let expected_len = ud_expected_len(ud);
            let result = cqe.result();
            self.in_flight -= 1;

            // Release the buffer slot on every CQE, success or failure.
            // Previously only the success path ran release, so a
            // kernel-level error or short write leaked the slot. The
            // writer typically tears down on error anyway, but keep the
            // pool accounting consistent so any code path that tries to
            // observe `pool.free_count()` or continue past an error
            // sees a sane state.
            self.pool.release(buf_idx);

            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result).into());
            }
            if result.cast_unsigned() != expected_len {
                return Err(Error::io_kind(
                    io::ErrorKind::WriteZero,
                    format!("io_uring short write: expected {expected_len} bytes, got {result}"),
                ));
            }
        }
        Ok(())
    }

    /// Drain all in-flight SQEs.
    fn drain(&mut self) -> Result<()> {
        while self.in_flight > 0 {
            self.reap_cqes(true)?;
        }
        Ok(())
    }

    /// Drain in-flight SQEs on the teardown path, ignoring their outcomes.
    ///
    /// [`drain`](Self::drain) stops at the first failing CQE, which is exactly
    /// the case teardown has to survive: `reap_cqes` decrements `in_flight`
    /// before returning the error, so the remaining completions are still
    /// pending when the state is dropped. This keeps reaping until the ring is
    /// quiet.
    ///
    /// This is best-effort cleanup, not a wall-clock shutdown guarantee.
    /// The round cap only limits retries after errors: `submit_and_wait(1)`
    /// itself has no deadline, and a submit error need not consume a CQE.
    /// Keep the blocking drain here because returning on a timer would not
    /// establish that registered buffers are no longer in use. The io-uring
    /// dependency's `IoUring::drop` unmaps the queues and closes the fd; it
    /// does not expose a timed quiescence handshake. Cancellation is also a
    /// request whose completion must be observed, not a deadline guarantee.
    /// A bounded return therefore needs a separate ownership policy for an
    /// abandoned ring and its allocation (leak them, or transfer them to a
    /// reaper that may itself wait indefinitely). That changes resource and
    /// shutdown semantics and is deliberately not hidden in a retry limit.
    fn drain_best_effort(&mut self) {
        let mut rounds = 0_u32;
        while self.in_flight > 0 && rounds < u32::from(NUM_BUFS) * 4 {
            rounds += 1;
            if self.reap_cqes(true).is_err() {
                // A failing CQE consumes one in-flight entry; a submit error
                // may consume none. Retry either case up to the round cap.
            }
        }
    }

    /// Flush the current partial buffer, drain all in-flight writes,
    /// truncate to logical size, and (when `finish` is durable) sync to disk.
    /// A deferred finish leaves the sync to the output's owner.
    fn flush_final(&mut self, file: &File, finish: super::Finish) -> Result<()> {
        self.submit_current()?;
        self.drain()?;
        file.set_len(self.logical_size)?;
        if finish == super::Finish::Durable {
            // A path-constructor writer's own durability.
            WRITER_METRICS.sync_recorded(file)?;
        }
        Ok(())
    }
}

/// Quiesce the ring before the registered buffers are deallocated.
///
/// Every error path out of `uring_init_and_run` - a failed `write`, an error
/// from `uring_main_loop`, a failed `flush_final` - drops `UringState` with
/// SQEs still in flight. Field order drops `ring` before `pool`, but closing
/// the ring fd is **not** a synchronous barrier: `io_uring_release` hands the
/// context to `io_ring_exit_work` on a workqueue, which cancels requests and
/// only then runs `io_sqe_buffers_unregister` to unpin the registered pages.
/// Userspace returns from `close(2)` long before that finishes, so `pool`'s
/// `dealloc` can hand the virtual range back to the allocator while the kernel
/// still holds pins on it.
///
/// The consequence today is bounded, and narrower than it first looks: the
/// only opcode this writer ever submits is `WriteFixed`, which the kernel
/// *reads* from. A completing SQE therefore cannot scribble into memory the
/// allocator has re-issued - it can only copy whatever now lives there into an
/// output file that is being abandoned anyway. That is not memory corruption.
/// It would become memory corruption the moment any read-side opcode is added
/// (the `ReadFixed` chain this writer used to submit for `CopyRange` was
/// exactly that shape), and it is a pin leak either way.
///
/// A successful drain removes the window. There are at most `NUM_BUFS`
/// completions outstanding, but no time bound on obtaining them. If repeated
/// submit errors exhaust the retry cap, cleanup remains best-effort and the
/// field-order fallback above still applies; this is not guaranteed quiescence.
impl Drop for UringState {
    fn drop(&mut self) {
        self.drain_best_effort();
    }
}

// ---------------------------------------------------------------------------
// CopyRange handling
// ---------------------------------------------------------------------------

/// A `CopyRange` payload is read by `copy_range::read_range` straight into
/// the current registered buffer, and the normal accumulator submits full
/// buffers via `submit_current` (always aligned, always safe).
///
/// The previous design used linked `ReadFixed` → `WriteFixed` SQE chains to
/// keep the data path fully async, but that submitted a `WriteFixed` whose
/// length was `copy_len` rounded up to the alignment on every CopyRange call.
/// When `copy_len` was not a multiple of the alignment, the write padded out
/// to the next aligned boundary on disk while `logical_size` tracked only the real
/// data bytes. The result: mid-stream zero-filled gaps in the output that
/// `set_len(logical_size)` could not remove (the gaps were inside the file,
/// not at the end). Readers saw `OSMHeader` followed by nulls where the next
/// `OSMData` blob header should have been.
///
/// This sink preserves the writer's core invariant: mid-stream `WriteFixed`
/// SQEs are always `IO_BUF_BYTES` (a multiple of the alignment) because
/// `filled` submits only when `current_len == IO_BUF_BYTES`; a partial
/// buffer carries over to the next item, and only the final flush_final SQE
/// is partial, its trailing zero-padding then truncated by `set_len`.
///
/// The read lands directly in the registered buffer at `current_len`, so
/// there is no user-space bounce buffer and no extra copy beyond what the
/// kernel's read path already performs. A short read is progress: the input
/// fd is an ordinary buffered one (no O_DIRECT alignment on the destination,
/// length or offset), and the alignment the O_DIRECT output needs is kept by
/// submitting full buffers only.
///
/// `read_range` enforces the reservation protocol; `space` and `filled`
/// still bounds-check against the buffer with checked arithmetic, because
/// `current_len` feeds the pointer arithmetic below.
#[cfg(feature = "linux-direct-io")]
impl super::copy_range::ReadSink for UringState {
    fn space(&mut self, want: usize) -> Result<&mut [u8]> {
        let buf_idx = match self.current_buf {
            Some(idx) => idx,
            None => {
                let idx = self.acquire_buffer()?;
                self.current_buf = Some(idx);
                self.current_len = 0;
                idx
            }
        };
        let free = self
            .pool
            .buf_size
            .checked_sub(self.current_len)
            .ok_or_else(|| Error::internal("io_uring buffer filled past its size"))?;
        let len = free.min(want);
        // Safety: `buf_idx` was acquired from the pool and is not in flight
        // (only submitted buffers are, and `current_buf` is taken on submit).
        // The slice covers `current_len..current_len + len` of that one
        // buffer, inside its allocation since `len <= buf_size - current_len`,
        // and never another buffer an in-flight `WriteFixed` may be reading.
        // The pool is allocated zeroed, so the bytes are initialised. The
        // borrow of `self` ends before `filled` can submit the buffer.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.pool.buf_ptr(buf_idx).add(self.current_len), len)
        })
    }

    fn filled(&mut self, n: usize) -> Result<()> {
        let free = self
            .pool
            .buf_size
            .checked_sub(self.current_len)
            .ok_or_else(|| Error::internal("io_uring buffer filled past its size"))?;
        if self.current_buf.is_none() || n > free {
            return Err(Error::internal(
                "io_uring CopyRange read reported more than its buffer space",
            ));
        }
        self.accept_buffered(n);
        if self.current_len == self.pool.buf_size {
            self.submit_current()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Writer thread entry point
// ---------------------------------------------------------------------------

/// io_uring writer thread: receives framed blobs via channel and writes them
/// to disk using io_uring `WriteFixed` with registered aligned buffers.
///
/// Opens the output file with `O_DIRECT`, creates an io_uring instance,
/// registers the fd and buffer pool. Init errors are sent back via `init_tx`
/// so the constructor can propagate them immediately.
#[allow(clippy::needless_pass_by_value)] // Thread entry point - owned values required for move closure.
pub(crate) fn uring_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<Result<()>>,
    expected: &SeqTotal,
    finish: super::Finish,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    // A failure is not logged here: it is the thread's return value, which the
    // writer's `flush` (or the failed send that noticed the thread was gone)
    // hands to the caller, so the operator sees it exactly once.
    uring_init_and_run(
        &rx,
        &path,
        &framed_header,
        &init_tx,
        expected,
        finish,
        cancel,
    )
}

/// Refuse, as a typed platform refusal, a ring whose kernel cannot run
/// `WriteFixed` or whose opcode probe fails for a reason other than age.
fn require_write_fixed(ring: &IoUring) -> Result<()> {
    let mut probe = io_uring::Probe::new();
    match ring.submitter().register_probe(&mut probe) {
        Ok(()) if probe.is_supported(opcode::WriteFixed::CODE) => Ok(()),
        Ok(()) => Err(Error::platform_unsupported(
            "io-uring",
            io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel does not support io_uring WriteFixed (requires Linux 5.1+)",
            ),
        )),
        // IORING_REGISTER_PROBE arrived in Linux 5.6, but WriteFixed has
        // existed since io_uring itself (5.1). With the ring already set up,
        // EINVAL here is a 5.1-5.5 kernel that supports WriteFixed and cannot
        // say so; refusing it would reject a working host.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        Err(e) => Err(Error::platform_unsupported(
            "io-uring",
            setup_step(e, "io_uring opcode probe failed"),
        )),
    }
}

/// A setup failure `source` with the failing step named, as the `io::Error`
/// a platform refusal holds. The original error stays the cause inside the
/// carried chain; `report()` prints `step: cause` after the refusal's own
/// text.
fn setup_step(source: io::Error, step: &'static str) -> io::Error {
    io::Error::from(Error::from(source).context(step))
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
    init_tx: &SyncSender<Result<()>>,
    expected: &SeqTotal,
    finish: super::Finish,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    // Step 1: Declare the buffer pool. It is allocated in step 3, once the
    // output file has reported its direct I/O alignment, but declared here
    // so that locals still drop ring first, pool second (as `UringState`'s
    // field order does) on an error return before the state is built.
    let pool: AlignedBufferPool;

    // Step 2: Create io_uring ring.
    let mut builder = IoUring::builder();
    builder.setup_clamp();
    // Ring depth: enough headroom so one `WriteFixed` can be in flight per
    // registered buffer without the submission queue stalling, plus slack
    // for bursty submission.
    let ring_depth = u32::from(NUM_BUFS) * 2;
    let ring = builder.build(ring_depth).map_err(|e| {
        // Before Linux 5.12, ring memory can be charged to RLIMIT_MEMLOCK.
        // io_uring_setup also uses ENOMEM for generic kernel resource
        // exhaustion. The later threshold covers registered buffers; ring
        // memory depends on kernel ring parameters. Keep ENOMEM ordinary until
        // a reliable ring-specific threshold is available.
        // Only setup refusal is skippable. Resource exhaustion and runtime
        // submission/completion errors remain ordinary I/O failures.
        let refused = matches!(
            e.raw_os_error(),
            Some(libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EOPNOTSUPP)
        );
        if refused {
            Error::platform_unsupported("io-uring", setup_step(e, "io_uring creation failed"))
        } else {
            Error::from(e).context("io_uring creation failed")
        }
    })?;

    // Step 2b: Probe supported opcodes for clear error messages on old kernels.
    require_write_fixed(&ring)?;

    // Step 3: Open file with O_DIRECT, after the ring refusals above so they
    // precede file creation, then allocate the pool at the alignment the
    // file reported.
    let (file, align) = crate::direct_io::create_for_write(path)?;
    pool = AlignedBufferPool::new(NUM_BUFS, IO_BUF_BYTES, align)?;
    let iovecs = pool.iovecs();

    // Step 4: Register the output fd so `WriteFixed` can reference it by slot.
    ring.submitter()
        .register_files(&[file.as_raw_fd()])
        .map_err(|e| Error::from(e).context("register_files failed"))?;

    // Step 5: Register buffers (this is where RLIMIT_MEMLOCK bites).
    // Safety: iovecs point into the AlignedBufferPool allocation which
    // lives for the duration of this function.
    unsafe { ring.submitter().register_buffers(&iovecs) }.map_err(|e| {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // The threshold is the exact registered size, not an estimate: every
        // buffer is whole page-aligned pages, and the kernel charges the
        // pinned pages against RLIMIT_MEMLOCK (fewer when it coalesces huge
        // pages, none with CAP_IPC_LOCK). A soft limit at or above it rules
        // the limit out, so only a lower limit earns the ulimit advice.
        // Safety: limit is a valid writable rlimit for getrlimit.
        let limited = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } == 0
            && limit.rlim_cur < (u64::from(NUM_BUFS) * IO_BUF_BYTES as u64);
        if limited && matches!(e.raw_os_error(), Some(libc::ENOMEM | libc::EPERM)) {
            Error::platform_unsupported(
                "io-uring",
                io::Error::from(Error::from(e).context(format!(
                    "register_buffers failed: RLIMIT_MEMLOCK too low \
                     (need {} MB, try `ulimit -l unlimited`)",
                    (NUM_BUFS as usize * IO_BUF_BYTES) / (1024 * 1024)
                ))),
            )
        } else {
            Error::from(e).context("register_buffers failed")
        }
    })?;

    // Step 6: Signal successful init.
    init_tx
        .send(Ok(()))
        .map_err(|_| Error::internal("constructor dropped before init completed"))?;

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
    uring_main_loop(rx, &mut state, expected, cancel)?;

    // Step 9: Flush and finalize.
    crate::write::metrics::with_sync_observer(cancel, || state.flush_final(&file, finish))?;

    Ok(())
}

/// Reorder and write loop using the shared sequence-number reorder buffer,
/// then dispatches to [`UringState::write`].
fn uring_main_loop(
    rx: &Receiver<PipelineItem>,
    state: &mut UringState,
    expected: &SeqTotal,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    #[cfg(not(any(feature = "linux-direct-io", feature = "test-hooks")))]
    let _ = cancel;
    let mut pending: ReorderBuffer<Result<OutputChunk>> = ReorderBuffer::with_capacity(WRITE_AHEAD);

    loop {
        let t_recv = crate::counters::Stopwatch::start();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS.record_recv_wait(t_recv);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            let chunk = result?;

            // Test-only: the fault plan's `uring_panic_at_dispatch`, counted
            // per item popped from the reorder buffer. Simulates a
            // mid-stream panic in the uring writer thread; the writer's
            // join surfaces it as an error. Release builds compile this
            // out entirely.
            #[cfg(feature = "test-hooks")]
            cancel.faults().on_uring_dispatch();

            match chunk {
                OutputChunk::Framed(parts) => {
                    // io_uring backend flattens at the backend boundary
                    // (registered-buffer copy is backend-local).
                    let bytes = parts.into_vec();
                    let len = bytes.len() as u64;
                    let t_write = crate::counters::Stopwatch::start();
                    state.write(&bytes)?;
                    t_write.record_ns(&WRITER_METRICS.write_ns);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                }
                OutputChunk::Raw(bytes) => {
                    let len = bytes.len() as u64;
                    let t_write = crate::counters::Stopwatch::start();
                    state.write(&bytes)?;
                    t_write.record_ns(&WRITER_METRICS.write_ns);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                }
                OutputChunk::RawChunks(chunks) => {
                    let total_bytes: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
                    let t_write = crate::counters::Stopwatch::start();
                    for chunk in &chunks {
                        state.write(chunk)?;
                    }
                    t_write.record_ns(&WRITER_METRICS.write_ns);
                    WRITER_METRICS.bytes_written.fetch_add(total_bytes, Relaxed);
                }
                #[cfg(feature = "linux-direct-io")]
                OutputChunk::CopyRange {
                    in_file,
                    offset,
                    len,
                } => {
                    let t_write = crate::counters::Stopwatch::start();
                    super::copy_range::read_range(&in_file, offset, len, cancel, state)?;
                    // The Arc keeps the input open across the read loop.
                    drop(in_file);
                    t_write.record_ns(&WRITER_METRICS.write_ns);
                    WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                }
            }
        }

        // Opportunistically reap CQEs without blocking to recycle buffers.
        state.reap_cqes(false)?;
    }

    // Channel closed. A dead upstream framer closes the channel exactly the
    // way a finished one does, so completeness has to be proved rather than
    // assumed - for a gap that stranded later items AND for a gap at the tail
    // that stranded nothing. Getting this wrong here is worse than in the
    // other backends: step 9 would go on to `flush_final`, padding and
    // `set_len`-ing a short file into a well-formed-looking one. See
    // `check_stream_complete`.
    check_stream_complete(
        "uring writer",
        pending.pending_len(),
        pending.next_seq(),
        expected,
    )
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
        // Above the floor, so the test shows the pool honours the alignment
        // it is handed rather than a fixed page size.
        const ALIGN: usize = crate::direct_io::DIO_ALIGN_FLOOR * 4;
        let mut pool = AlignedBufferPool::new(4, ALIGN * 2, ALIGN).unwrap();
        assert_eq!(pool.buf_ptr(0) as usize % ALIGN, 0);
        assert_eq!(pool.buf_ptr(1) as usize % ALIGN, 0);
        // Buffers are distinct.
        let diff = unsafe { pool.buf_ptr(1).offset_from(pool.buf_ptr(0)) };
        assert_eq!(diff, (ALIGN * 2).cast_signed());
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
    fn pool_refuses_a_buffer_size_off_its_alignment() {
        let floor = crate::direct_io::DIO_ALIGN_FLOOR;
        assert!(AlignedBufferPool::new(2, floor * 3, floor * 2).is_err());
        assert!(AlignedBufferPool::new(2, floor * 3, floor * 3).is_err());
    }

    #[test]
    fn user_data_encoding() {
        // Write CQE: buf_idx=42, aligned_len=256KB
        let write_ud = (u64::try_from(IO_BUF_BYTES).unwrap() << 16) | 42u64;
        assert_eq!(ud_buf_idx(write_ud), 42);
        assert_eq!(
            ud_expected_len(write_ud),
            u32::try_from(IO_BUF_BYTES).unwrap()
        );

        // Edge case: buf_idx=63 (max with NUM_BUFS=64)
        let max_ud = (4096u64 << 16) | 63u64;
        assert_eq!(ud_buf_idx(max_ud), 63);
        assert_eq!(ud_expected_len(max_ud), 4096);
    }
}
