//! Blob decompression helpers: the shared `DecompressPool` for buffer reuse,
//! the zlib thread-local state, and the suite of `decompress_*` entry points
//! called from pipelined and pread read paths.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use flate2::Decompress;
use std::cell::RefCell;

use crate::error::{BlobError, ErrorKind, Result, new_blob_error, new_error};

use super::blob_wire::{BlobData, MAX_BLOB_MESSAGE_SIZE, WireBlob};

thread_local! {
    /// Per-thread reusable zlib decompressor state (~32 KB inflate tables).
    /// Reset via `Decompress::reset(true)` between blobs instead of allocating
    /// a fresh instance each time.
    static ZLIB_DECOMPRESS: RefCell<Decompress> = RefCell::new(Decompress::new(true));
}

/// Maximum capacity (bytes) of a buffer retained in the pool.
/// Buffers larger than this are dropped on return instead of recycled.
/// This prevents outlier blobs (up to 32 MB decompressed) from permanently
/// inflating the pool's retained memory.
///
/// Set to 4 MB: covers >99% of real-world PBF blocks (8000 elements at
/// typical sizes) while dropping the long tail of outlier blocks.
const MAX_RETAINED_CAPACITY: usize = 4 * 1024 * 1024;

/// Maximum number of buffers retained in the pool.
/// Defense-in-depth: prevents unbounded pool growth if pipeline topology changes.
const MAX_POOL_SIZE: usize = 64;

/// Thread-safe pool of decompression buffers for reuse across pipeline blobs.
///
/// In the pipelined read path, each blob decompresses into a fresh `Vec<u8>`
/// (~1.4 MB average), which is then wrapped as `Bytes` for zero-copy protobuf
/// parsing. Without pooling, this causes 10.2 GB of cumulative alloc/dealloc for
/// Denmark (483 MB), or ~1.7 TB for a planet file.
///
/// The pool holds `Vec<u8>` buffers returned by [`PooledBuffer`]'s `Drop` impl.
/// Buffers are popped via [`DecompressPool::get`] and returned automatically when
/// the `Bytes` (and the `PrimitiveBlock` holding slices of it) is dropped.
pub(crate) struct DecompressPool {
    buffers: Mutex<Vec<Vec<u8>>>,
}

impl DecompressPool {
    /// Create a new empty pool wrapped in `Arc` for shared ownership.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            buffers: Mutex::new(Vec::new()),
        })
    }

    /// Pop a buffer from the pool, or return an empty Vec if the pool is empty.
    // wontfix(type-result-fallible): .ok() on Mutex::lock() - poisoning means another
    // thread panicked; falling back to a fresh Vec is the correct recovery here.
    pub fn get(&self) -> Vec<u8> {
        self.buffers
            .lock()
            .ok()
            .and_then(|mut v| v.pop())
            .unwrap_or_default()
    }

    /// Return a buffer to the pool for reuse.
    ///
    /// Drops oversized buffers (capacity > 4 MB) instead of retaining them,
    /// preventing outlier blobs from permanently inflating pool memory.
    /// Also enforces a count cap as defense-in-depth.
    fn put(&self, mut buf: Vec<u8>) {
        if buf.capacity() > MAX_RETAINED_CAPACITY {
            return;
        }
        buf.clear();
        if let Ok(mut v) = self.buffers.lock()
            && v.len() < MAX_POOL_SIZE
        {
            v.push(buf);
        }
    }
}

/// Owner type for `Bytes::from_owner` that returns its buffer to a
/// [`DecompressPool`] on drop instead of freeing it.
struct PooledBuffer {
    vec: Vec<u8>,
    pool: Arc<DecompressPool>,
}

impl AsRef<[u8]> for PooledBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.vec
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        let v = std::mem::take(&mut self.vec);
        self.pool.put(v);
    }
}

/// Get a decompression buffer - from the pool if available, otherwise fresh.
pub(super) fn pool_get(pool: Option<&Arc<DecompressPool>>, capacity: usize) -> Vec<u8> {
    match pool {
        Some(p) => {
            let mut buf = p.get();
            buf.reserve(capacity.saturating_sub(buf.capacity()));
            buf
        }
        None => Vec::with_capacity(capacity),
    }
}

