//! Read and decode blobs

use super::block::{HeaderBlock, PrimitiveBlock};
use super::file_reader::FileReader;
use crate::error::{BlobError, ErrorKind, Result, new_blob_error, new_error};
use bytes::Bytes;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

// Decompression infrastructure (pool, zlib helper, decompress_blob_*) lives
// in the sibling `decompress` module. Re-exported here so existing paths like
// `crate::blob::DecompressPool` keep resolving.
pub(crate) use super::decompress::{
    DecompressPool, decompress_blob, decompress_blob_data_into, decompress_blob_raw,
    decompress_wire_blob_into, pool_get_pub, pool_wrap,
};
use super::decompress::{decompress_parsed_blob_into, pool_get};

// Blob-level wire-format parsers live in the sibling `blob_wire` module.
// Re-exported at pub(crate) so existing paths like `crate::blob::WireBlob`
// and `crate::blob::parse_blob_header_with_index` keep resolving.
pub(crate) use super::blob_wire::{
    BlobData, BlobKind, WireBlob, WireBlobHeader, parse_blob_header_with_index,
};
pub use super::blob_wire::{MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE};

/// The content type of a blob.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlobType<'a> {
    /// Blob contains a [`HeaderBlock`].
    OsmHeader,
    /// Blob contains a [`PrimitiveBlock`].
    OsmData,
    /// An unknown blob type with the given string identifier.
    /// Parsers should ignore unknown blobs they do not expect.
    Unknown(&'a str),
}

impl<'a> BlobType<'a> {
    #[inline]
    pub const fn as_str(&self) -> &'a str {
        match self {
            Self::OsmHeader => "OSMHeader",
            Self::OsmData => "OSMData",
            Self::Unknown(x) => x,
        }
    }
}

/// The decoded content of a blob (analogous to [`BlobType`]).
///
/// Does not implement `Clone` because `OsmData` contains a `PrimitiveBlock`, which is
/// intentionally not `Clone` (see `PrimitiveBlock` docs for rationale).
#[derive(Debug)]
#[non_exhaustive]
pub enum BlobDecode<'a> {
    /// Blob contains a [`HeaderBlock`].
    OsmHeader(Box<HeaderBlock>),
    /// Blob contains a [`PrimitiveBlock`].
    OsmData(PrimitiveBlock),
    /// An unknown blob type with the given string identifier.
    /// Parsers should ignore unknown blobs they do not expect.
    Unknown(&'a str),
}

/// The offset of a blob in bytes from stream start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteOffset(pub u64);

/// A blob.
///
/// A PBF file consists of a sequence of blobs. This type supports decoding the content of a blob
/// to different types of blocks that are usually more interesting to the user.
#[derive(Clone, Debug)]
pub struct Blob {
    header: WireBlobHeader,
    blob: WireBlob,
    offset: Option<ByteOffset>,
}

impl Blob {
    fn new(header: WireBlobHeader, blob: WireBlob, offset: Option<ByteOffset>) -> Blob {
        Blob {
            header,
            blob,
            offset,
        }
    }

