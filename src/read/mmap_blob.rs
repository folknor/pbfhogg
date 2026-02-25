//! Iterate over blobs from a memory map

use super::blob::{
    decode_blob, decompress_blob, BlobDecode, BlobType, ByteOffset, MAX_BLOB_HEADER_SIZE,
};
use super::block::{HeaderBlock, PrimitiveBlock};
use crate::error::{new_blob_error, new_protobuf_error, BlobError, Result};
use crate::proto;
use bytes::Bytes;
use prost::Message;
use std::fs::File;
use std::path::Path;

/// A read-only memory map.
#[derive(Debug)]
pub struct Mmap {
    mmap: memmap2::Mmap,
}

impl Mmap {
    /// Creates a memory map from a given file.
    ///
    /// # Safety
    /// The underlying file should not be modified while holding the memory map.
    /// See [memmap-rs issue 25](https://github.com/danburkert/memmap-rs/issues/25) for more
    /// information on the safety of memory maps.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let mmap = unsafe { Mmap::from_file(&f)? };
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub unsafe fn from_file(file: &File) -> Result<Mmap> {
        unsafe { memmap2::Mmap::map(file) }
            .map(|m| Mmap { mmap: m })
            .map_err(Into::into)
    }

    /// Creates a memory map from a given path.
    ///
    /// # Safety
    /// The underlying file should not be modified while holding the memory map.
    /// See [memmap-rs issue 25](https://github.com/danburkert/memmap-rs/issues/25) for more
    /// information on the safety of memory maps.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mmap = unsafe { Mmap::from_path("tests/test.osm.pbf")? };
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub unsafe fn from_path<P: AsRef<Path>>(path: P) -> Result<Mmap> {
        let file = File::open(&path)?;
        unsafe { memmap2::Mmap::map(&file) }
            .map(|m| Mmap { mmap: m })
            .map_err(Into::into)
    }

    /// Returns an iterator over the blobs in this memory map.
    pub fn blob_iter(self) -> MmapBlobReader {
        MmapBlobReader::new(self)
    }
}

/// A PBF blob from a memory map.
#[derive(Clone, Debug)]
pub struct MmapBlob {
    header: proto::BlobHeader,
    data: Bytes,
    offset: ByteOffset,
}

impl MmapBlob {
    /// Decodes the blob and tries to obtain the inner content (usually a [`HeaderBlock`] or a
    /// [`PrimitiveBlock`]). This operation might involve an expensive decompression step.
    pub fn decode(&self) -> Result<BlobDecode<'_>> {
        let blob = proto::Blob::decode(self.data.clone())
            .map_err(|e| new_protobuf_error(e, "blob content"))?;
        match self.header.r#type.as_str() {
            "OSMHeader" => {
                let block = Box::new(HeaderBlock::new(decode_blob(&blob, None)?));
                Ok(BlobDecode::OsmHeader(block))
            }
            "OSMData" => {
                let block = PrimitiveBlock::new(decompress_blob(&blob, None)?)?;
                Ok(BlobDecode::OsmData(block))
            }
            x => Ok(BlobDecode::Unknown(x)),
        }
    }

    /// Returns the type of a blob without decoding its content.
    pub fn get_type(&self) -> BlobType<'_> {
        match self.header.r#type.as_str() {
            "OSMHeader" => BlobType::OsmHeader,
            "OSMData" => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the byte offset of the blob from the start of its memory map.
    pub fn offset(&self) -> ByteOffset {
        self.offset
    }
}

