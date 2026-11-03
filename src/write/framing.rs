//! Blob framing and compression helpers. Produces the PBF on-wire format
//! `[4-byte BE header_len][BlobHeader bytes][Blob bytes]` from an uncompressed
//! block byte payload.
//!
//! The `FrameScratch` struct holds the reusable per-thread scratch buffers and
//! the lazy-initialized zlib/zstd compressors. `PIPELINE_SCRATCH` is the
//! thread-local instance used by the rayon-parallel pipelined path;
//! `PbfWriter` holds its own for the sync path.

use std::cell::RefCell;

use crate::error::{Error, OversizePart, Result};
use flate2::Compress;
use flate2::Compression as FlateCompression;
use flate2::FlushCompress;
use flate2::Status;
use protohoggr::{encode_bytes_field, encode_int32_field};

use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE};
use crate::write::metrics::WriterMetrics;
use std::sync::atomic::Ordering::Relaxed;

use crate::read::wire_schema::{BlobHeaderField, HeaderBlockField, blob, blob_header};

use super::compression::Compression;

use crate::tunables::{MAX_RETAINED_SCRATCH_CAPACITY, ZLIB_RESERVE_SHIFT, ZLIB_RESERVE_SLACK};

/// The `BlobHeader.type` of the header blob, whose size refusals are named
/// apart from a data blob's.
const OSM_HEADER: &str = "OSMHeader";

/// Per-thread reusable blob framing scratch: header/body/compression buffers
/// plus lazy-initialized zlib/zstd compressors. Reused across ordinary blobs
/// to keep steady-state allocation flat. An oversized buffer is not retained
/// in a rayon thread for the process lifetime.
pub(super) struct FrameScratch {
    /// The Blob body's fields ahead of its payload bytes: `raw_size` and the
    /// payload field's tag and length (a dozen bytes at most). The payload
    /// itself is never copied in here; see [`ScratchFrame`].
    pub(super) body_head: Vec<u8>,
    /// BlobHeader protobuf.
    pub(super) header_buf: Vec<u8>,
    /// Compression output (zlib/zstd only): the payload of a compressed
    /// blob, read from here by whoever writes the frame.
    compress_buf: Vec<u8>,
    /// Reusable zlib compressor (lazy-initialized on first zlib blob).
    zlib_compressor: Option<(u32, Compress)>,
    /// Reusable zstd compressor (lazy-initialized on first zstd blob).
    zstd_compressor: Option<(i32, zstd::bulk::Compressor<'static>)>,
}

impl FrameScratch {
    pub(super) const fn new() -> Self {
        Self {
            body_head: Vec::new(),
            header_buf: Vec::new(),
            compress_buf: Vec::new(),
            zlib_compressor: None,
            zstd_compressor: None,
        }
    }

    /// Drop a buffer grown past `MAX_RETAINED_SCRATCH_CAPACITY`. Only
    /// between frames: a [`ScratchFrame`] borrows the header and the payload
    /// from here until its frame is written or assembled.
    pub(super) fn trim_oversize_retained_buffers(&mut self, metrics: &WriterMetrics) {
        if self.header_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.header_buf.capacity(), metrics);
            self.header_buf = Vec::new();
        }
        if self.compress_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.compress_buf.capacity(), metrics);
            self.compress_buf = Vec::new();
        }
    }

    fn record_capped_capacity(capacity: usize, metrics: &WriterMetrics) {
        metrics.frame_scratch_capped_buffers.fetch_add(1, Relaxed);
        metrics
            .frame_scratch_capped_capacity_bytes
            .fetch_add(u64::try_from(capacity).unwrap_or(u64::MAX), Relaxed);
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
/// `write_framed_blob` (sync), which reuse scratch buffers through
/// `frame_in_scratch`.
pub(crate) fn frame_blob(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    metrics: &WriterMetrics,
) -> Result<Vec<u8>> {
    let mut scratch = FrameScratch::new();
    frame_blob_into(
        blob_type,
        uncompressed,
        compression,
        indexdata,
        None,
        None,
        &mut scratch,
        metrics,
    )
}

/// Compress and frame a blob using per-thread scratch buffers.
///
/// Intended for rayon `par_iter` workers that produce fully framed blobs
/// in the parallel phase, so the sequential write phase can use
/// `write_raw_owned` with bounded backpressure (no second rayon dispatch).
/// The framing counters record into `metrics`, the invocation's writer set.
pub(crate) fn frame_blob_pipelined(
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    metrics: &WriterMetrics,
) -> Result<Vec<u8>> {
    PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
        frame_blob_into(
            "OSMData",
            uncompressed,
            compression,
            indexdata,
            tagdata,
            way_members,
            scratch,
            metrics,
        )
    })
}

/// Compress and frame a blob using reusable scratch buffers, returning the
/// whole frame `[4-byte BE header_len][BlobHeader][Blob]` as one exactly
/// sized `Vec<u8>`, ready for any backend to write as it stands.
/// The framing itself is [`frame_in_scratch`]; this adds the hand-off.
///
/// # Per-call cost
///
/// The scratch keeps its buffers and both compressors, so after warmup a
/// call allocates exactly one `Vec<u8>` - the frame - and copies the header
/// and body into it once, timed into `writer_frame_ns`: the payload straight
/// from the compressor's output, or from `uncompressed` itself under
/// `Compression::None`. A scratch buffer grown past
/// `MAX_RETAINED_SCRATCH_CAPACITY` is dropped once its frame is assembled
/// (and on every error return), so an outlier is not pinned on a long-lived
/// executor thread.
///
/// This replaced a hand-off of the header and body as separate buffers,
/// swapped out of the scratch with two freshly allocated replacements
/// installed behind them, which every backend then flattened into one
/// buffer anyway - on the writer thread, for blocks the writer framed. That
/// was three allocations and two copies of the body per frame, against one
/// and one now.
///
/// NOTE: a buffer recycling pool was attempted to eliminate the per-call
/// allocation via `Arc<Mutex<Vec<Vec<u8>>>>` shared between rayon workers and
/// the writer thread. Regressed throughput by +12% (Germany, Compression::None)
/// due to Mutex contention. Measured at commit 2bf438c.
///
/// A bounded return path for frames, writer thread back to the framing
/// workers, was considered again on 2026-11-01 and not built. Its case was
/// that per-frame allocations above the tcache limit made a two-arena glibc
/// cap cost 10 to 82 % of planet wall. Measurement put about a tenth of that
/// on per-blob buffers and the rest on the block string table's
/// per-string allocations; with the table on one byte arena, capped walls
/// equal uncapped ones (planet cat `--clean` 153.0 s, repack 173.2 s, bygg),
/// so the return path has no measured cost left to remove. Any gain under
/// default arenas is untested.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(super) fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    scratch: &mut FrameScratch,
    metrics: &WriterMetrics,
) -> Result<Vec<u8>> {
    let framed = frame_in_scratch(
        blob_type,
        uncompressed,
        compression,
        indexdata,
        tagdata,
        way_members,
        scratch,
        metrics,
    )?;
    let t_assemble = crate::counters::Stopwatch::start();
    let mut frame = Vec::with_capacity(framed.len());
    for part in framed.parts() {
        frame.extend_from_slice(part);
    }
    t_assemble.record_ns(&metrics.frame_ns);
    scratch.trim_oversize_retained_buffers(metrics);
    Ok(frame)
}

