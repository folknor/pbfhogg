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
/// Fields: type (string, field 1), indexdata (bytes, field 2), datasize
/// (int32, field 3), tagdata (bytes, field 4), and waymembers (bytes, field 5).
#[derive(Clone, Debug)]
pub(crate) struct WireBlobHeader {
    pub blob_type: BlobKind,
    pub datasize: i32,
    /// Blob-level index: 42 bytes (v2) or 26 bytes (v1, zero-padded), stored inline.
    pub indexdata: Option<[u8; crate::blob_meta::INDEX_SIZE]>,
    /// Per-blob tag key index (BlobHeader field 4). Variable-length.
    pub tagdata: Option<Box<[u8]>>,
    /// Per-blob way-member bitmap (BlobHeader field 5). Variable-length.
    pub waymembers: Option<Box<[u8]>>,
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
    pub fn parse(
        data: &[u8],
        parse_tagdata: bool,
        parse_indexdata: bool,
        parse_waymembers: bool,
    ) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(data);
        let mut blob_type = BlobKind::Unknown(String::new());
        let mut datasize: i32 = 0;
        let mut indexdata: Option<[u8; crate::blob_meta::INDEX_SIZE]> = None;
        let mut tagdata: Option<Box<[u8]>> = None;
        let mut waymembers: Option<Box<[u8]>> = None;

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
                5 if parse_waymembers => {
                    // waymembers: WayMembers-v1 bitmap incl. preamble (len-delimited)
                    let bytes = cursor.read_len_delimited()?;
                    if !bytes.is_empty() {
                        waymembers = Some(bytes.into());
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
            waymembers,
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

/// Maximum accepted declared `BlobHeader.datasize` in bytes: 32 MiB, equal to
/// [`MAX_BLOB_MESSAGE_SIZE`].
///
/// `datasize` is the on-wire length of the serialized (usually compressed)
/// `Blob` message that follows the header - the byte count a reader
/// preallocates and reads *before* any decompression happens.
/// `MAX_BLOB_MESSAGE_SIZE` caps the *decompressed* block content, but that
/// guard only fires after the compressed body has already been read into
/// memory, so a hostile or corrupt `datasize` can drive an arbitrarily large
/// pre-decompression allocation ahead of it. This cap rejects the declared
/// size before that allocation.
///
/// This value is not a written format limit. The OSM PBF spec caps the
/// *uncompressed* block at 32 MiB and the `BlobHeader` message at 64 KiB, but
/// defines no ceiling on the serialized-`Blob` `datasize`. The bound here is
/// the *de facto interoperability limit*: the reference reader (OSM-binary,
/// `FileBlockHead`) applies a single 32 MiB `MAX_BODY_SIZE` directly to
/// `BlobHeader.datasize` and rejects anything at or above it. We mirror that
/// reader exactly, so every file pbfhogg accepts the reference reader also
/// accepts. A consequence worth stating: a blob carrying its content
/// uncompressed in the `raw` field at exactly the 32 MiB content cap serializes
/// to a few bytes over 32 MiB (protobuf field tags, the length prefix, and the
/// `raw_size` hint), so it too is rejected by the reference reader -
/// interoperable files cannot carry such a blob.
///
/// The two constants denote different things - a decompressed-content cap and a
/// declared-datasize cap - but coincide at 32 MiB because the reference reader
/// enforces one number on both fields. Defined as an alias of
/// `MAX_BLOB_MESSAGE_SIZE` rather than a bare literal to keep that relationship
/// explicit.
pub const MAX_BLOB_DATASIZE: u64 = MAX_BLOB_MESSAGE_SIZE;

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
    let header = WireBlobHeader::parse(header_bytes, true, true, false)?;
    if header.datasize < 0 {
        return Err(new_blob_error(BlobError::InvalidDataSize {
            size: header.datasize,
        }));
    }
    // Reject an oversized declared datasize before any downstream site
    // preallocates or preads the compressed body. This is the shared funnel
    // for `read_raw_frame`, `read_blob_header_only`, and
    // `HeaderWalker::next_header`; enforcing here bounds the `data_size`
    // every one of those returns, so none can drive an outsized allocation
    // ahead of the 32 MiB post-decompression `MAX_BLOB_MESSAGE_SIZE` guard.
    // `BlobReader::read_blob_header` bypasses this funnel and carries the
    // equivalent check inline. datasize is known non-negative here (checked
    // above), so the u64 cast cannot lose sign.
    #[allow(clippy::cast_sign_loss)]
    let datasize_u64 = header.datasize as u64;
    if datasize_u64 >= MAX_BLOB_DATASIZE {
        return Err(new_blob_error(BlobError::DataSizeTooBig {
            size: datasize_u64,
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

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests. See the note in `blob.rs`'s test module for the rationale and
// the crate-wide `unwrap_used = "deny"` exemption.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::{BlobError, BlobKind, MAX_BLOB_DATASIZE, parse_blob_header_with_index};
    use crate::error::ErrorKind;

    /// Hand-encode a minimal BlobHeader protobuf (`OSMData` type + a chosen
    /// datasize) so a test can drive `parse_blob_header_with_index` with a
    /// declared datasize that no legitimate writer would produce. The payload
    /// itself is never written - the funnel guard rejects the declared size
    /// before any payload read, which is exactly the property under test.
    fn header_with_datasize(datasize: u64) -> Vec<u8> {
        let mut h = Vec::new();
        // Field 1 (type): tag 0x0A, len 7, "OSMData".
        h.push(0x0A);
        h.push(7);
        h.extend_from_slice(b"OSMData");
        // Field 3 (datasize): tag 0x18, LEB128 varint.
        h.push(0x18);
        let mut v = datasize;
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            h.push(byte);
            if v == 0 {
                break;
            }
        }
        h
    }

    /// The largest legal declared datasize (one below the cap) parses cleanly
    /// and reports its size; the cap value itself is rejected with the typed
    /// `DataSizeTooBig` error before any allocation. The guard is strict
    /// (`>=`), matching the sibling `MAX_BLOB_HEADER_SIZE` / `HeaderTooBig`
    /// contract.
    #[test]
    fn datasize_at_or_over_cap_is_rejected() {
        // One below the cap: accepted, size flows through unchanged.
        let max_legal = MAX_BLOB_DATASIZE - 1;
        let (_, data_size, _, _) =
            parse_blob_header_with_index(&header_with_datasize(max_legal)).unwrap();
        assert_eq!(data_size as u64, max_legal);

        // Exactly at the cap: rejected.
        let err =
            parse_blob_header_with_index(&header_with_datasize(MAX_BLOB_DATASIZE)).unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                assert_eq!(size, MAX_BLOB_DATASIZE);
            }
            other => panic!("expected DataSizeTooBig, got {other:?}"),
        }

        // Comfortably over the cap: also rejected, and the surfaced size is the
        // declared value verbatim (the guard is `>=`, not `==`).
        let over = MAX_BLOB_DATASIZE + 4096;
        let err = parse_blob_header_with_index(&header_with_datasize(over)).unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                assert_eq!(size, over);
            }
            other => panic!("expected DataSizeTooBig, got {other:?}"),
        }
    }

    /// A tiny, ordinary datasize is unaffected - the guard only trips at the
    /// cap, so well-formed blobs parse exactly as before.
    #[test]
    fn small_datasize_parses_normally() {
        let (blob_type, data_size, _, _) =
            parse_blob_header_with_index(&header_with_datasize(1234)).unwrap();
        assert_eq!(blob_type, BlobKind::OsmData);
        assert_eq!(data_size, 1234);
    }
}
