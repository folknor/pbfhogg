//! The one blob-frame decoder.
//!
//! Every PBF read path starts at the same envelope:
//! `[4-byte BE header_len][BlobHeader protobuf][Blob protobuf]`. That envelope
//! used to be hand-rolled at five sites (`BlobReader::read_blob_header`,
//! `read_raw_frame`, `read_blob_header_only`, `HeaderWalker::next_header`, and
//! the `cat --dedupe` frame loop), each with its own copy of the length-prefix
//! read, the EOF-tolerance policy and the two size guards. The copies drifted:
//! the `header_len == 0` rejection had to be added to four of them
//! independently and was never added to the fifth, and the fifth also went
//! without a `MAX_BLOB_HEADER_SIZE` guard, so four bytes of corrupt input drove
//! an unbounded `resize`.
//!
//! This module owns the policy so a future framing guard is a one-line change
//! here rather than a five-line change spread across the crate:
//!
//! * [`check_header_len`] - the two size guards, used by every decoder
//!   including the two that cannot share the I/O (BlobReader, which stamps byte
//!   offsets between the steps, and HeaderWalker, which preads).
//! * [`read_frame_prefix`] - the length prefix plus the EOF policy, for
//!   `Read`-based decoders.
//! * [`read_frame_header`] - prefix, header bytes, and header parse in one
//!   call, for the decoders that go on to materialise, skip, or index a
//!   payload.
//!
//! # EOF policy
//!
//! A frame boundary with 0-3 bytes remaining is clean EOF: a partial length
//! prefix cannot start a frame, and per
//! [`reference/truncation-handling.md`](../../reference/truncation-handling.md)
//! a clean cut at a frame boundary is tolerated. Every prior copy already
//! agreed on this outcome despite spelling it three different ways
//! (`BlobReader`'s two-part `read_exact`, `read_raw_frame`'s single
//! `UnexpectedEof` arm, `HeaderWalker`'s `probe_len < 4`) - the divergence was
//! cosmetic, not behavioural. Anything past the prefix is a committed read:
//! truncation there is a hard error, not EOF.

use std::io::Read;

use crate::blob::{BlobKind, MAX_BLOB_HEADER_SIZE, parse_blob_header_with_index};
use crate::error::{BlobError, Result, new_blob_error};

/// Apply both length-prefix guards and return the validated header length.
///
/// * A zero-length `BlobHeader` consumes 4 bytes and declares nothing: no
///   type, `datasize == 0`. Accepting it turns a run of zero bytes into a
///   stream of legal empty frames that every consumer skips as "unknown blob
///   type", so infinite garbage reads as a clean success with empty output.
/// * A length at or above [`MAX_BLOB_HEADER_SIZE`] is beyond the format's cap,
///   and honouring it means allocating up to 4 GiB on the strength of four
///   unvalidated bytes.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn check_header_len(header_len: u32) -> Result<usize> {
    let len = u64::from(header_len);
    if len == 0 {
        return Err(new_blob_error(BlobError::InvalidHeaderSize));
    }
    if len >= MAX_BLOB_HEADER_SIZE {
        return Err(new_blob_error(BlobError::HeaderTooBig { size: len }));
    }
    Ok(header_len as usize)
}

/// Read and validate the 4-byte length prefix of the next frame.
///
/// Returns `Ok(None)` at clean EOF (see the module's EOF policy), the
/// validated header length otherwise.
pub(crate) fn read_frame_prefix<R: Read>(reader: &mut R) -> Result<Option<usize>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(crate::error::new_error(crate::error::ErrorKind::Io(e))),
    }
    check_header_len(u32::from_be_bytes(len_buf)).map(Some)
}

/// A frame's header, parsed. The payload has not been touched.
pub(crate) struct FrameHeader {
    /// Length of the `BlobHeader` protobuf, i.e. the frame's prefix value.
    pub(crate) header_len: usize,
    pub(crate) blob_type: BlobKind,
    /// Declared payload length. Bounded by `MAX_BLOB_DATASIZE`.
    pub(crate) data_size: usize,
    pub(crate) raw_index: Option<[u8; crate::blob_meta::INDEX_SIZE]>,
    /// Whether the header carried a non-empty `indexdata` field at all, even
    /// one whose length this crate does not recognise and therefore did not
    /// keep in `raw_index`. Only `check --index` reads it; see
    /// `WireBlobHeader::indexdata_present` for why a verifier needs the
    /// distinction every reader is right to collapse.
    pub(crate) raw_index_present: bool,
    pub(crate) tagdata: Option<Box<[u8]>>,
}

impl FrameHeader {
    /// Bytes consumed by prefix + header, i.e. the offset of the payload
    /// within the frame.
    pub(crate) fn payload_offset(&self) -> usize {
        4 + self.header_len
    }
}

