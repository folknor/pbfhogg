//! Blob-level protobuf wire-format parsers: `BlobHeader` and `Blob` message
//! envelopes plus the typed classification of the blob payload. The rest of
//! blob handling (decompression, iterators, high-level APIs) lives in the
//! sibling `blob` and `decompress` modules.

use bytes::Bytes;

use crate::blob_meta::{INDEX_SIZE, INDEX_SIZE_V1, WireIndexdata};
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
    pub unrecognised_indexdata_len: Option<usize>,
    /// Blob-level index in the fixed-size reader carrier: a 42-byte v2/v3
    /// field, or a 26-byte v1 field zero-padded. Built only by
    /// [`crate::blob_meta::classify_wire_indexdata`], which refuses a field
    /// whose length disagrees with its version byte, so the padding can never
    /// be read as data. `None` also when the field occurred twice with data.
    pub indexdata: Option<[u8; INDEX_SIZE]>,
    /// Whether field 2 was present and non-empty on the wire, *regardless* of
    /// whether its length was one this crate recognises.
    ///
    /// Every read path collapses an unrecognised-length indexdata to
    /// `indexdata: None` and treats the blob as unindexed, which is the safe
    /// direction for a reader but erases the distinction a *verifier* needs:
    /// "this blob makes no claim" and "this blob makes a claim I cannot read"
    /// are different facts about the file, and `check --index` must be able to
    /// tell them apart. Only `parse_indexdata` parses can set this; a parse
    /// that skips field 2 leaves it `false`.
    pub indexdata_present: bool,
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
        let mut indexdata: Option<[u8; INDEX_SIZE]> = None;
        let mut indexdata_present = false;
        let mut unrecognised_indexdata_len = None;
        let mut tagdata: Option<Box<[u8]>> = None;
        let mut tagdata_seen = false;
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
                    // indexdata: bytes (len-delimited). The length and version
                    // checks live in `blob_meta`, next to the format.
                    //
                    // An empty occurrence contributes nothing. A second
                    // non-empty occurrence is two claims about one blob;
                    // protobuf would keep the last, a reader stopping early
                    // the first, and no choice is safe for both, so the blob
                    // gets none - sticky, so a third occurrence cannot
                    // restore one. `indexdata_present` stays set, which is
                    // how `check --index` sees the malformed claim.
                    let bytes = cursor.read_len_delimited()?;
                    if !bytes.is_empty() {
                        if indexdata_present {
                            indexdata = None;
                        } else {
                            match crate::blob_meta::classify_wire_indexdata(bytes) {
                                WireIndexdata::Carrier(carrier) => indexdata = Some(carrier),
                                WireIndexdata::UnrecognisedLength(len) => {
                                    unrecognised_indexdata_len = Some(len);
                                }
                                // A 26-byte field claiming the 42-byte layout
                                // (or the reverse): zero padding would read as
                                // a real bbox. Not an index; not a length
                                // problem either, so no length warning.
                                WireIndexdata::VersionLengthMismatch => {}
                            }
                        }
                        indexdata_present = true;
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
                    // tagdata: per-blob tag key index (len-delimited). Same
                    // duplicate rule as indexdata: a second non-empty
                    // occurrence leaves the blob with no tag index, sticky.
                    //
                    // `None` rather than an invalid sentinel on purpose:
                    // sort, cat --dedupe and apply-changes copy these bytes
                    // into the headers they write, so a sentinel would
                    // manufacture a malformed field in their output. The
                    // cost is that `check --index` cannot see a duplicate
                    // tagdata on its input (there is no tagdata presence
                    // flag); no reader acts on it either way.
                    let bytes = cursor.read_len_delimited()?;
                    if !bytes.is_empty() {
                        tagdata = if tagdata_seen {
                            None
                        } else {
                            Some(bytes.into())
                        };
                        tagdata_seen = true;
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

        // `type` is a required field. A header that omits it (or carries an
        // empty string) is not a valid frame descriptor, but it parses to
        // `Unknown("")` with `datasize == 0`, which every consumer skips as an
        // unrecognised blob type - so garbage would read as clean success.
        // Rejecting here covers every frame decoder at one site.
        if matches!(&blob_type, BlobKind::Unknown(s) if s.is_empty()) {
            return Err(new_wire_error("BlobHeader has no type field"));
        }

        Ok(WireBlobHeader {
            blob_type,
            datasize,
            indexdata,
            indexdata_present,
            unrecognised_indexdata_len,
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

/// A `Blob` carrying more than one of `raw` / `zlib_data` / `zstd_data`.
///
/// The three payload fields are mutually exclusive by construction: they are
/// three encodings of one payload, not three payloads. A blob carrying two of
/// them has no defined content, and the crate's two Blob parsers used to
/// disagree about which one won - `WireBlob::parse` was last-field-wins, while
/// `decompress_blob_raw` returned on the first `raw` field it saw, so one
/// adversarial blob decoded to two different blocks depending on which read
/// path reached it. Picking a winner would enshrine that ambiguity; rejecting
/// removes it, and costs a conforming writer nothing.
///
/// Repeating a non-repeated protobuf scalar is legal wire encoding with
/// last-one-wins semantics, so such a blob is unusual rather than provably
/// corrupt, and a file carrying one used to read successfully here. No known
/// writer produces one: libosmium's PBF output builder emits exactly one
/// payload field per blob, chosen by a switch on the compression mode. The
/// reference *reader* does tolerate the shape, and with a third policy again -
/// it returns immediately on `raw` and otherwise keeps the last compressed
/// field - which is the argument for rejecting rather than for copying it. The
/// message names the offending fields because a user hitting this has to know
/// what to look for in a file they did not write.
pub(crate) fn multiple_payload_fields() -> crate::error::Error {
    new_wire_error(
        "Blob carries more than one of the mutually exclusive payload fields \
         raw, zlib_data and zstd_data. The three are alternative encodings of \
         one payload, so a blob with two of them has no defined content and is \
         rejected rather than guessed at. Re-fetch or re-encode the file with a \
         conforming writer, and report the bug to whatever produced it.",
    )
}

/// Cross-check a `Blob.raw_size` against the payload length actually produced.
///
/// `raw_size` is the writer's declaration of the decompressed size. Every read
/// path here used it only as a `Vec::reserve` hint, so a lying value was
/// silently ignored; the reference implementation (libosmium) treats a
/// mismatch as corruption, and so do we. An absent `raw_size` is legal - our
/// own writer omits it on uncompressed blobs, as libosmium does - and is not
/// checked.
///
/// We are strictly stricter than libosmium in one direction. It sizes its
/// output buffer to `raw_size` and calls `uncompress`, so an undersized
/// `raw_size` fails with `Z_BUF_ERROR`, while an *oversized* one succeeds and
/// silently yields a buffer padded with trailing zeros to the declared length.
/// Both directions are rejected here.
pub(crate) fn check_raw_size(declared: Option<i32>, actual: usize) -> Result<()> {
    match declared {
        Some(d) if i64::from(d) != i64::try_from(actual).unwrap_or(i64::MAX) => {
            Err(new_wire_error(
                "Blob raw_size disagrees with the payload that came out of it. \
                 raw_size declares the decompressed length, so a mismatch means \
                 the payload is truncated, padded, or not what the writer \
                 intended, and the block is rejected rather than decoded from a \
                 length nothing agrees on. Re-fetch or re-encode the file.",
            ))
        }
        _ => Ok(()),
    }
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
                1 | 3 | 7 => {
                    // raw (1) / zlib_data (3) / zstd_data (7): the three
                    // mutually exclusive payload encodings.
                    let slice = cursor.read_len_delimited()?;
                    let offset = slice.as_ptr() as usize - input.as_ptr() as usize;
                    let payload = input.slice(offset..offset + slice.len());
                    if data.is_some() {
                        return Err(multiple_payload_fields());
                    }
                    data = Some(match field {
                        1 => BlobData::Raw(payload),
                        3 => BlobData::Zlib(payload),
                        _ => BlobData::Zstd(payload),
                    });
                }
                2 => {
                    // raw_size: int32 (varint)
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        raw_size = Some(cursor.read_varint()? as i32);
                    }
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
    ///
    /// Clamped to [`MAX_BLOB_MESSAGE_SIZE`]. `raw_size` is an unvalidated
    /// `int32` straight off the wire, so a corrupt or hostile blob can declare
    /// up to 2 GiB and drive the pre-decompression `Vec::reserve` into an
    /// outsized allocation ahead of every guard that runs during inflation. No
    /// legitimate blob decompresses past the cap, so a larger hint is a lie and
    /// clamping costs a truthful writer nothing.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn estimated_capacity(&self) -> usize {
        (self.raw_size.unwrap_or(0).max(0) as usize).min(MAX_BLOB_MESSAGE_SIZE as usize)
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

/// Parse a blob header and extract type, datasize, indexdata, whether an
/// indexdata field was present at all, and tagdata.
///
/// Used by the pread-from-workers pattern to classify and dispatch raw
/// blobs without decompression. The tagdata contains per-blob tag key metadata.
///
/// The fourth element is deliberately separate from the third: an indexdata of
/// an unrecognised length parses to `None` here, and every reader is right to
/// treat that as "no index". A verifier is not - see
/// [`WireBlobHeader::indexdata_present`].
#[allow(clippy::type_complexity)]
pub(crate) fn parse_blob_header_with_index(
    header_bytes: &[u8],
    diagnostics: &crate::host::Diagnostics,
) -> Result<(
    BlobKind,
    usize,
    Option<[u8; INDEX_SIZE]>,
    bool,
    Option<Box<[u8]>>,
)> {
    let header = WireBlobHeader::parse(header_bytes, true, true, false)?;
    if let Some(len) = header.unrecognised_indexdata_len {
        crate::host::diag_once!(
            diagnostics,
            Warning,
            "indexdata.unrecognised_length",
            "warning: BlobHeader.indexdata has an unrecognised length ({len} bytes; \
             expected {INDEX_SIZE_V1} or {INDEX_SIZE}) - blob index ignored, reads fall back \
             to full decode. Further occurrences are not reported."
        );
    }

    if header.datasize < 0 {
        return Err(new_blob_error(BlobError::InvalidDataSize {
            size: header.datasize,
        }));
    }
    // Reject an oversized declared datasize before any downstream site
    // preallocates or preads the compressed body. This is the shared funnel
    // for `HeaderWalker::next_header` and for `read::frame::read_frame_header`
    // (hence `read_raw_frame`, `read_blob_header_only` and the `cat --dedupe`
    // frame loop); enforcing here bounds the `data_size`
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
        header.indexdata_present,
        header.tagdata,
    ))
}

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests. See the note in `blob.rs`'s test module for the rationale and
// the crate-wide `unwrap_used = "deny"` exemption.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::{
        BlobError, BlobKind, MAX_BLOB_DATASIZE, WireBlobHeader, parse_blob_header_with_index,
    };
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
        let (_, data_size, _, _, _) = parse_blob_header_with_index(
            &header_with_datasize(max_legal),
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        assert_eq!(data_size as u64, max_legal);

        // Exactly at the cap: rejected.
        let err = parse_blob_header_with_index(
            &header_with_datasize(MAX_BLOB_DATASIZE),
            &crate::host::Diagnostics::discard(),
        )
        .unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
                assert_eq!(size, MAX_BLOB_DATASIZE);
            }
            other => panic!("expected DataSizeTooBig, got {other:?}"),
        }

        // Comfortably over the cap: also rejected, and the surfaced size is the
        // declared value verbatim (the guard is `>=`, not `==`).
        let over = MAX_BLOB_DATASIZE + 4096;
        let err = parse_blob_header_with_index(
            &header_with_datasize(over),
            &crate::host::Diagnostics::discard(),
        )
        .unwrap_err();
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
        let (blob_type, data_size, _, _, _) = parse_blob_header_with_index(
            &header_with_datasize(1234),
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        assert_eq!(blob_type, BlobKind::OsmData);
        assert_eq!(data_size, 1234);
    }

    /// A `BlobHeader` with no `type` field is rejected here, at the one site
    /// every frame decoder funnels through.
    ///
    /// `type` is required by the format, but an absent (or empty) one used to
    /// parse to `BlobKind::Unknown("")`, which every consumer skips as an
    /// unrecognised blob type - so a garbage frame read as clean success with
    /// no output. Rejecting inside `WireBlobHeader::parse` covers
    /// `BlobReader`, `read_raw_frame`, `read_blob_header_only`,
    /// `HeaderWalker::next_header` and the `cat --dedupe` frame loop at once,
    /// which is why the check lives here rather than at each decoder.
    ///
    /// An unknown but non-empty type stays acceptable: forward compatibility
    /// with future blob types is a property of the format, and conflating
    /// "a type this build does not know" with "no type at all" would break it.
    #[test]
    fn blob_header_without_a_type_field_is_rejected() {
        // Field 3 (datasize) = 100, and nothing else. A structurally valid
        // protobuf message that is not a valid frame descriptor.
        let no_type = vec![0x18, 100];
        let err = parse_blob_header_with_index(&no_type, &crate::host::Diagnostics::discard())
            .unwrap_err();
        assert!(
            matches!(err.into_kind(), ErrorKind::WireFormat { .. }),
            "a typeless BlobHeader must be a wire-format error"
        );

        // An empty type string is the same defect spelled differently.
        let empty_type = vec![0x0A, 0x00, 0x18, 100];
        parse_blob_header_with_index(&empty_type, &crate::host::Diagnostics::discard())
            .expect_err("an empty type string must be rejected too");

        // A completely empty header - what a zero-length frame decodes to.
        parse_blob_header_with_index(&[], &crate::host::Diagnostics::discard())
            .expect_err("an empty header must be rejected");

        // An unrecognised but non-empty type still parses, so the check has not
        // cost the format its forward compatibility.
        let future_type = vec![0x0A, 0x06, b'O', b'S', b'M', b'F', b'o', b'o', 0x18, 100];
        let (blob_type, data_size, _, _, _) =
            parse_blob_header_with_index(&future_type, &crate::host::Diagnostics::discard())
                .expect("an unknown but named blob type must still parse");
        assert_eq!(blob_type, BlobKind::Unknown("OSMFoo".to_string()));
        assert_eq!(data_size, 100);
    }

    /// `OSMData` header carrying the given field-2 and field-4 occurrences.
    fn header_with_fields(indexdata: &[Vec<u8>], tagdata: &[Vec<u8>]) -> Vec<u8> {
        let mut h = vec![0x0A, 7];
        h.extend_from_slice(b"OSMData");
        for field in indexdata {
            h.push(0x12);
            h.push(field.len() as u8);
            h.extend_from_slice(field);
        }
        for field in tagdata {
            h.push(0x22);
            h.push(field.len() as u8);
            h.extend_from_slice(field);
        }
        h.extend_from_slice(&[0x18, 100]);
        h
    }

    /// A node index of the given version in a field of `len` bytes, with an
    /// honest ID range; bytes past 26 are zero.
    fn node_index_field(version: u8, len: usize) -> Vec<u8> {
        let mut f = vec![0u8; len];
        f[0] = version;
        f[1] = 0; // node
        f[2..10].copy_from_slice(&1i64.to_le_bytes());
        f[10..18].copy_from_slice(&5i64.to_le_bytes());
        f[18..26].copy_from_slice(&5u64.to_le_bytes());
        f
    }

    fn parse(h: &[u8]) -> WireBlobHeader {
        WireBlobHeader::parse(h, true, true, false).unwrap()
    }

    /// A 26-byte field whose version byte says v3 used to be padded
    /// to 42 bytes and read back with the bbox `(0, 0, 0, 0)` - under v3 a
    /// real box at Null Island, so a spatial filter skipped the blob for any
    /// region away from (0, 0).
    #[test]
    fn a_short_field_claiming_the_long_layout_is_not_an_index() {
        for version in [0x02u8, 0x03] {
            let h = parse(&header_with_fields(&[node_index_field(version, 26)], &[]));
            assert!(
                h.indexdata.is_none(),
                "v{version} in 26 bytes must not be read"
            );
            assert!(
                h.indexdata_present,
                "but it is a claim check --index must see"
            );
            assert!(
                h.unrecognised_indexdata_len.is_none(),
                "26 is a known length"
            );
        }
        // The reverse mismatch: a v1 index padded out to 42 bytes on the wire.
        let h = parse(&header_with_fields(&[node_index_field(0x01, 42)], &[]));
        assert!(h.indexdata.is_none());
        assert!(h.indexdata_present);
    }

    /// Passing counterpart: honest v1 and v3 fields still read, and the v1
    /// carrier's zero padding is never taken as a bbox.
    #[test]
    fn honest_v1_and_v3_fields_still_read() {
        let h = parse(&header_with_fields(&[node_index_field(0x01, 26)], &[]));
        let index = crate::blob_meta::BlobIndex::deserialize(&h.indexdata.unwrap()).unwrap();
        assert_eq!((index.min_id, index.max_id, index.count), (1, 5, 5));
        assert!(
            index.bbox.is_none(),
            "v1 has no bbox; the padding is not one"
        );

        let v3 = crate::blob_meta::BlobIndex::deserialize(&node_index_field(0x03, 42))
            .unwrap()
            .serialize();
        let h = parse(&header_with_fields(&[v3.to_vec()], &[]));
        assert!(h.indexdata.is_some());
    }

    /// Two non-empty indexdata occurrences are two claims; the blob gets
    /// neither, and a third occurrence does not bring one back.
    #[test]
    fn duplicate_indexdata_is_no_index() {
        let a = node_index_field(0x01, 26);
        let h = parse(&header_with_fields(&[a.clone(), a.clone()], &[]));
        assert!(h.indexdata.is_none());
        assert!(h.indexdata_present);
        let h = parse(&header_with_fields(&[a.clone(), a.clone(), a.clone()], &[]));
        assert!(h.indexdata.is_none());

        // An empty occurrence contributes nothing, in either position.
        let h = parse(&header_with_fields(&[Vec::new(), a, Vec::new()], &[]));
        assert!(h.indexdata.is_some());
    }

    #[test]
    fn duplicate_tagdata_is_no_tagdata() {
        let t = vec![0x01u8, 0x00, 0x00];
        let h = parse(&header_with_fields(&[], &[t.clone(), t.clone(), t.clone()]));
        assert!(h.tagdata.is_none());
        let h = parse(&header_with_fields(&[], &[Vec::new(), t.clone()]));
        assert_eq!(h.tagdata.as_deref(), Some(t.as_slice()));
    }
}
