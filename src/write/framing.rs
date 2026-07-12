//! Blob framing and compression helpers. Produces the PBF on-wire format
//! `[4-byte BE header_len][BlobHeader bytes][Blob bytes]` from an uncompressed
//! block byte payload.
//!
//! The `FrameScratch` struct holds the reusable per-thread scratch buffers and
//! the lazy-initialized zlib/zstd compressors. `PIPELINE_SCRATCH` is the
//! thread-local instance used by the rayon-parallel pipelined path;
//! `PbfWriter` holds its own for the sync path.

use std::cell::RefCell;
use std::io;

use flate2::Compress;
use flate2::Compression as FlateCompression;
use flate2::FlushCompress;
use flate2::Status;
use protohoggr::{encode_bytes_field, encode_int32_field};

use crate::read::blob_wire::MAX_BLOB_HEADER_SIZE;
use crate::write::metrics::WRITER_METRICS;
use std::sync::atomic::Ordering::Relaxed;

use super::compression::Compression;
use super::pipeline::{FramedBlobParts, elapsed_ns_u64};

/// Per-thread reusable blob framing scratch: header/body/compression buffers
/// plus lazy-initialized zlib/zstd compressors. Reused across blobs to keep
/// steady-state allocation flat.
pub(super) struct FrameScratch {
    /// Blob protobuf body (raw/compressed data fields).
    pub(super) blob_buf: Vec<u8>,
    /// BlobHeader protobuf.
    pub(super) header_buf: Vec<u8>,
    /// Intermediate compression output (zlib/zstd only).
    compress_buf: Vec<u8>,
    /// Reusable zlib compressor (lazy-initialized on first zlib blob).
    zlib_compressor: Option<(u32, Compress)>,
    /// Reusable zstd compressor (lazy-initialized on first zstd blob).
    zstd_compressor: Option<(i32, zstd::bulk::Compressor<'static>)>,
}

impl FrameScratch {
    pub(super) const fn new() -> Self {
        Self {
            blob_buf: Vec::new(),
            header_buf: Vec::new(),
            compress_buf: Vec::new(),
            zlib_compressor: None,
            zstd_compressor: None,
        }
    }
}

thread_local! {
    /// Per-rayon-thread scratch buffers for pipelined blob framing.
    pub(super) static PIPELINE_SCRATCH: RefCell<FrameScratch> = const { RefCell::new(FrameScratch::new()) };
}

/// Compress and frame a blob into the complete PBF wire format:
/// `[4-byte BE header_len][BlobHeader bytes][Blob bytes]`.
///
/// Allocates fresh buffers - use only for one-off calls (e.g. header framing).
/// For hot-path data blocks, use `frame_blob_into` (pipelined) or
/// `write_framed_blob` (sync) which reuse scratch buffers.
pub(crate) fn frame_blob(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut scratch = FrameScratch::new();
    Ok(frame_blob_into(
        blob_type,
        uncompressed,
        compression,
        indexdata,
        None,
        None,
        &mut scratch,
    )?
    .into_vec())
}

/// Compress and frame a blob using per-thread scratch buffers.
///
/// Intended for rayon `par_iter` workers that produce fully framed blobs
/// in the parallel phase, so the sequential write phase can use
/// `write_raw_owned` with bounded backpressure (no second rayon dispatch).
pub(crate) fn frame_blob_pipelined(
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
) -> io::Result<FramedBlobParts> {
    PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
        frame_blob_into(
            "OSMData",
            uncompressed,
            compression,
            indexdata,
            tagdata,
            way_members,
            scratch,
        )
    })
}

