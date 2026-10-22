//! Raw blob frame I/O for passthrough and selective decode.
//!
//! These primitives expose blob frames as opaque byte buffers so callers can
//! pass them through unchanged (e.g. `copy_file_range` zero-copy splicing,
//! cat passthrough, getid raw passthrough) or selectively decode only the
//! blobs that match a filter. The frame layout is the standard PBF wire
//! envelope: `[4-byte BE header_len][BlobHeader protobuf][Blob protobuf]`.
//!
//! Used across many command implementations and by the read-side pipeline,
//! which is why these live in `read/` rather than under `commands/`.

use std::io::Read;

use crate::BoxResult as Result;
use crate::blob::BlobKind;
use crate::blob_meta::BlobIndex;
use crate::file_reader::FileReader;
use crate::frame::read_frame_header;

/// A raw blob frame for passthrough or selective decode.
///
/// The Blob protobuf bytes are a suffix of `frame_bytes` starting at
/// `blob_offset`, eliminating a separate allocation per blob.
pub(crate) struct RawBlobFrame {
    /// Complete framed bytes: `[4-byte header_len][BlobHeader][Blob]`.
    pub(crate) frame_bytes: Vec<u8>,
    pub(crate) blob_type: BlobKind,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    pub(crate) blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present *and* readable.
    pub(crate) index: Option<BlobIndex>,
    /// Whether the header carried a non-empty indexdata field at all.
    ///
    /// `index` collapses "absent" and "present but unreadable" into `None`,
    /// which is correct for every reader - an index it cannot read is an index
    /// it must not act on - and wrong for `check --index`, whose entire job is
    /// to notice that the file claims something it cannot back up. Malformed
    /// indexdata was passing verification silently until this field existed.
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) index_present: bool,
    /// Per-blob tag key data from BlobHeader field 4, if present. Retained
    /// so `check --index` can compare the stored claim with decoded tags.
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    // Duplicate non-empty tagdata is collapsed to None by WireBlobHeader
    // before this layer sees it. Adding presence here alone cannot recover
    // that fact. Verification needs a separate presence/duplicate signal from
    // read/blob_wire.rs through parse_blob_header_with_index; keep it separate
    // from copied bytes so passthrough does not emit an invalid sentinel.
    pub(crate) tagdata: Option<Box<[u8]>>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) file_offset: u64,
}

impl RawBlobFrame {
    /// The raw Blob protobuf message bytes (for selective decoding).
    pub(crate) fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }

    /// The raw `BlobHeader` protobuf message bytes, sitting between the
    /// 4-byte big-endian length prefix and the Blob payload. Callers that
    /// reframe a passthrough blob use this to preserve every original header
    /// field verbatim rather than reconstructing fields from parsed values.
    pub(crate) fn header_bytes(&self) -> &[u8] {
        &self.frame_bytes[4..self.blob_offset]
    }
}

/// Read the next raw blob frame. Returns `None` at EOF.
/// Updates `file_offset` to track position for copy_file_range.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
    diagnostics: &crate::host::Diagnostics,
) -> Result<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    // Prefix, EOF policy, both size guards and the header parse all live in
    // `read::frame` - see that module for why they are not spelled out here.
    let mut header_bytes = Vec::new();
    let Some(header) = read_frame_header(reader, &mut header_bytes, diagnostics)? else {
        return Ok(None);
    };
    let index = header
        .raw_index
        .and_then(|ref data| BlobIndex::deserialize(data));

    let blob_offset = header.payload_offset();
    let frame_len = blob_offset + header.data_size;
    let mut frame_bytes = vec![0u8; frame_len];
    frame_bytes[..4].copy_from_slice(&u32::to_be_bytes(header.header_len as u32));
    frame_bytes[4..blob_offset].copy_from_slice(&header_bytes);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;
    *file_offset += frame_len as u64;

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type: header.blob_type,
        blob_offset,
        index,
        index_present: header.raw_index_present,
        tagdata: header.tagdata,
        file_offset: frame_start,
    }))
}

/// Parsed blob header without the blob data payload.
///
/// Used by lightweight header-only probes (`check_sorted_and_indexed`,
/// indexed-check short-circuit). For random-access pread-based header
/// walks use `crate::read::header_walker::HeaderWalker` instead - it
/// skips the `BufReader` buffer-passthrough amplification on cold
/// caches.
pub(crate) struct BlobHeaderInfo {
    pub blob_type: BlobKind,
    pub data_size: usize,
    pub index: Option<BlobIndex>,
    /// Whether the header carried a `tagdata` (field 4) tag key index.
    pub has_tagdata: bool,
}

/// Read the next blob header without reading the blob data payload.
///
/// Returns `None` at EOF. Updates `file_offset` past the header only.
/// The caller must either:
/// - Read `data_size` bytes (e.g. for OsmHeader blobs)
/// - Skip `data_size` bytes via `reader.skip()` (for index-only mode)
///
/// and then advance `*file_offset += data_size as u64`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn read_blob_header_only(
    reader: &mut FileReader,
    file_offset: &mut u64,
    diagnostics: &crate::host::Diagnostics,
) -> Result<Option<BlobHeaderInfo>> {
    let mut header_bytes = Vec::new();
    let Some(header) = read_frame_header(reader, &mut header_bytes, diagnostics)? else {
        return Ok(None);
    };
    let index = header
        .raw_index
        .and_then(|ref data| BlobIndex::deserialize(data));

    *file_offset += header.payload_offset() as u64;

    Ok(Some(BlobHeaderInfo {
        blob_type: header.blob_type,
        data_size: header.data_size,
        index,
        has_tagdata: header.tagdata.is_some(),
    }))
}

#[cfg(test)]
mod tests {
    use super::read_raw_frame;
    use std::io::Cursor;

    #[test]
    fn truncated_payload_does_not_advance_file_offset() {
        // BlobHeader: type = "OSMData", datasize = 1. The frame ends before
        // the declared Blob payload, so read_raw_frame must leave the logical
        // offset at the beginning of the incomplete frame.
        let mut input = Cursor::new(vec![
            0, 0, 0, 11, 0x0A, 7, b'O', b'S', b'M', b'D', b'a', b't', b'a', 0x18, 1,
        ]);
        let mut file_offset = 42;
        let result = read_raw_frame(
            &mut input,
            &mut file_offset,
            &crate::host::Diagnostics::discard(),
        );

        assert!(result.is_err(), "the missing payload must be an error");
        assert_eq!(file_offset, 42);
    }
}
