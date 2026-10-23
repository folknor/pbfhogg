//! Blob decompression helpers: the shared `DecompressPool` for buffer reuse,
//! the zlib thread-local state, and the suite of `decompress_*` entry points
//! called from pipelined and pread read paths.

use super::wire_schema::blob as blob_fields;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use flate2::Decompress;
use std::cell::RefCell;

use crate::error::{BlobError, Error, Result, new_blob_error};
use crate::tunables::{
    DECOMPRESS_EXPANSION_GUESS, DECOMPRESS_POOL_MAX_BUFFERS, DECOMPRESS_POOL_MAX_RETAINED_CAPACITY,
};

use super::blob_wire::{BlobData, MAX_BLOB_MESSAGE_SIZE, WireBlob};
use super::pipeline_metrics::PIPELINE_METRICS;

thread_local! {
    /// Per-thread reusable zlib decompressor state (~32 KB inflate tables).
    /// Reset via `Decompress::reset(true)` between blobs instead of allocating
    /// a fresh instance each time.
    static ZLIB_DECOMPRESS: RefCell<Decompress> = RefCell::new(Decompress::new(true));
}

// Buffers rejected by either pool limit (`DECOMPRESS_POOL_MAX_RETAINED_CAPACITY`,
// `DECOMPRESS_POOL_MAX_BUFFERS`) are dropped and counted for the read sidecar.

/// Thread-safe pool of decompression buffers for reuse across pipeline blobs.
///
/// In the pipelined read path, each blob decompresses into a fresh `Vec<u8>`
/// (~1.4 MB average), which is then wrapped as `Bytes` for zero-copy protobuf
/// parsing. Without pooling, this causes 10.2 GB of cumulative alloc/dealloc for
/// Denmark (483 MB), or ~1.7 TB for a planet file.
///
/// The pool holds `Vec<u8>` buffers returned by [`PooledBuffer`]'s `Drop` impl.
/// Buffers are popped via [`DecompressPool::get`] and returned automatically when
/// the `Bytes` (and the `PrimitiveBlock` holding slices of it) is dropped.
pub(crate) struct DecompressPool {
    buffers: Mutex<Vec<Vec<u8>>>,
}

impl DecompressPool {
    /// Create a new empty pool wrapped in `Arc` for shared ownership.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            buffers: Mutex::new(Vec::new()),
        })
    }

    /// Pop a buffer from the pool, or return an empty Vec if the pool is empty.
    // wontfix(type-result-fallible): .ok() on Mutex::lock() - poisoning means another
    // thread panicked; falling back to a fresh Vec is the correct recovery here.
    pub fn get(&self) -> Vec<u8> {
        self.buffers
            .lock()
            .ok()
            .and_then(|mut v| v.pop())
            .unwrap_or_default()
    }

    /// Return a buffer to the pool for reuse.
    ///
    /// Drops oversized buffers (capacity > 4 MB) instead of retaining them,
    /// preventing outlier blobs from permanently inflating pool memory.
    /// Also enforces a count cap as defense-in-depth.
    fn put(&self, mut buf: Vec<u8>) {
        let capacity = buf.capacity();
        if capacity > DECOMPRESS_POOL_MAX_RETAINED_CAPACITY {
            PIPELINE_METRICS.record_decompress_pool_oversize_drop(capacity);
            return;
        }
        buf.clear();
        match self.buffers.lock() {
            Ok(mut buffers) if buffers.len() < DECOMPRESS_POOL_MAX_BUFFERS => buffers.push(buf),
            Ok(_) => PIPELINE_METRICS.record_decompress_pool_full_drop(capacity),
            Err(_) => PIPELINE_METRICS.record_decompress_pool_poisoned_drop(capacity),
        }
    }
}

/// Owner type for `Bytes::from_owner` that returns its buffer to a
/// [`DecompressPool`] on drop instead of freeing it.
struct PooledBuffer {
    vec: Vec<u8>,
    pool: Arc<DecompressPool>,
}

impl AsRef<[u8]> for PooledBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.vec
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        let v = std::mem::take(&mut self.vec);
        self.pool.put(v);
    }
}

/// Get a decompression buffer - from the pool if available, otherwise fresh.
pub(crate) fn pool_get(pool: Option<&Arc<DecompressPool>>, capacity: usize) -> Vec<u8> {
    match pool {
        Some(p) => {
            let mut buf = p.get();
            // Pooled buffers are cleared on return. `reserve` is relative to
            // length, so reserve the full hint to ensure the buffer can hold it
            // before inflation.
            buf.reserve(capacity);
            buf
        }
        None => Vec::with_capacity(capacity),
    }
}

/// Wrap decoded bytes as `Bytes` - returning to pool on drop if pooled.
pub(crate) fn pool_wrap(decoded: Vec<u8>, pool: Option<&Arc<DecompressPool>>) -> Bytes {
    match pool {
        Some(p) => Bytes::from_owner(PooledBuffer {
            vec: decoded,
            pool: Arc::clone(p),
        }),
        None => Bytes::from(decoded),
    }
}

