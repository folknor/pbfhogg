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
use super::decompress::decompress_parsed_blob_into;
pub(crate) use super::decompress::{
    DecompressPool, decompress_blob, decompress_blob_data_into, decompress_blob_raw,
    decompress_wire_blob_into, pool_get, pool_wrap,
};

// Blob-level wire-format parsers live in the sibling `blob_wire` module.
// Re-exported at pub(crate) so existing paths like `crate::blob::WireBlob`
// and `crate::blob::parse_blob_header_with_index` keep resolving.
pub(crate) use super::blob_wire::{
    BlobData, BlobKind, WireBlob, WireBlobHeader, parse_blob_header_with_index,
};
pub use super::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE};

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
        // Decompress straight into an owned Vec and parse it in place with
        // `from_vec`. The prior `decompress_blob(...).and_then(new)` route
        // decompressed into one buffer and then `PrimitiveBlock::new` copied the
        // whole thing again via `to_vec()`; going through `decompress_into` pays
        // a single decompress and no second whole-buffer copy. Raw/Zlib/Zstd
        // boundary semantics are identical (`decompress_wire_blob_into` keeps
        // the same `> MAX_BLOB_MESSAGE_SIZE` Raw check as `decompress_blob`).
        let mut buf = Vec::new();
        self.decompress_into(&mut buf)?;
        PrimitiveBlock::from_vec(buf)
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
    diagnostics: crate::host::Diagnostics,
    reader: R,
    /// Current reader offset in bytes from the start of the stream.
    offset: Option<ByteOffset>,
    /// Fixed construction offset of the PBF stream, independent of later seeks.
    stream_start: u64,
    /// The file this reader was opened from, when it was opened from a
    /// path at all. Stamped onto every error the framing layer raises, so
    /// a failure on a multi-input command says *which* input failed.
    /// `BlobReader::new` takes an arbitrary `Read` and has no path to
    /// record; it leaves this `None` rather than guessing.
    source: Option<std::sync::Arc<Path>>,
    /// 0-based index of the blob currently being read. Paired with
    /// `source` on every error; `BlobHeader`s are variable-width, so a
    /// byte offset alone does not tell you which blob to look at.
    /// `None` after a seek away from the stream start: the count is only
    /// meaningful while iteration is sequential from blob 0, and a stale
    /// index is a precise-looking attribution pointing at the wrong blob,
    /// which is worse than none.
    blob_index: Option<u64>,
    last_blob_ok: bool,
    /// Reusable buffer for reading blob header bytes. Cleared and refilled each
    /// iteration to avoid allocating a new Vec per blob (~16K allocs per Denmark,
    /// ~2.5M per planet).
    header_buf: Vec<u8>,
    /// When `true`, `WireBlobHeader::parse` allocates tagdata (field 4).
    /// Only needed for tag-filtered reads and merge/sort passthrough.
    parse_tagdata: bool,
    /// When `true`, `WireBlobHeader::parse` copies indexdata (field 2).
    /// Default `true` so standalone readers expose `Blob::index()`. Disabled
    /// in hot paths that never call `Blob::index()` (par_map_reduce,
    /// unfiltered pipeline).
    parse_indexdata: bool,
    /// When `true`, `WireBlobHeader::parse` allocates field-5 waymembers.
    parse_waymembers: bool,
    /// Borrowed-reader callback for fadvise(DONTNEED) after each blob read. When set,
    /// the reader evicts page cache pages behind the read head, preventing
    /// sequential reads from accumulating the entire file in the page cache.
    /// Only set for buffered FileReader - O_DIRECT has no pages to evict - and
    /// only when the caller opted in (`evict_page_cache(true)` or a host with
    /// `PageCachePolicy::Evict`).
    #[cfg(target_os = "linux")]
    evict_prefix: Option<fn(&R, u64) -> bool>,
    /// Count of `posix_fadvise(DONTNEED)` calls this reader has issued.
    ///
    /// Exists so a test can tell "never evicted anything" apart from "evicted,
    /// then had eviction switched off" - a distinction the `evict_prefix` field
    /// alone cannot express, because a reader that already advised the header
    /// blob away looks identical to one that never advised at all once eviction
    /// is disabled. One increment inside a branch that is already making a
    /// syscall.
    #[cfg(target_os = "linux")]
    evict_calls: u64,
}

