pub(crate) mod batched_sink;
pub mod block_builder;
#[cfg(feature = "linux-direct-io")]
pub mod direct_writer;
pub mod file_writer;
#[cfg(feature = "linux-io-uring")]
pub mod uring_writer;
pub(crate) mod metrics;
pub(crate) mod raw_passthrough;
pub mod writer;

pub(crate) fn should_sync_all() -> bool {
    static SHOULD_SYNC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHOULD_SYNC.get_or_init(|| {
        !matches!(
            std::env::var("PBFHOGG_WRITE_SKIP_SYNC_ALL").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        )
    })
}

pub(crate) fn fallocate_hint_bytes() -> Option<u64> {
    static HINT_BYTES: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *HINT_BYTES.get_or_init(|| {
        std::env::var("PBFHOGG_WRITE_FALLOCATE_BYTES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
    })
}

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
