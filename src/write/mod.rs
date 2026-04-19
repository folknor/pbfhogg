pub mod block_builder;
pub(crate) mod buf_pool;
pub mod header_builder;
#[cfg(feature = "linux-direct-io")]
pub mod direct_writer;
pub mod file_writer;
#[cfg(feature = "linux-io-uring")]
pub mod uring_writer;
pub(crate) mod metrics;
pub(crate) mod raw_passthrough;
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
    let ptr = std::ptr::NonNull::new(ptr)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "aligned alloc failed"))?;
    Ok((ptr, layout))
}
