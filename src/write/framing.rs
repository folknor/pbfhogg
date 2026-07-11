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