/// A blob framed in place: the frame's wire parts in order, borrowed from
/// the [`FrameScratch`] that produced them and, under `Compression::None`,
/// from the block bytes themselves.
///
/// The Blob body is two parts, its head (`raw_size` and the payload field's
/// tag and length) and its payload, so the payload is never copied into a
/// scratch buffer of its own: a compressed payload stays in the
/// compressor's output buffer and an uncompressed one in the caller's
/// block. A body buffer holding a copy of the payload was a second retained
/// buffer the size of the largest compressed block on every framing thread
/// (about 2 MB on each of apply-changes' 30 planet workers), and one more
/// copy of every frame's bytes.
pub(super) struct ScratchFrame<'a> {
    /// The 4-byte big-endian `BlobHeader` length.
    pub(super) prefix: [u8; 4],
    pub(super) header: &'a [u8],
    pub(super) body_head: &'a [u8],
    pub(super) payload: &'a [u8],
}

impl<'a> ScratchFrame<'a> {
    /// The frame's bytes, in wire order: prefix, header, body head, payload.
    pub(super) fn parts(&self) -> [&[u8]; 4] {
        [&self.prefix, self.header, self.body_head, self.payload]
    }

    /// The frame's length in bytes.
    pub(super) fn len(&self) -> usize {
        self.parts().iter().map(|part| part.len()).sum()
    }
}

/// Where a Blob body's payload bytes are: [`encode_blob_body`]'s answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadSource {
    /// The uncompressed block itself (`Compression::None`).
    Input,
    /// `FrameScratch::compress_buf`.
    Compressed,
}

/// Compress and frame a blob into `scratch`, leaving every part in place
/// (see [`ScratchFrame`]). The one owner of the framing rules - size limits,
/// header encoding and the `compress_ns` / `frame_ns` / `bytes_framed`
/// metrics - for both write paths, which differ only in what they do with
/// the result:
///
/// - [`frame_blob_into`] (pipelined, and `frame_blob`) copies the parts into
///   one owned frame, because the frame crosses a channel to the writer
///   thread.
/// - `PbfWriter::write_framed_blob` (sync) writes the parts straight from the
///   scratch, so after warmup it allocates nothing per blob. Routing it
///   through `frame_blob_into` would cost it the frame allocation and copy.
///
/// The frame-assembly rules themselves (size limits, header encoding) are
/// [`encode_frame_header`], shared with the passthrough reframe; compression,
/// scratch retention and the metrics stay here.
///
/// On an error return, oversize retained buffers have been trimmed. On
/// success they have not: the returned parts borrow them, so the caller
/// trims once it has written or assembled the frame.
#[allow(clippy::too_many_arguments)]
pub(super) fn frame_in_scratch<'a>(
    blob_type: &str,
    uncompressed: &'a [u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    scratch: &'a mut FrameScratch,
    metrics: &WriterMetrics,
) -> Result<ScratchFrame<'a>> {
    let t_compress = crate::counters::Stopwatch::start();
    scratch.trim_oversize_retained_buffers(metrics);
    // The OSMHeader block's size comes from its header fields, never from an
    // element count, so its refusal names that part: a front end answers an
    // OSMData refusal with its element cap, which cannot shrink a header.
    let payload_part = if blob_type == OSM_HEADER {
        OversizePart::OsmHeaderPayload
    } else {
        OversizePart::Payload
    };
    let source = match encode_blob_body(uncompressed, compression, payload_part, scratch) {
        Ok(source) => source,
        Err(error) => {
            scratch.trim_oversize_retained_buffers(metrics);
            return Err(error);
        }
    };
    t_compress.record_ns(&metrics.compress_ns);
    let t_frame = crate::counters::Stopwatch::start();
    let payload_len = match source {
        PayloadSource::Input => uncompressed.len(),
        PayloadSource::Compressed => scratch.compress_buf.len(),
    };

    let prefix = match encode_frame_header(
        blob_type,
        scratch.body_head.len() + payload_len,
        indexdata,
        tagdata,
        way_members,
        &mut scratch.header_buf,
        metrics,
    ) {
        Ok(prefix) => prefix,
        Err(error) => {
            scratch.trim_oversize_retained_buffers(metrics);
            return Err(error);
        }
    };
    let frame = ScratchFrame {
        prefix,
        header: &scratch.header_buf,
        body_head: &scratch.body_head,
        payload: match source {
            PayloadSource::Input => uncompressed,
            PayloadSource::Compressed => &scratch.compress_buf,
        },
    };
    t_frame.record_ns(&metrics.frame_ns);
    metrics.bytes_framed.fetch_add(frame.len() as u64, Relaxed);
    Ok(frame)
}

/// Encode the `BlobHeader` for a `body_len`-byte Blob body into `header_buf`
/// and return the frame's 4-byte big-endian header-length prefix.
///
/// The one owner of the frame-assembly rules, for every writer path that
/// builds a frame: freshly encoded blocks ([`frame_in_scratch`]) and
/// already-compressed bodies under a rebuilt header
/// ([`reframe_raw_with_index`]). A newly assembled frame obeys the reader's
/// datasize and header-length bounds. Paths that copy an existing frame
/// verbatim (`write_raw`, `write_raw_copy`) assemble nothing and do not come
/// through here.
///
/// The body refusal is policy that belongs to frame construction, so it
/// applies to the reframe as well, where no input can reach it today: every
/// passthrough body was read through the frame decoder, which refuses the
/// same bound. That is a precondition of the callers, and keeping the rule
/// here means no caller has to be trusted with it.
fn encode_frame_header(
    blob_type: &str,
    body_len: usize,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
    metrics: &WriterMetrics,
) -> Result<[u8; 4]> {
    // pbfhogg caps a Blob body at `MAX_BLOB_DATASIZE`, and our own reader
    // enforces it on the way back in ("blob header declares datasize too
    // big"). Without this guard the writer happily emitted a larger body and
    // the check fired only when someone later tried to *read* the file - found
    // by repacking Denmark at `--elements-per-blob 20000000`, which produced a
    // 143 MB blob body and a PBF that pbfhogg itself refuses to open. Fail at
    // the write, with a typed error a front end can answer with the knob
    // that caused it.
    if body_len as u64 >= MAX_BLOB_DATASIZE {
        return Err(crate::error::new_blob_too_large(
            if blob_type == OSM_HEADER {
                OversizePart::OsmHeaderBody
            } else {
                OversizePart::Body
            },
            body_len as u64,
            MAX_BLOB_DATASIZE,
        ));
    }
    let datasize = i32::try_from(body_len)
        .map_err(|_| Error::internal(format!("blob datasize overflow: {body_len} bytes")))?;
    if encode_blob_header_into(
        blob_type,
        datasize,
        indexdata,
        tagdata,
        way_members,
        header_buf,
    )? {
        metrics.tagdata_dropped_oversize.fetch_add(1, Relaxed);
    }
    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| Error::internal(format!("header too large: {} bytes", header_buf.len())))?;
    Ok(header_len.to_be_bytes())
}