impl<R: Read + Send> BlobReader<R> {
    /// Attach per-run diagnostics to this reader.
    #[must_use]
    pub fn diagnostics(mut self, diagnostics: crate::host::Diagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

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
            stream_start: 0,
            // No path exists for a bare `Read`. Absent, not guessed.
            source: None,
            blob_index: Some(0),
            last_blob_ok: true,
            header_buf: Vec::new(),
            diagnostics: crate::host::Diagnostics::discard(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_prefix: None,
            #[cfg(target_os = "linux")]
            evict_calls: 0,
        }
    }

    /// Attach this reader's input identity to an error.
    ///
    /// Every path out of the framing layer must go through here or through
    /// [`BlobReader::handle_error`]: an error that escapes unstamped makes
    /// `Error::source_path` lie by omission, and a caller that has learned
    /// to trust it on one code path will trust it on all of them.
    fn stamp_input(&self, error: crate::error::Error) -> crate::error::Error {
        error.in_input(self.source.as_ref(), self.blob_index)
    }

    fn handle_error<T>(&mut self, error: crate::error::Error) -> Option<Result<T>> {
        // Stamp the position before clearing it: the framing layer is the
        // lowest layer that knows where in the stream the failure sits, and
        // without it a wire-format error from a 90 GB planet file says
        // nothing about where to look.
        let at = self.offset;
        self.offset = None;
        self.last_blob_ok = false;
        let error = match at {
            Some(o) => error.at(o),
            None => error,
        };
        Some(Err(self.stamp_input(error)))
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

    /// Read the 4-byte big-endian length prefix. `None` is clean EOF.
    fn read_length_prefix(&mut self) -> Option<Result<u64>> {
        {
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
                    return Some(Err(self.stamp_input(e.into())));
                }
            }
            match self.reader.read_exact(&mut buf[1..]) {
                Ok(()) => {
                    self.offset = self.offset.map(|x| ByteOffset(x.0 + 4));
                    Some(Ok(u64::from(u32::from_be_bytes(buf))))
                }
                Err(e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof => {
                    // 1-3 trailing bytes after a complete previous frame
                    // are tolerated per `reference/truncation-handling.md`
                    // ("clean cut at frame boundary"). The partial length
                    // prefix can't start a new frame; treat as EOF.
                    None
                }
                Err(e) => {
                    // Genuine I/O error (broken pipe, permission denied,
                    // etc.) - propagate the real cause.
                    self.offset = None;
                    self.last_blob_ok = false;
                    Some(Err(self.stamp_input(e.into())))
                }
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn read_blob_header(&mut self) -> Option<Result<WireBlobHeader>> {
        let header_size = match self.read_length_prefix()? {
            Ok(n) => n,
            Err(e) => return Some(Err(e)),
        };

        // Both length-prefix guards (zero-length and MAX_BLOB_HEADER_SIZE)
        // come from `read::frame`, the one owner of frame policy. This decoder
        // keeps its own I/O because it stamps a byte offset between each step,
        // which the streaming helper deliberately does not do.
        #[allow(clippy::cast_possible_truncation)]
        if let Err(err) = crate::frame::check_header_len(header_size as u32) {
            self.last_blob_ok = false;
            return Some(Err(self.stamp_input(match self.offset {
                Some(o) => err.at(o),
                None => err,
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
            let err = new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                format!(
                    "BlobHeader truncated at byte {trunc_at} (shape 3): \
                         declared {header_size} bytes from offset \
                         {header_start}, got {got}"
                ),
            )));
            // Stamp the exact truncation byte here. `handle_error` would
            // otherwise attach `header_start`, which points at the start of
            // the frame rather than at the byte that failed - the message and
            // the accessor would then disagree.
            let err = match self.offset {
                Some(_) => err.at(ByteOffset(trunc_at)),
                None => err,
            };
            return self.handle_error(err);
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
        // The construction position leads an embedded PBF too. Keep that
        // origin fixed across seeks; the blob index is only a fallback for
        // non-seekable inputs (currently past the 4-byte prefix).
        let at_start = self.offset.map_or(self.blob_index == Some(0), |offset| {
            offset.0.checked_sub(4) == Some(self.stream_start)
        });
        if let Err(err) = crate::frame::check_header_position(&header.blob_type, at_start) {
            return self.handle_error(err);
        }
        // These are wire lengths, not version numbers: v1 is 26 bytes,
        // while both v2 and v3 are 42. A new version need not add a length;
        // the version/length consistency check lives in blob_meta.
        if let Some(len) = header.unrecognised_indexdata_len {
            crate::host::diag_once!(
                self.diagnostics,
                Warning,
                "indexdata.unrecognised_length",
                "warning: BlobHeader.indexdata has an unrecognised length ({len} bytes; \
             expected 26 or 42) - blob index ignored, reads fall back to full decode. \
             Further occurrences are not reported."
            );
        }

        if header.datasize < 0 {
            return self.handle_error(new_blob_error(BlobError::InvalidDataSize {
                size: header.datasize,
            }));
        }

        // Reject an oversized declared datasize before `next` /
        // `next_header_skip_blob` allocate or skip the compressed body. The
        // 32 MiB `MAX_BLOB_MESSAGE_SIZE` cap only fires after decompression,
        // so without this guard a hostile datasize forces the
        // `Vec::with_capacity(header.datasize)` in `next` into an outsized
        // pre-decompression allocation. This is the inline twin of the check
        // in `parse_blob_header_with_index`, which covers the raw-frame and
        // header-walker paths that bypass `read_blob_header`. datasize is
        // known non-negative here, so the u64 cast cannot lose sign.
        #[allow(clippy::cast_sign_loss)]
        let datasize = header.datasize as u64;
        if datasize >= MAX_BLOB_DATASIZE {
            self.last_blob_ok = false;
            let err = new_blob_error(BlobError::DataSizeTooBig { size: datasize });
            return Some(Err(self.stamp_input(match self.offset {
                Some(o) => err.at(o),
                None => err,
            })));
        }

        self.offset = self.offset.map(|x| ByteOffset(x.0 + header_size));

        Some(Ok(header))
    }
}

impl BlobReader<FileReader> {
    /// Enable or disable page-cache eviction behind the read head.
    ///
    /// When enabled, a buffered reader advises `POSIX_FADV_DONTNEED` over the
    /// consumed prefix after every blob, which keeps a sequential pass over a
    /// 90 GB planet file from pulling the whole file into the page cache and
    /// displacing everything else there. It is a *global* effect: the page
    /// cache is shared, so a second pass over the same file - a two-pass
    /// command, a test suite, an `osmium` running alongside - then reads cold.
    /// That is why the default is to keep: the file constructors leave the page
    /// cache alone, and eviction is something the caller opts into for one-pass
    /// reads of files larger than RAM.
    ///
    /// Passing `true` enables eviction, `false` disables it again. Enabling has
    /// no effect on an O_DIRECT reader, which has no cached pages to drop, and
    /// the whole method is a no-op off Linux.
    ///
    /// ```
    /// # fn foo() -> pbfhogg::Result<()> {
    /// // A one-pass read of a file larger than RAM.
    /// let reader = pbfhogg::BlobReader::from_path("tests/test.osm.pbf")?
    ///     .evict_page_cache(true);
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    ///
    /// Note that this is a *builder*, so it can only govern reads that have not
    /// happened yet. `BlobReader` reads nothing at construction, so on this type
    /// that covers the whole file - but `ElementReader` decodes the header blob
    /// eagerly, which is why it also carries
    /// [`ElementReader::from_path_with_eviction`](crate::ElementReader::from_path_with_eviction).
    #[must_use]
    pub fn evict_page_cache(mut self, enabled: bool) -> Self {
        #[cfg(target_os = "linux")]
        {
            // A callback preserves the generic Read API without caching an fd
            // or imposing a file-specific trait bound on every BlobReader.
            self.evict_prefix = if enabled && matches!(&self.reader, FileReader::Buffered(_)) {
                Some(FileReader::evict_consumed_prefix)
            } else {
                None
            };
        }
        #[cfg(not(target_os = "linux"))]
        let _ = enabled;
        self
    }

    /// How many `posix_fadvise(DONTNEED)` calls this reader has issued so far.
    ///
    /// Crate-internal: lets a test tell what the kernel was actually asked to
    /// drop, which the armed-or-not state of the reader cannot show.
    #[cfg(target_os = "linux")]
    #[allow(
        dead_code,
        reason = "read only from the blob.rs and reader.rs eviction tests"
    )]
    pub(crate) fn evict_call_count(&self) -> u64 {
        self.evict_calls
    }

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
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            stream_start: 0,
            source: Some(std::sync::Arc::from(path.as_ref())),
            blob_index: Some(0),
            last_blob_ok: true,
            header_buf: Vec::new(),
            diagnostics: crate::host::Diagnostics::discard(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_prefix: None,
            #[cfg(target_os = "linux")]
            evict_calls: 0,
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
            stream_start: 0,
            source: Some(std::sync::Arc::from(path.as_ref())),
            blob_index: Some(0),
            last_blob_ok: true,
            header_buf: Vec::new(),
            diagnostics: crate::host::Diagnostics::discard(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            evict_prefix: None, // O_DIRECT: no pages to evict
            evict_calls: 0,
        })
    }

    /// Open a file for a command run under `host`: buffered or O_DIRECT per
    /// `direct`, with the host's diagnostics and page-cache policy.
    pub(crate) fn open_in<P: AsRef<Path>>(
        path: P,
        direct: bool,
        host: &crate::host::Host,
    ) -> Result<Self> {
        Self::open(path, direct).map(|r| {
            r.diagnostics(host.diagnostics.clone())
                .evict_page_cache(host.page_cache == crate::host::PageCachePolicy::Evict)
        })
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    /// The page cache is left alone; see [`BlobReader::evict_page_cache`].
    pub fn open<P: AsRef<Path>>(path: P, direct: bool) -> Result<Self> {
        let reader = FileReader::open(path.as_ref(), direct)?;
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            stream_start: 0,
            source: Some(std::sync::Arc::from(path.as_ref())),
            blob_index: Some(0),
            last_blob_ok: true,
            header_buf: Vec::new(),
            diagnostics: crate::host::Diagnostics::discard(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_prefix: None,
            #[cfg(target_os = "linux")]
            evict_calls: 0,
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
            let err = new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                format!(
                    "Blob payload truncated at byte {trunc_at} (shape 4): \
                         declared {} bytes from offset {payload_start}, got {got}",
                    header.datasize
                ),
            )));
            // As above: the exact truncation byte beats the payload start
            // that `handle_error` would otherwise stamp.
            let err = match self.offset {
                Some(_) => err.at(ByteOffset(trunc_at)),
                None => err,
            };
            return self.handle_error(err);
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
        // in the page cache - critical for 30+ GB PBFs on memory-constrained
        // hosts. Off unless the caller opts in with
        // `BlobReader::evict_page_cache(true)` or a host with
        // `PageCachePolicy::Evict`.
        //
        // The advised range is the whole consumed prefix `0..offset`, re-issued
        // per blob, where `prev_offset..offset` would express the same intent
        // over a bounded range. That looks like an easy win and is left alone
        // deliberately: the cost was never measured, and changing which pages
        // get dropped changes the cache state every subsequent benchmark on a
        // host observes, so it is not a change to make on inspection alone:
        // it wants a matched A/B of both ranges on a planet-sized input with
        // the cache state recorded before it is narrowed.
        #[cfg(target_os = "linux")]
        if let Some(evict) = self.evict_prefix
            && let Some(offset) = self.offset
            && evict(&self.reader, offset.0)
        {
            self.evict_calls += 1;
        }

        self.blob_index = self.blob_index.map(|i| i + 1);
        Some(Ok(Blob::new(header, blob, prev_offset)))
    }
}

impl<R: BlobReaderSource + Send> BlobReader<R> {
    /// Creates a new `BlobReader` from the given reader that is seekable and will be initialized
    /// with a valid offset. The construction position is the PBF stream start:
    /// an OSMHeader there is leading even at a nonzero absolute offset. Later
    /// seeks do not redefine that start. Reported byte offsets remain absolute.
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
            stream_start: pos,
            // `new_seekable` is generic over any seekable reader; the path
            // (if any) is the caller's, not something this can recover.
            source: None,
            // Only a stream handed over at its start can be counted from
            // blob 0; mid-stream the index is unknowable, not zero.
            blob_index: (pos == 0).then_some(0),
            last_blob_ok: true,
            header_buf: Vec::new(),
            diagnostics: crate::host::Diagnostics::discard(),
            parse_tagdata: false,
            parse_indexdata: true,
            parse_waymembers: false,
            #[cfg(target_os = "linux")]
            evict_prefix: None,
            #[cfg(target_os = "linux")]
            evict_calls: 0,
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
                // The blob count is only meaningful while iteration runs
                // sequentially from the stream start. A rewind to 0
                // restarts it; any other target invalidates it - a stale
                // index stamped onto a later error would name the wrong
                // blob, which is worse than naming none. A reader built at
                // a nonzero position (an embedded PBF) never knows its
                // absolute blob index, and offset 0 there is outside its
                // stream, so no seek restores the count.
                self.blob_index = (offset == 0 && self.stream_start == 0).then_some(0);
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
                // Same invalidation rule as `seek`.
                self.blob_index = (offset == 0 && self.stream_start == 0).then_some(0);
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
        // MAX_BLOB_DATASIZE upstream. Comfortably fits in i64.
        #[allow(clippy::cast_possible_wrap)]
        let signed = (n - 1) as i64;
        // Capture the position before any error path clears it: header-only
        // consumers (`next_header_skip_blob`) return this error verbatim, so
        // if the offset is not stamped here it is lost for good and
        // `Error::byte_offset` reports `None` on exactly the truncation
        // errors it exists to locate.
        let at = self.offset;
        if let Err(e) = self.reader.skip_relative(signed) {
            self.offset = None;
            let err: crate::error::Error = e.into();
            return Err(match at {
                Some(o) => err.at(o),
                None => err,
            });
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
                let err = new_error(ErrorKind::Io(::std::io::Error::new(
                    ::std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "Blob payload truncated at byte {trunc_at} (shape 4): \
                         declared {n} bytes from offset {payload_start}, \
                         file ended early"
                    ),
                )));
                // The exact byte we failed to read, not the payload start -
                // but only when the reader was actually tracking a position;
                // `payload_start` falls back to 0 when it was not, and a
                // confidently wrong offset is worse than none.
                Err(match at {
                    Some(_) => err.at(ByteOffset(trunc_at)),
                    None => err,
                })
            }
            Err(e) => {
                self.offset = None;
                let err: crate::error::Error = e.into();
                Err(match at {
                    Some(o) => err.at(o),
                    None => err,
                })
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
            return Some(Err(self.stamp_input(err)));
        }

