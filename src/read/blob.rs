//! Read and decode blobs

use super::block::{HeaderBlock, PrimitiveBlock};
use crate::error::{new_blob_error, new_error, new_protobuf_error, BlobError, ErrorKind, Result};
use crate::proto::fileformat;
use bytes::Bytes;
use protobuf::Message;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::ZlibDecoder;

/// Maximum allowed [`BlobHeader`] size in bytes.
pub static MAX_BLOB_HEADER_SIZE: u64 = 64 * 1024;

/// Maximum allowed uncompressed [`Blob`] content size in bytes.
pub static MAX_BLOB_MESSAGE_SIZE: u64 = 32 * 1024 * 1024;

/// The content type of a blob.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    pub const fn as_str(&self) -> &'a str {
        match self {
            Self::OsmHeader => "OSMHeader",
            Self::OsmData => "OSMData",
            Self::Unknown(x) => x,
        }
    }
}

/// The decoded content of a blob (analogous to [`BlobType`]).
#[derive(Clone, Debug)]
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
    header: fileformat::BlobHeader,
    blob: fileformat::Blob,
    offset: Option<ByteOffset>,
}

impl Blob {
    fn new(
        header: fileformat::BlobHeader,
        blob: fileformat::Blob,
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
    pub fn get_type(&self) -> BlobType<'_> {
        match self.header.type_() {
            x if x == BlobType::OsmHeader.as_str() => BlobType::OsmHeader,
            x if x == BlobType::OsmData.as_str() => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the byte offset of the blob from the start of its source stream.
    /// This might be [`None`] if the source stream does not implement [`Seek`].
    pub fn offset(&self) -> Option<ByteOffset> {
        self.offset
    }

    /// Tries to decode the blob to a [`HeaderBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_headerblock(&self) -> Result<HeaderBlock> {
        decode_blob(&self.blob).map(HeaderBlock::new)
    }

    /// Tries to decode the blob to a [`PrimitiveBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_primitiveblock(&self) -> Result<PrimitiveBlock> {
        decode_blob(&self.blob).map(PrimitiveBlock::new)
    }
}

/// A blob header.
///
/// Just contains information about the size and type of the following [`Blob`].
#[derive(Clone, Debug)]
pub struct BlobHeader {
    header: fileformat::BlobHeader,
}

impl BlobHeader {
    fn new(header: fileformat::BlobHeader) -> Self {
        BlobHeader { header }
    }

    /// Returns the type of the following blob.
    pub fn blob_type(&self) -> BlobType<'_> {
        match self.header.type_() {
            "OSMHeader" => BlobType::OsmHeader,
            "OSMData" => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the size of the following blob in bytes.
    pub fn get_blob_size(&self) -> i32 {
        self.header.datasize()
    }
}

/// A reader for PBF files that allows iterating over [`Blob`]s.
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

    fn handle_decode_error<T>(
        &mut self,
        error: protobuf::Error,
        msg: &'static str,
    ) -> Option<Result<T>> {
        self.offset = None;
        self.last_blob_ok = false;
        Some(Err(new_protobuf_error(error, msg)))
    }

    #[allow(clippy::cast_possible_truncation)]
    fn read_blob_header(&mut self) -> Option<Result<fileformat::BlobHeader>> {
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
                Err(_) => {
                    self.offset = None;
                    self.last_blob_ok = false;
                    return Some(Err(new_blob_error(BlobError::InvalidHeaderSize)));
                }
            }
            match self.reader.read_exact(&mut buf[1..]) {
                Ok(()) => {
                    self.offset = self.offset.map(|x| ByteOffset(x.0 + 4));
                    u64::from(u32::from_be_bytes(buf))
                }
                Err(_) => {
                    // Had 1-3 bytes then EOF: truncated header length.
                    self.offset = None;
                    self.last_blob_ok = false;
                    return Some(Err(new_blob_error(BlobError::InvalidHeaderSize)));
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
            return self.handle_decode_error(e.into(), "could not read from reader");
        }

        let header = match fileformat::BlobHeader::parse_from_tokio_bytes(&Bytes::from(header_data))
        {
            Ok(header) => header,
            Err(e) => return self.handle_decode_error(e, "could not parse read header data"),
        };

        self.offset = self.offset.map(|x| ByteOffset(x.0 + header_size));

        Some(Ok(header))
    }
}

impl BlobReader<BufReader<File>> {
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
        let f = File::open(path)?;
        let reader = BufReader::new(f);

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

        let mut reader = self.reader.by_ref().take(header.datasize() as u64);
        let mut blob_data = Vec::with_capacity(header.datasize() as usize);
        if let Err(e) = reader.read_to_end(&mut blob_data) {
            return self.handle_decode_error(e.into(), "could not read from blob");
        }

        let blob = match fileformat::Blob::parse_from_tokio_bytes(&Bytes::from(blob_data)) {
            Ok(blob) => blob,
            Err(e) => return self.handle_decode_error(e, "blob content"),
        };