/// Encode the Blob protobuf body (optionally compressed): its head into
/// `scratch.body_head`, its payload compressed into `scratch.compress_buf`
/// or left in `uncompressed`, as the returned [`PayloadSource`] says. The
/// body is the head followed by the payload.
///
/// Both zlib and zstd compressors are lazy-initialized and reused to avoid
/// ~312 KB (zlib) or ~512 KB (zstd) of compressor state allocation per blob.
///
/// # Why the payload field is always emitted
///
/// The payload field's head (`raw`, `zlib_data`, or `zstd_data`: tag and
/// length) is always written, a zero length included, never skipped when
/// the payload is empty. A `Blob` carries exactly one
/// payload field and it is what identifies the blob's encoding; with
/// skip-if-empty, a zero-length payload produced a `Blob` message with *no
/// fields at all* for `Compression::None`, which no reader can distinguish
/// from a corrupt or truncated blob. An empty block is not reachable from any
/// current caller - `BlockBuilder` never emits an element-free block - but a
/// format writer must not have a path that emits a malformed message at all,
/// and "unreachable today" is a property of the callers, not of this encoder.
///
/// This is the sibling of `encode_blob_header_fields`, where the opposite rule
/// holds: there the optional hint fields are genuinely optional and absence is
/// meaningful. Here absence is not a spelling of anything.
///
/// `payload_part` is the part an oversized payload is reported as, which
/// depends on the blob type the caller is framing.
fn encode_blob_body(
    uncompressed: &[u8],
    compression: &Compression,
    payload_part: OversizePart,
    scratch: &mut FrameScratch,
) -> Result<PayloadSource> {
    scratch.body_head.clear();
    // Check once per blob, before compression or copying. The encoded-body
    // guard cannot catch a highly compressible oversized block. Element caps
    // do not bound bytes: even one element can carry a very large tag value.
    // Both sync and pipelined framing use this encoder. Exactly the raw limit
    // is legal; the separate encoded-body limit can still reject its framing.
    if uncompressed.len() as u64 > MAX_BLOB_MESSAGE_SIZE {
        return Err(crate::error::new_blob_too_large(
            payload_part,
            uncompressed.len() as u64,
            MAX_BLOB_MESSAGE_SIZE,
        ));
    }
    match compression {
        Compression::None => {
            // Blob field 1: raw (bytes, len-delimited). Always emitted; see
            // the doc block.
            encode_payload_field_head(&mut scratch.body_head, blob::RAW, uncompressed.len());
            Ok(PayloadSource::Input)
        }
        Compression::Zlib(level) => {
            compress_zlib(uncompressed, *level, scratch)?;
            Ok(PayloadSource::Compressed)
        }
        Compression::Zstd(level) => {
            let compressor = match &mut scratch.zstd_compressor {
                Some((cached_level, compressor)) if *cached_level == *level => compressor,
                // Built lazily, per worker, on its first zstd blob. A parsed
                // `Compression` (the CLI's argument boundary) has its level
                // range-checked already, so from the CLI a failure here is a
                // resource failure (zstd could not allocate its context), not
                // a configuration error found late; only a library caller
                // building `Compression::Zstd` by hand can reach it with a
                // bad level. Built before it replaces the cached one, so a
                // failure leaves that one in place.
                slot => {
                    let built = zstd::bulk::Compressor::new(*level)?;
                    &mut slot.insert((*level, built)).1
                }
            };
            let bound = zstd::zstd_safe::compress_bound(uncompressed.len());
            scratch.compress_buf.clear();
            scratch.compress_buf.reserve(bound);
            compressor.compress_to_buffer(uncompressed, &mut scratch.compress_buf)?;
            let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
                Error::internal(format!(
                    "blob raw_size overflow: {} bytes",
                    uncompressed.len()
                ))
            })?;
            // Blob field 2: raw_size (int32, varint)
            encode_int32_field(&mut scratch.body_head, blob::RAW_SIZE, raw_size);
            // Blob field 7: zstd_data (bytes, len-delimited). Always emitted.
            encode_payload_field_head(
                &mut scratch.body_head,
                blob::ZSTD_DATA,
                scratch.compress_buf.len(),
            );
            Ok(PayloadSource::Compressed)
        }
    }
}

/// The tag and length of a Blob payload field (`raw`, `zlib_data`,
/// `zstd_data`) whose `len` bytes follow it, appended to `head`: what
/// `encode_bytes_field_always` writes ahead of the bytes. Always written,
/// a zero length included; see `encode_blob_body`.
fn encode_payload_field_head(head: &mut Vec<u8>, field: u32, len: usize) {
    protohoggr::encode_tag(head, field, protohoggr::WIRE_LEN);
    protohoggr::encode_varint(head, len as u64);
}

/// Zlib compression via reusable `flate2::Compress` with `reset()`.
fn compress_zlib(uncompressed: &[u8], level: u32, scratch: &mut FrameScratch) -> Result<()> {
    let compressor = match &mut scratch.zlib_compressor {
        Some((cached_level, compressor)) if *cached_level == level => compressor,
        slot => {
            &mut slot
                .insert((level, Compress::new(FlateCompression::new(level), true)))
                .1
        }
    };
    scratch.compress_buf.clear();
    // Zlib worst-case bound: input + ~0.1% + header/trailer.
    scratch.compress_buf.reserve(
        uncompressed.len() + (uncompressed.len() >> ZLIB_RESERVE_SHIFT) + ZLIB_RESERVE_SLACK,
    );
    let status = compressor
        .compress_vec(
            uncompressed,
            &mut scratch.compress_buf,
            FlushCompress::Finish,
        )
        .map_err(|e| Error::internal_with("zlib compress error", e))?;
    if !matches!(status, Status::StreamEnd) {
        return Err(Error::internal(
            "zlib compress did not complete in one call",
        ));
    }
    compressor.reset();
    let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
        Error::internal(format!(
            "blob raw_size overflow: {} bytes",
            uncompressed.len()
        ))
    })?;
    encode_int32_field(&mut scratch.body_head, blob::RAW_SIZE, raw_size);
    // Blob field 3: zlib_data. Always emitted; see `encode_blob_body`.
    encode_payload_field_head(
        &mut scratch.body_head,
        blob::ZLIB_DATA,
        scratch.compress_buf.len(),
    );
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
///
/// # Oversize headers
///
/// The 64 KiB cap is a hard format limit, and `tagdata` is the one header
/// field whose size scales with the number of elements packed into the blob:
/// it lists every distinct tag key in the block, so a large
/// `--elements-per-blob` grows it without bound. Measured on the Denmark
/// extract, the whole-file key set serializes to 48 043 bytes, i.e. 73 % of
/// the cap for a single small country - a planet-scale blob at
/// `--elements-per-blob 228000` can exceed it outright.
///
/// When the header would overflow, `tagdata` is **dropped**, never truncated.
/// Truncation is the dangerous option: `tagdata` is consumed as a skip filter,
/// so a key list missing entries makes `tags-filter` skip blobs that *do*
/// contain the key - wrong results, not merely slow ones. Absence, by
/// contrast, is already the format's spelling of "decode this blob", so a
/// dropped index costs only the skip optimization. Returns `true` when it
/// dropped `tagdata`; [`encode_frame_header`] counts each drop in the
/// invocation's `tagdata_dropped_oversize` (sidecar counter
/// `writer_tagdata_dropped_oversize`) so the loss is observable rather than
/// silent.
///
/// `indexdata` and `way_members` are *not* droppable. Both are consumed as
/// claims about blob contents rather than as hints: an absent `WayMembers-v1`
/// bitmap on one blob of a file whose `HeaderBlock` advertises the feature
/// reads as "this blob has no way-member nodes", which would silently drop
/// nodes. If the header still overflows once `tagdata` is gone, that is a
/// hard error naming the field responsible.
pub(crate) fn encode_blob_header_into(
    blob_type: &str,
    datasize: i32,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    buf: &mut Vec<u8>,
) -> Result<bool> {
    encode_blob_header_fields(blob_type, datasize, indexdata, tagdata, way_members, buf);
    if (buf.len() as u64) < MAX_BLOB_HEADER_SIZE {
        return Ok(false);
    }
    if tagdata.is_some() {
        // Retry without the skip index before giving up: see the doc block.
        encode_blob_header_fields(blob_type, datasize, indexdata, None, way_members, buf);
        if (buf.len() as u64) < MAX_BLOB_HEADER_SIZE {
            return Ok(true);
        }
    }
    // Name the culprit by computing it, not by guessing from the bitmap's
    // size: the bitmap is to blame exactly when the header, already without
    // its tag index, would fit without the bitmap too. The probe is the cold
    // error path and reuses `buf`, so the reported size is captured first.
    let oversize = buf.len() as u64;
    let mut part = OversizePart::Header;
    if way_members.is_some() {
        encode_blob_header_fields(blob_type, datasize, indexdata, None, None, buf);
        if (buf.len() as u64) < MAX_BLOB_HEADER_SIZE {
            part = OversizePart::HeaderWayMembers;
        }
    }
    Err(crate::error::new_blob_too_large(
        part,
        oversize,
        MAX_BLOB_HEADER_SIZE,
    ))
}

