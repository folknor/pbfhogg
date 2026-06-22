//! Blob-level protobuf wire-format parsers: `BlobHeader` and `Blob` message
//! envelopes plus the typed classification of the blob payload. The rest of
//! blob handling (decompression, iterators, high-level APIs) lives in the
//! sibling `blob` and `decompress` modules.

use bytes::Bytes;

use crate::error::{BlobError, Result, new_blob_error, new_wire_error};

/// Blob type parsed from BlobHeader, avoiding per-blob String allocation.
///
/// OSM PBF files use exactly two blob types: `"OSMHeader"` and `"OSMData"`.
/// Unknown types are preserved as `String` for forward compatibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlobKind {
    OsmHeader,
    OsmData,
    Unknown(String),
}

/// Parsed BlobHeader from protobuf wire format.
///
/// Fields: type (string, field 1), indexdata (bytes, field 2), datasize (int32, field 3).
#[derive(Clone, Debug)]
pub(crate) struct WireBlobHeader {
    pub blob_type: BlobKind,
    pub datasize: i32,
    /// Blob-level index: 42 bytes (v2) or 26 bytes (v1, zero-padded), stored inline.
    pub indexdata: Option<[u8; crate::blob_meta::INDEX_SIZE]>,
    /// Per-blob tag key index (BlobHeader field 4). Variable-length.
    pub tagdata: Option<Box<[u8]>>,
}

impl WireBlobHeader {
    /// Parse a BlobHeader from raw protobuf bytes.
    ///
    /// When `parse_tagdata` is `false`, field 4 (tagdata) is skipped instead of
    /// allocated. Most read paths don't need tagdata - only tag-filtered reads
    /// and merge/sort passthrough use it.
    ///
    /// When `parse_indexdata` is `false`, field 2 (indexdata) is skipped instead
    /// of copied. Hot read paths that never call `Blob::index()` (e.g.
    /// `par_map_reduce`, unfiltered pipeline) can skip the 42-byte per-blob copy.
    #[hotpath::measure]
    pub fn parse(data: &[u8], parse_tagdata: bool, parse_indexdata: bool) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(data);
        let mut blob_type = BlobKind::Unknown(String::new());
        let mut datasize: i32 = 0;
        let mut indexdata: Option<[u8; crate::blob_meta::INDEX_SIZE]> = None;
        let mut tagdata: Option<Box<[u8]>> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match field {
                1 => {
                    // type: string (len-delimited)
                    let bytes = cursor.read_len_delimited()?;
                    blob_type = match bytes {
                        b"OSMHeader" => BlobKind::OsmHeader,
                        b"OSMData" => BlobKind::OsmData,
                        _ => BlobKind::Unknown(
                            String::from_utf8(bytes.to_vec())
                                .map_err(|_| new_wire_error("invalid UTF-8 in BlobHeader type"))?,
                        ),
                    };
                }
                2 if parse_indexdata => {
                    // indexdata: bytes (len-delimited) - accept v1 (26) or v2 (42) sizes
                    let bytes = cursor.read_len_delimited()?;
                    let len = bytes.len();
                    if len == crate::blob_meta::INDEX_SIZE || len == 26 {
                        let mut buf = [0u8; crate::blob_meta::INDEX_SIZE];
                        buf[..len].copy_from_slice(bytes);
                        indexdata = Some(buf);
                    }
                }
                3 => {
                    // datasize: int32 (varint)
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        datasize = cursor.read_varint()? as i32;
                    }
                }
                4 if parse_tagdata => {
                    // tagdata: per-blob tag key index (len-delimited)
                    let bytes = cursor.read_len_delimited()?;
                    if !bytes.is_empty() {
                        tagdata = Some(bytes.into());
                    }
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireBlobHeader {
            blob_type,
            datasize,
            indexdata,
            tagdata,
        })
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
                    {
                        raw_size = Some(cursor.read_varint()? as i32);
                    }
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

    /// Returns the declared `raw_size` as a `usize` capacity hint for buffer
    /// pre-allocation, or 0 if absent or negative.
    #[allow(clippy::cast_sign_loss)]
    pub fn estimated_capacity(&self) -> usize {
        self.raw_size.unwrap_or(0).max(0) as usize
    }
}

/// Maximum allowed `BlobHeader` size in bytes.
/// Compile-time constant per the PBF spec. Uses `const` (not `static`) so the value
/// is inlined at each use site with no memory address or indirection overhead.
pub const MAX_BLOB_HEADER_SIZE: u64 = 64 * 1024;

/// Maximum allowed uncompressed `Blob` content size in bytes.
/// Compile-time constant per the PBF spec. Uses `const` (not `static`) so the value
/// is inlined at each use site with no memory address or indirection overhead.
pub const MAX_BLOB_MESSAGE_SIZE: u64 = 32 * 1024 * 1024;

/// Parse a blob header and extract type, datasize, indexdata, and tagdata.
///
/// Used by the pread-from-workers pattern to classify and dispatch raw
/// blobs without decompression. The tagdata contains per-blob tag key metadata.
#[allow(clippy::type_complexity)]
pub(crate) fn parse_blob_header_with_index(
    header_bytes: &[u8],
) -> Result<(
    BlobKind,
    usize,
    Option<[u8; crate::blob_meta::INDEX_SIZE]>,
    Option<Box<[u8]>>,
)> {
    let header = WireBlobHeader::parse(header_bytes, true, true)?;
    if header.datasize < 0 {
        return Err(new_blob_error(BlobError::InvalidDataSize {
            size: header.datasize,
        }));
    }
    #[allow(clippy::cast_sign_loss)]
    Ok((
        header.blob_type,
        header.datasize as usize,
        header.indexdata,
        header.tagdata,
    ))
}