    /// Decodes the Blob and tries to obtain the inner content (usually a [`HeaderBlock`] or a
    /// [`PrimitiveBlock`]). This operation might involve an expensive decompression step.
    pub fn decode(&self) -> Result<BlobDecode<'_>> {
        match self.get_type() {
            BlobType::OsmHeader => {
                let block = Box::new(self.to_headerblock()?);
                Ok(BlobDecode::OsmHeader(block))
            }
            BlobType::OsmData => {
                let block = self.to_primitiveblock()?;
                Ok(BlobDecode::OsmData(block))
            }
            BlobType::Unknown(x) => Ok(BlobDecode::Unknown(x)),
        }
    }

    /// Returns the type of a blob without decoding its content.
    // wontfix(name-no-get-prefix): inherited from osmpbf public API
    #[inline]
    pub fn get_type(&self) -> BlobType<'_> {
        match &self.header.blob_type {
            BlobKind::OsmHeader => BlobType::OsmHeader,
            BlobKind::OsmData => BlobType::OsmData,
            BlobKind::Unknown(s) => BlobType::Unknown(s),
        }
    }

    /// Returns the byte offset of the blob from the start of its source stream.
    /// This might be [`None`] if the source stream does not implement [`Seek`].
    #[inline]
    pub fn offset(&self) -> Option<ByteOffset> {
        self.offset
    }

    /// Raw way-member bitmap from BlobHeader field 5 (`pbfhogg.WayMembers-v1`).
    /// The version byte and encoded way count are validated and stripped.
    /// Returns `None` when absent, parsing was disabled, or the payload is malformed.
    pub fn way_members(&self) -> Option<&[u8]> {
        let (_, bitmap) = self.way_members_parts()?;
        Some(bitmap)
    }

    /// Encoded field-5 way count for cross-checking against decoded ways.
    /// Returns `None` under the same conditions as [`Self::way_members`].
    pub fn way_member_count(&self) -> Option<u32> {
        self.way_members_parts().map(|(count, _)| count)
    }

    fn way_members_parts(&self) -> Option<(u32, &[u8])> {
        let data = self.header.waymembers.as_deref()?;
        if data.first().copied()? != 1 {
            return None;
        }
        let mut value = 0u32;
        let mut shift = 0u32;
        let mut end = None;
        for (i, byte) in data[1..].iter().copied().enumerate() {
            if shift >= 32 {
                return None;
            }
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                end = Some(i + 2);
                break;
            }
            shift += 7;
        }
        let end = end?;
        let bitmap = data.get(end..)?;
        let expected = usize::try_from(u64::from(value).div_ceil(8)).ok()?;
        (bitmap.len() == expected).then_some((value, bitmap))
    }

    /// Tries to decode the blob to a [`HeaderBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_headerblock(&self) -> Result<HeaderBlock> {
        decode_headerblock(&self.blob, None).map(HeaderBlock::new)
    }

    /// Tries to decode the blob to a [`PrimitiveBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_primitiveblock(&self) -> Result<PrimitiveBlock> {
        decompress_blob(&self.blob, None).and_then(PrimitiveBlock::new)
    }

    /// Decompress into a caller-owned buffer, avoiding the Bytes→Vec copy.
    ///
    /// The buffer is cleared and refilled. Callers typically pass ownership
    /// to `PrimitiveBlock::from_vec_with_scratch(std::mem::take(&mut buf))`
    /// which leaves `buf` empty - the next call will re-allocate. This trades
    /// per-blob allocation (~220 KB) for eliminating the 1.5 MB Bytes→Vec copy
    /// that the old `decompress_pooled()` + `new_with_scratch()` path incurred.
    #[hotpath::measure]
    pub(crate) fn decompress_into(&self, buf: &mut Vec<u8>) -> Result<()> {
        decompress_wire_blob_into(&self.blob, buf)
    }

    /// Returns the blob-level index from the header's `indexdata` field, if present.
    ///
    /// PBFs written by pbfhogg embed indexdata automatically. Third-party PBFs
    /// (Geofabrik, osmium) typically do not - this returns `None` for those.
    pub(crate) fn index(&self) -> Option<crate::blob_meta::BlobIndex> {
        self.header
            .indexdata
            .as_ref()
            .and_then(|d| crate::blob_meta::BlobIndex::deserialize(d))
    }

    /// Returns the compression kind and payload bytes for blob equality comparison.
    ///
    /// Two blobs with the same compression kind and identical payload bytes are
    /// guaranteed to contain identical elements. Returns `None` if the blob has
    /// no data payload.
    pub(crate) fn compressed_data(&self) -> Option<(u8, &[u8])> {
        match &self.blob.data {
            Some(BlobData::Raw(b)) => Some((0, b)),
            Some(BlobData::Zlib(b)) => Some((1, b)),
            Some(BlobData::Zstd(b)) => Some((2, b)),
            None => None,
        }
    }

    /// Total bytes retained by this blob's payload allocation.
    ///
    /// [`compressed_data`](Self::compressed_data) returns only the selected
    /// compression field, but that `Bytes` slice shares the single parent
    /// buffer holding the entire Blob message body (`BlobReader::next` fills a
    /// `datasize`-byte `Vec` and every `BlobData` variant is a zero-copy slice
    /// into it). Slicing never shrinks that buffer, so the whole
    /// `datasize`-byte allocation stays resident as long as any slice lives -
    /// even a one-byte data field beside a large unknown field pins the full
    /// body. Byte-budget accounting must charge this, not the field length,
    /// or pathological blobs accumulate file-sized memory under the cap.
    pub(crate) fn retained_len(&self) -> u64 {
        // datasize is the Blob message size = the parent buffer length that the
        // payload slices keep alive. Negative sizes are rejected upstream in
        // `BlobReader`; treat any stray negative as zero rather than wrapping.
        u64::try_from(self.header.datasize).unwrap_or(0)
    }

    /// Returns the per-blob tag key index from the header's `tagdata` field, if present.
    ///
    /// PBFs written by pbfhogg embed tag key data automatically. Third-party PBFs
    /// do not - this returns `None` for those.
    pub(crate) fn tag_index(&self) -> Option<crate::blob_meta::TagIndex> {
        self.header
            .tagdata
            .as_ref()
            .and_then(|d| crate::blob_meta::TagIndex::deserialize(d))
    }

    /// Decompress and construct PrimitiveBlock with inline string table entries,
    /// reusing caller-provided scratch buffers
    /// for `parse_and_inline`. Used by the pipelined reader with thread-local scratch
    /// to avoid per-blob `Vec<(u32, u32)>` allocations in rayon decode tasks.
    pub(crate) fn to_primitiveblock_inline_with_scratch(
        &self,
        pool: &Arc<DecompressPool>,
        st_scratch: &mut Vec<(u32, u32)>,
        gr_scratch: &mut Vec<(u32, u32)>,
    ) -> Result<PrimitiveBlock> {
        let mut buf = pool_get(Some(pool), self.blob.estimated_capacity());
        decompress_parsed_blob_into(&self.blob, &mut buf)?;
        PrimitiveBlock::from_vec_pooled_with_scratch(buf, pool, st_scratch, gr_scratch)
    }
}