/// Serialize the `BlobHeader` fields into `buf` with no size policy applied.
fn encode_blob_header_fields(
    blob_type: &str,
    datasize: i32,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>,
    buf: &mut Vec<u8>,
) {
    buf.clear();
    // Field 1: type (string = len-delimited bytes)
    encode_bytes_field(buf, blob_header::TYPE, blob_type.as_bytes());
    // Field 2: indexdata (optional bytes)
    if let Some(data) = indexdata {
        encode_bytes_field(buf, blob_header::INDEXDATA, data);
    }
    // Field 3: datasize (int32 varint)
    encode_int32_field(buf, blob_header::DATASIZE, datasize);
    // Field 4: tagdata (optional bytes - per-blob tag key index)
    if let Some(data) = tagdata {
        encode_bytes_field(buf, blob_header::TAGDATA, data);
    }
    // Field 5: WayMembers-v1 payload (optional bytes).
    if let Some(data) = way_members {
        encode_bytes_field(buf, blob_header::WAYMEMBERS, data);
    }
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
/// fields whose wire type this can skip. Only the targeted field number(s)
/// are removed.
///
/// Wire-type coverage: fields using varint (0), 64-bit (1), length-delimited
/// (2), or 32-bit (5) wire types are copied verbatim regardless of field
/// number - the only wire types `BlobHeader` and `HeaderBlock` actually use.
/// The deprecated protobuf group wire types (3 = `SGROUP`, 4 = `EGROUP`) are
/// not supported by the underlying cursor and return a clean parse error
/// instead of being silently dropped or miscopied; neither message type
/// emits them, so this is not expected to trigger in practice.
///
/// Used by `degrade`'s header-only passthrough so a strip changes exactly
/// the field it targets (`indexdata` for `--strip-indexdata`, `tagdata` for
/// `--strip-tagdata`) and leaves every other header field byte-for-byte
/// identical. The blob payload it accompanies is unchanged, so the preserved
/// `datasize` (field 3) stays consistent.
pub(crate) fn strip_blob_header_fields(
    header_bytes: &[u8],
    strip_fields: &[BlobHeaderField],
    out: &mut Vec<u8>,
) -> Result<()> {
    strip_message_fields(
        header_bytes,
        |field| strip_fields.iter().any(|f| *f as u32 == field),
        out,
        "BlobHeader",
    )
}

/// Re-emit a `HeaderBlock` protobuf message (the decompressed OSMHeader
/// payload), copying every field through verbatim except the field numbers
/// listed in `strip_fields`, which are dropped.
///
/// The counterpart of [`strip_blob_header_fields`] for the file-level
/// `HeaderBlock`. Used by `degrade`'s header-only passthrough so
/// `--strip-bbox` removes exactly the bbox (field 1) while leaving every
/// other HeaderBlock field byte-for-byte identical: `writingprogram`
/// (field 16), `source` (field 17), the required/optional feature lists
/// (fields 4/5, including custom extension features like
/// `pbfhogg.WayMembers-v1` and `SharedNodePins-v1`), the osmosis
/// replication metadata (fields 32/33/34), and any unknown/extension
/// fields whose wire type this can skip (see the wire-type coverage note
/// on [`strip_blob_header_fields`]). `HeaderBuilder` carries the fields it
/// models, but it cannot model unknown/extension fields, and it drops the
/// optional-feature list by design (the output-header policy declares only
/// features it can vouch for).
/// This preserves everything.
pub(crate) fn strip_header_block_fields(
    header_block_bytes: &[u8],
    strip_fields: &[HeaderBlockField],
    out: &mut Vec<u8>,
) -> Result<()> {
    strip_message_fields(
        header_block_bytes,
        |field| strip_fields.iter().any(|f| *f as u32 == field),
        out,
        "HeaderBlock",
    )
}

/// Shared protobuf field-copy core for the two strippers above. Copies every
/// top-level field's tag+value bytes verbatim except the field numbers in
/// `strip_fields`. `ctx` names the message in parse errors.
fn strip_message_fields(
    msg_bytes: &[u8],
    strip_field: impl Fn(u32) -> bool,
    out: &mut Vec<u8>,
    ctx: &str,
) -> Result<()> {
    use protohoggr::Cursor;
    out.clear();
    let mut cursor = Cursor::new(msg_bytes);
    loop {
        let field_start = cursor.position();
        let Some((field, wire_type)) = cursor
            .read_tag()
            .map_err(|e| Error::from(e).context(format!("{ctx} parse")))?
        else {
            break;
        };
        cursor
            .skip_field(wire_type)
            .map_err(|e| Error::from(e).context(format!("{ctx} parse")))?;
        let field_end = cursor.position();
        if !strip_field(field) {
            out.extend_from_slice(&msg_bytes[field_start..field_end]);
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
/// This is used by cat, merge and apply-changes to add blob-level index
/// metadata to passthrough blobs so that later runs can classify them without
/// decompression. The frame goes through the same assembly rules as a freshly
/// encoded block ([`encode_frame_header`]), including the body-size refusal.
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
    metrics: &WriterMetrics,
) -> Result<Vec<u8>> {
    let mut header_buf = Vec::new();
    let prefix = encode_frame_header(
        "OSMData",
        blob_bytes.len(),
        Some(indexdata),
        tagdata,
        None,
        &mut header_buf,
        metrics,
    )?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();

    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&prefix);
    out.extend_from_slice(&header_buf);
    out.extend_from_slice(blob_bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::blob_wire::WireBlobHeader;

    /// The Blob body `encode_blob_body` produced: its head, then its payload
    /// from wherever the returned source says it is.
    fn encoded_body(
        uncompressed: &[u8],
        compression: &Compression,
        scratch: &mut FrameScratch,
    ) -> Vec<u8> {
        let source = encode_blob_body(uncompressed, compression, OversizePart::Payload, scratch)
            .expect("body must encode");
        let payload = match source {
            PayloadSource::Input => uncompressed,
            PayloadSource::Compressed => scratch.compress_buf.as_slice(),
        };
        [scratch.body_head.as_slice(), payload].concat()
    }

    /// The body head plus payload is exactly the Blob message the old
    /// single-buffer encoder wrote (`raw_size` then the payload field), for
    /// every compression. The uncompressed payload is never copied into the
    /// scratch.
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn body_head_and_payload_spell_the_whole_blob() {
        use protohoggr::encode_bytes_field_always;
        let block: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        for compression in [
            Compression::None,
            Compression::Zlib(1),
            Compression::Zstd(1),
        ] {
            let mut scratch = FrameScratch::new();
            let body = encoded_body(&block, &compression, &mut scratch);
            let mut expected = Vec::new();
            match compression {
                Compression::None => {
                    encode_bytes_field_always(&mut expected, blob::RAW, &block);
                    assert!(scratch.compress_buf.is_empty(), "no copy of a raw payload");
                }
                Compression::Zlib(_) | Compression::Zstd(_) => {
                    let field = if matches!(compression, Compression::Zlib(_)) {
                        blob::ZLIB_DATA
                    } else {
                        blob::ZSTD_DATA
                    };
                    let raw_size = i32::try_from(block.len()).expect("fits");
                    encode_int32_field(&mut expected, blob::RAW_SIZE, raw_size);
                    encode_bytes_field_always(&mut expected, field, &scratch.compress_buf);
                }
            }
            assert_eq!(body, expected, "{compression:?}");
            assert!(scratch.body_head.len() <= 12, "the head is a few bytes");
        }
    }

    #[test]
    fn oversized_raw_payload_is_refused_before_compression() {
        let payload = vec![0; usize::try_from(MAX_BLOB_MESSAGE_SIZE).expect("limit fits") + 1];
        for compression in [
            Compression::None,
            Compression::Zlib(1),
            Compression::Zstd(1),
        ] {
            let mut scratch = FrameScratch::new();
            let error =
                encode_blob_body(&payload, &compression, OversizePart::Payload, &mut scratch)
                    .expect_err("compression must not hide an oversized raw block");
            assert!(error.to_string().contains("uncompressed blob payload"));
            assert!(scratch.body_head.is_empty());
            assert!(scratch.compress_buf.is_empty());
            assert!(scratch.zlib_compressor.is_none());
            assert!(scratch.zstd_compressor.is_none());
        }
    }

    #[test]
    fn compressed_payload_at_raw_limit_can_be_framed() {
        let payload = vec![0; usize::try_from(MAX_BLOB_MESSAGE_SIZE).expect("limit fits")];
        for compression in [Compression::Zlib(1), Compression::Zstd(1)] {
            frame_blob(
                "OSMData",
                &payload,
                &compression,
                None,
                &WriterMetrics::new(),
            )
            .expect("raw limit is inclusive when the encoded body fits");
        }
    }

    /// An empty payload must still produce a `Blob` with its payload field.
    ///
    /// With the skip-if-empty encoder, `Compression::None` on a zero-length
    /// block emitted a `Blob` message of length zero - no fields at all - and
    /// nothing downstream could tell it from a truncated blob. The assertion
    /// is on the encoded bytes rather than on a round-trip, because a reader
    /// that tolerates the malformed form would hide exactly this defect: an
    /// empty message decodes "successfully" into a default `Blob`.
    ///
    /// # Only the `None` arm can fail
    ///
    /// Said plainly, because an arm that cannot fail is worth exactly nothing
    /// unless its reader knows that. Zlib and zstd both turn a zero-byte
    /// input into a non-empty stream - a header and an end marker - so their
    /// payload field is non-empty however its head is written, and making
    /// those two heads skip an empty payload would leave this test green. `Compression::None` is the arm that carries the property: it is
    /// the only path where the payload bytes are the block's bytes and are
    /// therefore empty when the block is. The compressed arms are kept as
    /// field-numbering and no-panic regression cover, not as evidence for the
    /// `_always` rule, and `no_payload_bytes_reach_the_output_uncompressed`
    /// below asserts the `None` case on exact bytes so the property has a
    /// check that bites rather than a loop that merely includes it.
    #[test]
    fn empty_payload_still_emits_the_blob_payload_field() {
        for compression in [
            Compression::None,
            Compression::Zlib(1),
            Compression::Zstd(1),
        ] {
            let mut scratch = FrameScratch::new();
            let body = encoded_body(&[], &compression, &mut scratch);
            assert!(
                !body.is_empty(),
                "{compression:?}: an empty payload produced a Blob message \
                 with no fields at all"
            );
            // Field 1 (raw) / 3 (zlib_data) / 7 (zstd_data).
            let payload_field: u32 = match compression {
                Compression::None => 1,
                Compression::Zlib(_) => 3,
                Compression::Zstd(_) => 7,
            };
            let present = present_field_numbers(&body);
            assert!(
                present.contains(&payload_field),
                "{compression:?}: encoded Blob carries fields {present:?}, \
                 missing the payload field {payload_field}"
            );
        }
    }

    /// The exact bytes of an empty uncompressed `Blob`.
    ///
    /// This is the assertion that actually bites for the `_always` rule:
    /// skip the payload field's head for an empty payload in the
    /// `Compression::None` arm of `encode_blob_body` and the output becomes
    /// zero bytes instead of these two.
    #[test]
    fn no_payload_bytes_reach_the_output_uncompressed() {
        let mut scratch = FrameScratch::new();
        let body = encoded_body(&[], &Compression::None, &mut scratch);
        assert_eq!(
            body.as_slice(),
            // Field 1, wire type 2 (0x0a), length 0.
            &[0x0a, 0x00],
            "an empty uncompressed Blob must still spell out its `raw` field \
             with a zero length, not serialize to nothing"
        );
    }

    /// Minimal independent protobuf field-number scanner.
    ///
    /// Deliberately not reusing the crate's own `Blob` reader: a decoder that
    /// tolerates a field-less message is exactly what would hide the defect
    /// the caller is asserting against.
    fn present_field_numbers(mut bytes: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let read_varint = |b: &mut &[u8]| -> u64 {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = *b.first().expect("truncated varint in encoded Blob");
                *b = &b[1..];
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        while !bytes.is_empty() {
            let key = read_varint(&mut bytes);
            #[allow(clippy::cast_possible_truncation)]
            out.push((key >> 3) as u32);
            match key & 7 {
                0 => {
                    read_varint(&mut bytes);
                }
                2 => {
                    let len = usize::try_from(read_varint(&mut bytes))
                        .expect("length-delimited field length must fit usize");
                    assert!(len <= bytes.len(), "length-delimited field overruns");
                    bytes = &bytes[len..];
                }
                other => panic!("unexpected wire type {other} in encoded Blob"),
            }
        }
        out
    }

    /// Pipelined framing hands out one exactly sized frame and keeps every
    /// scratch buffer warm: the frame's bytes are the sync path's
    /// `prefix | header | body head | payload`, its capacity is its length
    /// (nothing over-reserved travels to the writer), and a second, smaller
    /// blob reuses the scratch without regrowing it. An outlier above the
    /// retention limit is dropped once its frame is assembled, not before:
    /// the compressed payload is read from the scratch until then.
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn framing_hands_out_one_exact_frame_and_keeps_the_scratch() {
        let frame = |payload: &[u8], compression: Compression, scratch: &mut FrameScratch| {
            frame_blob_into(
                "OSMData",
                payload,
                &compression,
                None,
                None,
                None,
                scratch,
                &WriterMetrics::new(),
            )
            .expect("blob must frame")
        };
        let parts_of = |payload: &[u8], compression: Compression| -> Vec<u8> {
            let mut probe = FrameScratch::new();
            let parts = frame_in_scratch(
                "OSMData",
                payload,
                &compression,
                None,
                None,
                None,
                &mut probe,
                &WriterMetrics::new(),
            )
            .expect("probe must frame");
            parts.parts().concat()
        };
        let payload = vec![7u8; 64 * 1024];
        for compression in [Compression::None, Compression::Zstd(1)] {
            let mut scratch = FrameScratch::new();
            let first = frame(&payload, compression, &mut scratch);
            assert_eq!(first, parts_of(&payload, compression), "{compression:?}");
            assert_eq!(first.capacity(), first.len(), "the frame is exactly sized");
            assert!(scratch.header_buf.capacity() > 0, "header buffer kept");
        }
        let raw = frame(&[1, 2, 3], Compression::None, &mut FrameScratch::new());
        assert!(raw.ends_with(&[0x0a, 3, 1, 2, 3]), "raw field of 3 bytes");

        let mut scratch = FrameScratch::new();
        frame(&payload, Compression::Zstd(1), &mut scratch);
        let warmed = scratch.compress_buf.capacity();
        assert!(warmed > 0, "the compressor's output buffer is kept");
        frame(&[1, 2, 3], Compression::Zstd(1), &mut scratch);
        assert_eq!(
            scratch.compress_buf.capacity(),
            warmed,
            "a smaller blob reuses the scratch without regrowing it"
        );

        let big: Vec<u8> = (0..MAX_RETAINED_SCRATCH_CAPACITY + 1)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        {
            // The outlier's compression buffer really is past the limit while
            // its frame borrows it: the trim has something to drop.
            let mut probe = FrameScratch::new();
            let parts = frame_in_scratch(
                "OSMData",
                &big,
                &Compression::Zstd(1),
                None,
                None,
                None,
                &mut probe,
                &WriterMetrics::new(),
            )
            .expect("outlier must frame");
            assert!(parts.payload.len() > 1024, "incompressible payload");
            assert!(probe.compress_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY);
        }
        let framed = frame(&big, Compression::Zstd(1), &mut scratch);
        let mut decoded = Vec::new();
        {
            // The assembled outlier frame carries the whole block: its body is
            // read back from the frame and decompressed.
            let header_len = u32::from_be_bytes(framed[..4].try_into().expect("prefix")) as usize;
            let body = &framed[4 + header_len..];
            let mut cursor = protohoggr::Cursor::new(body);
            while let Some((field, _)) = cursor.read_tag().expect("tag") {
                if field == blob::ZSTD_DATA {
                    let data = cursor.read_len_delimited().expect("payload");
                    decoded = zstd::bulk::decompress(data, big.len()).expect("zstd");
                } else {
                    cursor.read_varint().expect("raw_size");
                }
            }
        }
        assert_eq!(decoded, big, "outlier frame intact");
        assert!(
            scratch.compress_buf.capacity() <= MAX_RETAINED_SCRATCH_CAPACITY,
            "an outlier compression buffer is not retained"
        );
    }

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
        strip_blob_header_fields(&header, &[BlobHeaderField::Tagdata], &mut out)
            .expect("strip tagdata");
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
        strip_blob_header_fields(&header, &[BlobHeaderField::Indexdata], &mut out)
            .expect("strip indexdata");
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
        strip_blob_header_fields(
            &header,
            &[BlobHeaderField::Indexdata, BlobHeaderField::Tagdata],
            &mut out,
        )
        .expect("strip both");
        assert_eq!(
            out, expect_neither,
            "stripping fields 2 and 4 must leave WayMembers and unknown fields intact"
        );
    }

    /// `strip_header_block_fields` copies every untargeted HeaderBlock field
    /// through byte-for-byte and drops only the requested field number(s).
    /// This pins the passthrough contract behind `degrade --strip-bbox`: the
    /// bbox (field 1) disappears while `writingprogram` (16), `source` (17),
    /// the optional-feature list (5, including custom extension features),
    /// the osmosis replication metadata (32/33/34), and any unknown field
    /// pbfhogg does not model all survive exactly - precisely the fields a
    /// `HeaderBuilder` rebuild would silently drop.
    #[test]
    fn strip_header_block_fields_preserves_untargeted_fields_verbatim() {
        use protohoggr::{encode_bytes_field, encode_int64_field, encode_varint_field};

        // Hand-build a HeaderBlock carrying a bbox plus every metadata field,
        // a custom optional feature, and one unknown field.
        let bbox = [1u8, 2, 3, 4];
        let source = b"survey-import-2019";
        let custom_feature = b"Custom.Extension-v9";
        let unknown = [0x77u8, 0x88];
        let mut header = Vec::new();
        encode_bytes_field(&mut header, 1, &bbox); // field 1: bbox (submessage)
        // field 4: required
        encode_bytes_field(
            &mut header,
            4,
            crate::HeaderBlock::OSM_SCHEMA_V06.as_bytes(),
        );
        // field 5: optional
        encode_bytes_field(
            &mut header,
            5,
            crate::HeaderBlock::SORT_TYPE_THEN_ID.as_bytes(),
        );
        encode_bytes_field(&mut header, 5, custom_feature); // field 5: custom optional
        encode_bytes_field(&mut header, 16, b"my-writer/2.0"); // field 16: writingprogram
        encode_bytes_field(&mut header, 17, source); // field 17: source
        encode_int64_field(&mut header, 32, 1_700_000_000); // field 32: repl timestamp
        encode_int64_field(&mut header, 33, 4242); // field 33: repl seq
        encode_bytes_field(&mut header, 34, b"https://example.org/repl"); // field 34: repl url
        encode_varint_field(&mut header, 60, 0x0102); // field 60: unknown varint
        encode_bytes_field(&mut header, 61, &unknown); // field 61: unknown bytes

        // Strip nothing: identity copy.
        let mut out = Vec::new();
        strip_header_block_fields(&header, &[], &mut out).expect("strip none");
        assert_eq!(out, header, "empty strip set must be an identity copy");

        // Strip the bbox (field 1) only: everything else survives byte-for-byte.
        let mut expect = Vec::new();
        encode_bytes_field(
            &mut expect,
            4,
            crate::HeaderBlock::OSM_SCHEMA_V06.as_bytes(),
        );
        encode_bytes_field(
            &mut expect,
            5,
            crate::HeaderBlock::SORT_TYPE_THEN_ID.as_bytes(),
        );
        encode_bytes_field(&mut expect, 5, custom_feature);
        encode_bytes_field(&mut expect, 16, b"my-writer/2.0");
        encode_bytes_field(&mut expect, 17, source);
        encode_int64_field(&mut expect, 32, 1_700_000_000);
        encode_int64_field(&mut expect, 33, 4242);
        encode_bytes_field(&mut expect, 34, b"https://example.org/repl");
        encode_varint_field(&mut expect, 60, 0x0102);
        encode_bytes_field(&mut expect, 61, &unknown);
        let mut out = Vec::new();
        strip_header_block_fields(&header, &[HeaderBlockField::Bbox], &mut out)
            .expect("strip bbox");
        assert_eq!(
            out, expect,
            "stripping field 1 must drop only the bbox and preserve source, \
             writingprogram, custom optional features, replication metadata, \
             and unknown fields byte-for-byte"
        );

        // The re-parsed HeaderBlock confirms the bbox is gone but the rest
        // reads back intact.
        let parsed = crate::read::block::HeaderBlock::parse_from_bytes(&out).expect("parse");
        assert!(parsed.bbox().is_none(), "bbox must be gone");
        assert_eq!(parsed.source(), Some("survey-import-2019"));
        assert_eq!(parsed.writing_program(), Some("my-writer/2.0"));
        assert!(parsed.is_sorted());
        assert!(
            parsed
                .optional_features()
                .iter()
                .any(|f| f == "Custom.Extension-v9"),
            "custom optional feature must survive"
        );
        assert_eq!(parsed.osmosis_replication_timestamp(), Some(1_700_000_000));
        assert_eq!(parsed.osmosis_replication_sequence_number(), Some(4242));
        assert_eq!(
            parsed.osmosis_replication_base_url(),
            Some("https://example.org/repl")
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

    /// Hand-rolled `BlobHeader` field scanner.
    ///
    /// Deliberately shares no code with the encoder under test: `encode_block`
    /// and `WireBlobHeader::parse` are two halves of the same model, so
    /// round-tripping through them proves only self-consistency. This walks
    /// raw protobuf tags and returns `(field_number, payload)` for every
    /// length-delimited field, so "was tagdata actually emitted" is answered
    /// from the bytes.
    fn scan_len_delimited_fields(buf: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < buf.len() {
            let (mut key, mut shift) = (0u64, 0u32);
            loop {
                let b = buf[i];
                i += 1;
                key |= u64::from(b & 0x7f) << shift;
                shift += 7;
                if b & 0x80 == 0 {
                    break;
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => {
                    while buf[i] & 0x80 != 0 {
                        i += 1;
                    }
                    i += 1;
                }
                2 => {
                    let (mut len, mut shift) = (0usize, 0u32);
                    loop {
                        let b = buf[i];
                        i += 1;
                        len |= (b as usize & 0x7f) << shift;
                        shift += 7;
                        if b & 0x80 == 0 {
                            break;
                        }
                    }
                    out.push((field, buf[i..i + len].to_vec()));
                    i += len;
                }
                w => panic!("unexpected wire type {w}"),
            }
        }
        out
    }

    /// An oversize `tagdata` must be dropped, not truncated and not fatal.
    ///
    /// This is the failure the `--elements-per-blob 228000` workflow hits:
    /// `tagdata` lists every distinct tag key in the block, so a dense blob
    /// pushes the `BlobHeader` past the 64 KiB format cap and used to abort
    /// the entire write. Truncating the key set instead would be worse than
    /// failing - a short key list makes `tags-filter` skip blobs that do carry
    /// the key - so the index is dropped whole.
    #[test]
    fn oversize_tagdata_is_dropped_not_truncated() {
        let indexdata = vec![0xABu8; 42];
        let way_members = vec![0xCDu8; 64];
        let tagdata = vec![0x11u8; 70_000];
        let mut buf = Vec::new();
        let dropped = encode_blob_header_into(
            "OSMData",
            4242,
            Some(&indexdata),
            Some(&tagdata),
            Some(&way_members),
            &mut buf,
        )
        .expect("an oversize tag index must be dropped, never fatal");
        assert!(dropped, "the drop must be reported, so it can be counted");

        assert!(
            (buf.len() as u64) < MAX_BLOB_HEADER_SIZE,
            "header must land under the format cap, got {}",
            buf.len()
        );
        let fields = scan_len_delimited_fields(&buf);
        assert!(
            !fields.iter().any(|(f, _)| *f == 4),
            "tagdata must be absent entirely, not truncated: a short key list \
             is read as a complete one and makes tags-filter skip matching blobs"
        );
        assert_eq!(
            fields.iter().find(|(f, _)| *f == 1).map(|(_, v)| v.clone()),
            Some(b"OSMData".to_vec()),
            "blob type must survive the retry"
        );
        assert_eq!(
            fields.iter().find(|(f, _)| *f == 2).map(|(_, v)| v.clone()),
            Some(indexdata.clone()),
            "indexdata must survive the retry"
        );
        assert_eq!(
            fields.iter().find(|(f, _)| *f == 5).map(|(_, v)| v.clone()),
            Some(way_members),
            "WayMembers-v1 must survive the retry"
        );
        // Each drop is counted into the framing invocation's set, exactly.
        let metrics = WriterMetrics::new();
        for _ in 0..4 {
            reframe_raw_with_index(&[0u8; 8], &indexdata, Some(&tagdata), &metrics)
                .expect("oversize tag index must be dropped");
        }
        assert_eq!(
            metrics.tagdata_dropped_oversize.load(Relaxed),
            4,
            "a dropped tag index must be counted, not lost silently"
        );
    }

    /// A tagdata that fits must be emitted - the drop is a last resort, not a
    /// policy. Guards against a fix that simply stops writing tagdata.
    #[test]
    fn fitting_tagdata_is_preserved() {
        let tagdata = vec![0x11u8; 20_000];
        let mut buf = Vec::new();
        encode_blob_header_into("OSMData", 1, None, Some(&tagdata), None, &mut buf)
            .expect("a 20 KB tag index fits and must encode");
        let fields = scan_len_delimited_fields(&buf);
        assert_eq!(
            fields.iter().find(|(f, _)| *f == 4).map(|(_, v)| v.clone()),
            Some(tagdata),
            "a tag index that fits must be written verbatim"
        );
    }

    /// The drop must trigger only at the cap, not one byte early. The retry
    /// path and the strict `>=` bound are separate decisions and a fix that
    /// got the bound right for the error path can still get it wrong here.
    #[test]
    fn tagdata_at_exact_boundary_is_kept() {
        let mut buf = Vec::new();
        let probe = vec![0u8; 60_000];
        encode_blob_header_into("OSMData", 1, None, Some(&probe), None, &mut buf)
            .expect("probe header must encode");
        let overhead = buf.len() - probe.len();
        let cap = usize::try_from(MAX_BLOB_HEADER_SIZE).unwrap_or(usize::MAX);

        let at_limit = vec![0x22u8; cap - 1 - overhead];
        encode_blob_header_into("OSMData", 1, None, Some(&at_limit), None, &mut buf)
            .expect("65,535-byte header must encode");
        assert_eq!(buf.len() as u64, MAX_BLOB_HEADER_SIZE - 1);
        assert!(
            scan_len_delimited_fields(&buf).iter().any(|(f, _)| *f == 4),
            "a tag index landing exactly on the last legal byte must be kept"
        );

        let over_limit = vec![0x22u8; cap - overhead];
        encode_blob_header_into("OSMData", 1, None, Some(&over_limit), None, &mut buf)
            .expect("one byte over must drop tagdata rather than fail");
        assert!(
            !scan_len_delimited_fields(&buf).iter().any(|(f, _)| *f == 4),
            "one byte over the cap must drop the tag index"
        );
    }

    /// A blob body past the format's datasize cap must fail at write time.
    ///
    /// It used to be written out happily, and the only thing that noticed was
    /// a *reader* - `pbfhogg sort` on a Denmark repacked at
    /// `--elements-per-blob 20000000` died with "blob header declares datasize
    /// too big: 143773494 bytes", pointing at the file rather than at the flag
    /// that made it. The writer refuses with a typed error a front end answers
    /// with its own knob.
    #[test]
    fn an_oversize_blob_body_fails_at_write_time_with_a_typed_error() {
        let cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap_or(usize::MAX);
        // Incompressible payload, so `Compression::None` keeps the body size.
        let body = vec![0x5Au8; cap + 1];
        let mut scratch = FrameScratch::new();
        let result = frame_blob_into(
            "OSMData",
            &body,
            &crate::write::compression::Compression::None,
            None,
            None,
            None,
            &mut scratch,
            &WriterMetrics::new(),
        );
        let Err(err) = result else {
            panic!("a blob body past the datasize cap must not be written");
        };
        assert!(
            matches!(
                err.kind(),
                crate::ErrorKind::BlobTooLarge {
                    part: OversizePart::Body | OversizePart::Payload,
                    ..
                }
            ),
            "the writer must refuse with BlobTooLarge, got: {err}"
        );
        assert!(err.to_string().contains("fewer elements per blob"), "{err}");
    }

    /// An oversize `WayMembers-v1` bitmap is a hard error, and the message
    /// says so. It cannot take the tagdata escape hatch: absence of the bitmap
    /// on one blob of a file whose HeaderBlock advertises the feature reads as
    /// "no way-member nodes here", which silently drops nodes.
    #[test]
    fn oversize_way_members_is_a_named_hard_error() {
        let way_members = vec![0xCDu8; 70_000];
        let tagdata = vec![0x11u8; 4_000];
        let mut buf = Vec::new();
        let err = encode_blob_header_into(
            "OSMData",
            1,
            None,
            Some(&tagdata),
            Some(&way_members),
            &mut buf,
        )
        .expect_err("an oversize WayMembers bitmap must not be silently dropped");
        let msg = err.to_string();
        assert!(
            msg.contains("WayMembers-v1 bitmap cannot be dropped"),
            "the error must name the undroppable field, got: {msg}"
        );
    }

    /// An oversized OSMHeader blob is named apart from a data blob, through
    /// the framing every writer uses, for both size guards: a payload over the
    /// raw limit, and a legal payload whose uncompressed `Blob` framing takes
    /// the body over its limit. A front end answers a data part with its
    /// element cap, which cannot shrink a header.
    #[test]
    fn an_oversized_header_blob_is_named_apart_from_a_data_blob() {
        let limit = usize::try_from(MAX_BLOB_MESSAGE_SIZE).expect("limit fits");
        let over_raw = vec![0x5Au8; limit + 1];
        let at_raw = vec![0x5Au8; limit];
        for (blob_type, payload_part, body_part) in [
            (
                "OSMHeader",
                OversizePart::OsmHeaderPayload,
                OversizePart::OsmHeaderBody,
            ),
            ("OSMData", OversizePart::Payload, OversizePart::Body),
        ] {
            for (payload, want) in [(&over_raw, payload_part), (&at_raw, body_part)] {
                let err = frame_blob(
                    blob_type,
                    payload,
                    &Compression::None,
                    None,
                    &WriterMetrics::new(),
                )
                .expect_err("an oversized blob must not be framed");
                assert_eq!(
                    oversize_part_and_size(&err).0,
                    want,
                    "{blob_type}, {} payload bytes: {err}",
                    payload.len()
                );
            }
        }
    }

    fn oversize_part_and_size(err: &Error) -> (OversizePart, u64) {
        match err.kind() {
            crate::ErrorKind::BlobTooLarge { part, size, .. } => (*part, *size),
            other => panic!("expected BlobTooLarge, got {other:?}"),
        }
    }

    /// The culprit is computed, not guessed from the bitmap's size: a small
    /// bitmap that tips an otherwise fitting header over the cap is named,
    /// and a large bitmap beside a header that overflows without it is not.
    /// The reported size is the failing header's, not the probe's.
    #[test]
    fn oversize_header_culprit_is_computed_from_sizes() {
        let cap = usize::try_from(MAX_BLOB_HEADER_SIZE).unwrap_or(usize::MAX);
        let mut buf = Vec::new();

        // Indexdata alone lands on the last legal byte; a 16-byte bitmap tips
        // it. Measure the fixed overhead once (the length varint stays 3 bytes
        // across this size range), then size the indexdata to fit exactly.
        let probe = vec![0u8; 60_000];
        encode_blob_header_into("OSMData", 1, Some(&probe), None, None, &mut buf)
            .expect("probe header must encode");
        let overhead = buf.len() - probe.len();
        let near_cap = vec![0xABu8; cap - 1 - overhead];
        let small_bitmap = [0xCDu8; 16];
        encode_blob_header_into("OSMData", 1, Some(&near_cap), None, None, &mut buf)
            .expect("the header without the bitmap must fit");
        let fitting_len = buf.len();
        assert_eq!(fitting_len, cap - 1);
        // A tag index rides along, so a size reported from before its drop
        // would show up as a mismatch below.
        let tagdata = [0x11u8; 100];
        let err = encode_blob_header_into(
            "OSMData",
            1,
            Some(&near_cap),
            Some(&tagdata),
            Some(&small_bitmap),
            &mut buf,
        )
        .expect_err("the bitmap pushes the header over the cap");
        let (part, size) = oversize_part_and_size(&err);
        assert_eq!(part, OversizePart::HeaderWayMembers);
        assert_eq!(size, (fitting_len + 2 + small_bitmap.len()) as u64);

        // Indexdata alone already overflows; a large bitmap is not the culprit.
        let over_cap = vec![0xABu8; cap];
        let big_bitmap = vec![0xCDu8; 4096];
        let err = encode_blob_header_into(
            "OSMData",
            1,
            Some(&over_cap),
            None,
            Some(&big_bitmap),
            &mut buf,
        )
        .expect_err("the header overflows without the bitmap");
        let (part, size) = oversize_part_and_size(&err);
        assert_eq!(part, OversizePart::Header);
        assert!(size > (cap + big_bitmap.len()) as u64, "size {size}");
    }

    /// The passthrough reframe assembles a frame, so it obeys the same body
    /// bound as a freshly encoded block.
    #[test]
    fn reframe_refuses_a_body_at_the_datasize_cap() {
        let cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap_or(usize::MAX);
        let index = [0u8; 42];
        let body = vec![0u8; cap];
        let metrics = WriterMetrics::new();
        let err = reframe_raw_with_index(&body, &index, None, &metrics)
            .expect_err("a body at the datasize cap must not be reframed");
        assert_eq!(oversize_part_and_size(&err).0, OversizePart::Body);
        reframe_raw_with_index(&body[..cap - 1], &index, None, &metrics)
            .expect("the largest legal body must reframe");
    }
}