/// Clamp a writer-declared `raw_size` capacity hint to the decompressed-content
/// cap.
///
/// `raw_size` is an unvalidated `int32` read straight off the wire, so a hostile
/// or corrupt blob can declare up to 2 GiB and drive the `Vec::reserve` that
/// precedes decompression into an outsized allocation - before the inflation
/// bound below ever runs. No legitimate blob decompresses past
/// `MAX_BLOB_MESSAGE_SIZE`, so any hint above it is a lie and clamping loses
/// nothing: a truthful hint below the cap still pre-sizes exactly.
pub(super) fn clamp_capacity_hint(hint: usize) -> usize {
    #[allow(clippy::cast_possible_truncation)]
    let cap = MAX_BLOB_MESSAGE_SIZE as usize;
    hint.min(cap)
}

/// Decompress zstd data into `buf`, bounded during inflation.
///
/// `zstd::stream::copy_decode` runs to completion before any size check can
/// fire, so a small blob declaring a huge dictionary-free expansion (zstd
/// reaches ratios around 10^5 trivially) would exhaust memory ahead of the
/// guard. Reading through a `take` capped at one byte past
/// `MAX_BLOB_MESSAGE_SIZE` bounds the allocation at the cap and still leaves
/// exactly enough evidence (one surplus byte) to distinguish "at the cap" from
/// "over the cap".
fn zstd_decompress_into(compressed: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    use std::io::Read as _;

    let start = buf.len();
    let limit = MAX_BLOB_MESSAGE_SIZE + 1;
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed))?;
    decoder.take(limit).read_to_end(buf)?;
    let size = (buf.len() - start) as u64;
    if size > MAX_BLOB_MESSAGE_SIZE {
        return Err(new_blob_error(BlobError::MessageTooBig { size }));
    }
    Ok(())
}

/// Decompress zlib data into `buf` using the thread-local reusable decompressor.
///
/// Resets the decompressor state between calls instead of allocating a fresh
/// ~32 KB inflate state per blob.
///
/// Two adversarial-input bounds are enforced *by construction*, both inside the
/// inflate loop rather than after it:
///
/// * **Output size.** Growth stops one byte past `MAX_BLOB_MESSAGE_SIZE` and
///   the surplus byte is reported as `MessageTooBig`. Inflating a decompression
///   bomb to completion in order to discover it was too big has already done
///   the damage the cap exists to prevent.
/// * **Progress.** `Decompress::decompress_vec` reports a truncated deflate
///   stream (input exhausted with no final block) as `Ok(Status::BufError)`,
///   not `Err`. With the input empty and output space still available, the loop
///   would then consume nothing, produce nothing and never see `StreamEnd` -
///   spinning at 100 % CPU forever. An iteration that moves neither counter is
///   terminal and surfaces as a typed error.
///
/// # Why the budget cannot be left to the destination buffer
///
/// `decompress_vec` fills the destination to its *capacity*, so capacity - not
/// any limit this function computes - is what actually bounds one inflate step.
/// Every caller here hands in a reusable, caller-owned buffer whose capacity is
/// a function of what previous blobs happened to need, so a buffer that already
/// carries more than `MAX_BLOB_MESSAGE_SIZE` of spare capacity would let a bomb
/// inflate to that capacity in a single call, before the loop ever re-checked.
/// A bound a caller's allocation history can defeat is not a bound, so the
/// spare capacity is brought under the budget first and only then inflated
/// into.
// MAX_BLOB_MESSAGE_SIZE is 32 MiB, so the cast is exact on any target this
// crate builds for; the same allow covers `clamp_capacity_hint` above.
#[allow(clippy::cast_possible_truncation)]
fn zlib_decompress_into(compressed: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    let budget = MAX_BLOB_MESSAGE_SIZE as usize + 1;

    if buf.capacity() - buf.len() > budget {
        // Rare: only reachable when a pooled buffer has been grown past 32 MiB
        // of spare capacity by an earlier blob. Inflate through a fresh
        // destination - whose capacity starts at zero and is grown only by the
        // budget-respecting loop below - then append. The extra copy costs
        // nothing on the hot path because the branch is not taken there.
        let mut bounded = Vec::new();
        zlib_inflate_bounded(compressed, &mut bounded, budget)?;
        buf.extend_from_slice(&bounded);
        return Ok(());
    }

    zlib_inflate_bounded(compressed, buf, budget)
}