/// A blob header.
///
/// Just contains information about the size and type of the following [`Blob`].
#[derive(Clone, Debug)]
pub struct BlobHeader {
    header: WireBlobHeader,
}

impl BlobHeader {
    fn new(header: WireBlobHeader) -> Self {
        BlobHeader { header }
    }

    /// Returns the type of the following blob.
    #[inline]
    pub fn blob_type(&self) -> BlobType<'_> {
        match &self.header.blob_type {
            BlobKind::OsmHeader => BlobType::OsmHeader,
            BlobKind::OsmData => BlobType::OsmData,
            BlobKind::Unknown(s) => BlobType::Unknown(s),
        }
    }

    /// Returns the size of the following blob in bytes.
    // wontfix(name-no-get-prefix): inherited from osmpbf public API
    #[inline]
    pub fn get_blob_size(&self) -> i32 {
        self.header.datasize
    }
}

/// Underlying source for [`BlobReader`] that supports seeking.
///
/// Provides a fast path for relative skips that preserves any internal buffer
/// (e.g. `BufReader`'s read-ahead). The default `skip_relative` falls through to
/// `Seek::seek(SeekFrom::Current(_))`, which is correct but discards any buffer
/// on `BufReader` - the cause of the ~10× header-walk read amplification this
/// trait exists to eliminate. Override for buffered readers to keep the buffer
/// when the target lies within the buffered window.
///
/// Implemented for `BufReader<R: Read + Seek>` (uses `BufReader::seek_relative`),
/// `File` (default), and `Cursor<T: AsRef<[u8]>>` (default - seeks on `Cursor`
/// are pure cursor-position bumps, no fd cost). Library users who pass a
/// different reader type to [`BlobReader::new_seekable`] can opt in by writing
/// `impl BlobReaderSource for MyReader {}` - the default impl is correct but
/// pays the `Seek::seek` discard cost on every header walk.
pub trait BlobReaderSource: Read + Seek {
    /// Skip relative to the current position. Default impl falls through to
    /// `Seek::seek(SeekFrom::Current(offset))`. Override for buffered sources
    /// to avoid discarding the buffer when the target is in-range.
    fn skip_relative(&mut self, offset: i64) -> std::io::Result<()> {
        self.seek(SeekFrom::Current(offset)).map(|_| ())
    }
}

impl<R: Read + Seek> BlobReaderSource for BufReader<R> {
    fn skip_relative(&mut self, offset: i64) -> std::io::Result<()> {
        // Preserves the BufReader's internal buffer when the target lies inside
        // the buffered window; falls back to discard+lseek otherwise. At the
        // 256 KB buffer used by `seekable_from_path`, this collapses ~10× file-
        // size amplification on header-walk paths to roughly the file size.
        BufReader::seek_relative(self, offset)
    }
}

impl BlobReaderSource for File {}
impl<T: AsRef<[u8]>> BlobReaderSource for Cursor<T> {}

/// A reader for PBF files that allows iterating over [`Blob`]s.
// wontfix(type-generic-bounds): bounds on struct match osmpbf API and document intent
#[derive(Clone, Debug)]
pub struct BlobReader<R: Read + Send> {
    reader: R,
    /// Current reader offset in bytes from the start of the stream.
    offset: Option<ByteOffset>,
    last_blob_ok: bool,
    /// Reusable buffer for reading blob header bytes. Cleared and refilled each
    /// iteration to avoid allocating a new Vec per blob (~16K allocs per Denmark,
    /// ~2.5M per planet).
    header_buf: Vec<u8>,
    /// When `true`, `WireBlobHeader::parse` allocates tagdata (field 4).
    /// Only needed for tag-filtered reads and merge/sort passthrough.
    parse_tagdata: bool,
    /// When `true`, `WireBlobHeader::parse` copies indexdata (field 2).
    /// Default `true` for compatibility. Disabled in hot paths that never
    /// call `Blob::index()` (par_map_reduce, unfiltered pipeline).
    parse_indexdata: bool,
    /// When `true`, `WireBlobHeader::parse` allocates field-5 waymembers.
    parse_waymembers: bool,
    /// File descriptor for fadvise(DONTNEED) after each blob read. When set,
    /// the reader evicts page cache pages behind the read head, preventing
    /// sequential reads from accumulating the entire file in RSS.
    /// Only set for buffered FileReader - O_DIRECT has no pages to evict.
    #[cfg(target_os = "linux")]
    evict_fd: Option<std::os::unix::io::RawFd>,
}