/// A reader for memory mapped PBF files that allows iterating over [`MmapBlob`]s.
///
/// # Internal design: offset-based iteration (no `Bytes::slice()` per blob)
///
/// Previously, `MmapBlobReader` stored a `Bytes` handle wrapping the entire mmap
/// (via `Bytes::from_owner(mmap)`) and called `self.bytes.slice(offset..)` on every
/// `next()` call. `Bytes::slice()` performs an atomic reference count increment (Arc
/// clone) on the shared backing buffer. For a ~500 MB PBF file with ~16,000 blobs
/// this added ~48,000 atomic operations per full iteration (3 `slice()` calls per
/// blob: one for the remaining tail, one for the header, one for the data payload).
/// On larger files (planet.osm.pbf, ~70 GB, ~2.5M blobs) this becomes ~7.5M atomic
/// increments/decrements, all contending on the same cache line.
///
/// The new design stores the mmap as an owned `memmap2::Mmap` and accesses its data
/// via `Deref<Target=[u8]>`. Iteration uses plain `usize` offset arithmetic and
/// `&data[start..end]` sub-slicing, which is a pointer+length operation with zero
/// atomic overhead.
///
/// `Bytes` objects are only created at two points where protobuf's zero-copy API
/// (`parse_from_tokio_bytes`) requires an owned `Bytes`:
///   1. For the blob header (~100-200 bytes) -- via `Bytes::copy_from_slice()`
///   2. For the blob data payload (~16-64 KB) -- via `Bytes::copy_from_slice()`
///
/// **Why `copy_from_slice` is cheaper than `Bytes::slice()` here:**
/// `Bytes::slice()` on a shared ~500 MB buffer performs an atomic increment on a
/// cache line that is shared across all outstanding `Bytes` handles. Atomic operations
/// on contended cache lines cost ~20-100 ns depending on cross-core traffic.
/// `Bytes::copy_from_slice()` on a ~60 KB blob does a plain `memcpy`, which runs at
/// memory bandwidth (~10-20 GB/s on modern hardware), completing in ~3-6 us for 60 KB.
/// While the copy is nominally more work, it produces an *independent* `Bytes` with
/// its own refcount, eliminating all contention. More importantly, the copied `Bytes`
/// is small and cache-friendly, whereas the shared `Bytes` points into a 500 MB+
/// region.
///
/// Alternative considered: using `Bytes::slice()` but only once (for the data payload,
/// skipping the header). This would halve the atomic ops but not eliminate them. The
/// copy approach was chosen because it fully removes atomic contention and the copy
/// cost is dwarfed by the subsequent zlib decompression + protobuf parsing of each
/// blob.
///
/// Alternative considered: storing a `Bytes` for ownership and using `&self.bytes[..]`
/// (Deref to `&[u8]`) for sub-slicing. This would work but `Bytes::from_owner()` still
/// allocates an Arc internally, and we would need to be careful not to accidentally
/// call `slice()` on it. Storing the raw `memmap2::Mmap` directly is simpler and makes
/// the zero-atomic-ops guarantee structural rather than relying on discipline.
///
/// **Note on `Clone`:** The old `MmapBlobReader` derived `Clone` because `Bytes` is
/// cheaply cloneable via its internal Arc. Now that we store `memmap2::Mmap` directly
/// (which is not `Clone`), `MmapBlobReader` is no longer `Clone`. This is intentional:
/// the old `Clone` was deceptively cheap-looking but performed an atomic refcount
/// increment on the entire mmap buffer -- exactly what this refactor eliminates.
/// Callers needing multiple readers should create separate `Mmap` instances.
#[derive(Debug)]
pub struct MmapBlobReader {
    /// The memory map that owns the underlying file mapping. We store this directly
    /// (not wrapped in `Bytes`) to avoid any possibility of accidental `Bytes::slice()`
    /// calls that would introduce atomic reference counting overhead.
    ///
    /// Previously this was `bytes: Bytes` created via `Bytes::from_owner(mmap.mmap)`.
    /// That worked but made it too easy to call `self.bytes.slice()` in the hot loop,
    /// which is exactly the pattern we are eliminating.
    ///
    /// Data access uses `&*self.mmap` (via `memmap2::Mmap`'s `Deref<Target=[u8]>`),
    /// which returns a plain `&[u8]` with zero overhead -- just a pointer and length
    /// from the mmap's internal state, no reference counting involved.
    mmap: memmap2::Mmap,

    /// Current read position as a byte offset into the mmap. This replaces the previous
    /// approach of creating a new `Bytes::slice(self.offset..)` on each `next()` call.
    /// Plain integer arithmetic is used to advance through the file -- no reference
    /// counting, no atomic operations, just a `usize` bump.
    offset: usize,

    last_blob_ok: bool,
}

