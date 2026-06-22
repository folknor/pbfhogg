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
pub(crate) mod raw_passthrough;
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