impl<R: Read + Send> BlobReader<R> {
    /// Creates a new `BlobReader`.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let reader = BlobReader::new(buf_reader);
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(reader: R) -> BlobReader<R> {
        BlobReader {
            reader,
            offset: None,
            last_blob_ok: true,
            header_buf: Vec::new(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_fd: None,
        }
    }

    fn handle_error<T>(&mut self, error: crate::error::Error) -> Option<Result<T>> {
        self.offset = None;
        self.last_blob_ok = false;
        Some(Err(error))
    }

    /// Enable or disable tagdata parsing (BlobHeader field 4).
    ///
    /// When enabled, `WireBlobHeader::parse` allocates tagdata per blob.
    /// Only needed for tag-filtered reads and merge/sort passthrough.
    pub(crate) fn set_parse_tagdata(&mut self, enable: bool) {
        self.parse_tagdata = enable;
    }

    /// Enable or disable indexdata parsing (BlobHeader field 2).
    ///
    /// When enabled (default), `WireBlobHeader::parse` copies the 42-byte
    /// indexdata per blob. Disable on hot paths that never call `Blob::index()`
    /// to skip the per-blob copy.
    pub(crate) fn set_parse_indexdata(&mut self, enable: bool) {
        self.parse_indexdata = enable;
    }

    /// Enable or disable way-member bitmap parsing (BlobHeader field 5).
    /// Disabled by default to avoid allocating metadata unused by normal reads.
    pub fn set_parse_waymembers(&mut self, enable: bool) {
        self.parse_waymembers = enable;
    }

    #[allow(clippy::cast_possible_truncation)]
    fn read_blob_header(&mut self) -> Option<Result<WireBlobHeader>> {
        let header_size: u64 = {
            let mut buf = [0u8; 4];
            // Read the first byte separately to distinguish clean EOF (0 bytes
            // available) from corruption (1-3 trailing bytes).
            match self.reader.read_exact(&mut buf[..1]) {
                Ok(()) => {}
                Err(e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof => {
                    // Clean EOF: no bytes remaining.
                    return None;
                }
                Err(e) => {
                    // Propagate the original I/O error (broken pipe, permission
                    // denied, etc.) instead of masking it as InvalidHeaderSize.
                    self.offset = None;
                    self.last_blob_ok = false;
                    return Some(Err(e.into()));
                }
            }
            match self.reader.read_exact(&mut buf[1..]) {
                Ok(()) => {
                    self.offset = self.offset.map(|x| ByteOffset(x.0 + 4));
                    u64::from(u32::from_be_bytes(buf))
                }
                Err(e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof => {
                    // 1-3 trailing bytes after a complete previous frame
                    // are tolerated per `reference/truncation-handling.md`
                    // ("clean cut at frame boundary"). The partial length
                    // prefix can't start a new frame; treat as EOF.
                    return None;
                }
                Err(e) => {
                    // Genuine I/O error (broken pipe, permission denied,
                    // etc.) - propagate the real cause.
                    self.offset = None;
                    self.last_blob_ok = false;
                    return Some(Err(e.into()));
                }
            }
        };

        if header_size >= MAX_BLOB_HEADER_SIZE {
            self.last_blob_ok = false;
            return Some(Err(new_blob_error(BlobError::HeaderTooBig {
                size: header_size,
            })));
        }

        let mut reader = self.reader.by_ref().take(header_size);
        self.header_buf.clear();
        self.header_buf.reserve(header_size as usize);
        if let Err(e) = reader.read_to_end(&mut self.header_buf) {
            return self.handle_error(e.into());
        }
        // `Take::read_to_end` returns Ok(short_count) on truncation; a
        // committed length prefix that promises N bytes of header but
        // delivers fewer is shape 3 ("EOF inside BlobHeader bytes") per
        // `reference/truncation-handling.md` and must hard-error.
        if self.header_buf.len() as u64 != header_size {
            // self.offset points at the start of the BlobHeader (after
            // the 4-byte length prefix). The truncation byte is at
            // `header_start + got` (the byte we couldn't read).
            let header_start = self.offset.map_or(0, |x| x.0);
            let got = self.header_buf.len() as u64;
            let trunc_at = header_start + got;
            return self.handle_error(new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                format!(
                    "BlobHeader truncated at byte {trunc_at} (shape 3): \
                         declared {header_size} bytes from offset \
                         {header_start}, got {got}"
                ),
            ))));
        }

        let header = match WireBlobHeader::parse(
            &self.header_buf,
            self.parse_tagdata,
            self.parse_indexdata,
            self.parse_waymembers,
        ) {
            Ok(header) => header,
            Err(e) => return self.handle_error(e),
        };

        if header.datasize < 0 {
            return self.handle_error(new_blob_error(BlobError::InvalidDataSize {
                size: header.datasize,
            }));
        }

        self.offset = self.offset.map(|x| ByteOffset(x.0 + header_size));

        Some(Ok(header))
    }
}