/// Read the next frame's length prefix and `BlobHeader`, leaving the reader
/// positioned at the payload. Returns `Ok(None)` at clean EOF.
///
/// `header_buf` is a caller-owned scratch buffer, resized to the header length
/// and left holding exactly the header bytes so callers that reframe a
/// passthrough blob can copy them verbatim.
pub(crate) fn read_frame_header<R: Read>(
    reader: &mut R,
    header_buf: &mut Vec<u8>,
) -> Result<Option<FrameHeader>> {
    let Some(header_len) = read_frame_prefix(reader)? else {
        return Ok(None);
    };
    header_buf.clear();
    header_buf.resize(header_len, 0);
    reader
        .read_exact(header_buf)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;

    // `parse_blob_header_with_index` rejects an oversized declared datasize
    // (>= MAX_BLOB_DATASIZE) before returning, so `data_size` is bounded here
    // and no caller can be driven into an outsized payload allocation by a
    // hostile `BlobHeader.datasize`.
    let (blob_type, data_size, raw_index, raw_index_present, tagdata) =
        parse_blob_header_with_index(header_buf)?;
    Ok(Some(FrameHeader {
        header_len,
        blob_type,
        data_size,
        raw_index,
        raw_index_present,
        tagdata,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{check_header_len, read_frame_header, read_frame_prefix};
    use crate::error::{BlobError, ErrorKind};

    /// The two guards every decoder shares, at their exact boundaries. The cap
    /// is strict (`>=`) so the largest accepted prefix is one below it.
    #[test]
    fn header_len_guards_bound_the_prefix() {
        assert!(matches!(
            check_header_len(0).unwrap_err().into_kind(),
            ErrorKind::Blob(BlobError::InvalidHeaderSize)
        ));
        assert_eq!(check_header_len(1).unwrap(), 1);

        #[allow(clippy::cast_possible_truncation)]
        let cap = crate::blob::MAX_BLOB_HEADER_SIZE as u32;
        assert_eq!(check_header_len(cap - 1).unwrap(), cap as usize - 1);
        match check_header_len(cap).unwrap_err().into_kind() {
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                assert_eq!(size, u64::from(cap));
            }
            other => panic!("expected HeaderTooBig, got {other:?}"),
        }
        // The worst case the guard exists for: four 0xFF bytes ask for a 4 GiB
        // allocation. It must be refused by the size guard, not by the
        // allocator.
        match check_header_len(u32::MAX).unwrap_err().into_kind() {
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                assert_eq!(size, u64::from(u32::MAX));
            }
            other => panic!("expected HeaderTooBig, got {other:?}"),
        }
    }

    /// 0-3 trailing bytes at a frame boundary are clean EOF; a validated
    /// prefix is returned otherwise.
    ///
    /// This pins the policy for the three decoders that share this helper's
    /// I/O. The other two spell it themselves and are pinned at their own call
    /// sites, because a unit test here cannot reach them:
    /// `tests/read_paths.rs::trailing_partial_length_prefix_returns_ok_none`
    /// and its `_4_bytes_is_committed_frame` sibling cover `BlobReader`, and
    /// `header_walker.rs`'s
    /// `the_walk_tolerates_a_short_tail_but_not_a_complete_prefix` covers the
    /// pread walk.
    #[test]
    fn prefix_read_tolerates_a_short_tail_only() {
        for tail in 0..4usize {
            let bytes = vec![0xFFu8; tail];
            let mut cursor = std::io::Cursor::new(bytes);
            assert_eq!(
                read_frame_prefix(&mut cursor).unwrap(),
                None,
                "a {tail}-byte tail must read as clean EOF"
            );
        }
        // A complete prefix is a committed frame, not a tail - a 4-byte input
        // declaring 0xFFFFFFFF is the unbounded-allocation case and must fail
        // rather than be excused as EOF.
        let mut cursor = std::io::Cursor::new(vec![0xFFu8; 4]);
        assert!(read_frame_prefix(&mut cursor).is_err());

        let mut cursor = std::io::Cursor::new(vec![0u8, 0, 0, 7]);
        assert_eq!(read_frame_prefix(&mut cursor).unwrap(), Some(7));
    }

    /// A prefix promising header bytes the stream does not have is a
    /// committed read: shape 3, a hard error, never EOF.
    #[test]
    fn truncated_header_bytes_are_an_error_not_eof() {
        // Declares 16 header bytes, supplies 3.
        let mut input = vec![0u8, 0, 0, 16];
        input.extend_from_slice(&[0x0A, 0x07, b'O']);
        let mut cursor = std::io::Cursor::new(input);
        let mut buf = Vec::new();
        match read_frame_header(&mut cursor, &mut buf) {
            Err(err) => assert!(
                matches!(err.into_kind(), ErrorKind::Io(_)),
                "a truncated BlobHeader must surface as an I/O error"
            ),
            Ok(_) => panic!("a truncated BlobHeader must not read as EOF or success"),
        }
    }
}
