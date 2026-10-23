//! Blob framing and compression helpers. Produces the PBF on-wire format
//! `[4-byte BE header_len][BlobHeader bytes][Blob bytes]` from an uncompressed
//! block byte payload.
//!
//! The `FrameScratch` struct holds the reusable per-thread scratch buffers and
//! the lazy-initialized zlib/zstd compressors. `PIPELINE_SCRATCH` is the
//! thread-local instance used by the rayon-parallel pipelined path;
//! `PbfWriter` holds its own for the sync path.

use std::cell::RefCell;

use crate::error::{Error, Result};
use flate2::Compress;
use flate2::Compression as FlateCompression;
use flate2::FlushCompress;
use flate2::Status;
use protohoggr::{encode_bytes_field, encode_bytes_field_always, encode_int32_field};

use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE};
use crate::write::metrics::WRITER_METRICS;
use std::sync::atomic::Ordering::Relaxed;

use crate::read::wire_schema::{BlobHeaderField, HeaderBlockField, blob, blob_header};

use super::compression::Compression;
use super::pipeline::{FramedBlobParts, elapsed_ns_u64};

use crate::tunables::{MAX_RETAINED_SCRATCH_CAPACITY, ZLIB_RESERVE_SHIFT, ZLIB_RESERVE_SLACK};

/// Per-thread reusable blob framing scratch: header/body/compression buffers
/// plus lazy-initialized zlib/zstd compressors. Reused across ordinary blobs
/// to keep steady-state allocation flat. An oversized buffer is not retained
/// in a rayon thread for the process lifetime.
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
    /// Reuse capacity target for `blob_buf`, capped at
    /// `MAX_RETAINED_SCRATCH_CAPACITY`.
    blob_cap: usize,
    /// Reuse capacity target for `header_buf`, capped at
    /// `MAX_RETAINED_SCRATCH_CAPACITY`.
    header_cap: usize,
}

impl FrameScratch {
    pub(super) const fn new() -> Self {
        Self {
            blob_buf: Vec::new(),
            header_buf: Vec::new(),
            compress_buf: Vec::new(),
            zlib_compressor: None,
            zstd_compressor: None,
            blob_cap: 0,
            header_cap: 0,
        }
    }