/// Get a buffer from the pool (or allocate fresh). Public for pread-from-workers.
pub(crate) fn pool_get_pub(pool: &Arc<DecompressPool>, capacity: usize) -> Vec<u8> {
    let mut buf = pool.get();
    buf.reserve(capacity.saturating_sub(buf.capacity()));
    buf
}

/// Wrap decoded bytes as `Bytes` - returning to pool on drop if pooled.
pub(crate) fn pool_wrap(decoded: Vec<u8>, pool: Option<&Arc<DecompressPool>>) -> Bytes {
    match pool {
        Some(p) => Bytes::from_owner(PooledBuffer {
            vec: decoded,
            pool: Arc::clone(p),
        }),
        None => Bytes::from(decoded),
    }
}

/// Decompress zlib data into `buf` using the thread-local reusable decompressor.
///
/// Resets the decompressor state between calls instead of allocating a fresh
/// ~32 KB inflate state per blob. Enforces `MAX_BLOB_MESSAGE_SIZE`.
#[allow(clippy::cast_possible_truncation)] // total_in delta bounded by input.len() (usize)
fn zlib_decompress_into(compressed: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    ZLIB_DECOMPRESS.with_borrow_mut(|decompress| {
        decompress.reset(true);
        let mut input = compressed;
        loop {
            if buf.len() == buf.capacity() {
                buf.reserve(input.len().max(4096));
            }
            let before_in = decompress.total_in();
            let status = decompress
                .decompress_vec(input, buf, flate2::FlushDecompress::None)
                .map_err(|e| {
                    new_error(ErrorKind::Io(std::io::Error::other(format!(
                        "zlib decompress error: {e}"
                    ))))
                })?;
            let consumed = (decompress.total_in() - before_in) as usize;
            input = &input[consumed..];
            if matches!(status, flate2::Status::StreamEnd) {
                break;
            }
            // Output buffer was full - grow and retry.
            if buf.len() == buf.capacity() {
                buf.reserve(buf.len().max(4096));
            }
        }
        let size = buf.len() as u64;
        if size > MAX_BLOB_MESSAGE_SIZE {
            return Err(new_blob_error(BlobError::MessageTooBig { size }));
        }
        Ok(())
    })
}

/// Decompress raw blob bytes into a caller-provided buffer.
///
/// Takes raw bytes (not a parsed `WireBlob`) and decompresses directly,
/// allocating a new `Vec` each time. For loops that decompress many blobs,
/// this avoids repeated large allocations -- the buffer grows to high-water
/// mark and stays there.
pub(crate) fn decompress_blob_data_into(blob_bytes: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    decompress_parsed_blob_into(&blob, buf)
}

