//! PBF write path: block building, blob framing, and the output backends.
//!
//! # Four backends, deliberately not unified
//!
//! `FileWriter` (buffered), `DirectWriter` (`O_DIRECT`), `UringState`
//! (`io_uring`) and `parallel_writer`'s `pwrite` pool sit behind the
//! `OutputSink` boundary as four separate implementations. That is a
//! decision, not drift - see `decisions/0014-output-backends-stay-separate.md`
//! for why, including the correction that only two of the four ever
//! duplicated the page-alignment logic that unification proposals cite.
//!
//! What the backends must share, they share by name rather than by
//! inheritance: the `SeqTotal` / `check_stream_complete` end-of-stream
//! handshake, the `PermitGuard` write-ahead discipline, the
//! `WriterMetrics::record_sync_all` / `record_recv_wait` funnels, and the
//! `flush`-versus-`sync` split. Each of those is enforced or asserted
//! somewhere concrete; changing one is the trigger to re-read the ADR.

pub mod block_builder;
pub mod compression;
#[cfg(feature = "linux-direct-io")]
mod copy_range;
#[cfg(feature = "linux-direct-io")]
pub mod direct_writer;
mod encode;
pub mod file_writer;
mod framing;
pub mod header_builder;
pub(crate) mod parallel_gzip;
pub(crate) mod parallel_writer;
mod pipeline;
mod string_table;

// Under the `test-hooks` feature, expose the static fault-injection
// hooks so integration tests can arm them. The rest of
// `parallel_writer` stays crate-private.
#[cfg(feature = "test-hooks")]
pub mod parallel_writer_test_hooks {
    pub use super::parallel_writer::test_hooks::{PANIC_AT_POOL_OP_COUNT, POOL_OP_COUNT, reset};
}

// Under the `test-hooks` feature, expose the parallel_gzip hooks and
// the `ParallelGzipWriter` type so integration tests can drive the
// writer directly.
#[cfg(feature = "test-hooks")]
pub mod parallel_gzip_test_hooks {
    pub use super::parallel_gzip::test_hooks::{PANIC_AT_POOL_OP_COUNT, POOL_OP_COUNT, reset};
    pub use super::parallel_gzip::{DEFAULT_CHUNK_SIZE, ParallelGzipWriter};
}

// Under the `test-hooks` feature, expose the uring_writer hooks.
// Test arms `PANIC_AT_DISPATCH_COUNT` before invoking any code path
// that constructs a `PbfWriter::to_path_uring`.
#[cfg(all(feature = "test-hooks", feature = "linux-io-uring"))]
pub mod uring_writer_test_hooks {
    pub use super::uring_writer::test_hooks::{DISPATCH_COUNT, PANIC_AT_DISPATCH_COUNT, reset};
}
pub(crate) mod metrics;

/// Emit the `writer_*` sidecar counters, at most once per process.
///
/// Call this once, on the way out, after every `PbfWriter` in the process has
/// been flushed - the CLI does so on its single exit path. Repeat calls are
/// no-ops, so an early-return path may call it too.
///
/// The counters are process totals rather than per-writer numbers; see
/// [`metrics::WRITER_METRICS`] for why, and for what per-writer attribution
/// would cost.
pub fn emit_writer_counters() {
    metrics::WRITER_METRICS.emit_once();
}
#[cfg(feature = "linux-io-uring")]
pub mod uring_writer;
pub mod writer;

/// Page size for alignment. 4096 is universally safe across Linux filesystems.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
const PAGE_SIZE: usize = 4096;

/// Allocate `size` bytes aligned to `PAGE_SIZE`. Returns the pointer and layout
/// (needed for deallocation). Centralizes the unsafe page-aligned allocation
/// used by both `DirectWriter` and `AlignedBufferPool`.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
fn alloc_page_aligned(size: usize) -> std::io::Result<(std::ptr::NonNull<u8>, std::alloc::Layout)> {
    let layout = std::alloc::Layout::from_size_align(size, PAGE_SIZE)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // Safety: layout has non-zero size (callers pass BUF_CAPACITY or total > 0).
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    let ptr = std::ptr::NonNull::new(ptr).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::OutOfMemory, "aligned alloc failed")
    })?;
    Ok((ptr, layout))
}