/// Compress and frame a blob using reusable scratch buffers.
///
/// Returns an owned `Vec<u8>` suitable for sending through a pipeline channel.
/// The scratch buffers are cleared and reused - after warmup, only the returned
/// `out` Vec is allocated per call.
///
/// NOTE: Buffer recycling pool was attempted to eliminate this per-call
/// allocation via `Arc<Mutex<Vec<Vec<u8>>>>` shared between rayon workers and
/// the writer thread. Regressed throughput by +12% (Germany, Compression::None)
/// due to Mutex contention. The allocator's own thread-local caching handles
/// the cross-thread alloc/free pattern better than an explicit pool.
/// Measured at commit 2bf438c.
#[hotpath::measure]
pub(super) fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    scratch: &mut FrameScratch,
) -> io::Result<FramedBlobParts> {
    let t_compress = std::time::Instant::now();
    encode_blob_body(uncompressed, compression, scratch)?;
    WRITER_METRICS
        .compress_ns
        .fetch_add(elapsed_ns_u64(t_compress), Relaxed);
    let t_frame = std::time::Instant::now();

    let datasize = i32::try_from(scratch.blob_buf.len()).map_err(|_| {
        io::Error::other(format!(
            "blob datasize overflow: {} bytes",
            scratch.blob_buf.len()
        ))
    })?;
    encode_blob_header_into(
        blob_type,
        datasize,
        indexdata,
        tagdata,
        way_members,
        &mut scratch.header_buf,
    )?;

    let header_len = u32::try_from(scratch.header_buf.len()).map_err(|_| {
        io::Error::other(format!(
            "header too large: {} bytes",
            scratch.header_buf.len()
        ))
    })?;
    let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
    WRITER_METRICS
        .frame_ns
        .fetch_add(elapsed_ns_u64(t_frame), Relaxed);
    WRITER_METRICS
        .bytes_framed
        .fetch_add(total_len as u64, Relaxed);

    // Clone (not swap) so `scratch.header_buf` / `scratch.blob_buf` retain
    // their high-water capacity across blobs. Swap would leave the scratch
    // with empty Vecs, forcing both buffers to re-grow from zero on every
    // subsequent blob - breaking the reuse invariant documented above.
    Ok(FramedBlobParts {
        prefix: header_len.to_be_bytes(),
        header: scratch.header_buf.clone(),
        blob: scratch.blob_buf.clone(),
    })
}

/// Encode the Blob protobuf body (optionally compressed) into scratch buffers.
///
/// Uses `compress_buf` as an intermediate for zlib/zstd compression output.
/// Both zlib and zstd compressors are lazy-initialized and reused to avoid
/// ~312 KB (zlib) or ~512 KB (zstd) of compressor state allocation per blob.
pub(super) fn encode_blob_body(
    uncompressed: &[u8],
    compression: &Compression,
    scratch: &mut FrameScratch,
) -> io::Result<()> {
    scratch.blob_buf.clear();
    match compression {
        Compression::None => {
            // Blob field 1: raw (bytes, len-delimited)
            encode_bytes_field(&mut scratch.blob_buf, 1, uncompressed);
        }
        Compression::Zlib(level) => {
            compress_zlib(uncompressed, *level, scratch)?;
        }
        Compression::Zstd(level) => {
            match &scratch.zstd_compressor {
                Some((cached_level, _)) if *cached_level == *level => {}
                _ => {
                    scratch.zstd_compressor = Some((
                        *level,
                        zstd::bulk::Compressor::new(*level).map_err(io::Error::other)?,
                    ));
                }
            }
            let (_, compressor) = scratch.zstd_compressor.as_mut().expect("just initialized");
            let bound = zstd::zstd_safe::compress_bound(uncompressed.len());
            scratch.compress_buf.clear();
            scratch.compress_buf.reserve(bound);
            compressor
                .compress_to_buffer(uncompressed, &mut scratch.compress_buf)
                .map_err(io::Error::other)?;
            let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
                io::Error::other(format!(
                    "blob raw_size overflow: {} bytes",
                    uncompressed.len()
                ))
            })?;
            // Blob field 2: raw_size (int32, varint)
            encode_int32_field(&mut scratch.blob_buf, 2, raw_size);
            // Blob field 7: zstd_data (bytes, len-delimited)
            encode_bytes_field(&mut scratch.blob_buf, 7, &scratch.compress_buf);
        }
    }
    Ok(())
}