impl BlobReader<FileReader> {
    /// Tries to open the file at the given path and constructs a `BlobReader` from this.
    /// If there are no errors, each blob will have a valid ([`Some`]) offset.
    ///
    /// # Errors
    /// Returns the same errors that `std::fs::File::open` returns.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = BlobReader::from_path("tests/test.osm.pbf")?;
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let reader = FileReader::buffered(path.as_ref())?;
        #[cfg(target_os = "linux")]
        let evict_fd = Some({
            use std::os::unix::io::AsRawFd;
            match &reader {
                FileReader::Buffered(r) => r.get_ref().as_raw_fd(),
                #[cfg(feature = "linux-direct-io")]
                FileReader::Direct(r) => r.raw_fd(),
            }
        });
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
            header_buf: Vec::new(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_fd,
        })
    }

    /// Open a file for reading with O_DIRECT (bypasses page cache).
    ///
    /// Requires the `linux-direct-io` feature. Returns an error if the
    /// filesystem does not support O_DIRECT (e.g. tmpfs).
    #[cfg(feature = "linux-direct-io")]
    pub fn from_path_direct<P: AsRef<Path>>(path: P) -> Result<Self> {
        let reader = FileReader::direct(path.as_ref())?;
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
            header_buf: Vec::new(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            evict_fd: None, // O_DIRECT: no pages to evict
        })
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    pub fn open<P: AsRef<Path>>(path: P, direct: bool) -> Result<Self> {
        let reader = FileReader::open(path.as_ref(), direct)?;
        #[cfg(target_os = "linux")]
        let evict_fd = if direct {
            None
        } else {
            Some({
                use std::os::unix::io::AsRawFd;
                match &reader {
                    FileReader::Buffered(r) => r.get_ref().as_raw_fd(),
                    #[cfg(feature = "linux-direct-io")]
                    FileReader::Direct(r) => r.raw_fd(),
                }
            })
        };
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
            header_buf: Vec::new(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_fd,
        })
    }
}

impl<R: Read + Send> Iterator for BlobReader<R> {
    type Item = Result<Blob>;

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn next(&mut self) -> Option<Self::Item> {
        // Stop iteration if there was an error.
        if !self.last_blob_ok {
            return None;
        }

        let prev_offset = self.offset;

        let header = match self.read_blob_header() {
            Some(Ok(header)) => header,
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        };

        let mut reader = self.reader.by_ref().take(header.datasize as u64);
        let mut blob_data = Vec::with_capacity(header.datasize as usize);
        if let Err(e) = reader.read_to_end(&mut blob_data) {
            return self.handle_error(e.into());
        }
        // `Take::read_to_end` returns Ok(short_count) on truncation; a
        // BlobHeader.datasize that promises N payload bytes but
        // delivers fewer is shape 4 ("EOF inside Blob payload") per
        // `reference/truncation-handling.md` and must hard-error.
        if blob_data.len() as u64 != header.datasize as u64 {
            // self.offset points at the start of the Blob payload (after
            // the BlobHeader). The truncation byte is at
            // `payload_start + got`.
            let payload_start = self.offset.map_or(0, |x| x.0);
            let got = blob_data.len() as u64;
            let trunc_at = payload_start + got;
            return self.handle_error(new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                format!(
                    "Blob payload truncated at byte {trunc_at} (shape 4): \
                         declared {} bytes from offset {payload_start}, got {got}",
                    header.datasize
                ),
            ))));
        }

        let blob_bytes = Bytes::from(blob_data);
        let blob = match WireBlob::parse(&blob_bytes) {
            Ok(blob) => blob,
            Err(e) => return self.handle_error(e),
        };

        self.offset = self
            .offset
            .map(|x| ByteOffset(x.0 + header.datasize as u64));

        // Evict page cache pages behind the read head. After the blob data is
        // copied into owned buffers (blob_data Vec above), the kernel's cached
        // pages for the consumed byte range are never accessed again. Advising
        // DONTNEED prevents sequential reads from accumulating the entire file
        // in RSS - critical for 30+ GB PBFs on memory-constrained hosts.
        #[cfg(target_os = "linux")]
        if let Some(fd) = self.evict_fd
            && let Some(offset) = self.offset
        {
            // posix_fadvise(fd, 0, offset, POSIX_FADV_DONTNEED)
            // SAFETY: fd is valid (owned by FileReader in same struct), offset is in range.
            unsafe {
                libc::posix_fadvise(
                    fd,
                    0,
                    offset.0.try_into().unwrap_or(i64::MAX),
                    libc::POSIX_FADV_DONTNEED,
                )
            };
        }

        Some(Ok(Blob::new(header, blob, prev_offset)))
    }
}