/// Inflate `compressed` onto the end of `buf`, producing at most `budget` bytes.
///
/// # Precondition
///
/// `buf.capacity() - buf.len() <= budget`. This is what makes the budget real:
/// `decompress_vec` writes up to `capacity`, so an oversized spare capacity
/// would overshoot the budget within a single call no matter what the loop
/// checks afterwards. [`zlib_decompress_into`] establishes the precondition;
/// the `debug_assert` pins it for any future caller.
#[allow(clippy::cast_possible_truncation)] // total_in delta bounded by input.len() (usize)
fn zlib_inflate_bounded(compressed: &[u8], buf: &mut Vec<u8>, budget: usize) -> Result<()> {
    ZLIB_DECOMPRESS.with_borrow_mut(|decompress| {
        decompress.reset(true);
        let start = buf.len();
        let limit = start + budget;
        debug_assert!(
            buf.capacity() <= limit,
            "zlib_inflate_bounded requires spare capacity within the budget"
        );
        let mut input = compressed;
        loop {
            if buf.len() >= limit {
                // Inflation is stopped at the detection point rather than run
                // to completion, so the true decompressed size is unknown by
                // design; `size` reports the one-byte-past-the-cap boundary.
                // For a payload that is merely one byte over, that is the exact
                // size - matching the Raw and Zstd branches.
                buf.truncate(limit);
                return Err(new_blob_error(BlobError::MessageTooBig {
                    size: MAX_BLOB_MESSAGE_SIZE + 1,
                }));
            }
            if buf.len() == buf.capacity() {
                // Double, but never reserve past the bound: `decompress_vec`
                // fills to `capacity`, so capacity is what actually caps a
                // single inflate step's output.
                let want = buf.capacity().max(4096).min(limit - buf.len());
                buf.reserve_exact(want);
            }
            let before_in = decompress.total_in();
            let before_out = buf.len();
            let status = decompress
                .decompress_vec(input, buf, flate2::FlushDecompress::None)
                .map_err(|e| Error::invalid_data_with("zlib decompress error", e))?;
            let consumed = (decompress.total_in() - before_in) as usize;
            input = &input[consumed..];
            if matches!(status, flate2::Status::StreamEnd) {
                break;
            }
            if consumed == 0 && buf.len() == before_out {
                // No input consumed, no output produced, no stream end: the
                // deflate stream cannot be completed from the bytes we have.
                return Err(Error::io_kind(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "zlib stream ended before the final deflate block: \
                         {} compressed bytes consumed, {} bytes decompressed",
                        decompress.total_in(),
                        buf.len() - start
                    ),
                ));
            }
        }
        let size = (buf.len() - start) as u64;
        if size > MAX_BLOB_MESSAGE_SIZE {
            return Err(new_blob_error(BlobError::MessageTooBig { size }));
        }
        Ok(())
    })
}

/// Decompress raw blob bytes into a caller-provided buffer.
///
/// Takes raw bytes (not a parsed `WireBlob`) and decompresses directly,
/// allocating a new `Vec` each time. For loops that decompress many blobs,
/// this avoids repeated large allocations -- the buffer grows to high-water
/// mark and stays there.
pub(crate) fn decompress_blob_data_into(blob_bytes: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    let blob = WireBlob::parse_slice(blob_bytes)?;
    decompress_parsed_blob_into(&blob, buf)
}

/// Decompress a parsed Blob protobuf into a caller-provided buffer.
#[allow(clippy::cast_sign_loss)]
pub(super) fn decompress_parsed_blob_into(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
    decompress_parsed_blob_body(blob, buf)?;
    // A `raw_size` that disagrees with what actually came out is a corruption
    // signal, not a stale hint - see `check_raw_size`.
    super::blob_wire::check_raw_size(blob.raw_size, buf.len())
}

#[allow(clippy::cast_sign_loss)]
fn decompress_parsed_blob_body(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
    buf.clear();
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            // Reject only when strictly greater than the cap, so a payload at
            // exactly MAX_BLOB_MESSAGE_SIZE is accepted - matching the Zlib and
            // Zstd branches below (which error on `size > MAX`) and the sibling
            // `decompress_wire_blob_into` / `decompress_blob_raw` / `decompress_blob`
            // Raw checks. This gives every decompression helper identical
            // boundary semantics; the earlier strict `<` here made the pipelined
            // path (via `decompress_blob_data_into`) reject an exact-cap Raw blob
            // that the sequential and pread paths accepted.
            let size = bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            } else {
                buf.extend_from_slice(bytes);
                Ok(())
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            let cap = blob.estimated_capacity();
            if cap > 0 {
                // `buf` was cleared above, so reserve the full hint to ensure
                // it can hold the decoded payload before inflation.
                buf.reserve(cap);
            }
            zlib_decompress_into(bytes, buf)
        }
        Some(BlobData::Zstd(bytes)) => {
            let cap = blob.estimated_capacity();
            if cap > 0 {
                // `buf` was cleared above, so reserve the full hint to ensure
                // it can hold the decoded payload before inflation.
                buf.reserve(cap);
            }
            zstd_decompress_into(bytes, buf)
        }
        None => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Decompress raw blob bytes (from pread) into a caller-owned buffer.