/// Zlib compression via reusable `flate2::Compress` with `reset()`.
fn compress_zlib(uncompressed: &[u8], level: u32, scratch: &mut FrameScratch) -> io::Result<()> {
    let needs_new = match &scratch.zlib_compressor {
        Some((cached_level, _)) => *cached_level != level,
        None => true,
    };
    if needs_new {
        scratch.zlib_compressor = Some((level, Compress::new(FlateCompression::new(level), true)));
    }
    let (_, compressor) = scratch.zlib_compressor.as_mut().expect("just initialized");
    scratch.compress_buf.clear();
    // Zlib worst-case bound: input + ~0.1% + header/trailer.
    scratch
        .compress_buf
        .reserve(uncompressed.len() + (uncompressed.len() >> 10) + 64);
    let status = compressor
        .compress_vec(
            uncompressed,
            &mut scratch.compress_buf,
            FlushCompress::Finish,
        )
        .map_err(|e| io::Error::other(format!("zlib compress error: {e}")))?;
    if !matches!(status, Status::StreamEnd) {
        return Err(io::Error::other(
            "zlib compress did not complete in one call",
        ));
    }
    compressor.reset();
    let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
        io::Error::other(format!(
            "blob raw_size overflow: {} bytes",
            uncompressed.len()
        ))
    })?;
    encode_int32_field(&mut scratch.blob_buf, 2, raw_size);
    encode_bytes_field(&mut scratch.blob_buf, 3, &scratch.compress_buf);
    Ok(())
}

/// Encode a BlobHeader into `buf` (cleared and reused).
///
/// BlobHeader fields: type (string, field 1), indexdata (bytes, field 2),
/// datasize (int32, field 3), tagdata (bytes, field 4), waymembers (bytes, field 5).
///
/// **libosmium compat note:** libosmium 2.23.0 has a signed-char sign-extension
/// bug in `get_size_in_network_byte_order` that rejects any BlobHeader > 127
/// bytes. With indexdata (42 bytes) + tagdata (variable), rewritten blobs
/// routinely exceed this. Filed as <https://github.com/osmcode/libosmium/issues/405>.
/// Not a problem for pbfhogg's own reader or the production pipeline - only
/// affects users who open pbfhogg-generated PBFs with osmium-tool.
pub(crate) fn encode_blob_header_into(
    blob_type: &str,
    datasize: i32,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    buf.clear();
    // Field 1: type (string = len-delimited bytes)
    encode_bytes_field(buf, 1, blob_type.as_bytes());
    // Field 2: indexdata (optional bytes)
    if let Some(data) = indexdata {
        encode_bytes_field(buf, 2, data);
    }
    // Field 3: datasize (int32 varint)
    encode_int32_field(buf, 3, datasize);
    // Field 4: tagdata (optional bytes - per-blob tag key index)
    if let Some(data) = tagdata {
        encode_bytes_field(buf, 4, data);
    }
    // Field 5: WayMembers-v1 payload (optional bytes).
    if let Some(data) = way_members {
        encode_bytes_field(buf, 5, data);
    }
    if buf.len() as u64 >= MAX_BLOB_HEADER_SIZE {
        return Err(io::Error::other(format!(
            "BlobHeader for {blob_type} is {} bytes, must be smaller than {MAX_BLOB_HEADER_SIZE}",
            buf.len()
        )));
    }
    Ok(())
}

/// Re-emit a `BlobHeader` protobuf message, copying every field through
/// verbatim except the field numbers listed in `strip_fields`, which are
/// dropped.
///
/// Untargeted fields are copied at the byte level - tag and value together -
/// straight from the original message, so nothing pbfhogg does not model is
/// disturbed. In particular this preserves the original `indexdata` bytes
/// exactly: a 26-byte v1 index stays v1 rather than being silently upgraded
/// to the 42-byte v2 layout that a deserialize-then-`BlobIndex::serialize()`
/// round-trip would produce, and an index that fails to deserialize is kept
/// instead of dropped. It equally preserves `tagdata` (field 4), the
/// `pbfhogg.WayMembers-v1` payload (field 5), and any unknown/extension
/// fields. Only the targeted field number(s) are removed.
///
/// Used by `degrade`'s header-only passthrough so a strip changes exactly
/// the field it targets (`indexdata` for `--strip-indexdata`, `tagdata` for
/// `--strip-tagdata`) and leaves every other header field byte-for-byte
/// identical. The blob payload it accompanies is unchanged, so the preserved
/// `datasize` (field 3) stays consistent.
pub(crate) fn strip_blob_header_fields(
    header_bytes: &[u8],
    strip_fields: &[u32],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    use protohoggr::Cursor;
    out.clear();
    let mut cursor = Cursor::new(header_bytes);
    loop {
        let field_start = cursor.position();
        let Some((field, wire_type)) = cursor
            .read_tag()
            .map_err(|e| io::Error::other(format!("BlobHeader parse: {e}")))?
        else {
            break;
        };
        cursor
            .skip_field(wire_type)
            .map_err(|e| io::Error::other(format!("BlobHeader parse: {e}")))?;
        let field_end = cursor.position();
        if !strip_fields.contains(&field) {
            out.extend_from_slice(&header_bytes[field_start..field_end]);
        }
    }
    Ok(())
}