impl<R: BlobReaderSource + Send> BlobReader<R> {
    /// Creates a new `BlobReader` from the given reader that is seekable and will be initialized
    /// with a valid offset.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let mut reader = BlobReader::new_seekable(buf_reader)?;
    /// let first_blob = reader.next().unwrap()?;
    ///
    /// assert_eq!(first_blob.offset(), Some(ByteOffset(0)));
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new_seekable(mut reader: R) -> Result<BlobReader<R>> {
        let pos = reader.stream_position()?;

        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(pos)),
            last_blob_ok: true,
            header_buf: Vec::new(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_fd: None,
        })
    }

    /// Read and return the [`Blob`] at the given offset. If successful, the cursor of the stream is
    /// positioned at the start of the next [`Blob`].
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::seekable_from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    /// let second_blob = reader.next().unwrap()?;
    ///
    /// let offset = first_blob.offset().unwrap();
    /// let first_blob_again = reader.blob_from_offset(offset)?;
    /// assert_eq!(first_blob.offset(), first_blob_again.offset());
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn blob_from_offset(&mut self, pos: ByteOffset) -> Result<Blob> {
        self.seek(pos)?;
        self.next().unwrap_or_else(|| {
            Err(new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                "no blob at this stream position",
            ))))
        })
    }

    /// Seek to an offset in bytes from the start of the stream.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::seekable_from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    /// let second_blob = reader.next().unwrap()?;
    ///
    /// reader.seek(first_blob.offset().unwrap())?;
    ///
    /// let first_blob_again = reader.next().unwrap()?;
    /// assert_eq!(first_blob.offset(), first_blob_again.offset());
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn seek(&mut self, pos: ByteOffset) -> Result<()> {
        match self.reader.seek(SeekFrom::Start(pos.0)) {
            Ok(offset) => {
                self.offset = Some(ByteOffset(offset));
                Ok(())
            }
            Err(e) => {
                self.offset = None;
                Err(e.into())
            }
        }
    }

    /// Seek to an offset in bytes. (See `std::io::Seek`)
    ///
    /// Note: this calls `Seek::seek` directly, which on `BufReader` discards
    /// the internal buffer regardless of the target. For the common header-walk
    /// pattern of "skip the just-read blob body forward", use the internal
    /// `skip_blob_body` helper which routes through [`BlobReaderSource::skip_relative`]
    /// to preserve the buffer when possible.
    ///
    /// A successful seek clears the sticky error state set by a previous
    /// failing `next()`, so callers that recover from a parse error by
    /// seeking past the bad blob can resume iteration.
    pub fn seek_raw(&mut self, pos: SeekFrom) -> Result<u64> {
        match self.reader.seek(pos) {
            Ok(offset) => {
                self.offset = Some(ByteOffset(offset));
                self.last_blob_ok = true;
                Ok(offset)
            }
            Err(e) => {
                self.offset = None;
                Err(e.into())
            }
        }
    }

    /// Skip `n` bytes forward from the current position, updating the running
    /// offset. Used by the iterator-style header walks
    /// (`next_header_skip_blob`, `next_header_with_data_offset`) to skip past
    /// the just-read blob body without discarding the `BufReader` buffer.
    ///
    /// Routes through [`BlobReaderSource::skip_relative`], which the `BufReader`
    /// impl satisfies via `BufReader::seek_relative`. For non-buffered readers
    /// (`File`, `Cursor`) the default impl is `Seek::seek`, which is already
    /// optimal for those types.
    fn skip_blob_body(&mut self, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        // Skip n-1 bytes via the seek-aware path (preserves the
        // BufReader buffer optimization for in-range targets), then
        // read exactly one byte to validate the file actually contains
        // n bytes from the current position. Without the post-skip
        // read, `BufReader::seek_relative` can succeed past EOF on
        // file-backed readers - the truncation would only surface at
        // the next caller's read. Per
        // `reference/truncation-handling.md` shape 4, a Blob payload
        // that doesn't deliver the declared `datasize` must
        // hard-error here, not be deferred.
        // header.datasize is i32 in the protobuf; capped at
        // MAX_BLOB_HEADER_SIZE upstream. Comfortably fits in i64.
        #[allow(clippy::cast_possible_wrap)]
        let signed = (n - 1) as i64;
        if let Err(e) = self.reader.skip_relative(signed) {
            self.offset = None;
            return Err(e.into());
        }
        let mut sentinel = [0u8; 1];
        match self.reader.read_exact(&mut sentinel) {
            Ok(()) => {
                self.offset = self.offset.map(|x| ByteOffset(x.0 + n));
                Ok(())
            }
            Err(e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof => {
                // Sentinel read at byte (offset + n - 1) returned EOF;
                // declared payload didn't fit in the file. Wrap with
                // offset-aware context per
                // `reference/truncation-handling.md` shape 4.
                let payload_start = self.offset.map_or(0, |x| x.0);
                let trunc_at = payload_start + n - 1;
                self.offset = None;
                Err(new_error(ErrorKind::Io(::std::io::Error::new(
                    ::std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "Blob payload truncated at byte {trunc_at} (shape 4): \
                         declared {n} bytes from offset {payload_start}, \
                         file ended early"
                    ),
                ))))
            }
            Err(e) => {
                self.offset = None;
                Err(e.into())
            }
        }
    }

    /// Read and return next [`BlobHeader`] but skip the following [`Blob`]. This allows really fast
    /// iteration of the PBF structure if only the byte offset and [`BlobType`] are important.
    /// On success, returns the [`BlobHeader`] and the byte offset of the header which can also be
    /// used as an offset for reading the entire [`Blob`] (including header).
    #[allow(clippy::cast_sign_loss)]
    #[hotpath::measure]
    pub fn next_header_skip_blob(&mut self) -> Option<Result<(BlobHeader, Option<ByteOffset>)>> {
        // Stop iteration if there was an error.
        if !self.last_blob_ok {
            return None;
        }

        let prev_offset = self.offset;

        // read header
        let header = match self.read_blob_header() {
            Some(Ok(header)) => header,
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        };

        // Skip blob body via skip_relative-aware helper (preserves BufReader
        // buffer when in-range; falls back to Seek::seek otherwise).
        #[allow(clippy::cast_sign_loss)]
        if let Err(err) = self.skip_blob_body(header.datasize as u64) {
            self.last_blob_ok = false;
            return Some(Err(err));
        }

        Some(Ok((BlobHeader::new(header), prev_offset)))
    }
}