/// Decompress a parsed Blob protobuf into a caller-provided buffer.
#[allow(clippy::cast_sign_loss)]
pub(super) fn decompress_parsed_blob_into(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
    buf.clear();
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size < MAX_BLOB_MESSAGE_SIZE {
                buf.extend_from_slice(bytes);
                Ok(())
            } else {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            let cap = blob.estimated_capacity();
            if cap > 0 {
                buf.reserve(cap.saturating_sub(buf.capacity()));
            }
            zlib_decompress_into(bytes, buf)
        }
        Some(BlobData::Zstd(bytes)) => {
            let cap = blob.estimated_capacity();
            if cap > 0 {
                buf.reserve(cap.saturating_sub(buf.capacity()));
            }
            zstd::stream::copy_decode(Cursor::new(&**bytes), &mut *buf)?;
            let size = buf.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
            Ok(())
        }
        None => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Decompress raw blob bytes (from pread) into a caller-owned buffer.
///
/// Parses the blob wire format inline and decompresses the payload without
/// constructing intermediate `WireBlob` or `Bytes` objects. Used by parallel
/// pipelines where workers read blob data via pread and need all alloc/free
/// to stay thread-local.
#[hotpath::measure]
pub(crate) fn decompress_blob_raw(raw_blob: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    use super::wire::Cursor;
    buf.clear();

    let mut cursor = Cursor::new(raw_blob);
    let mut raw_size: Option<i32> = None;
    let mut found = false;

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match field {
            1 => {
                // raw (uncompressed): bytes
                let slice = cursor.read_len_delimited()?;
                let size = slice.len() as u64;
                if size > MAX_BLOB_MESSAGE_SIZE {
                    return Err(new_blob_error(BlobError::MessageTooBig { size }));
                }
                buf.extend_from_slice(slice);
                return Ok(());
            }
            2 => {
                // raw_size: int32
                #[allow(clippy::cast_possible_truncation)]
                {
                    raw_size = Some(cursor.read_varint()? as i32);
                }
            }
            3 => {
                // zlib_data: bytes
                let slice = cursor.read_len_delimited()?;
                if let Some(rs) = raw_size
                    && rs > 0
                {
                    #[allow(clippy::cast_sign_loss)]
                    buf.reserve((rs as usize).saturating_sub(buf.capacity()));
                }
                zlib_decompress_into(slice, buf)?;
                found = true;
            }
            7 => {
                // zstd_data: bytes
                let slice = cursor.read_len_delimited()?;
                if let Some(rs) = raw_size
                    && rs > 0
                {
                    #[allow(clippy::cast_sign_loss)]
                    buf.reserve((rs as usize).saturating_sub(buf.capacity()));
                }
                zstd::stream::copy_decode(std::io::Cursor::new(slice), &mut *buf)?;
                let size = buf.len() as u64;
                if size > MAX_BLOB_MESSAGE_SIZE {
                    return Err(new_blob_error(BlobError::MessageTooBig { size }));
                }
                found = true;
            }
            _ => cursor.skip_field(wire_type)?,
        }
    }

    if found {
        Ok(())
    } else {
        Err(new_blob_error(BlobError::Empty))
    }
}

/// Decompress a parsed WireBlob into an owned `Bytes`.
pub(crate) fn decompress_blob(
    blob: &WireBlob,
    pool: Option<&Arc<DecompressPool>>,
) -> Result<Bytes> {
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            } else {
                Ok(bytes.clone())
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            let est = blob.estimated_capacity();
            let capacity = if est > 0 { est } else { bytes.len() * 4 };
            let mut decoded_bytes = pool_get(pool, capacity);
            zlib_decompress_into(bytes, &mut decoded_bytes)?;
            Ok(pool_wrap(decoded_bytes, pool))
        }
        Some(BlobData::Zstd(bytes)) => {
            let est = blob.estimated_capacity();
            let capacity = if est > 0 { est } else { bytes.len() * 4 };
            let mut decoded_bytes = pool_get(pool, capacity);
            zstd::stream::copy_decode(Cursor::new(&**bytes), &mut decoded_bytes)?;
            let size = decoded_bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
            Ok(pool_wrap(decoded_bytes, pool))
        }
        None => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Decompress a WireBlob into a caller-owned buffer.
///
/// Avoids the Bytes->Vec round-trip of `decompress_blob()` + `.to_vec()`.
/// The buffer is cleared and refilled; its backing allocation is retained
/// across calls for sequential decode loops.
pub(crate) fn decompress_wire_blob_into(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
    buf.clear();
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
            buf.extend_from_slice(bytes);
        }
        Some(BlobData::Zlib(bytes)) => {
            let est = blob.estimated_capacity();
            if est > 0 {
                buf.reserve(est);
            }
            zlib_decompress_into(bytes, buf)?;
        }
        Some(BlobData::Zstd(bytes)) => {
            let est = blob.estimated_capacity();
            if est > 0 {
                buf.reserve(est);
            }
            zstd::stream::copy_decode(Cursor::new(&**bytes), &mut *buf)?;
            let size = buf.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
        }
        None => return Err(new_blob_error(BlobError::Empty)),
    }
    Ok(())
}