    /// Hand out `buf`'s contents and leave a replacement behind. Preserve the
    /// high-water capacity for ordinary blocks, but do not pin an outlier
    /// buffer in this thread's scratch for the process lifetime.
    ///
    /// This is the capacity-preserving alternative to `buf.clone()`: the
    /// caller receives the bytes without a copy, and the scratch slot keeps
    /// its warmed-up capacity for ordinary blocks. Buffers above the 4 MiB
    /// retention limit are handed out with their contents and capacity intact,
    /// while the scratch slot starts empty for its next frame.
    fn take_with_capacity(buf: &mut Vec<u8>, cap: &mut usize) -> Vec<u8> {
        let observed_capacity = buf.capacity();
        if observed_capacity > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(observed_capacity);
            *cap = 0;
            return std::mem::take(buf);
        }
        *cap = (*cap)
            .max(observed_capacity)
            .min(MAX_RETAINED_SCRATCH_CAPACITY);
        std::mem::replace(buf, Vec::with_capacity(*cap))
    }

    pub(super) fn trim_oversize_retained_buffers(&mut self) {
        if self.blob_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.blob_buf.capacity());
            self.blob_buf = Vec::new();
            self.blob_cap = 0;
        }
        if self.header_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.header_buf.capacity());
            self.header_buf = Vec::new();
            self.header_cap = 0;
        }
        if self.compress_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.compress_buf.capacity());
            self.compress_buf = Vec::new();
        }
    }

    fn trim_oversize_compress_buffer(&mut self) {
        if self.compress_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            Self::record_capped_capacity(self.compress_buf.capacity());
            self.compress_buf = Vec::new();
        }
    }

    fn record_capped_capacity(capacity: usize) {
        WRITER_METRICS
            .frame_scratch_capped_buffers
            .fetch_add(1, Relaxed);
        WRITER_METRICS
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
/// `write_framed_blob` (sync) which reuse scratch buffers.
pub(crate) fn frame_blob(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
) -> Result<Vec<u8>> {
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
) -> Result<FramedBlobParts> {
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
/// Returns owned buffers suitable for sending through a pipeline channel.
///
/// # Per-call cost
///
/// The compression scratch (`compress_buf`) and both compressors are reused
/// across calls and allocate nothing after warmup, except that a
/// `compress_buf` grown past `MAX_RETAINED_SCRATCH_CAPACITY` is dropped after
/// its blob. The two buffers that *are*
/// handed to the caller - the `BlobHeader` and the `Blob` body - are swapped
/// out rather than copied out, so each call allocates exactly two
/// `Vec<u8>`s - the replacements installed in the scratch - and performs zero
/// memcpys of blob-sized data.
///
/// This block previously claimed the same steady-state behaviour while the
/// code below cloned both buffers: two allocations *and* two full memcpys per
/// blob, on the hottest write path there is.
///
/// **The swap is not a measured speedup.** Six interleaved matched cells of
/// `repack --dataset japan --compression none --bench 1` on `bygg` at commit
/// `eea35d27` gave swap 8.6 / 8.6 / 8.3 s against clone 8.3 / 8.5 / 8.5 s -
/// sign-inconsistent across the three pairs and inside the reported
/// granularity. `--compression none` is the arrangement that maximizes the
/// memcpy (the blob body is the whole uncompressed block), and it still did
/// not show. The copies are also spread across rayon workers, so they dilute
/// into wall time rather than adding to it. Keep the swap because the code
/// now does what the doc says and because it removes real per-blob work, not
/// because it made anything faster; do not cite a throughput win for it.
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
) -> Result<FramedBlobParts> {
    let t_compress = std::time::Instant::now();
    scratch.trim_oversize_retained_buffers();
    if let Err(error) = encode_blob_body(uncompressed, compression, scratch) {
        scratch.trim_oversize_retained_buffers();
        return Err(error);
    }
    WRITER_METRICS
        .compress_ns
        .fetch_add(elapsed_ns_u64(t_compress), Relaxed);
    let t_frame = std::time::Instant::now();

    // The format caps a Blob body at `MAX_BLOB_DATASIZE`, and our own reader
    // enforces it on the way back in ("blob header declares datasize too
    // big"). Without this guard the writer happily emitted a larger body and
    // the check fired only when someone later tried to *read* the file - found
    // by repacking Denmark at `--elements-per-blob 20000000`, which produced a
    // 143 MB blob body and a PBF that pbfhogg itself refuses to open. Fail at
    // the point where the knob that caused it can still be named.
    if scratch.blob_buf.len() as u64 >= MAX_BLOB_DATASIZE {
        let body_len = scratch.blob_buf.len();
        let error = Error::invalid_argument(format!(
            "blob body for {blob_type} is {body_len} bytes, must be smaller than \
             {MAX_BLOB_DATASIZE} (lower --elements-per-blob)"
        ));
        scratch.trim_oversize_retained_buffers();
        return Err(error);
    }
    let datasize = i32::try_from(scratch.blob_buf.len()).map_err(|_| {
        Error::internal(format!(
            "blob datasize overflow: {} bytes",
            scratch.blob_buf.len()
        ))
    })?;
    if let Err(error) = encode_blob_header_into(
        blob_type,
        datasize,
        indexdata,
        tagdata,
        way_members,
        &mut scratch.header_buf,
    ) {
        scratch.trim_oversize_retained_buffers();
        return Err(error);
    }
    // `compress_buf` is not handed off, so drop an outlier before the next
    // blob can reuse this thread-local scratch. If header encoding failed
    // above, the next call's entry trim handles it instead.
    scratch.trim_oversize_compress_buffer();

    let header_len = u32::try_from(scratch.header_buf.len()).map_err(|_| {
        Error::internal(format!(
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

    // Swap out, do not clone. A plain `swap` would leave the scratch holding
    // empty Vecs that re-grow from zero on every subsequent blob, which is why
    // this used to clone; `take_with_capacity` preserves the warmed capacity
    // for ordinary blocks, while the retention cap lets outliers warm up again
    // on demand. The two per-blob memcpys stay gone.
    let header = FrameScratch::take_with_capacity(&mut scratch.header_buf, &mut scratch.header_cap);
    let blob = FrameScratch::take_with_capacity(&mut scratch.blob_buf, &mut scratch.blob_cap);
    Ok(FramedBlobParts {
        prefix: header_len.to_be_bytes(),
        header,
        blob,
    })
}

/// Encode the Blob protobuf body (optionally compressed) into scratch buffers.
///
/// Uses `compress_buf` as an intermediate for zlib/zstd compression output.
/// Both zlib and zstd compressors are lazy-initialized and reused to avoid
/// ~312 KB (zlib) or ~512 KB (zstd) of compressor state allocation per blob.
///
/// # Why the payload field is always emitted
///
/// The payload (`raw`, `zlib_data`, or `zstd_data`) is written with the
/// `_always` encoder, not the skip-if-empty one. A `Blob` carries exactly one
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
pub(super) fn encode_blob_body(
    uncompressed: &[u8],
    compression: &Compression,
    scratch: &mut FrameScratch,
) -> Result<()> {
    scratch.blob_buf.clear();
    // Check once per blob, before compression or copying. The encoded-body
    // guard cannot catch a highly compressible oversized block. Element caps
    // do not bound bytes: even one element can carry a very large tag value.
    // Both sync and pipelined framing use this encoder. Exactly the raw limit
    // is legal; the separate encoded-body limit can still reject its framing.
    if uncompressed.len() as u64 > MAX_BLOB_MESSAGE_SIZE {
        return Err(Error::invalid_argument(format!(
            "uncompressed blob payload is {} bytes, must be at most \
             {MAX_BLOB_MESSAGE_SIZE} (lower --elements-per-blob or reduce element size)",
            uncompressed.len()
        )));
    }
    match compression {
        Compression::None => {
            // Blob field 1: raw (bytes, len-delimited). Always emitted; see
            // the doc block.
            encode_bytes_field_always(&mut scratch.blob_buf, blob::RAW, uncompressed);
        }
        Compression::Zlib(level) => {
            compress_zlib(uncompressed, *level, scratch)?;
        }
        Compression::Zstd(level) => {
            match &scratch.zstd_compressor {
                Some((cached_level, _)) if *cached_level == *level => {}
                _ => {
                    // Built lazily, per worker, on its first zstd blob. A
                    // parsed `Compression` (the CLI's argument boundary) has
                    // its level range-checked already, so from the CLI a
                    // failure here is a resource failure (zstd could not
                    // allocate its context), not a configuration error found
                    // late; only a library caller building
                    // `Compression::Zstd` by hand can reach it with a bad
                    // level.
                    scratch.zstd_compressor = Some((*level, zstd::bulk::Compressor::new(*level)?));
                }
            }
            let (_, compressor) = scratch.zstd_compressor.as_mut().expect("just initialized");
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
            encode_int32_field(&mut scratch.blob_buf, blob::RAW_SIZE, raw_size);
            // Blob field 7: zstd_data (bytes, len-delimited). Always emitted.
            encode_bytes_field_always(
                &mut scratch.blob_buf,
                blob::ZSTD_DATA,
                &scratch.compress_buf,
            );
        }
    }
    Ok(())
}

/// Zlib compression via reusable `flate2::Compress` with `reset()`.
fn compress_zlib(uncompressed: &[u8], level: u32, scratch: &mut FrameScratch) -> Result<()> {
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
    encode_int32_field(&mut scratch.blob_buf, blob::RAW_SIZE, raw_size);
    // Blob field 3: zlib_data. Always emitted; see `encode_blob_body`.
    encode_bytes_field_always(
        &mut scratch.blob_buf,
        blob::ZLIB_DATA,
        &scratch.compress_buf,
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
/// dropped index costs only the skip optimization. Drops are counted in
/// `WRITER_METRICS.tagdata_dropped_oversize` (sidecar counter
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
) -> Result<()> {
    encode_blob_header_fields(blob_type, datasize, indexdata, tagdata, way_members, buf);
    if (buf.len() as u64) < MAX_BLOB_HEADER_SIZE {
        return Ok(());
    }
    if tagdata.is_some() {
        // Retry without the skip index before giving up: see the doc block.
        encode_blob_header_fields(blob_type, datasize, indexdata, None, way_members, buf);
        if (buf.len() as u64) < MAX_BLOB_HEADER_SIZE {
            WRITER_METRICS
                .tagdata_dropped_oversize
                .fetch_add(1, Relaxed);
            return Ok(());
        }
    }
    let culprit = if way_members.is_some_and(|d| d.len() > 1024) {
        " (the WayMembers-v1 bitmap, which cannot be dropped without \
          misreporting way-member nodes; lower --elements-per-blob)"
    } else {
        " (lower --elements-per-blob)"
    };
    Err(Error::invalid_argument(format!(
        "BlobHeader for {blob_type} is {} bytes, must be smaller than \
         {MAX_BLOB_HEADER_SIZE}{culprit}",
        buf.len()
    )))
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
/// optional-feature list by design (`from_header` does not copy field 5).
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
/// This is used by merge to add blob-level index metadata to passthrough blobs
/// so that subsequent merges can classify them without decompression.
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut header_buf = Vec::new();
    reframe_raw_with_index_scratch(blob_bytes, indexdata, tagdata, &mut header_buf)
}

pub(crate) fn reframe_raw_with_index_scratch(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        Error::internal(format!(
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
        .map_err(|_| Error::internal(format!("header too large: {} bytes", header_buf.len())))?;
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
    fn oversized_raw_payload_is_refused_before_compression() {
        let payload = vec![0; usize::try_from(MAX_BLOB_MESSAGE_SIZE).expect("limit fits") + 1];
        for compression in [
            Compression::None,
            Compression::Zlib(1),
            Compression::Zstd(1),
        ] {
            let mut scratch = FrameScratch::new();
            let error = encode_blob_body(&payload, &compression, &mut scratch)
                .expect_err("compression must not hide an oversized raw block");
            assert!(error.to_string().contains("uncompressed blob payload"));
            assert!(scratch.blob_buf.is_empty());
            assert!(scratch.compress_buf.is_empty());
            assert!(scratch.zlib_compressor.is_none());
            assert!(scratch.zstd_compressor.is_none());
        }
    }

    #[test]
    fn compressed_payload_at_raw_limit_can_be_framed() {
        let payload = vec![0; usize::try_from(MAX_BLOB_MESSAGE_SIZE).expect("limit fits")];
        for compression in [Compression::Zlib(1), Compression::Zstd(1)] {
            frame_blob("OSMData", &payload, &compression, None)
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
    /// payload field is non-empty whichever encoder writes it, and reverting
    /// those two call sites to `encode_bytes_field` would leave this test
    /// green. `Compression::None` is the arm that carries the property: it is
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
            encode_blob_body(&[], &compression, &mut scratch).expect("empty body must encode");
            assert!(
                !scratch.blob_buf.is_empty(),
                "{compression:?}: an empty payload produced a Blob message \
                 with no fields at all"
            );
            // Field 1 (raw) / 3 (zlib_data) / 7 (zstd_data).
            let payload_field: u32 = match compression {
                Compression::None => 1,
                Compression::Zlib(_) => 3,
                Compression::Zstd(_) => 7,
            };
            let present = present_field_numbers(&scratch.blob_buf);
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
    /// swap `encode_bytes_field_always` back to `encode_bytes_field` in the
    /// `Compression::None` arm of `encode_blob_body` and the output becomes
    /// zero bytes instead of these two.
    #[test]
    fn no_payload_bytes_reach_the_output_uncompressed() {
        let mut scratch = FrameScratch::new();
        encode_blob_body(&[], &Compression::None, &mut scratch).expect("empty body must encode");
        assert_eq!(
            scratch.blob_buf.as_slice(),
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

    /// Framing must hand out its buffers without copying them, and must not
    /// leave the scratch to re-grow from zero.
    ///
    /// Both halves matter and each on its own is satisfiable by the bug the
    /// other one describes: `clone()` preserves the scratch capacity but pays
    /// two memcpys per blob, and a bare `swap` skips the memcpys but empties
    /// the scratch. The observable proxy for "swapped, not cloned" is that
    /// the scratch buffer no longer holds the bytes it just produced.
    #[test]
    fn framing_swaps_buffers_out_and_keeps_scratch_capacity() {
        let mut scratch = FrameScratch::new();
        let payload = vec![7u8; 64 * 1024];
        let first = frame_blob_into(
            "OSMData",
            &payload,
            &Compression::None,
            None,
            None,
            None,
            &mut scratch,
        )
        .expect("first blob must frame");
        assert!(!first.blob.is_empty(), "framed blob must carry bytes");
        assert!(
            scratch.blob_buf.is_empty() && scratch.header_buf.is_empty(),
            "buffers must be swapped out, not cloned out"
        );
        let warmed_blob_cap = scratch.blob_buf.capacity();
        let warmed_header_cap = scratch.header_buf.capacity();
        assert!(
            warmed_blob_cap >= payload.len(),
            "the replacement blob buffer must be reserved to the high-water \
             capacity, got {warmed_blob_cap} for a {} byte payload",
            payload.len()
        );
        assert!(
            warmed_header_cap > 0,
            "the replacement header buffer must be reserved, got 0"
        );

        // A second, smaller blob must neither re-grow nor shrink the scratch.
        let second = frame_blob_into(
            "OSMData",
            &[1, 2, 3],
            &Compression::None,
            None,
            None,
            None,
            &mut scratch,
        )
        .expect("second blob must frame");
        assert_eq!(second.blob.len(), 5, "3 payload bytes plus tag and length");
        assert!(
            scratch.blob_buf.capacity() >= warmed_blob_cap,
            "scratch capacity regressed from {warmed_blob_cap} to {}",
            scratch.blob_buf.capacity()
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
    /// `HeaderBuilder::from_header` rebuild would silently drop.
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
    /// and `parse_blob_header_with_index` are two halves of the same model, so
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
        let before = WRITER_METRICS.tagdata_dropped_oversize.load(Relaxed);
        let indexdata = vec![0xABu8; 42];
        let way_members = vec![0xCDu8; 64];
        let tagdata = vec![0x11u8; 70_000];
        let mut buf = Vec::new();
        encode_blob_header_into(
            "OSMData",
            4242,
            Some(&indexdata),
            Some(&tagdata),
            Some(&way_members),
            &mut buf,
        )
        .expect("an oversize tag index must be dropped, never fatal");

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
            Some(indexdata),
            "indexdata must survive the retry"
        );
        assert_eq!(
            fields.iter().find(|(f, _)| *f == 5).map(|(_, v)| v.clone()),
            Some(way_members),
            "WayMembers-v1 must survive the retry"
        );
        // Drop three more, so the observed delta cannot be explained by the
        // single drop the sibling boundary test performs concurrently.
        for _ in 0..3 {
            encode_blob_header_into("OSMData", 1, None, Some(&tagdata), None, &mut buf)
                .expect("oversize tag index must be dropped");
        }
        let delta = WRITER_METRICS.tagdata_dropped_oversize.load(Relaxed) - before;
        assert!(
            delta >= 4,
            "a dropped tag index must be counted, not lost silently; \
             saw only {delta} of 4 drops"
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
    /// that made it. The writer knows both.
    #[test]
    fn an_oversize_blob_body_fails_at_write_time_naming_the_knob() {
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
        );
        let Err(err) = result else {
            panic!("a blob body past the datasize cap must not be written");
        };
        let msg = err.to_string();
        assert!(
            (msg.contains("blob body for OSMData") || msg.contains("uncompressed blob payload"))
                && msg.contains("--elements-per-blob"),
            "the writer must name the cap and the knob, got: {msg}"
        );
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
            msg.contains("WayMembers-v1 bitmap, which cannot be dropped"),
            "the error must name the undroppable field, got: {msg}"
        );
    }
}
