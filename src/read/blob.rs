//! Read and decode blobs

use super::block::{HeaderBlock, PrimitiveBlock};
use crate::error::{new_blob_error, new_error, new_wire_error, BlobError, ErrorKind, Result};
use bytes::Bytes;
use super::file_reader::FileReader;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::ZlibDecoder;
use std::io::Cursor;

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
    // wontfix(type-result-fallible): .ok() on Mutex::lock() — poisoning means another
    // thread panicked; falling back to a fresh Vec is the correct recovery here.
    pub fn get(&self) -> Vec<u8> {
        self.buffers
            .lock()
            .ok()
            .and_then(|mut v| v.pop())
            .unwrap_or_default()
    }

    /// Return a buffer to the pool for reuse.
    fn put(&self, mut buf: Vec<u8>) {
        buf.clear();
        if let Ok(mut v) = self.buffers.lock() {
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

/// Get a decompression buffer — from the pool if available, otherwise fresh.
fn pool_get(pool: Option<&Arc<DecompressPool>>, capacity: usize) -> Vec<u8> {
    match pool {
        Some(p) => {
            let mut buf = p.get();
            buf.reserve(capacity.saturating_sub(buf.capacity()));
            buf
        }
        None => Vec::with_capacity(capacity),
    }
}

/// Wrap decoded bytes as `Bytes` — returning to pool on drop if pooled.
fn pool_wrap(decoded: Vec<u8>, pool: Option<&Arc<DecompressPool>>) -> Bytes {
    match pool {
        Some(p) => Bytes::from_owner(PooledBuffer {
            vec: decoded,
            pool: Arc::clone(p),
        }),
        None => Bytes::from(decoded),
    }
}

// ---------------------------------------------------------------------------
// Wire-format protobuf message types for blob reading
// ---------------------------------------------------------------------------

/// Parsed BlobHeader from protobuf wire format.
///
/// Fields: type (string, field 1), indexdata (bytes, field 2), datasize (int32, field 3).
#[derive(Clone, Debug)]
pub(crate) struct WireBlobHeader {
    pub blob_type: String,
    pub datasize: i32,
    pub indexdata: Option<Vec<u8>>,
}

impl WireBlobHeader {
    /// Parse a BlobHeader from raw protobuf bytes.
    pub fn parse(data: &[u8]) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(data);
        let mut blob_type = String::new();
        let mut datasize: i32 = 0;
        let mut indexdata: Option<Vec<u8>> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match field {
                1 => {
                    // type: string (len-delimited)
                    let bytes = cursor.read_len_delimited()?;
                    blob_type = String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in BlobHeader type"))?;
                }
                2 => {
                    // indexdata: bytes (len-delimited)
                    let bytes = cursor.read_len_delimited()?;
                    indexdata = Some(bytes.to_vec());
                }
                3 => {
                    // datasize: int32 (varint)
                    #[allow(clippy::cast_possible_truncation)]
                    { datasize = cursor.read_varint()? as i32; }
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireBlobHeader { blob_type, datasize, indexdata })
    }
}

/// Compressed data variant in a Blob.
#[derive(Clone, Debug)]
pub(crate) enum BlobData {
    Raw(Bytes),
    Zlib(Bytes),
    Zstd(Bytes),
}

/// Parsed Blob envelope from protobuf wire format.
///
/// Fields: raw (bytes, field 1), raw_size (int32, field 2),
/// zlib_data (bytes, field 3), zstd_data (bytes, field 7).
#[derive(Clone, Debug)]
pub(crate) struct WireBlob {
    pub data: Option<BlobData>,
    pub raw_size: Option<i32>,
}

impl WireBlob {
    /// Parse a Blob from `Bytes`, preserving zero-copy slices for compressed data.
    ///
    /// The returned `BlobData` variants hold `Bytes` slices of the input, so
    /// decompressors get zero-copy access to the compressed payload.
    pub fn parse(input: &Bytes) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(input);
        let mut data: Option<BlobData> = None;
        let mut raw_size: Option<i32> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match field {
                1 => {
                    // raw: bytes (len-delimited)
                    let slice = cursor.read_len_delimited()?;
                    let offset = slice.as_ptr() as usize - input.as_ptr() as usize;
                    data = Some(BlobData::Raw(input.slice(offset..offset + slice.len())));
                }
                2 => {
                    // raw_size: int32 (varint)
                    #[allow(clippy::cast_possible_truncation)]
                    { raw_size = Some(cursor.read_varint()? as i32); }
                }
                3 => {
                    // zlib_data: bytes (len-delimited)
                    let slice = cursor.read_len_delimited()?;
                    let offset = slice.as_ptr() as usize - input.as_ptr() as usize;
                    data = Some(BlobData::Zlib(input.slice(offset..offset + slice.len())));
                }
                7 => {
                    // zstd_data: bytes (len-delimited)
                    let slice = cursor.read_len_delimited()?;
                    let offset = slice.as_ptr() as usize - input.as_ptr() as usize;
                    data = Some(BlobData::Zstd(input.slice(offset..offset + slice.len())));
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireBlob { data, raw_size })
    }

    /// Parse a Blob from `&[u8]`, copying the input internally.
    pub fn parse_slice(data: &[u8]) -> Result<Self> {
        let bytes = Bytes::copy_from_slice(data);
        Self::parse(&bytes)
    }
}

/// Maximum allowed [`BlobHeader`] size in bytes.
/// Compile-time constant per the PBF spec. Uses `const` (not `static`) so the value
/// is inlined at each use site with no memory address or indirection overhead.
pub const MAX_BLOB_HEADER_SIZE: u64 = 64 * 1024;

/// Maximum allowed uncompressed [`Blob`] content size in bytes.
/// Compile-time constant per the PBF spec. Uses `const` (not `static`) so the value
/// is inlined at each use site with no memory address or indirection overhead.
pub const MAX_BLOB_MESSAGE_SIZE: u64 = 32 * 1024 * 1024;

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
    fn new(
        header: WireBlobHeader,
        blob: WireBlob,
        offset: Option<ByteOffset>,
    ) -> Blob {
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
        match self.header.blob_type.as_str() {
            x if x == BlobType::OsmHeader.as_str() => BlobType::OsmHeader,
            x if x == BlobType::OsmData.as_str() => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the byte offset of the blob from the start of its source stream.
    /// This might be [`None`] if the source stream does not implement [`Seek`].
    #[inline]
    pub fn offset(&self) -> Option<ByteOffset> {
        self.offset
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

    /// Returns the blob-level index from the header's `indexdata` field, if present.
    ///
    /// PBFs written by pbfhogg embed indexdata automatically. Third-party PBFs
    /// (Geofabrik, osmium) typically do not — this returns `None` for those.
    pub(crate) fn index(&self) -> Option<crate::blob_index::BlobIndex> {
        self.header
            .indexdata
            .as_deref()
            .and_then(crate::blob_index::BlobIndex::deserialize)
    }

    /// Like [`to_primitiveblock`](Self::to_primitiveblock), but reuses decompression buffers
    /// from a [`DecompressPool`]. Used by the pipeline for buffer reuse.
    pub(crate) fn to_primitiveblock_pooled(
        &self,
        pool: &Arc<DecompressPool>,
    ) -> Result<PrimitiveBlock> {
        decompress_blob(&self.blob, Some(pool)).and_then(PrimitiveBlock::new)
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
        match self.header.blob_type.as_str() {
            "OSMHeader" => BlobType::OsmHeader,
            "OSMData" => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the size of the following blob in bytes.
    // wontfix(name-no-get-prefix): inherited from osmpbf public API
    #[inline]
    pub fn get_blob_size(&self) -> i32 {
        self.header.datasize
    }
}

/// A reader for PBF files that allows iterating over [`Blob`]s.
// wontfix(type-generic-bounds): bounds on struct match osmpbf API and document intent
#[derive(Clone, Debug)]
pub struct BlobReader<R: Read + Send> {
    reader: R,
    /// Current reader offset in bytes from the start of the stream.
    offset: Option<ByteOffset>,
    last_blob_ok: bool,
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
        }
    }

    fn handle_error<T>(&mut self, error: crate::error::Error) -> Option<Result<T>> {
        self.offset = None;
        self.last_blob_ok = false;
        Some(Err(error))
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
                Err(e) => {
                    // Truncated header or I/O error -- propagate the real cause.
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
        let mut header_data = Vec::with_capacity(header_size as usize);
        if let Err(e) = reader.read_to_end(&mut header_data) {
            return self.handle_error(e.into());
        }

        let header = match WireBlobHeader::parse(&header_data) {
            Ok(header) => header,
            Err(e) => {
                return self.handle_error(e)
            }
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
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
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
        })
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    pub fn open<P: AsRef<Path>>(path: P, direct: bool) -> Result<Self> {
        let reader = FileReader::open(path.as_ref(), direct)?;
        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
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

        let blob_bytes = Bytes::from(blob_data);
        let blob = match WireBlob::parse(&blob_bytes) {
            Ok(blob) => blob,
            Err(e) => return self.handle_error(e),
        };

        self.offset = self
            .offset
            .map(|x| ByteOffset(x.0 + header.datasize as u64));

        Some(Ok(Blob::new(header, blob, prev_offset)))
    }
}

impl<R: Read + Seek + Send> BlobReader<R> {
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
    pub fn seek_raw(&mut self, pos: SeekFrom) -> Result<u64> {
        match self.reader.seek(pos) {
            Ok(offset) => {
                self.offset = Some(ByteOffset(offset));
                Ok(offset)
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

        // skip blob (which also adjusts self.offset)
        if let Err(err) = self.seek_raw(SeekFrom::Current(header.datasize as i64)) {
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

/// Parse raw blob frame bytes into a BlobHeader type string and Blob payload bytes.
///
/// Input: the raw bytes after the 4-byte header length prefix (i.e. just the BlobHeader bytes).
/// Returns: `(blob_type, data_size)`.
pub fn parse_blob_header(header_bytes: &[u8]) -> Result<(String, usize)> {
    let header = WireBlobHeader::parse(header_bytes)?;
    if header.datasize < 0 {
        return Err(new_blob_error(BlobError::InvalidDataSize {
            size: header.datasize,
        }));
    }
    #[allow(clippy::cast_sign_loss)]
    Ok((header.blob_type, header.datasize as usize))
}

/// Zero-copy variant of [`parse_blob_header`].
///
/// Accepts a `&Bytes` for backward compatibility. Delegates to the `&[u8]`
/// variant since the hand-rolled parser operates on `&[u8]` directly.
///
/// Input: the raw bytes after the 4-byte header length prefix (i.e. just the BlobHeader bytes).
/// Returns: `(blob_type, data_size)`.
pub fn parse_blob_header_from_bytes(header_bytes: &Bytes) -> Result<(String, usize)> {
    parse_blob_header(header_bytes)
}

/// Parse a BlobHeader and also extract the optional `indexdata` field.
///
/// Returns `(blob_type, data_size, optional_indexdata)`. The indexdata, when
/// present, contains blob-level metadata (element type, ID range, count)
/// that allows merge to classify blobs without decompression.
pub fn parse_blob_header_with_index(
    header_bytes: &[u8],
) -> Result<(String, usize, Option<Vec<u8>>)> {
    let header = WireBlobHeader::parse(header_bytes)?;
    if header.datasize < 0 {
        return Err(new_blob_error(BlobError::InvalidDataSize {
            size: header.datasize,
        }));
    }
    #[allow(clippy::cast_sign_loss)]
    Ok((header.blob_type, header.datasize as usize, header.indexdata))
}

/// Decode raw Blob protobuf bytes into a [`PrimitiveBlock`].
///
/// This variant accepts `&[u8]` for convenience but must copy the bytes
/// internally. If you already have a `Vec<u8>` or `Bytes`, prefer
/// [`decode_blob_to_primitiveblock_from_bytes`] to avoid the copy.
pub fn decode_blob_to_primitiveblock(blob_bytes: &[u8]) -> Result<crate::PrimitiveBlock> {
    decode_blob_to_primitiveblock_from_bytes(&Bytes::copy_from_slice(blob_bytes))
}

/// Zero-copy variant of [`decode_blob_to_primitiveblock`].
///
/// Accepts a `Bytes` value directly, avoiding the copy that the `&[u8]`
/// variant must perform. Use `Bytes::from(vec)` to wrap a `Vec<u8>` in
/// O(1).
pub fn decode_blob_to_primitiveblock_from_bytes(
    blob_bytes: &Bytes,
) -> Result<crate::PrimitiveBlock> {
    let blob = WireBlob::parse(blob_bytes)?;
    decompress_blob(&blob, None).and_then(crate::PrimitiveBlock::new)
}

/// Decompress a blob's data without parsing it into a typed message.
/// Returns the raw decompressed protobuf bytes.
///
/// This variant accepts `&[u8]` for convenience but must copy the bytes
/// internally to parse the Blob protobuf envelope. If you already have a
/// `Vec<u8>` or `Bytes`, prefer [`decompress_blob_data_from_bytes`] to
/// avoid the copy.
pub fn decompress_blob_data(blob_bytes: &[u8]) -> Result<Vec<u8>> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    decompress_blob(&blob, None).map(|b| b.to_vec())
}

/// Decompress a blob's data into a caller-provided buffer for reuse.
///
/// Like [`decompress_blob_data`] but clears and reuses `buf` instead of
/// allocating a new `Vec` each time. For loops that decompress many blobs,
/// this avoids repeated large allocations -- the buffer grows to high-water
/// mark and stays there.
pub fn decompress_blob_data_into(blob_bytes: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    decompress_parsed_blob_into(&blob, buf)
}

/// Zero-copy variant of [`decompress_blob_data_into`].
///
/// Accepts a `Bytes` value directly, avoiding the envelope copy.
pub fn decompress_blob_data_into_from_bytes(blob_bytes: &Bytes, buf: &mut Vec<u8>) -> Result<()> {
    let blob = WireBlob::parse(blob_bytes)?;
    decompress_parsed_blob_into(&blob, buf)
}

/// Decompress a parsed Blob protobuf into a caller-provided buffer.
#[allow(clippy::cast_sign_loss)]
fn decompress_parsed_blob_into(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
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
            if blob.raw_size.unwrap_or(0) > 0 {
                let capacity = blob.raw_size.unwrap_or(0) as usize;
                buf.reserve(capacity.saturating_sub(buf.capacity()));
            }
            let mut decoder = ZlibDecoder::new(&**bytes).take(MAX_BLOB_MESSAGE_SIZE);
            decoder.read_to_end(buf)?;
            Ok(())
        }
        Some(BlobData::Zstd(bytes)) => {
            if blob.raw_size.unwrap_or(0) > 0 {
                let capacity = blob.raw_size.unwrap_or(0) as usize;
                buf.reserve(capacity.saturating_sub(buf.capacity()));
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

/// Zero-copy variant of [`decompress_blob_data`].
///
/// Accepts a `Bytes` value directly, avoiding the copy that the `&[u8]`
/// variant must perform. Use `Bytes::from(vec)` to wrap a `Vec<u8>` in
/// O(1).
///
/// Returns the raw decompressed protobuf bytes.
#[allow(clippy::cast_sign_loss)]
#[hotpath::measure]
pub fn decompress_blob_data_from_bytes(blob_bytes: &Bytes) -> Result<Vec<u8>> {
    let blob = WireBlob::parse(blob_bytes)?;
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size < MAX_BLOB_MESSAGE_SIZE {
                Ok(bytes.to_vec())
            } else {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            // When raw_size is set (the common case for modern PBF files), we use
            // the exact decompressed size -- one perfect allocation, no reallocs.
            //
            // When raw_size is missing (rare, older PBF files), compressed size alone
            // is 3-10x too small because zlib typically achieves that compression ratio.
            // Using compressed size as capacity would cause multiple Vec reallocations
            // as read_to_end grows the buffer.
            //
            // 4x is a conservative middle ground: it avoids most reallocations without
            // grossly over-allocating. Even if we over-estimate, the Vec only uses
            // actual bytes written after read_to_end returns.
            let capacity = if blob.raw_size.unwrap_or(0) > 0 {
                blob.raw_size.unwrap_or(0) as usize
            } else {
                bytes.len() * 4
            };
            let mut decoder = ZlibDecoder::new(&**bytes).take(MAX_BLOB_MESSAGE_SIZE);
            let mut decoded = Vec::with_capacity(capacity);
            decoder.read_to_end(&mut decoded)?;
            Ok(decoded)
        }
        Some(BlobData::Zstd(bytes)) => {
            let capacity = if blob.raw_size.unwrap_or(0) > 0 {
                blob.raw_size.unwrap_or(0) as usize
            } else {
                bytes.len() * 4
            };
            let mut decoded = Vec::with_capacity(capacity);
            zstd::stream::copy_decode(Cursor::new(&**bytes), &mut decoded)?;
            let size = decoded.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
            Ok(decoded)
        }
        None => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Parse already-decompressed bytes into a [`PrimitiveBlock`].
///
/// This variant accepts `&[u8]` for convenience but must copy the bytes
/// internally. If you already have a `Vec<u8>` or `Bytes`, prefer
/// [`parse_primitive_block_from_bytes_owned`] to avoid the copy.
pub fn parse_primitive_block_from_bytes(raw: &[u8]) -> Result<crate::PrimitiveBlock> {
    crate::PrimitiveBlock::new(Bytes::copy_from_slice(raw))
}

/// Zero-copy variant of [`parse_primitive_block_from_bytes`].
///
/// Accepts a `Bytes` value directly, avoiding the copy that the `&[u8]`
/// variant must perform. Use `Bytes::from(vec)` to wrap a `Vec<u8>` in
/// O(1).
///
/// Named `_owned` rather than `_from_bytes` to avoid confusion with the
/// existing `parse_primitive_block_from_bytes` which already has
/// `from_bytes` in its name. The `_owned` suffix signals that this
/// variant takes ownership of the buffer.
pub fn parse_primitive_block_from_bytes_owned(raw: &Bytes) -> Result<crate::PrimitiveBlock> {
    crate::PrimitiveBlock::new(raw.clone())
}

/// Decode raw Blob protobuf bytes into a [`HeaderBlock`].
///
/// This variant accepts `&[u8]` for convenience but must copy the bytes
/// internally. If you already have a `Vec<u8>` or `Bytes`, prefer
/// [`decode_blob_to_headerblock_from_bytes`] to avoid the copy.
pub fn decode_blob_to_headerblock(blob_bytes: &[u8]) -> Result<crate::HeaderBlock> {
    decode_blob_to_headerblock_from_bytes(&Bytes::copy_from_slice(blob_bytes))
}

/// Zero-copy variant of [`decode_blob_to_headerblock`].
///
/// Accepts a `Bytes` value directly, avoiding the copy that the `&[u8]`
/// variant must perform. Use `Bytes::from(vec)` to wrap a `Vec<u8>` in
/// O(1).
pub fn decode_blob_to_headerblock_from_bytes(blob_bytes: &Bytes) -> Result<crate::HeaderBlock> {
    let blob = WireBlob::parse(blob_bytes)?;
    let raw = decompress_blob(&blob, None)?;
    crate::HeaderBlock::parse_from_bytes(&raw)
}

/// Decompress a blob's data into `Bytes` without parsing it as a protobuf message.
///
/// This is the PrimitiveBlock hot path: decompress -> wrap as Bytes ->
/// pass to `PrimitiveBlock::new()` which does zero-copy wire-format parsing.
#[allow(clippy::cast_sign_loss)]
#[hotpath::measure]
pub(crate) fn decompress_blob(
    blob: &WireBlob,
    pool: Option<&Arc<DecompressPool>>,
) -> Result<Bytes> {
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size < MAX_BLOB_MESSAGE_SIZE {
                Ok(bytes.clone())
            } else {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            let capacity = if blob.raw_size.unwrap_or(0) > 0 {
                blob.raw_size.unwrap_or(0) as usize
            } else {
                bytes.len() * 4
            };
            let mut decoder = ZlibDecoder::new(&**bytes).take(MAX_BLOB_MESSAGE_SIZE);
            let mut decoded_bytes = pool_get(pool, capacity);
            decoder.read_to_end(&mut decoded_bytes)?;
            Ok(pool_wrap(decoded_bytes, pool))
        }
        Some(BlobData::Zstd(bytes)) => {
            let capacity = if blob.raw_size.unwrap_or(0) > 0 {
                blob.raw_size.unwrap_or(0) as usize
            } else {
                bytes.len() * 4
            };
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
        let pairs = [
            ("", BlobType::Unknown("")),
            ("abc", BlobType::Unknown("abc")),
            ("OSMHeader", BlobType::OsmHeader),
            ("OSMData", BlobType::OsmData),
        ];

        for (string, blob_type) in pairs {
            let ff_header = WireBlobHeader {
                blob_type: string.to_string(),
                datasize: 0,
                indexdata: None,
            };
            let ff_blob = WireBlob { data: None, raw_size: None };

            let blob = Blob::new(ff_header, ff_blob, None);
            assert_eq!(blob.get_type(), blob_type);
        }
    }
}