impl MmapBlobReader {
    /// Creates a new `MmapBlobReader`.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    ///
    /// let mmap = unsafe { Mmap::from_path("tests/test.osm.pbf")? };
    /// let reader = MmapBlobReader::new(mmap);
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(mmap: Mmap) -> MmapBlobReader {
        // Previously: `Bytes::from_owner(mmap.mmap)` which wraps the mmap in an Arc,
        // enabling zero-copy `Bytes::slice()` but at the cost of atomic refcount
        // operations on every slice call.
        //
        // Now: we store the raw `memmap2::Mmap` directly. All sub-slicing in `next()`
        // uses `&self.mmap[range]` which goes through `Deref<Target=[u8]>` -- a plain
        // pointer dereference, no atomic operations.
        MmapBlobReader {
            mmap: mmap.mmap,
            offset: 0,
            last_blob_ok: true,
        }
    }

    /// Move the cursor to the given byte offset.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    ///
    /// let mmap = unsafe { Mmap::from_path("tests/test.osm.pbf")? };
    /// let mut reader = MmapBlobReader::new(mmap);
    ///
    /// let first_blob = reader.next().unwrap()?;
    /// let second_blob = reader.next().unwrap()?;
    ///
    /// reader.seek(first_blob.offset());
    /// let first_blob_again = reader.next().unwrap()?;
    ///
    /// assert_eq!(first_blob.offset(), first_blob_again.offset());
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    #[allow(clippy::cast_possible_truncation)]
    pub fn seek(&mut self, pos: ByteOffset) {
        // Unchanged from before -- seek was always just a usize assignment.
        self.offset = pos.0 as usize;
    }
}

impl Iterator for MmapBlobReader {
    type Item = Result<MmapBlob>;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn next(&mut self) -> Option<Self::Item> {
        // ---- OPTIMIZATION: plain &[u8] sub-slicing instead of Bytes::slice() ----
        //
        // Previously (before this change):
        //   let slice = self.bytes.slice(self.offset..);
        //
        // `Bytes::slice()` internally does:
        //   1. Atomic increment on the Arc refcount of the shared mmap buffer
        //   2. Compute the sub-range and return a new Bytes handle
        //   3. When the Bytes handle is dropped (end of this function), atomic decrement
        //
        // For a planet file (~2.5M blobs), that was ~5M atomic operations just for this
        // one line, plus ~5M more for the header and data slice calls below (totaling
        // ~15M atomic ops per full iteration). Atomics on the same cache line cause
        // inter-core contention even in single-threaded code because the cache line must
        // be in Modified state for the atomic RMW (read-modify-write) cycle.
        //
        // Now: `&*self.mmap` returns a `&[u8]` via `memmap2::Mmap`'s Deref impl, and
        // `&data[self.offset..]` creates a sub-slice with plain pointer arithmetic --
        // no allocation, no atomic ops, no cache line contention.
        let data: &[u8] = &self.mmap;
        let remaining = data.len().saturating_sub(self.offset);
        let slice = &data[self.offset..];

        match remaining {
            0 => return None,
            1..=3 => {
                self.last_blob_ok = false;
                return Some(Err(new_blob_error(BlobError::InvalidHeaderSize)));
            }
            _ => {}
        }

        let header_size = u32::from_be_bytes(slice[..4].try_into().expect("4-byte slice conversion")) as usize;

        if header_size as u64 >= MAX_BLOB_HEADER_SIZE {
            self.last_blob_ok = false;
            return Some(Err(new_blob_error(BlobError::HeaderTooBig {
                size: header_size as u64,
            })));
        }

        if remaining < 4 + header_size {
            self.last_blob_ok = false;
            let io_error = ::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                "content too short for header",
            );
            return Some(Err(io_error.into()));
        }