        self.blob_index = self.blob_index.map(|i| i + 1);
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
        // Although seekable_from_path supports seeking, this path is mostly used
        // for sequential iteration with occasional seek-back, where the large
        // buffer is still beneficial.
        let buf_reader = BufReader::with_capacity(crate::write::IO_BUF_BYTES, f);
        let mut reader = Self::new_seekable(buf_reader)?;
        // `new_seekable` is generic and cannot know the path; this entry
        // point does, so it fills the input identity back in.
        reader.source = Some(std::sync::Arc::from(path.as_ref()));
        Ok(reader)
    }
}

// ---------------------------------------------------------------------------
// Public decode helpers
// ---------------------------------------------------------------------------

/// Decode raw Blob protobuf bytes into a [`PrimitiveBlock`].
pub(crate) fn decode_blob_to_primitiveblock(blob_bytes: &[u8]) -> Result<crate::PrimitiveBlock> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    // Decompress into an owned Vec and parse in place via `from_vec`, avoiding
    // the second whole-buffer `to_vec()` copy that `PrimitiveBlock::new` pays.
    // `decompress_wire_blob_into` keeps the same `> MAX_BLOB_MESSAGE_SIZE` Raw
    // boundary as `decompress_blob`, so decode semantics are unchanged.
    let mut buf = Vec::new();
    decompress_wire_blob_into(&blob, &mut buf)?;
    crate::PrimitiveBlock::from_vec(buf)
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
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    /// Truncate `tests/test.osm.pbf` inside the payload of its last blob and
    /// return the file, its path kept alive by the returned tempdir.
    ///
    /// The cut point is derived from the real frame layout rather than guessed,
    /// so the fixture is provably shape 4 ("EOF inside the Blob payload") and
    /// not shape 1-3, which have their own unified detection in `frame.rs` and
    /// would make the test below pass for the wrong reason.
    fn pbf_truncated_inside_the_last_payload() -> (tempfile::TempDir, std::path::PathBuf) {
        let full = std::fs::read("tests/test.osm.pbf").unwrap();

        let mut walker = BlobReader::new_seekable(std::io::Cursor::new(full.as_slice())).unwrap();
        let mut last_payload_start = 0u64;
        let mut last_payload_end = 0u64;
        while let Some(item) = walker.next_header_skip_blob() {
            let (header, frame_start) = item.unwrap();
            let frame_start = frame_start.unwrap().0;
            // 4-byte length prefix + BlobHeader + payload. The payload start is
            // the position the reader reached after consuming the header, which
            // is `frame_start + 4 + header_len`; recovering `header_len` from
            // the file is simpler than threading it out of the reader.
            let header_len = u32::from_be_bytes(
                full[frame_start as usize..frame_start as usize + 4]
                    .try_into()
                    .unwrap(),
            ) as u64;
            last_payload_start = frame_start + 4 + header_len;
            last_payload_end = last_payload_start + u64::try_from(header.get_blob_size()).unwrap();
        }
        assert_eq!(
            last_payload_end,
            full.len() as u64,
            "the fixture's last frame should end exactly at EOF"
        );

        // Cut three bytes into the last payload: past its first byte, so the
        // header is fully committed and only the payload is short.
        let cut = last_payload_start + 3;
        assert!(cut < last_payload_end, "cut must land inside the payload");

        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("shape4.osm.pbf");
        std::fs::write(&path, &full[..cut as usize]).unwrap();
        (dir, path)
    }

    /// All three shape-4 detectors must agree on the same file.
    ///
    /// "EOF inside the Blob payload" is the one truncation shape `frame.rs`
    /// could not unify, because each site has only the mechanism its I/O shape
    /// offers: `BlobReader::next` compares the byte count returned by
    /// `read_to_end` against the declared `datasize`; `next_header_skip_blob`
    /// seeks past the payload and reads one sentinel byte; `HeaderWalker`
    /// compares the payload extent against the file size captured at `open`.
    /// Unifying them needs a seekable-source abstraction that reports "bytes
    /// available from here", which is a refactor rather than a guard.
    ///
    /// Until that exists, this test is what keeps the three from drifting: if
    /// any one of them ever stops hard-erroring - the silent-truncation failure
    /// the whole shape catalogue exists to prevent - it fails here rather than
    /// in the field. The intact file is walked by all three arms first, so a
    /// blanket "everything errors" regression cannot make this pass.
    #[test]
    fn all_three_shape_four_detectors_reject_the_same_file() {
        let intact = std::path::Path::new("tests/test.osm.pbf");
        let (_dir, truncated) = pbf_truncated_inside_the_last_payload();

        // Arm 1: full-payload read, counting bytes delivered.
        let read_arm = |path: &std::path::Path| -> Result<usize> {
            let mut count = 0;
            for blob in BlobReader::from_path(path)? {
                blob?;
                count += 1;
            }
            Ok(count)
        };
        // Arm 2: header walk that seeks past each payload and probes for EOF.
        let skip_arm = |path: &std::path::Path| -> Result<usize> {
            let file = std::fs::File::open(path).unwrap();
            let mut reader = BlobReader::new_seekable(std::io::BufReader::new(file))?;
            let mut count = 0;
            while let Some(item) = reader.next_header_skip_blob() {
                item?;
                count += 1;
            }
            Ok(count)
        };
        // Arm 3: pread walk that checks each payload extent against file size.
        let walk_arm = |path: &std::path::Path| -> Result<usize> {
            let mut walker = super::super::header_walker::HeaderWalker::open(
                path,
                &crate::host::Diagnostics::discard(),
            )?;
            let mut count = 0;
            while walker.next_header()?.is_some() {
                count += 1;
            }
            Ok(count)
        };

        let intact_counts = [
            read_arm(intact).expect("read arm on the intact file"),
            skip_arm(intact).expect("skip arm on the intact file"),
            walk_arm(intact).expect("walk arm on the intact file"),
        ];
        assert!(
            intact_counts[0] > 1 && intact_counts.iter().all(|c| *c == intact_counts[0]),
            "the three arms must see the same intact file: {intact_counts:?}"
        );

        for (name, result) in [
            ("read", read_arm(&truncated)),
            ("skip", skip_arm(&truncated)),
            ("walk", walk_arm(&truncated)),
        ] {
            assert!(
                result.is_err(),
                "the {name} arm must hard-error on a payload-truncated file, \
                 got Ok({result:?}) - shape 4 has drifted apart"
            );
        }
    }

    /// Public file readers leave the page cache alone unless the caller asks,
    /// and asking does not change what the reader delivers.
    ///
    /// The assertion is on the fadvise call count the reader actually issued,
    /// not on the element count alone, which would pass just as well against
    /// a builder that did nothing at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn public_readers_keep_the_page_cache_unless_asked() {
        fn element_count(reader: &mut BlobReader<FileReader>) -> usize {
            reader
                .map(|blob| match blob.unwrap().decode().unwrap() {
                    BlobDecode::OsmData(block) => block.elements().count(),
                    _ => 0,
                })
                .sum()
        }
        let path = std::path::Path::new("tests/test.osm.pbf");

        let mut kept = BlobReader::from_path(path).unwrap();
        let kept_elements = element_count(&mut kept);
        assert_eq!(
            kept.evict_call_count(),
            0,
            "a public reader must not evict by default"
        );

        let mut evicted = BlobReader::from_path(path).unwrap().evict_page_cache(true);
        let evicted_elements = element_count(&mut evicted);
        assert!(
            evicted.evict_call_count() > 0,
            "evict_page_cache(true) must evict behind the read head"
        );

        assert_eq!(
            kept_elements, evicted_elements,
            "eviction must not change what is delivered"
        );
        assert!(kept_elements > 0, "the fixture must contain elements");
    }

    /// Hand-encode `[4-byte BE header_len][BlobHeader]` for an `OSMData` blob
    /// declaring the given datasize, with no Blob payload following. Lets a
    /// test drive `BlobReader` with a hostile declared datasize.
    fn frame_with_datasize(datasize: u64) -> Vec<u8> {
        let mut header = Vec::new();
        // Field 1 (type): tag 0x0A, len 7, "OSMData".
        header.push(0x0A);
        header.push(7);
        header.extend_from_slice(b"OSMData");
        // Field 3 (datasize): tag 0x18, LEB128 varint.
        header.push(0x18);
        let mut v = datasize;
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            header.push(byte);
            if v == 0 {
                break;
            }
        }
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&header);
        frame
    }

    #[test]
    fn overflowing_datasize_is_refused_before_reading_the_payload() {
        let frame = frame_with_datasize((1_u64 << 32) + 100);
        let mut reader = BlobReader::new(Cursor::new(frame.clone()));
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            err.to_string()
                .contains("BlobHeader.datasize exceeds int32")
        );
        assert!(reader.next().is_none());

        let mut reader = BlobReader::new_seekable(Cursor::new(frame)).unwrap();
        let err = reader.next_header_skip_blob().unwrap().unwrap_err();
        assert!(
            err.to_string()
                .contains("BlobHeader.datasize exceeds int32")
        );
        assert!(reader.next_header_skip_blob().is_none());
    }

    /// A BlobHeader declaring `datasize == MAX_BLOB_DATASIZE` is rejected with
    /// the typed `DataSizeTooBig` error *before* `next` reaches the
    /// `Vec::with_capacity(datasize)` payload allocation. Without the guard the
    /// reader would attempt a 32 MiB pre-decompression allocation driven purely
    /// by the declared size and only then hit the (absent) payload as a
    /// truncation Io error. The frame carries no payload, so the fact that the
    /// surfaced error is `DataSizeTooBig` and not `Io(UnexpectedEof)` proves the
    /// cap fires ahead of the allocation. Iteration then stops (sticky error).
    #[test]
    fn datasize_over_cap_rejected_before_payload_alloc() {
        let frame = frame_with_datasize(MAX_BLOB_DATASIZE);
        let mut reader = BlobReader::new(Cursor::new(frame));
        let err = reader.next().unwrap().unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                assert_eq!(size, MAX_BLOB_DATASIZE);
            }
            other => panic!("expected DataSizeTooBig, got {other:?}"),
        }
        assert!(reader.next().is_none(), "iteration stops after error");
    }

    /// The `next_header_skip_blob` funnel (the header-walk spine, distinct from
    /// `next`) enforces the same cap: `datasize == MAX_BLOB_DATASIZE` is rejected
    /// with `DataSizeTooBig` before `skip_blob_body` is asked to skip the body.
    #[test]
    fn next_header_skip_blob_rejects_over_cap_datasize() {
        let frame = frame_with_datasize(MAX_BLOB_DATASIZE);
        let mut reader = BlobReader::new(Cursor::new(frame));
        let err = reader.next_header_skip_blob().unwrap().unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                assert_eq!(size, MAX_BLOB_DATASIZE);
            }
            other => panic!("expected DataSizeTooBig, got {other:?}"),
        }
        assert!(
            reader.next_header_skip_blob().is_none(),
            "iteration stops after error"
        );
    }

    /// The largest legal declared datasize (`MAX_BLOB_DATASIZE - 1`) passes the
    /// cap guard; the read then fails downstream because this fixture carries no
    /// payload. Driven through `next_header_skip_blob` so the sentinel-byte skip
    /// surfaces the truncation without a 32 MiB payload allocation. The surfaced
    /// error being `Io(UnexpectedEof)` and *not* `DataSizeTooBig` is the proof
    /// that one-below-cap is admitted by the guard, pinning the `>=` boundary at
    /// the BlobReader funnel exactly as the sibling `parse_blob_header_with_index`
    /// test pins it at the raw-frame funnel.
    #[test]
    fn datasize_just_below_cap_passes_guard_then_truncates() {
        let frame = frame_with_datasize(MAX_BLOB_DATASIZE - 1);
        let mut reader = BlobReader::new(Cursor::new(frame));
        let err = reader.next_header_skip_blob().unwrap().unwrap_err();
        match err.into_kind() {
            ErrorKind::Io(ref e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof => {}
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                panic!("one-below-cap datasize {size} was wrongly rejected by the guard");
            }
            other => panic!("expected Io(UnexpectedEof) truncation, got {other:?}"),
        }
    }

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
                indexdata_present: false,
                unrecognised_indexdata_len: None,
                tagdata: None,
                tagdata_present: false,
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
            indexdata_present: false,
            unrecognised_indexdata_len: None,
            tagdata: None,
            tagdata_present: false,
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
            indexdata_present: false,
            unrecognised_indexdata_len: None,
            tagdata: None,
            tagdata_present: false,
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
            indexdata_present: false,
            unrecognised_indexdata_len: None,
            tagdata: None,
            tagdata_present: false,
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

    /// D8 count-gap fixture: a hand-built blob whose field-5 preamble declares
    /// a `way_count` differing from the blob's actual decoded Way count *within
    /// one bitmap byte* (encoded 7, actual 8; `ceil(7/8) == ceil(8/8) == 1`).
    /// The producer always keeps encoded == actual, so this class of gap can
    /// only be built by hand. It is invisible through `way_members().len()`
    /// alone (both round to a 1-byte bitmap); only `way_member_count()` exposes
    /// it, which is why that accessor is on the public surface - the enriched
    /// -file consumer compares the encoded count against the blob's real Way
    /// count and hard-errors on a mismatch.
    #[test]
    fn way_member_count_exposes_within_byte_gap_vs_decoded_ways() {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let mut bb = BlockBuilder::new();
        for id in 1_i64..=8 {
            bb.add_way(
                id,
                std::iter::empty::<(&str, &str)>(),
                &[id * 10, id * 10 + 1],
                None,
            );
        }
        let owned = bb.take_owned().expect("encode").expect("nonempty block");

        // Preamble: version 1, way_count 7 (one short of the 8 real ways), one
        // bitmap byte. ceil(7/8) == ceil(8/8) == 1, so the gap stays within the
        // single bitmap byte and cannot be seen from the bitmap length.
        let field5 = [0x01_u8, 7, 0x00];

        let header = HeaderBuilder::new().sorted().build().expect("build header");
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut buf, Compression::default());
            writer.write_header(&header).expect("write header");
            writer
                .write_primitive_block_owned(owned.bytes, owned.index, None, Some(&field5))
                .expect("write block");
            writer.flush().expect("flush");
        }

        let mut reader = BlobReader::new(Cursor::new(buf));
        reader.set_parse_waymembers(true);
        reader.next().expect("header blob").expect("read header");
        let data = reader.next().expect("data blob").expect("read data");

        assert_eq!(data.way_member_count(), Some(7), "encoded count preserved");
        assert_eq!(
            data.way_members().map(<[u8]>::len),
            Some(1),
            "the gap is invisible through the bitmap length",
        );
        let actual_ways = match data.decode().expect("decode") {
            BlobDecode::OsmData(block) => block
                .elements()
                .filter(|e| matches!(e, crate::Element::Way(_)))
                .count(),
            other => panic!("expected OsmData, got {other:?}"),
        };
        assert_eq!(actual_ways, 8, "blob really carries 8 ways");
        assert_ne!(
            u32::try_from(actual_ways).expect("fits"),
            data.way_member_count().expect("count"),
            "the encoded-vs-actual count gap is detectable via way_member_count()",
        );
    }
}