///
/// Parses the blob wire format inline and decompresses the payload without
/// constructing intermediate `WireBlob` or `Bytes` objects. Used by parallel
/// pipelines where workers read blob data via pread and need all alloc/free
/// to stay thread-local.
#[hotpath::measure]
pub(crate) fn decompress_blob_raw(raw_blob: &[u8], buf: &mut Vec<u8>) -> Result<()> {
    use super::wire::Cursor;
    buf.clear();

    let mut cursor = Cursor::new(raw_blob);
    let mut raw_size: Option<i32> = None;
    // The payload field, recorded rather than acted on: `raw_size` (field 2)
    // may follow the payload on the wire, and the whole envelope has to be
    // walked anyway to detect a second payload field. This loop used to
    // `return` on the first `raw`, which both skipped the multi-field check
    // and made this parser disagree with `WireBlob::parse` (last-field-wins)
    // about which payload an adversarial two-field blob carried.
    let mut payload: Option<(u32, &[u8])> = None;

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match field {
            blob_fields::RAW | blob_fields::ZLIB_DATA | blob_fields::ZSTD_DATA => {
                let slice = cursor.read_len_delimited()?;
                if payload.is_some() {
                    return Err(crate::read::blob_wire::multiple_payload_fields());
                }
                payload = Some((field, slice));
            }
            blob_fields::RAW_SIZE => {
                // Validate before narrowing so 2^32 + N cannot alias N and
                // pass the decompressed-length check as an honest size.
                raw_size = Some(super::wire::read_int32(
                    &mut cursor,
                    "Blob.raw_size exceeds int32",
                )?);
            }
            _ => cursor.skip_field(wire_type)?,
        }
    }

    let Some((field, slice)) = payload else {
        return Err(new_blob_error(BlobError::Empty));
    };
    if field == blob_fields::RAW {
        let size = slice.len() as u64;
        if size > MAX_BLOB_MESSAGE_SIZE {
            return Err(new_blob_error(BlobError::MessageTooBig { size }));
        }
        buf.extend_from_slice(slice);
    } else {
        if let Some(rs) = raw_size
            && rs > 0
        {
            #[allow(clippy::cast_sign_loss)]
            // `buf` was cleared at function entry; reserve the full clamped
            // hint because `reserve` counts additional bytes from its length.
            buf.reserve(clamp_capacity_hint(rs as usize));
        }
        if field == blob_fields::ZLIB_DATA {
            zlib_decompress_into(slice, buf)?;
        } else {
            zstd_decompress_into(slice, buf)?;
        }
    }
    crate::read::blob_wire::check_raw_size(raw_size, buf.len())
}

/// Decompress a parsed WireBlob into an owned `Bytes`.
pub(crate) fn decompress_blob(
    blob: &WireBlob,
    pool: Option<&Arc<DecompressPool>>,
) -> Result<Bytes> {
    let decoded = decompress_blob_body(blob, pool)?;
    super::blob_wire::check_raw_size(blob.raw_size, decoded.len())?;
    Ok(decoded)
}

fn decompress_blob_body(blob: &WireBlob, pool: Option<&Arc<DecompressPool>>) -> Result<Bytes> {
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                Err(new_blob_error(BlobError::MessageTooBig { size }))
            } else {
                Ok(bytes.clone())
            }
        }
        Some(BlobData::Zlib(bytes)) => {
            let est = blob.estimated_capacity();
            let capacity = if est > 0 {
                est
            } else {
                clamp_capacity_hint(bytes.len().saturating_mul(DECOMPRESS_EXPANSION_GUESS))
            };
            let mut decoded_bytes = pool_get(pool, capacity);
            zlib_decompress_into(bytes, &mut decoded_bytes)?;
            Ok(pool_wrap(decoded_bytes, pool))
        }
        Some(BlobData::Zstd(bytes)) => {
            let est = blob.estimated_capacity();
            let capacity = if est > 0 {
                est
            } else {
                clamp_capacity_hint(bytes.len().saturating_mul(DECOMPRESS_EXPANSION_GUESS))
            };
            let mut decoded_bytes = pool_get(pool, capacity);
            zstd_decompress_into(bytes, &mut decoded_bytes)?;
            Ok(pool_wrap(decoded_bytes, pool))
        }
        None => Err(new_blob_error(BlobError::Empty)),
    }
}

/// Decompress a WireBlob into a caller-owned buffer.
///
/// Avoids the Bytes->Vec round-trip of `decompress_blob()` + `.to_vec()`.
/// The buffer is cleared and refilled; its backing allocation is retained
/// across calls for sequential decode loops.
pub(crate) fn decompress_wire_blob_into(blob: &WireBlob, buf: &mut Vec<u8>) -> Result<()> {
    buf.clear();
    match &blob.data {
        Some(BlobData::Raw(bytes)) => {
            let size = bytes.len() as u64;
            if size > MAX_BLOB_MESSAGE_SIZE {
                return Err(new_blob_error(BlobError::MessageTooBig { size }));
            }
            buf.extend_from_slice(bytes);
        }
        Some(BlobData::Zlib(bytes)) => {
            let est = blob.estimated_capacity();
            if est > 0 {
                buf.reserve(est);
            }
            zlib_decompress_into(bytes, buf)?;
        }
        Some(BlobData::Zstd(bytes)) => {
            let est = blob.estimated_capacity();
            if est > 0 {
                buf.reserve(est);
            }
            zstd_decompress_into(bytes, buf)?;
        }
        None => return Err(new_blob_error(BlobError::Empty)),
    }
    super::blob_wire::check_raw_size(blob.raw_size, buf.len())
}