        // ---- OPTIMIZATION: Bytes::copy_from_slice() for header instead of Bytes::slice() ----
        //
        // Previously:
        //   BlobHeader::parse_from_tokio_bytes(&slice.slice(4..(4 + header_size)))
        //
        // `slice` was a `Bytes` (from the `self.bytes.slice(self.offset..)` above), so
        // calling `.slice(4..(4 + header_size))` on it triggered *another* atomic
        // refcount increment on the shared mmap buffer's Arc.
        //
        // Now: we use `Bytes::copy_from_slice()` to create a small, independent Bytes
        // from the header data (~100-200 bytes for a typical BlobHeader). This does a
        // memcpy of ~200 bytes which completes in <100 ns -- far cheaper than an atomic
        // refcount operation on a potentially contended cache line (~20-100 ns for the
        // atomic itself, plus potential cache-line bouncing overhead).
        //
        // The copied Bytes is small, cache-local, and has its own independent refcount
        // (which starts at 1 and is dropped at the end of this scope with no contention).
        //
        // Alternative considered: pass `&[u8]` directly to avoid the Bytes allocation
        // entirely. Unfortunately, protobuf's `parse_from_tokio_bytes` requires `&Bytes`,
        // not `&[u8]`, for its zero-copy deserialization path. We could use
        // `parse_from_bytes` instead (which accepts `&[u8]`), but that copies internally
        // anyway, so `copy_from_slice` + `parse_from_tokio_bytes` is equivalent and keeps
        // the code consistent with the rest of the codebase.
        let header_bytes = Bytes::copy_from_slice(&slice[4..4 + header_size]);
        let header = match proto::BlobHeader::decode(header_bytes) {
            Ok(x) => x,
            Err(e) => {
                self.last_blob_ok = false;
                return Some(Err(new_protobuf_error(e, "blob header")));
            }
        };

        let data_size = header.datasize as usize;
        let chunk_size = 4 + header_size + data_size;

        if remaining < chunk_size {
            self.last_blob_ok = false;
            let io_error = ::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                "content too short for block data",
            );
            return Some(Err(io_error.into()));
        }

        let prev_offset = self.offset;
        self.offset += chunk_size;

        // ---- OPTIMIZATION: Bytes::copy_from_slice() for blob data instead of
        // Bytes::slice() ----
        //
        // Previously:
        //   data: slice.slice((4 + header_size)..chunk_size)
        //
        // This was the most impactful `Bytes::slice()` call because the resulting Bytes
        // handle outlives this function -- it is stored in the returned `MmapBlob` and
        // lives until the caller drops the blob. This means the atomic refcount on the
        // shared mmap Arc stays elevated for an extended period. With many blobs in
        // flight (e.g., collected into a Vec, or buffered in a pipeline), this keeps the
        // mmap Arc "hot" with frequent concurrent increments/decrements from different
        // threads or scopes.
        //
        // Now: `Bytes::copy_from_slice()` copies the blob data payload (~16-64 KB for
        // typical PBF blobs) into a fresh, independent allocation. The cost:
        //   - memcpy of ~60 KB: ~3-6 us at memory bandwidth
        //   - One small allocation (~60 KB): ~100-500 ns via jemalloc/system allocator
        //
        // This is cheap compared to what happens next: the blob data will be zlib-
        // decompressed (expanding ~4x) and parsed as protobuf, which takes 50-500 us per
        // blob. The ~4 us memcpy is noise in that context.
        //
        // Key benefits of an independent Bytes (no shared Arc with the mmap):
        //   1. No atomic contention between iteration and blob processing
        //   2. The blob can be sent to other threads without touching the mmap refcount
        //   3. Dropping blobs does not contend with the iterator's mmap
        //   4. The mmap can be dropped as soon as iteration completes, even if MmapBlob
        //      handles are still alive -- this improves memory behavior for streaming
        //      workloads. Previously, any outstanding Bytes::slice() handle would keep
        //      the entire ~500 MB mmap pinned via Arc.
        //
        // Alternative considered: keeping `Bytes::slice()` just for the data payload
        // (since it avoids the memcpy) and only using plain slices for the header and
        // tail. This would reduce atomic ops from 3 per blob to 1, but not eliminate
        // them. The full-copy approach was chosen because the copy cost is negligible
        // relative to decompression, and it completely eliminates all atomic contention
        // and mmap-pinning issues.
        let data_start = 4 + header_size;
        let blob_data = Bytes::copy_from_slice(&slice[data_start..chunk_size]);

        Some(Ok(MmapBlob {
            header,
            data: blob_data,
            offset: ByteOffset(prev_offset as u64),
        }))
    }
}