/// Re-frame an already-compressed Blob with a new BlobHeader that includes indexdata.
///
/// Takes the raw compressed Blob protobuf bytes (from a passthrough frame) and
/// builds a new frame `[4-byte header_len][BlobHeader][Blob]` with the indexdata
/// field set. The Blob bytes are not modified - only the BlobHeader is rebuilt.
///
/// This is used by merge to add blob-level index metadata to passthrough blobs
/// so that subsequent merges can classify them without decompression.
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut header_buf = Vec::new();
    reframe_raw_with_index_scratch(blob_bytes, indexdata, tagdata, &mut header_buf)
}

pub(crate) fn reframe_raw_with_index_scratch(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
) -> io::Result<Vec<u8>> {
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        io::Error::other(format!(
            "blob datasize overflow: {} bytes",
            blob_bytes.len()
        ))
    })?;
    header_buf.clear();
    encode_blob_header_into(
        "OSMData",
        datasize,
        Some(indexdata),
        tagdata,
        None,
        header_buf,
    )?;

    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| io::Error::other(format!("header too large: {} bytes", header_buf.len())))?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();

    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(header_buf);
    out.extend_from_slice(blob_bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::blob_wire::WireBlobHeader;

    #[test]
    fn way_members_header_roundtrip_respects_toggle() {
        let mut bytes = Vec::new();
        let payload = [1, 9, 0b1010_0101, 0b0000_0001];
        encode_blob_header_into("OSMData", 12, None, None, Some(&payload), &mut bytes)
            .expect("way-members header must encode");

        match WireBlobHeader::parse(&bytes, false, false, true) {
            Ok(parsed) => assert_eq!(parsed.waymembers.as_deref(), Some(payload.as_slice())),
            Err(err) => panic!("failed to parse way-members header: {err}"),
        }
        match WireBlobHeader::parse(&bytes, false, false, false) {
            Ok(skipped) => assert!(skipped.waymembers.is_none()),
            Err(err) => panic!("failed to parse toggle-off header: {err}"),
        }
    }

    /// `strip_blob_header_fields` copies every untargeted field through
    /// byte-for-byte and drops only the requested field number(s). This pins
    /// the passthrough contract that a header-only strip touches exactly the
    /// field it targets: a 26-byte v1 indexdata stays v1 (never upgraded to
    /// the 42-byte v2 layout), the WayMembers-v1 payload (field 5) survives,
    /// and an unknown/extension field pbfhogg does not model is preserved.
    #[test]
    fn strip_blob_header_fields_preserves_untargeted_fields_verbatim() {
        use protohoggr::{encode_bytes_field, encode_int32_field, encode_varint_field};

        // Hand-build a BlobHeader carrying every field the wire format models
        // plus one unknown field. indexdata is a 26-byte v1 index (not the
        // 42-byte v2 our writer emits) precisely to prove v1 stays v1.
        let v1_index = [0xABu8; 26];
        let tag_index = [1u8, 2, 3, 4];
        let way_members = [1u8, 9, 0b1010_0101, 0b0000_0001];
        let unknown = [0x77u8, 0x88];
        let mut header = Vec::new();
        encode_bytes_field(&mut header, 1, b"OSMData"); // field 1: type
        encode_bytes_field(&mut header, 2, &v1_index); // field 2: indexdata (v1)
        encode_int32_field(&mut header, 3, 4242); // field 3: datasize
        encode_bytes_field(&mut header, 4, &tag_index); // field 4: tagdata
        encode_bytes_field(&mut header, 5, &way_members); // field 5: waymembers
        encode_varint_field(&mut header, 9, 0x0102); // field 9: unknown varint
        encode_bytes_field(&mut header, 11, &unknown); // field 11: unknown bytes

        // Strip nothing: output is byte-identical to the input header.
        let mut out = Vec::new();
        strip_blob_header_fields(&header, &[], &mut out).expect("strip none");
        assert_eq!(out, header, "empty strip set must be an identity copy");

        // Strip tagdata (field 4) only: every other field - including the
        // 26-byte v1 index, the WayMembers payload, and both unknown fields -
        // is preserved exactly.
        let mut expect_no_tag = Vec::new();
        encode_bytes_field(&mut expect_no_tag, 1, b"OSMData");
        encode_bytes_field(&mut expect_no_tag, 2, &v1_index);
        encode_int32_field(&mut expect_no_tag, 3, 4242);
        encode_bytes_field(&mut expect_no_tag, 5, &way_members);
        encode_varint_field(&mut expect_no_tag, 9, 0x0102);
        encode_bytes_field(&mut expect_no_tag, 11, &unknown);
        let mut out = Vec::new();
        strip_blob_header_fields(&header, &[4], &mut out).expect("strip tagdata");
        assert_eq!(
            out, expect_no_tag,
            "stripping field 4 must drop only tagdata and preserve v1 index, \
             WayMembers, and unknown fields byte-for-byte"
        );
        // The preserved indexdata is still the 26-byte v1 payload verbatim.
        let parsed = WireBlobHeader::parse(&out, true, true, true).expect("parse stripped header");
        assert!(parsed.tagdata.is_none(), "tagdata must be gone");
        assert_eq!(
            parsed.waymembers.as_deref(),
            Some(way_members.as_slice()),
            "WayMembers-v1 must survive a tagdata strip"
        );
        assert_eq!(
            &parsed.indexdata.expect("index present")[..26],
            v1_index.as_slice(),
            "v1 index bytes must be preserved unchanged"
        );

        // Strip indexdata (field 2) only: tagdata, WayMembers, and unknown
        // fields all survive.
        let mut expect_no_index = Vec::new();
        encode_bytes_field(&mut expect_no_index, 1, b"OSMData");
        encode_int32_field(&mut expect_no_index, 3, 4242);
        encode_bytes_field(&mut expect_no_index, 4, &tag_index);
        encode_bytes_field(&mut expect_no_index, 5, &way_members);
        encode_varint_field(&mut expect_no_index, 9, 0x0102);
        encode_bytes_field(&mut expect_no_index, 11, &unknown);
        let mut out = Vec::new();
        strip_blob_header_fields(&header, &[2], &mut out).expect("strip indexdata");
        assert_eq!(
            out, expect_no_index,
            "stripping field 2 must drop only indexdata and preserve tagdata, \
             WayMembers, and unknown fields byte-for-byte"
        );

        // Strip both header hints at once: only fields 1, 3, 5, 9, 11 remain.
        let mut expect_neither = Vec::new();
        encode_bytes_field(&mut expect_neither, 1, b"OSMData");
        encode_int32_field(&mut expect_neither, 3, 4242);
        encode_bytes_field(&mut expect_neither, 5, &way_members);
        encode_varint_field(&mut expect_neither, 9, 0x0102);
        encode_bytes_field(&mut expect_neither, 11, &unknown);
        let mut out = Vec::new();
        strip_blob_header_fields(&header, &[2, 4], &mut out).expect("strip both");
        assert_eq!(
            out, expect_neither,
            "stripping fields 2 and 4 must leave WayMembers and unknown fields intact"
        );
    }

    #[test]
    fn blob_header_cap_rejects_at_strict_boundary() {
        // The cap is strict (`>= MAX_BLOB_HEADER_SIZE` errors), mirroring the
        // reader reject in blob.rs, so a 65,535-byte header must encode and a
        // 65,536-byte header must fail. Measure the fixed field-5 encoder
        // overhead once (length varint stays 3 bytes across this size range),
        // then size the payloads to land exactly on either side of the bound.
        let mut buf = Vec::new();
        let probe = vec![0u8; 60_000];
        encode_blob_header_into("OSMData", 1, None, None, Some(&probe), &mut buf)
            .expect("probe header must encode");
        let overhead = buf.len() - probe.len();

        let cap = usize::try_from(MAX_BLOB_HEADER_SIZE).unwrap_or(usize::MAX);
        let pass_payload = vec![0u8; cap - 1 - overhead];
        encode_blob_header_into("OSMData", 1, None, None, Some(&pass_payload), &mut buf)
            .expect("65,535-byte header must encode");
        assert_eq!(buf.len() as u64, MAX_BLOB_HEADER_SIZE - 1);

        let fail_payload = vec![0u8; cap - overhead];
        let err = encode_blob_header_into("OSMData", 1, None, None, Some(&fail_payload), &mut buf)
            .expect_err("65,536-byte header must error");
        assert!(err.to_string().contains("BlobHeader"));
    }
}