impl BlobReader<BufReader<File>> {
    /// Creates a new `BlobReader` from the given path that is seekable and will be initialized
    /// with a valid offset.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::seekable_from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    ///
    /// assert_eq!(first_blob.offset(), Some(ByteOffset(0)));
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn seekable_from_path<P: AsRef<Path>>(path: P) -> Result<BlobReader<BufReader<File>>> {
        let f = File::open(path.as_ref())?;
        // Use a 256KB BufReader for the same reasons as from_path above:
        // PBF blobs are 16-32KB compressed, so the default 8KB buffer causes 2-4
        // syscalls per blob. 256KB fits several blobs per read and dramatically
        // reduces syscall overhead on sequential iteration.
        //
        // Although seekable_from_path supports seeking, in practice callers that need
        // random access use IndexedReader (which has no BufReader). This path is
        // mostly used for sequential iteration with occasional seek-back, where the
        // large buffer is still beneficial.
        let buf_reader = BufReader::with_capacity(256 * 1024, f);
        Self::new_seekable(buf_reader)
    }
}

// ---------------------------------------------------------------------------
// Public decode helpers
// ---------------------------------------------------------------------------

/// Decode raw Blob protobuf bytes into a [`PrimitiveBlock`].
pub(crate) fn decode_blob_to_primitiveblock(blob_bytes: &[u8]) -> Result<crate::PrimitiveBlock> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    decompress_blob(&blob, None).and_then(crate::PrimitiveBlock::new)
}

/// Parse already-decompressed bytes into a [`PrimitiveBlock`].
///
/// Accepts a `Bytes` value directly. Use `Bytes::from(vec)` to wrap a
/// `Vec<u8>` in O(1).
pub(crate) fn parse_primitive_block_from_bytes_owned(raw: &Bytes) -> Result<crate::PrimitiveBlock> {
    crate::PrimitiveBlock::new(raw.clone())
}

/// Decode raw Blob protobuf bytes into a [`HeaderBlock`].
///
/// This variant accepts `&[u8]` for convenience but must copy the bytes
/// internally. If you already have a `Vec<u8>` or `Bytes`, prefer
/// [`decode_blob_to_headerblock_from_bytes`] to avoid the copy.
pub(crate) fn decode_blob_to_headerblock(blob_bytes: &[u8]) -> Result<crate::HeaderBlock> {
    decode_blob_to_headerblock_from_bytes(&Bytes::copy_from_slice(blob_bytes))
}

/// Zero-copy variant of [`decode_blob_to_headerblock`].
///
/// Accepts a `Bytes` value directly, avoiding the copy that the `&[u8]`
/// variant must perform. Use `Bytes::from(vec)` to wrap a `Vec<u8>` in
/// O(1).
pub(crate) fn decode_blob_to_headerblock_from_bytes(
    blob_bytes: &Bytes,
) -> Result<crate::HeaderBlock> {
    let blob = WireBlob::parse(blob_bytes)?;
    let raw = decompress_blob(&blob, None)?;
    crate::HeaderBlock::parse_from_bytes(&raw)
}