// Tests use `unwrap()` throughout because panicking is the correct failure mode
// for unit tests. See the note in `blob_wire.rs`'s test module for the rationale
// and the crate-wide `unwrap_used = "deny"` exemption.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::{
        BlobData, DecompressPool, MAX_BLOB_MESSAGE_SIZE, WireBlob, decompress_blob,
        decompress_parsed_blob_into, decompress_wire_blob_into, pool_get,
    };
    use crate::error::{BlobError, ErrorKind};
    use bytes::Bytes;

    /// Which `BlobData` variant a boundary case exercises.
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Raw,
        Zlib,
        Zstd,
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write as _;
        let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn zstd_compress(data: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(std::io::Cursor::new(data), 1).unwrap()
    }

    #[test]
    fn pooled_buffer_reserves_the_requested_capacity() {
        let pool = DecompressPool::new();
        let buffer = Vec::with_capacity(1024);
        let previous_capacity = buffer.capacity();
        pool.put(buffer);

        let requested_capacity = previous_capacity + 1;
        let buffer = pool_get(Some(&pool), requested_capacity);

        assert!(
            buffer.capacity() >= requested_capacity,
            "pooled buffer capacity {} is below requested hint {requested_capacity}",
            buffer.capacity()
        );
    }

    /// Build a `WireBlob` whose *decompressed* payload is exactly `payload`.
    /// The Raw variant stores it verbatim; Zlib/Zstd store the compressed form
    /// and declare `raw_size` so the helpers pre-size their output buffer.
    fn make_blob(kind: Kind, payload: &[u8]) -> WireBlob {
        let raw_size = Some(i32::try_from(payload.len()).unwrap());
        match kind {
            Kind::Raw => WireBlob {
                data: Some(BlobData::Raw(Bytes::copy_from_slice(payload))),
                raw_size,
            },
            Kind::Zlib => WireBlob {
                data: Some(BlobData::Zlib(Bytes::from(zlib_compress(payload)))),
                raw_size,
            },
            Kind::Zstd => WireBlob {
                data: Some(BlobData::Zstd(Bytes::from(zstd_compress(payload)))),
                raw_size,
            },
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "call sites pass fresh results; the error is only inspected"
    )]
    fn assert_too_big(result: crate::error::Result<()>, expected_size: u64) {
        match result.as_ref().map_err(crate::error::Error::kind) {
            Err(ErrorKind::Blob(BlobError::MessageTooBig { size })) => {
                assert_eq!(
                    *size, expected_size,
                    "surfaced size must be the payload size"
                );
            }
            other => panic!("expected MessageTooBig, got {other:?}"),
        }
    }

    /// A decompressed payload at exactly `MAX_BLOB_MESSAGE_SIZE` is accepted, and
    /// one byte over is rejected with `MessageTooBig`, for Raw, Zlib, and Zstd -
    /// across every helper that decodes a `WireBlob`. This pins the boundary the
    /// copy-removal refactor had to preserve: the sequential
    /// (`decompress_wire_blob_into`) and pipelined (`decompress_parsed_blob_into`,
    /// via `decompress_blob_data_into`) routes must agree with each other and with
    /// the `decompress_blob` Bytes route on the exact-cap decision. Before the
    /// fix, `decompress_parsed_blob_into` used a strict `<` for Raw and rejected an
    /// exact-cap Raw blob the other helpers accepted.
    #[test]
    fn decompress_helpers_agree_at_message_size_boundary() {
        let cap = MAX_BLOB_MESSAGE_SIZE as usize;

        for kind in [Kind::Raw, Kind::Zlib, Kind::Zstd] {
            // Exactly at the cap: every helper accepts and yields `cap` bytes.
            let at_cap = vec![0u8; cap];
            let blob = make_blob(kind, &at_cap);

            let mut into = Vec::new();
            decompress_wire_blob_into(&blob, &mut into)
                .unwrap_or_else(|e| panic!("{kind:?} wire_blob_into at cap: {e:?}"));
            assert_eq!(into.len(), cap, "{kind:?} wire_blob_into length at cap");

            let mut parsed = Vec::new();
            decompress_parsed_blob_into(&blob, &mut parsed)
                .unwrap_or_else(|e| panic!("{kind:?} parsed_blob_into at cap: {e:?}"));
            assert_eq!(parsed.len(), cap, "{kind:?} parsed_blob_into length at cap");

            let bytes = decompress_blob(&blob, None)
                .unwrap_or_else(|e| panic!("{kind:?} decompress_blob at cap: {e:?}"));
            assert_eq!(bytes.len(), cap, "{kind:?} decompress_blob length at cap");

            // One byte over the cap: every helper rejects with the same typed
            // error and surfaces the over-cap size verbatim.
            let over_len = cap + 1;
            let over = vec![0u8; over_len];
            let blob = make_blob(kind, &over);

            let mut into = Vec::new();
            assert_too_big(decompress_wire_blob_into(&blob, &mut into), over_len as u64);

            let mut parsed = Vec::new();
            assert_too_big(
                decompress_parsed_blob_into(&blob, &mut parsed),
                over_len as u64,
            );

            assert_too_big(decompress_blob(&blob, None).map(|_| ()), over_len as u64);
        }
    }

    /// Hand-encode a `Blob` protobuf from `(field, payload)` pairs, so a test
    /// can build the two-payload-field envelope no writer produces.
    fn encode_blob(fields: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (field, payload) in fields {
            out.push(u8::try_from((field << 3) | 2).unwrap());
            let mut len = payload.len() as u64;
            loop {
                let mut byte = (len & 0x7f) as u8;
                len >>= 7;
                if len != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if len == 0 {
                    break;
                }
            }
            out.extend_from_slice(payload);
        }
        out
    }

    /// A `Blob` carrying two payload fields is rejected by both parsers.
    ///
    /// The three payload fields are three encodings of one payload, so a blob
    /// carrying two of them has no defined content. The two parsers used to
    /// disagree about it and neither rejected it: `WireBlob::parse` was
    /// last-field-wins, so `raw` + `zlib_data` decoded as the zlib content,
    /// while `decompress_blob_raw` returned on the first `raw` field it saw
    /// and decoded the same bytes as the raw content. One adversarial blob,
    /// two different blocks, decided by which read path reached it.
    ///
    /// The fixture puts distinguishable content in each field precisely so a
    /// regression that picks a winner instead of rejecting shows up as the
    /// wrong bytes rather than as a silent pass: `decompress_blob_raw` writes
    /// into `out`, so its content is asserted too.
    #[test]
    fn a_blob_with_two_payload_fields_is_rejected_by_both_parsers() {
        let zlib = zlib_compress(b"from-zlib");
        let blob = encode_blob(&[(1, b"from-raw"), (3, &zlib)]);

        let mut out = Vec::new();
        let err = super::decompress_blob_raw(&blob, &mut out)
            .expect_err("decompress_blob_raw must reject a two-payload blob");
        assert!(
            matches!(err.kind(), ErrorKind::WireFormat { .. }),
            "a multi-payload Blob must be a wire-format error"
        );
        assert!(
            out.is_empty(),
            "no payload may be produced for an ambiguous blob, got {out:?}"
        );

        WireBlob::parse_slice(&blob)
            .expect_err("WireBlob::parse must reject a two-payload blob too");

        // Field order reversed: the defect was order-dependent, so both orders
        // have to be covered or the test only pins half the fix.
        let reversed = encode_blob(&[(3, &zlib), (1, b"from-raw")]);
        let mut out = Vec::new();
        super::decompress_blob_raw(&reversed, &mut out)
            .expect_err("decompress_blob_raw must reject either field order");
        WireBlob::parse_slice(&reversed)
            .expect_err("WireBlob::parse must reject either field order");

        // A single payload field with the same content still decodes, so the
        // rejection costs a conforming blob nothing.
        let single = encode_blob(&[(1, b"from-raw")]);
        let mut out = Vec::new();
        super::decompress_blob_raw(&single, &mut out).expect("a single-payload blob still decodes");
        assert_eq!(out, b"from-raw");
    }

    /// Pin int32 bounds in both envelope parsers, in either field order.
    #[test]
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    fn raw_size_is_validated_before_narrowing() {
        let payload = b"size";
        let honest = payload.len() as u64;
        for declared in [
            honest,
            0,
            i32::MAX as u64,
            i32::MAX as u64 + 1,
            (1_u64 << 32) + honest,
            (1_u64 << 32) + i32::MAX as u64,
            u64::MAX, // Sign-extended -1 is a legal int32, but a wrong size.
            i32::MIN as i64 as u64,
        ] {
            let in_range = i32::try_from(declared as i64).is_ok();
            let mut size_field = vec![0x10];
            let mut value = declared;
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                size_field.push(byte | if value == 0 { 0 } else { 0x80 });
                if value == 0 {
                    break;
                }
            }
            for size_first in [false, true] {
                let mut encoded = encode_blob(&[(1, payload)]);
                if size_first {
                    encoded.splice(0..0, size_field.iter().copied());
                } else {
                    encoded.extend_from_slice(&size_field);
                }
                let parsed = WireBlob::parse_slice(&encoded);
                if in_range {
                    assert_eq!(parsed.unwrap().raw_size, Some(declared as i32));
                } else {
                    assert!(
                        parsed
                            .unwrap_err()
                            .to_string()
                            .contains("Blob.raw_size exceeds int32")
                    );
                }
                let mut out = Vec::new();
                let decoded = super::decompress_blob_raw(&encoded, &mut out);
                if declared == honest {
                    decoded.unwrap();
                    assert_eq!(out, payload);
                    continue;
                }
                let expected = if in_range {
                    "raw_size disagrees"
                } else {
                    "Blob.raw_size exceeds int32"
                };
                assert!(decoded.unwrap_err().to_string().contains(expected));
                if !in_range {
                    assert!(out.is_empty());
                }
            }
        }
    }

    /// A `raw_size` that disagrees with the payload actually produced is
    /// corruption, and is rejected rather than silently ignored as a stale
    /// capacity hint. libosmium cross-checks the same field.
    #[test]
    fn a_lying_raw_size_is_rejected() {
        let payload = b"exactly-fifteen";
        let honest = i32::try_from(payload.len()).unwrap();

        for kind in [Kind::Raw, Kind::Zlib, Kind::Zstd] {
            let mut blob = make_blob(kind, payload);
            assert_eq!(blob.raw_size, Some(honest));

            // Honest: accepted, and yields the payload.
            let mut out = Vec::new();
            decompress_wire_blob_into(&blob, &mut out).expect("an honest raw_size decodes");
            assert_eq!(out, payload);

            // Off by one in each direction. Both must be rejected: a
            // one-sided check would pass a truncating writer.
            for lie in [honest - 1, honest + 1] {
                blob.raw_size = Some(lie);
                let mut out = Vec::new();
                assert!(
                    matches!(
                        decompress_wire_blob_into(&blob, &mut out)
                            .as_ref()
                            .map_err(crate::error::Error::kind),
                        Err(ErrorKind::WireFormat { .. })
                    ),
                    "{kind:?}: raw_size {lie} against {honest} bytes must be rejected"
                );
                let mut out = Vec::new();
                assert!(
                    decompress_parsed_blob_into(&blob, &mut out).is_err(),
                    "{kind:?}: the pipelined helper must agree with the sequential one"
                );
                assert!(
                    decompress_blob(&blob, None).is_err(),
                    "{kind:?}: the Bytes helper must agree too"
                );
            }
        }

        // An absent raw_size stays legal - our own writer omits it on
        // uncompressed blobs, so requiring it would reject our own output.
        let blob = WireBlob {
            data: Some(BlobData::Raw(Bytes::from_static(b"no declared size"))),
            raw_size: None,
        };
        let mut out = Vec::new();
        decompress_wire_blob_into(&blob, &mut out).expect("an absent raw_size is legal");
        assert_eq!(out, b"no declared size");
    }

    /// A deflate stream that ends before its final block must terminate with a
    /// typed error.
    ///
    /// `Decompress::decompress_vec` reports this as `Ok(Status::BufError)`, not
    /// `Err`: the input is exhausted, nothing is consumed, nothing is produced,
    /// and `StreamEnd` never arrives. The inflate loop used to spin on that
    /// forever at 100 % CPU with no output and no diagnostic - and every read
    /// path in the crate funnels through it, so on a rayon decode worker the
    /// spin presents as a deadlock.
    ///
    /// The watchdog is the point of the test: it runs the decompression on a
    /// worker thread and fails if no answer arrives, so a regression surfaces
    /// as a bounded failure rather than as a hung test suite. The budget is
    /// deliberately well inside the 20 s per-test watchdog `brokkr check`
    /// enforces - a correct implementation answers in microseconds.
    #[test]
    fn truncated_zlib_stream_errors_instead_of_hanging() {
        // Compressible payload, then cut the deflate stream in half: the
        // prefix is byte-valid deflate, it just never reaches a final block.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let full = zlib_compress(&payload);
        assert!(full.len() > 64, "need a stream long enough to bisect");
        let truncated = full[..full.len() / 2].to_vec();

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let blob = WireBlob {
                data: Some(BlobData::Zlib(Bytes::from(truncated))),
                raw_size: Some(200_000),
            };
            let mut out = Vec::new();
            let result = decompress_wire_blob_into(&blob, &mut out);
            // Send failure means the receiver already gave up on the watchdog.
            let _sent = tx.send(result.is_err());
        });

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(is_err) => {
                handle.join().expect("decompress thread must not panic");
                assert!(
                    is_err,
                    "a truncated deflate stream must be an error, not a short read"
                );
            }
            Err(_) => panic!(
                "zlib decompression of a truncated stream did not terminate \
                 within 5s - the no-progress guard is gone"
            ),
        }
    }

    /// The decompressed-size cap is enforced *during* inflation, not after.
    ///
    /// A blob whose declared `datasize` sits under the 32 MiB pre-read cap can
    /// inflate to orders of magnitude more (zlib reaches ~1000x on zeroes;
    /// zstd reaches ~10^5), so a post-hoc `buf.len() > MAX` check fires only
    /// once the process has already allocated the bomb. The surfaced size is
    /// the assertion that bites: it is exactly one byte past the cap, which is
    /// only possible if inflation stopped there. Before the fix the same call
    /// inflated the whole payload and reported its full size.
    #[test]
    fn decompression_bomb_is_bounded_during_inflation() {
        let cap = MAX_BLOB_MESSAGE_SIZE as usize;
        // Four times the cap, all zeroes: compresses to a few hundred KB of
        // zlib and a few hundred bytes of zstd.
        let bomb = vec![0u8; cap * 2];

        for (kind, compressed) in [
            (Kind::Zlib, zlib_compress(&bomb)),
            (Kind::Zstd, zstd_compress(&bomb)),
        ] {
            assert!(
                compressed.len() < cap,
                "{kind:?} bomb must be small enough to pass the datasize guard"
            );
            let blob = WireBlob {
                data: Some(match kind {
                    Kind::Zlib => BlobData::Zlib(Bytes::from(compressed)),
                    _ => BlobData::Zstd(Bytes::from(compressed)),
                }),
                // A lying raw_size: 2 GiB, the largest an int32 can declare.
                // The capacity hint must be clamped or this alone allocates it.
                raw_size: Some(i32::MAX),
            };

            let mut out = Vec::new();
            assert_too_big(
                decompress_wire_blob_into(&blob, &mut out),
                MAX_BLOB_MESSAGE_SIZE + 1,
            );
            assert!(
                out.len() <= cap + 1,
                "{kind:?} inflated {} bytes past the cap before erroring",
                out.len() - cap
            );

            let mut parsed = Vec::new();
            assert_too_big(
                decompress_parsed_blob_into(&blob, &mut parsed),
                MAX_BLOB_MESSAGE_SIZE + 1,
            );

            assert_too_big(
                decompress_blob(&blob, None).map(|_| ()),
                MAX_BLOB_MESSAGE_SIZE + 1,
            );
        }
    }

    /// A `raw_size` hint above the decompressed-content cap is a lie no
    /// legitimate writer tells, and it is read straight off the wire with no
    /// validation - so it must not reach `Vec::reserve` unclamped. Pins the
    /// clamp independently of the inflation bound above, which would otherwise
    /// be the only thing standing between a 5-byte field and a 2 GiB
    /// allocation.
    #[test]
    fn raw_size_capacity_hint_is_clamped_to_the_cap() {
        let blob = WireBlob {
            data: Some(BlobData::Zlib(Bytes::from(zlib_compress(b"tiny")))),
            raw_size: Some(i32::MAX),
        };
        assert_eq!(
            blob.estimated_capacity(),
            MAX_BLOB_MESSAGE_SIZE as usize,
            "an over-cap raw_size must clamp to the cap, not pass through"
        );

        // The blob still inflates - the clamp governs the `reserve`, not the
        // decode - but a `raw_size` this far from the four bytes actually
        // produced is now rejected as corruption by `check_raw_size`. The
        // capacity assertion below is the one under test here and is unaffected
        // by that rejection: it reads the buffer the clamped hint sized.
        let mut out = Vec::new();
        let err = decompress_wire_blob_into(&blob, &mut out)
            .expect_err("a raw_size of i32::MAX over a 4-byte payload is a lie");
        assert!(
            matches!(err.kind(), ErrorKind::WireFormat { .. }),
            "a raw_size mismatch must be a wire-format error"
        );
        assert_eq!(out, b"tiny");
        assert!(
            out.capacity() <= MAX_BLOB_MESSAGE_SIZE as usize,
            "buffer capacity followed the unclamped hint: {}",
            out.capacity()
        );
    }

    /// The inflation bound must not depend on the destination buffer's
    /// allocation history.
    ///
    /// Every zlib caller in this module hands in a reusable, caller-owned
    /// buffer, and `Decompress::decompress_vec` fills the destination to its
    /// *capacity* - so a buffer an earlier large blob already grew past the cap
    /// let a bomb inflate to that whole capacity in one call, before any loop
    /// check ran again. The cap was computed correctly and then not enforced.
    ///
    /// The pre-sized buffer is the entire point: the same bomb through a fresh
    /// `Vec` (asserted above) stays bounded even with the bug present, which is
    /// why the passing suite did not catch this. Both assertions bite - with
    /// the bound defeated the surfaced size is the bomb's full 64 MiB rather
    /// than the one-byte-past-the-cap boundary, and `reused` holds all of it.
    #[test]
    fn a_prewarmed_buffer_cannot_widen_the_inflation_bound() {
        let cap = MAX_BLOB_MESSAGE_SIZE as usize;
        let bomb = vec![0u8; cap * 2];
        let blob = WireBlob {
            data: Some(BlobData::Zlib(Bytes::from(zlib_compress(&bomb)))),
            raw_size: Some(i32::MAX),
        };

        // A pooled buffer that a previous blob grew well past the cap. Cleared
        // for reuse, so `len` is 0 and only the capacity carries over - exactly
        // the state `DecompressPool` recycles buffers in.
        let mut reused = Vec::with_capacity(cap * 2 + 4096);
        reused.push(0u8);
        reused.clear();
        assert!(
            reused.capacity() > cap + 1,
            "fixture must start with more spare capacity than the cap permits"
        );

        assert_too_big(
            decompress_wire_blob_into(&blob, &mut reused),
            MAX_BLOB_MESSAGE_SIZE + 1,
        );
        assert!(
            reused.len() <= cap + 1,
            "the bomb inflated to {} bytes - the caller's spare capacity, not the cap, bounded it",
            reused.len()
        );
    }

    /// The bounded-destination path must still decode a legitimate blob
    /// correctly, not just refuse bombs.
    ///
    /// A pre-sized buffer routes through a different branch of
    /// `zlib_decompress_into` than the hot path does, and that branch inflates
    /// into a fresh vector and appends. Appending is the part that can go
    /// wrong: `zlib_decompress_into` appends at `buf.len()`, and the rare
    /// branch has to reproduce that through a temporary. Getting it wrong
    /// would corrupt output rather than fail loudly.
    ///
    /// Both levels are checked. Every public entry point clears its
    /// destination, so the append contract is only observable by calling
    /// `zlib_decompress_into` directly - which is also the only way to reach
    /// the branch with a non-empty buffer at all.
    #[test]
    fn a_prewarmed_buffer_still_decodes_and_appends_correctly() {
        let cap = MAX_BLOB_MESSAGE_SIZE as usize;
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let compressed = zlib_compress(&payload);

        // Append contract, straight at the helper.
        let mut appended = Vec::with_capacity(cap * 2 + 4096);
        appended.extend_from_slice(b"prefix");
        assert!(
            appended.capacity() - appended.len() > cap + 1,
            "fixture must take the bounded-destination branch"
        );
        super::zlib_decompress_into(&compressed, &mut appended).expect("a legitimate blob decodes");
        assert_eq!(&appended[..6], b"prefix", "existing content must survive");
        assert_eq!(
            &appended[6..],
            &payload[..],
            "the decoded payload must be appended intact"
        );

        // And through a public entry point, whose `clear()` leaves the
        // oversized capacity in place so the branch is still taken.
        let blob = WireBlob {
            data: Some(BlobData::Zlib(Bytes::from(compressed))),
            raw_size: Some(i32::try_from(payload.len()).unwrap()),
        };
        let mut reused = Vec::with_capacity(cap * 2 + 4096);
        reused.push(0u8);
        reused.clear();
        decompress_wire_blob_into(&blob, &mut reused).expect("a legitimate blob still decodes");
        assert_eq!(reused, payload, "the decoded payload must be intact");
    }
}