        self.offset = self
            .offset
            .map(|x| ByteOffset(x.0 + header.datasize() as u64));

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
    /// let mut reader = BlobReader::from_path("tests/test.osm.pbf")?;
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
    /// let mut reader = BlobReader::from_path("tests/test.osm.pbf")?;
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
        if let Err(err) = self.seek_raw(SeekFrom::Current(header.datasize() as i64)) {
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
        let buf_reader = BufReader::new(f);
        Self::new_seekable(buf_reader)
    }
}

/// Parse raw blob frame bytes into a BlobHeader type string and Blob payload bytes.
///
/// Input: the raw bytes after the 4-byte header length prefix (i.e. just the BlobHeader bytes).
/// Returns: `(blob_type, data_size)`.
pub fn parse_blob_header(header_bytes: &[u8]) -> Result<(String, usize)> {
    let header =
        fileformat::BlobHeader::parse_from_tokio_bytes(&Bytes::from(header_bytes.to_vec()))
            .map_err(|e| new_protobuf_error(e, "parse blob header"))?;
    #[allow(clippy::cast_sign_loss)]
    Ok((header.type_().to_string(), header.datasize() as usize))
}

/// Decode raw Blob protobuf bytes into a [`PrimitiveBlock`].
pub fn decode_blob_to_primitiveblock(blob_bytes: &[u8]) -> Result<crate::PrimitiveBlock> {
    let blob = fileformat::Blob::parse_from_tokio_bytes(&Bytes::from(blob_bytes.to_vec()))
        .map_err(|e| new_protobuf_error(e, "parse blob"))?;
    decode_blob::<crate::proto::osmformat::PrimitiveBlock>(&blob).map(crate::PrimitiveBlock::new)
}

/// Decompress a blob's data without parsing it into a typed message.
/// Returns the raw decompressed protobuf bytes.
#[allow(clippy::cast_sign_loss)]
pub fn decompress_blob_data(blob_bytes: &[u8]) -> Result<Vec<u8>> {
    let blob = fileformat::Blob::parse_from_tokio_bytes(&Bytes::from(blob_bytes.to_vec()))
        .map_err(|e| new_protobuf_error(e, "parse blob"))?;
    match &blob.data {
        Some(fileformat::blob::Data::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size < MAX_BLOB_MESSAGE_SIZE {
                Ok(bytes.to_vec())
            } else {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            }
        }
        Some(fileformat::blob::Data::ZlibData(bytes)) => {
            let capacity = if blob.raw_size() > 0 {
                blob.raw_size() as usize
            } else {
                bytes.len()
            };
            let mut decoder = ZlibDecoder::new(&**bytes).take(MAX_BLOB_MESSAGE_SIZE);
            let mut decoded = Vec::with_capacity(capacity);
            decoder.read_to_end(&mut decoded)?;
            Ok(decoded)
        }
        _ => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Parse already-decompressed bytes into a [`PrimitiveBlock`].
pub fn parse_primitive_block_from_bytes(raw: &[u8]) -> Result<crate::PrimitiveBlock> {
    crate::proto::osmformat::PrimitiveBlock::parse_from_tokio_bytes(&Bytes::from(raw.to_vec()))
        .map(crate::PrimitiveBlock::new)
        .map_err(|e| new_protobuf_error(e, "parse primitive block"))
}

/// Decode raw Blob protobuf bytes into a [`HeaderBlock`].
pub fn decode_blob_to_headerblock(blob_bytes: &[u8]) -> Result<crate::HeaderBlock> {
    let blob = fileformat::Blob::parse_from_tokio_bytes(&Bytes::from(blob_bytes.to_vec()))
        .map_err(|e| new_protobuf_error(e, "parse blob"))?;
    decode_blob::<crate::proto::osmformat::HeaderBlock>(&blob).map(crate::HeaderBlock::new)
}

#[allow(clippy::cast_sign_loss)]
pub fn decode_blob<T: Message>(blob: &fileformat::Blob) -> Result<T> {
    match &blob.data {
        Some(fileformat::blob::Data::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size < MAX_BLOB_MESSAGE_SIZE {
                T::parse_from_tokio_bytes(bytes).map_err(|e| new_protobuf_error(e, "raw blob data"))
            } else {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            }
        }
        Some(fileformat::blob::Data::ZlibData(bytes)) => {
            let mut decoder = ZlibDecoder::new(&**bytes).take(MAX_BLOB_MESSAGE_SIZE);
            let capacity = if blob.raw_size() > 0 {
                blob.raw_size() as usize
            } else {
                bytes.len()
            };
            let mut decoded_bytes = Vec::with_capacity(capacity);
            decoder.read_to_end(&mut decoded_bytes)?;

            T::parse_from_tokio_bytes(&Bytes::from(decoded_bytes))
                .map_err(|e| new_protobuf_error(e, "blob zlib data"))
        }
        _ => Err(new_blob_error(BlobError::Empty)),
    }
}

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
            let mut ff_header = fileformat::BlobHeader::new();
            ff_header.set_type(protobuf::Chars::from(string));
            let ff_blob = fileformat::Blob::new();

            let blob = Blob::new(ff_header, ff_blob, None);
            assert_eq!(blob.get_type(), blob_type);
        }
    }
}