/// Decompress and parse a blob's data as a HeaderBlock.
///
/// Used for the OsmHeader blob path where the decompressed bytes need to be
/// parsed as a HeaderBlock message.
pub(crate) fn decode_headerblock(
    blob: &WireBlob,
    pool: Option<&Arc<DecompressPool>>,
) -> Result<super::block::WireHeaderBlock> {
    let raw = decompress_blob(blob, pool)?;
    super::block::WireHeaderBlock::parse(&raw)
}

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests -- it immediately fails the test with a clear backtrace pointing
// to the exact call site. Propagating Results via `-> Result<()>` in tests would
// lose the backtrace and produce less actionable error messages. The crate-wide
// `unwrap_used = "deny"` lint is designed for production code where panics are
// unacceptable; test code is exempt via this module-level allow.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_get_type() {
        let pairs: &[(BlobKind, BlobType<'_>)] = &[
            (BlobKind::Unknown(String::new()), BlobType::Unknown("")),
            (
                BlobKind::Unknown("abc".to_string()),
                BlobType::Unknown("abc"),
            ),
            (BlobKind::OsmHeader, BlobType::OsmHeader),
            (BlobKind::OsmData, BlobType::OsmData),
        ];

        for (kind, expected_type) in pairs {
            let ff_header = WireBlobHeader {
                blob_type: kind.clone(),
                datasize: 0,
                indexdata: None,
                tagdata: None,
                waymembers: None,
            };
            let ff_blob = WireBlob {
                data: None,
                raw_size: None,
            };

            let blob = Blob::new(ff_header, ff_blob, None);
            assert_eq!(blob.get_type(), *expected_type);
        }
    }

    #[test]
    fn retained_len_charges_full_body_not_just_compression_field() {
        // The selected compression field is one byte, but the declared datasize
        // (the parent body allocation those Bytes slices keep alive) is large.
        // Budget accounting must charge the full body: otherwise a one-byte data
        // field beside a large unknown field is wildly under-charged and
        // file-sized memory can accumulate under the in-flight cap.
        let header = WireBlobHeader {
            blob_type: BlobKind::OsmData,
            datasize: 1_000_000,
            indexdata: None,
            tagdata: None,
            waymembers: None,
        };
        let wire = WireBlob {
            data: Some(BlobData::Raw(Bytes::from_static(&[0u8]))),
            raw_size: None,
        };
        let blob = Blob::new(header, wire, None);
        assert_eq!(blob.retained_len(), 1_000_000);
        assert_eq!(blob.compressed_data().map(|(_, d)| d.len()), Some(1));
    }

    #[test]
    fn retained_len_treats_negative_datasize_as_zero() {
        // datasize is validated non-negative upstream in BlobReader; a stray
        // negative must clamp to 0 rather than wrap to a huge u64 charge.
        let header = WireBlobHeader {
            blob_type: BlobKind::OsmData,
            datasize: -1,
            indexdata: None,
            tagdata: None,
            waymembers: None,
        };
        let wire = WireBlob {
            data: None,
            raw_size: None,
        };
        assert_eq!(Blob::new(header, wire, None).retained_len(), 0);
    }

    fn blob_with_waymembers(waymembers: Option<Vec<u8>>) -> Blob {
        let header = WireBlobHeader {
            blob_type: BlobKind::OsmData,
            datasize: 0,
            indexdata: None,
            tagdata: None,
            waymembers: waymembers.map(Vec::into_boxed_slice),
        };
        let blob = WireBlob {
            data: None,
            raw_size: None,
        };
        Blob::new(header, blob, None)
    }

    #[test]
    fn way_members_strips_preamble_and_reports_count() {
        // version 1, count 9, ceil(9/8) = 2 bitmap bytes.
        let blob = blob_with_waymembers(Some(vec![0x01, 9, 0xA5, 0x01]));
        assert_eq!(blob.way_members(), Some([0xA5u8, 0x01].as_slice()));
        assert_eq!(blob.way_member_count(), Some(9));
    }

    #[test]
    fn way_members_handles_multibyte_count() {
        // count 200 -> varint [0xC8, 0x01]; ceil(200/8) = 25 bitmap bytes.
        let mut payload = vec![0u8; 3 + 25];
        payload[0] = 0x01;
        payload[1] = 0xC8;
        payload[2] = 0x01;
        let blob = blob_with_waymembers(Some(payload));
        assert_eq!(blob.way_member_count(), Some(200));
        assert_eq!(blob.way_members().map(<[u8]>::len), Some(25));
    }

    #[test]
    fn way_members_rejects_malformed() {
        // Absent field.
        assert_eq!(blob_with_waymembers(None).way_members(), None);
        assert_eq!(blob_with_waymembers(None).way_member_count(), None);
        // Wrong version byte.
        assert_eq!(
            blob_with_waymembers(Some(vec![0x02, 1, 0x00])).way_members(),
            None
        );
        // Bitmap shorter than ceil(count/8): count 9 needs 2 bytes, 1 supplied.
        assert_eq!(
            blob_with_waymembers(Some(vec![0x01, 9, 0x00])).way_members(),
            None
        );
        // Bitmap longer than ceil(count/8): count 1 needs 1 byte, 2 supplied.
        assert_eq!(
            blob_with_waymembers(Some(vec![0x01, 1, 0x00, 0x00])).way_members(),
            None
        );
        // Truncated count varint (continuation bit set with no following byte).
        assert_eq!(
            blob_with_waymembers(Some(vec![0x01, 0x80])).way_members(),
            None
        );
    }
}
