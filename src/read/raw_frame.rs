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
use crate::blob::{BlobKind, MAX_BLOB_HEADER_SIZE, parse_blob_header_with_index};
use crate::blob_meta::BlobIndex;
use crate::file_reader::FileReader;

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
    /// Blob-level index from BlobHeader indexdata, if present.
    pub(crate) index: Option<BlobIndex>,
    /// Per-blob tag key data from BlobHeader field 4, if present.
    /// Populated by `read_raw_frame` for downstream callers; current
    /// readers (extract, cat, diff, etc.) don't access it directly,
    /// but it's part of the wire-correct frame snapshot.
    #[allow(dead_code)]
    pub(crate) tagdata: Option<Box<[u8]>>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[allow(dead_code)]
    pub(crate) file_offset: u64,
}

impl RawBlobFrame {
    /// The raw Blob protobuf message bytes (for selective decoding).
    pub(crate) fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }
}

/// Read the next raw blob frame. Returns `None` at EOF.
/// Updates `file_offset` to track position for copy_file_range.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
) -> Result<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_len = u32::from_be_bytes(len_buf) as usize;
    // Match BlobReader's MAX_BLOB_HEADER_SIZE guard (blob.rs:390).
    // Without this cap, an adversarial or corrupted length prefix
    // forces the `vec![0u8; header_len]` allocation below into a
    // multi-GB alloc that aborts the process.
    if header_len as u64 >= MAX_BLOB_HEADER_SIZE {
        return Err(
            crate::error::new_blob_error(crate::error::BlobError::HeaderTooBig {
                size: header_len as u64,
            })
            .into(),
        );
    }

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;

    // parse_blob_header_with_index rejects an oversized declared datasize
    // (>= MAX_BLOB_DATASIZE) before returning, so `data_size` here is already
    // bounded and the `vec![0u8; frame_len]` allocation below cannot be driven
    // to an outsized size by a hostile BlobHeader.datasize.
    let (blob_type, data_size, raw_index, tagdata) = parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

    let blob_offset = 4 + header_len;
    let frame_len = blob_offset + data_size;
    *file_offset += frame_len as u64;
    let mut frame_bytes = vec![0u8; frame_len];
    frame_bytes[..4].copy_from_slice(&len_buf);
    frame_bytes[4..blob_offset].copy_from_slice(&header_bytes);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_offset,
        index,
        tagdata,
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
) -> Result<Option<BlobHeaderInfo>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_len = u32::from_be_bytes(len_buf) as usize;
    // Match BlobReader's MAX_BLOB_HEADER_SIZE guard (blob.rs:390).
    // See `read_raw_frame` above for the same guard's rationale.
    if header_len as u64 >= MAX_BLOB_HEADER_SIZE {
        return Err(
            crate::error::new_blob_error(crate::error::BlobError::HeaderTooBig {
                size: header_len as u64,
            })
            .into(),
        );
    }

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;

    let (blob_type, data_size, raw_index, _tagdata) = parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

    *file_offset += (4 + header_len) as u64;

    Ok(Some(BlobHeaderInfo {
        blob_type,
        data_size,
        index,
    }))
}
