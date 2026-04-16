//! Blob-ordered delta-varint coord payload format.
//!
//! Stage 3 produces `coord_payloads` inline (integrated path, the default).
//! Each way blob's coordinates are delta-varint encoded and stored contiguously;
//! stage 4 preads one payload per way blob instead of mmapping the flat
//! coord_slots array. Measured compression is ~1.81× vs coord_slots (37.5 GB
//! Europe → 20.8 GB; 99 GB planet projects to ~55 GB). The original 3–4×
//! estimate did not account for OSM's moderate varint widths after delta
//! encoding; see notes/altw-optimization-history.md for the reconciliation.
//! The integration is pursued primarily for its non-wall benefits
//! (scratch footprint, page faults, memory pressure), not raw compression.
//!
//! # File format
//!
//! ```text
//! u64            num_way_blobs         (LE)
//! u64            total_payload_bytes   (LE)
//! u64 * (N+1)    blob_offsets[0..=N]   (LE, byte offsets into payload
//!                                       section; blob i's payload spans
//!                                       offsets[i]..offsets[i+1];
//!                                       offsets[N] == total_payload_bytes)
//! bytes          payload section (concatenated per-blob delta-varint streams)
//! ```
//!
//! # Per-blob payload
//!
//! Walk the blob's ways in PBF order (order recorded in the per-way refcount
//! sidecar). For each way with N refs, read N coord slots sequentially from
//! the bucket's dense scatter buffer in stage 3. Emit `2*N` zigzag-varints:
//! (lat_delta_0, lon_delta_0, ..., lat_delta_N-1, lon_delta_N-1) where
//! delta_0 is the absolute value (delta from 0) and deltas reset per way.
//!
//! # Decoder contract (stage 4)
//!
//! Stage 4 knows ref_count per way from the per-way refcount sidecar.
//! For each way, consume `2*ref_count` varints, unzigzag, accumulate running
//! lat/lon. No per-way framing bytes.

use std::path::Path;

use super::super::Result;
use super::COORD_SLOT_SIZE;

// ---------------------------------------------------------------------------
// Integrated path types
// ---------------------------------------------------------------------------

/// Two-piece state for a straddler blob in the integrated stage 3 path.
///
/// Invariant: transitions `None → Some(Left|Right) → Some(Both)`.
/// Workers never delta-encode straddler pieces — raw slot bytes only.
pub(super) enum StraddlerSlot {
    Left(Vec<u8>),
    Right(Vec<u8>),
    Both { left: Vec<u8>, right: Vec<u8> },
}

/// Per-worker manifest entry produced during stage 3 for a fully-contained blob.
pub(super) struct ManifestEntry {
    pub blob_idx: u32,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// Location of one way blob's encoded coord payload.
///
/// Replaces the consolidated `coord_payloads` file. Fully-contained blobs
/// live in a per-worker tmp file (`Worker`); straddler blobs are encoded
/// into RAM at router-build time (`Straddler`); zero-ref blobs have no
/// payload at all (`Empty`). Stage 4 preads directly from the right
/// worker tmp fd or reads the straddler bytes in-place.
pub(super) enum BlobLocation {
    Worker {
        worker_id: u32,
        byte_offset: u64,
        byte_length: u64,
    },
    Straddler {
        bytes: Vec<u8>,
    },
    Empty,
}

/// Dispatch table keyed by `blob_idx`. Replaces `CoordPayloadsReader`
/// + the consolidated `coord_payloads` file produced by finalize.
///
/// Owns the open worker tmp `File` handles; stage 4 borrows `&Self` and
/// calls [`BlobLocationRouter::pread_blob_payload`] per blob.
pub(super) struct BlobLocationRouter {
    worker_files: Vec<std::fs::File>,
    locations: Vec<BlobLocation>,
}

impl BlobLocationRouter {
    /// Read blob `blob_idx`'s encoded payload into `buf` (resized to exact length).
    /// Drop-in replacement for the old `CoordPayloadsReader::pread_blob_payload`.
    pub(super) fn pread_blob_payload(&self, blob_idx: usize, buf: &mut Vec<u8>) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        match &self.locations[blob_idx] {
            BlobLocation::Worker { worker_id, byte_offset, byte_length } => {
                #[allow(clippy::cast_possible_truncation)]
                let len = *byte_length as usize;
                buf.resize(len, 0);
                if len > 0 {
                    self.worker_files[*worker_id as usize]
                        .read_exact_at(buf, *byte_offset)
                        .map_err(|e| format!(
                            "router pread worker {worker_id} blob {blob_idx}: {e}"
                        ))?;
                }
            }
            BlobLocation::Straddler { bytes } => {
                buf.clear();
                buf.extend_from_slice(bytes);
            }
            BlobLocation::Empty => {
                buf.clear();
            }
        }
        Ok(())
    }
}

/// Stats from `build_blob_location_router`.
#[derive(Debug, Default)]
pub(super) struct RouterStats {
    pub num_way_blobs: u64,
    pub num_straddlers: u64,
    pub num_worker: u64,
    pub num_empty: u64,
    pub straddler_bytes: u64,
    pub worker_bytes: u64,
    pub build_ms: u64,
}

/// Indexed per-way ref-count sidecar.
///
/// Retains the original varint-encoded sidecar bytes and records only the
/// per-blob byte offsets. Callers decode one blob's refcounts on demand,
/// avoiding the planet-scale flat `Vec<u32>` residency that stage 3/finalize
/// used to keep alive simultaneously.
pub(super) struct PerWayRcs {
    data: Vec<u8>,
    offsets: Vec<usize>, // len == num_blobs + 1; offsets[num_blobs] == data.len()
}

impl PerWayRcs {
    pub(super) fn blob_record(&self, blob_idx: usize) -> &[u8] {
        &self.data[self.offsets[blob_idx]..self.offsets[blob_idx + 1]]
    }

    pub(super) fn num_blobs(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub(super) fn decode_blob_into<'a>(
        &self,
        blob_idx: usize,
        scratch: &'a mut Vec<u32>,
    ) -> Result<&'a [u32]> {
        decode_blob_record_into(self.blob_record(blob_idx), blob_idx, scratch)?;
        Ok(scratch.as_slice())
    }

    pub(super) fn blob_has_nonzero_refs(&self, blob_idx: usize) -> Result<bool> {
        blob_record_has_nonzero_refs(self.blob_record(blob_idx), blob_idx)
    }
}

fn scan_blob_record(cursor: &mut protohoggr::Cursor<'_>, blob_idx: usize) -> Result<()> {
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    for way_idx in 0..num_ways_usize {
        cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
    }
    Ok(())
}

fn decode_blob_record_into(
    record: &[u8],
    blob_idx: usize,
    scratch: &mut Vec<u32>,
) -> Result<()> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    scratch.clear();
    scratch.reserve(num_ways_usize);
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        scratch.push(rc as u32);
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }
    Ok(())
}

fn blob_record_has_nonzero_refs(record: &[u8], blob_idx: usize) -> Result<bool> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        if rc != 0 {
            return Ok(true);
        }
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }
    Ok(false)
}

fn build_per_way_refcount_index(data: Vec<u8>, num_way_blobs: usize) -> Result<PerWayRcs> {
    let mut cursor = protohoggr::Cursor::new(&data);
    let mut offsets: Vec<usize> = Vec::with_capacity(num_way_blobs + 1);
    offsets.push(0);
    let data_len = data.len();

    for blob_idx in 0..num_way_blobs {
        scan_blob_record(&mut cursor, blob_idx)?;
        offsets.push(data_len - cursor.remaining());
    }

    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way refcount sidecar has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }

    Ok(PerWayRcs {
        data,
        offsets,
    })
}

/// Parse the per-way refcount sidecar bytes into an indexed `PerWayRcs`.
///
/// Extracted as a pure function so both the file loader and tests can call it.
pub(super) fn parse_per_way_refcount_sidecar_bytes(
    data: &[u8],
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    build_per_way_refcount_index(data.to_vec(), num_way_blobs)
}

/// Load the per-way refcount sidecar into an indexed `PerWayRcs`.
pub(super) fn load_per_way_refcount_sidecar_indexed(
    path: &Path,
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    let data = std::fs::read(path)
        .map_err(|e| format!("read per-way refcount sidecar: {e}"))?;
    build_per_way_refcount_index(data, num_way_blobs)
}

/// Build the blob-location routing table from stage-3 outputs.
///
/// Replaces the old `finalize_coord_payloads` consolidation pass. Instead of
/// pwrite-copying ~55 GB of worker-tmp bytes + encoded straddler bytes into a
/// single consolidated `coord_payloads` file (which stage 4 then pread from),
/// keep the worker tmps open and route stage 4's per-blob pread directly to
/// the correct fd (or to an in-RAM buffer for straddlers).
///
/// Walks blobs in `blob_idx` order. For straddlers (`StraddlerSlot::Both`),
/// encodes the coord payload in-place (same encoding path the old finalize
/// used for straddlers) and stashes the bytes in the returned router. For
/// fully-contained blobs, records `(worker_id, byte_offset, byte_length)`
/// from the per-worker manifest. For zero-ref blobs, records `Empty`.
///
/// Worker tmp files are opened once as plain `File` handles held by the
/// router; `File: Sync` on Unix via `FileExt::read_exact_at` (pread-backed),
/// so `&BlobLocationRouter` can be shared across stage-4 worker threads.
#[allow(clippy::needless_pass_by_value, clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn build_blob_location_router(
    per_way_rcs: &PerWayRcs,
    worker_manifests: Vec<Vec<ManifestEntry>>,
    worker_tmp_paths: &[std::path::PathBuf],
    straddler_slots: Vec<std::sync::Mutex<Option<StraddlerSlot>>>,
) -> Result<(BlobLocationRouter, RouterStats)> {
    use std::time::Instant;

    crate::debug::emit_marker("COORD_PAYLOADS_ROUTER_BUILD_START");
    let t_all = Instant::now();

    let num_way_blobs = per_way_rcs.num_blobs();

    // Fold worker manifests into a single per-blob lookup. Verify no
    // blob appears in multiple worker manifests.
    let mut blob_in_worker: Vec<Option<(u32, u64, u64)>> = vec![None; num_way_blobs];
    for (worker_id, manifest) in worker_manifests.iter().enumerate() {
        for entry in manifest {
            let idx = entry.blob_idx as usize;
            if idx >= num_way_blobs {
                return Err(format!(
                    "worker {worker_id} manifest entry blob_idx {idx} out of range (num_way_blobs={num_way_blobs})"
                ).into());
            }
            if blob_in_worker[idx].is_some() {
                return Err(format!("blob {idx} appears in multiple worker manifests").into());
            }
            blob_in_worker[idx] = Some((worker_id as u32, entry.byte_offset, entry.byte_length));
        }
    }

    // Mutual-exclusivity check: a blob must not be in both worker manifests
    // and straddler staging.
    for (idx, slot) in straddler_slots.iter().enumerate() {
        let guard = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() && blob_in_worker[idx].is_some() {
            return Err(format!(
                "blob {idx} appears in both a worker manifest and straddler staging"
            )
            .into());
        }
    }

    let mut locations: Vec<BlobLocation> = Vec::with_capacity(num_way_blobs);
    let mut encode_scratch: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut stats = RouterStats {
        num_way_blobs: num_way_blobs as u64,
        ..Default::default()
    };

    for blob_idx in 0..num_way_blobs {
        let straddler_taken = straddler_slots[blob_idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        let location = match straddler_taken {
            Some(StraddlerSlot::Both { left, right }) => {
                let mut coord_bytes = left;
                coord_bytes.extend_from_slice(&right);
                encode_scratch.clear();
                encode_blob_payload_from_record(
                    &coord_bytes,
                    per_way_rcs.blob_record(blob_idx),
                    blob_idx,
                    &mut encode_scratch,
                )
                .map_err(|e| format!("router straddler encode blob {blob_idx}: {e}"))?;
                stats.num_straddlers += 1;
                stats.straddler_bytes += encode_scratch.len() as u64;
                BlobLocation::Straddler {
                    bytes: std::mem::take(&mut encode_scratch),
                }
            }
            Some(StraddlerSlot::Left(_)) => {
                return Err(format!("blob {blob_idx}: straddler missing right half").into());
            }
            Some(StraddlerSlot::Right(_)) => {
                return Err(format!("blob {blob_idx}: straddler missing left half").into());
            }
            None => match blob_in_worker[blob_idx] {
                Some((worker_id, byte_offset, byte_length)) => {
                    stats.num_worker += 1;
                    stats.worker_bytes += byte_length;
                    BlobLocation::Worker {
                        worker_id,
                        byte_offset,
                        byte_length,
                    }
                }
                None => {
                    // Zero-ref blob: no manifest entry and no straddler. Any
                    // non-zero ref count here means stage 3 lost the blob.
                    if per_way_rcs.blob_has_nonzero_refs(blob_idx)? {
                        return Err(format!(
                            "blob {blob_idx} has non-zero ref counts but no worker manifest \
                             entry and no straddler staging — upstream bug"
                        )
                        .into());
                    }
                    stats.num_empty += 1;
                    BlobLocation::Empty
                }
            },
        };
        locations.push(location);
    }

    // Open worker tmps once; they live as long as the router does. Stage 4
    // shares `&BlobLocationRouter` across worker threads; plain `&File` is
    // Sync via pread (`FileExt::read_exact_at`) on Unix, no `Arc` needed.
    let worker_files: Vec<std::fs::File> = worker_tmp_paths
        .iter()
        .map(|path| {
            std::fs::File::open(path).map_err(|e| -> Box<dyn std::error::Error> {
                format!("open worker tmp {}: {e}", path.display()).into()
            })
        })
        .collect::<Result<Vec<_>>>()?;

    #[allow(clippy::cast_possible_truncation)]
    {
        stats.build_ms = t_all.elapsed().as_millis() as u64;
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_router_build_ms", stats.build_ms as i64);
        crate::debug::emit_counter("s3_router_num_way_blobs", stats.num_way_blobs as i64);
        crate::debug::emit_counter("s3_router_num_straddlers", stats.num_straddlers as i64);
        crate::debug::emit_counter("s3_router_num_worker", stats.num_worker as i64);
        crate::debug::emit_counter("s3_router_num_empty", stats.num_empty as i64);
        crate::debug::emit_counter("s3_router_straddler_bytes", stats.straddler_bytes as i64);
        crate::debug::emit_counter("s3_router_worker_bytes", stats.worker_bytes as i64);
    }
    crate::debug::emit_marker("COORD_PAYLOADS_ROUTER_BUILD_END");

    Ok((BlobLocationRouter { worker_files, locations }, stats))
}

const _: () = {
    assert!(COORD_SLOT_SIZE == 8);
};

/// Delta-encode one blob's coord slice into `output`.
///
/// `coord_bytes.len() == 8 * sum(per_way_rcs)`. Within `coord_bytes`
/// each 8-byte slot is `[i32 LE lat][i32 LE lon]`. For each way
/// (refcount `rc` from `per_way_rcs`), consume `rc` consecutive slots
/// and emit `2*rc` zigzag-varints into `output`: `lat_delta_0`,
/// `lon_delta_0`, `lat_delta_1`, `lon_delta_1`, ... where
/// `delta_0` is absolute (delta from 0), deltas reset per way.
/// Bytes already in `output` are preserved; encoder appends.
pub(super) fn encode_blob_payload(
    coord_bytes: &[u8],
    per_way_rcs: &[u32],
    output: &mut Vec<u8>,
) -> std::result::Result<(), String> {
    let expected_bytes: u64 = per_way_rcs.iter().map(|&r| u64::from(r)).sum::<u64>() * 8;
    if coord_bytes.len() as u64 != expected_bytes {
        return Err(format!(
            "coord_bytes length mismatch: got {} bytes, expected {} (8 * sum(per_way_rcs))",
            coord_bytes.len(),
            expected_bytes
        ));
    }
    let mut cursor: usize = 0;
    for &rc in per_way_rcs {
        let mut last_lat: i32 = 0;
        let mut last_lon: i32 = 0;
        for _ in 0..rc {
            let off = cursor;
            cursor += COORD_SLOT_SIZE;
            let lat = i32::from_le_bytes([
                coord_bytes[off],
                coord_bytes[off + 1],
                coord_bytes[off + 2],
                coord_bytes[off + 3],
            ]);
            let lon = i32::from_le_bytes([
                coord_bytes[off + 4],
                coord_bytes[off + 5],
                coord_bytes[off + 6],
                coord_bytes[off + 7],
            ]);
            let dlat = i64::from(lat) - i64::from(last_lat);
            let dlon = i64::from(lon) - i64::from(last_lon);
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlat));
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlon));
            last_lat = lat;
            last_lon = lon;
        }
    }
    Ok(())
}

/// Delta-encode one blob's coord slice using the raw sidecar record for that
/// blob instead of a pre-decoded `&[u32]`.
pub(super) fn encode_blob_payload_from_record(
    coord_bytes: &[u8],
    record: &[u8],
    blob_idx: usize,
    output: &mut Vec<u8>,
) -> std::result::Result<(), String> {
    let mut cursor = protohoggr::Cursor::new(record);
    let num_ways = cursor
        .read_varint()
        .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
    #[allow(clippy::cast_possible_truncation)]
    let num_ways_usize = num_ways as usize;

    let mut coord_cursor: usize = 0;
    for way_idx in 0..num_ways_usize {
        let rc = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let rc_usize = rc as usize;
        let way_bytes = rc_usize
            .checked_mul(COORD_SLOT_SIZE)
            .ok_or_else(|| format!("blob {blob_idx} way {way_idx}: refcount byte size overflow"))?;
        if coord_cursor + way_bytes > coord_bytes.len() {
            return Err(format!(
                "coord_bytes length mismatch for blob {blob_idx}: got {} bytes, \
                 need at least {} bytes by way {way_idx}",
                coord_bytes.len(),
                coord_cursor + way_bytes,
            ));
        }

        let mut last_lat: i32 = 0;
        let mut last_lon: i32 = 0;
        for _ in 0..rc_usize {
            let off = coord_cursor;
            coord_cursor += COORD_SLOT_SIZE;
            let lat = i32::from_le_bytes([
                coord_bytes[off],
                coord_bytes[off + 1],
                coord_bytes[off + 2],
                coord_bytes[off + 3],
            ]);
            let lon = i32::from_le_bytes([
                coord_bytes[off + 4],
                coord_bytes[off + 5],
                coord_bytes[off + 6],
                coord_bytes[off + 7],
            ]);
            let dlat = i64::from(lat) - i64::from(last_lat);
            let dlon = i64::from(lon) - i64::from(last_lon);
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlat));
            protohoggr::encode_varint(output, protohoggr::zigzag_encode_64(dlon));
            last_lat = lat;
            last_lon = lon;
        }
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way sidecar blob {blob_idx} has {} trailing bytes",
            cursor.remaining()
        ));
    }
    if coord_cursor != coord_bytes.len() {
        return Err(format!(
            "coord_bytes length mismatch for blob {blob_idx}: got {} bytes, expected {}",
            coord_bytes.len(),
            coord_cursor,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // In tests we use .expect() liberally; clippy::unwrap_used covers .unwrap()
    // but .expect() is always fine.

    fn make_coord_bytes(coords: &[(i32, i32)]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(coords.len() * 8);
        for &(lat, lon) in coords {
            buf.extend_from_slice(&lat.to_le_bytes());
            buf.extend_from_slice(&lon.to_le_bytes());
        }
        buf
    }

    fn decode_zigzag_varints(data: &[u8], count: usize) -> Vec<i64> {
        let mut cursor = protohoggr::Cursor::new(data);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let v = cursor.read_varint().expect("read varint");
            out.push(protohoggr::zigzag_decode_64(v));
        }
        out
    }

    fn reconstruct_coords(output: &[u8], per_way_rcs: &[u32]) -> Vec<(i32, i32)> {
        let total_refs: usize = per_way_rcs.iter().map(|&r| r as usize).sum();
        let deltas = decode_zigzag_varints(output, total_refs * 2);
        let mut result = Vec::with_capacity(total_refs);
        let mut delta_idx = 0;
        for &rc in per_way_rcs {
            let mut last_lat: i64 = 0;
            let mut last_lon: i64 = 0;
            for _ in 0..rc {
                let lat = last_lat + deltas[delta_idx];
                let lon = last_lon + deltas[delta_idx + 1];
                delta_idx += 2;
                #[allow(clippy::cast_possible_truncation)]
                result.push((lat as i32, lon as i32));
                last_lat = lat;
                last_lon = lon;
            }
        }
        result
    }

    #[test]
    fn encode_blob_payload_single_way_single_ref() {
        let coords = [(12345_i32, 67890_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [1u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_single_way_multi_ref() {
        let coords = [(100_i32, 200_i32), (150_i32, 250_i32), (90_i32, 180_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [3u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_multiple_ways() {
        // Three ways of 2/3/1 refs; deltas reset at way boundaries.
        let coords = [
            (10_i32, 20_i32), (30_i32, 40_i32),           // way 0
            (500_i32, 600_i32), (510_i32, 610_i32), (490_i32, 590_i32), // way 1
            (9999_i32, -9999_i32),                          // way 2
        ];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32, 3u32, 1u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_empty_blob() {
        let cb: Vec<u8> = Vec::new();
        let rcs: &[u32] = &[];
        let mut out = vec![0xAAu8, 0xBBu8];
        encode_blob_payload(&cb, rcs, &mut out).expect("encode");
        // Output unchanged — only the sentinel bytes we put in.
        assert_eq!(out, [0xAAu8, 0xBBu8]);
    }

    #[test]
    fn encode_blob_payload_zero_coords() {
        let coords = [(0_i32, 0_i32), (0_i32, 0_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_negative_coords() {
        let coords = [(-1_000_000_i32, -1_000_000_i32), (0_i32, 0_i32)];
        let cb = make_coord_bytes(&coords);
        let rcs = [2u32];
        let mut out = Vec::new();
        encode_blob_payload(&cb, &rcs, &mut out).expect("encode");
        let reconstructed = reconstruct_coords(&out, &rcs);
        assert_eq!(reconstructed, coords);
    }

    #[test]
    fn encode_blob_payload_length_mismatch() {
        // per_way_rcs = [2] expects 16 bytes; we pass 15.
        let cb = vec![0u8; 15];
        let rcs = [2u32];
        let result = encode_blob_payload(&cb, &rcs, &mut Vec::new());
        assert!(result.is_err(), "expected Err for length mismatch");
    }

    /// Build a minimal per-way refcount sidecar in memory for `num_way_blobs`
    /// blobs where each blob has the per-way ref counts given by `blobs`.
    fn make_sidecar_bytes(blobs: &[&[u32]]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for blob_rcs in blobs {
            protohoggr::encode_varint(&mut out, blob_rcs.len() as u64);
            for &rc in *blob_rcs {
                protohoggr::encode_varint(&mut out, rc as u64);
            }
        }
        out
    }

    #[test]
    fn parse_per_way_refcount_sidecar_bytes_basic() {
        // blob 0: 2 ways with ref counts [3, 1]
        // blob 1: 1 way with ref count [2]
        let sidecar = make_sidecar_bytes(&[&[3, 1], &[2]]);
        let pwr = parse_per_way_refcount_sidecar_bytes(&sidecar, 2).expect("parse");
        let mut scratch: Vec<u32> = Vec::new();
        assert_eq!(pwr.num_blobs(), 2);
        assert_eq!(pwr.decode_blob_into(0, &mut scratch).expect("decode 0"), &[3u32, 1u32]);
        assert_eq!(pwr.decode_blob_into(1, &mut scratch).expect("decode 1"), &[2u32]);
    }

    #[test]
    fn parse_per_way_refcount_sidecar_bytes_empty_blob() {
        // blob 0: 0 ways; blob 1: 1 way with 1 ref
        let sidecar = make_sidecar_bytes(&[&[], &[1]]);
        let pwr = parse_per_way_refcount_sidecar_bytes(&sidecar, 2).expect("parse");
        let mut scratch: Vec<u32> = Vec::new();
        assert_eq!(pwr.decode_blob_into(0, &mut scratch).expect("decode 0"), &[] as &[u32]);
        assert_eq!(pwr.decode_blob_into(1, &mut scratch).expect("decode 1"), &[1u32]);
    }

    #[test]
    fn encode_blob_payload_from_record_matches_decoded_path() {
        let coords = [
            (10_i32, 20_i32), (30_i32, 40_i32),
            (500_i32, 600_i32), (510_i32, 610_i32), (490_i32, 590_i32),
            (9999_i32, -9999_i32),
        ];
        let cb = make_coord_bytes(&coords);
        let pwr = make_per_way_rcs(&[&[2u32, 3u32, 1u32]]);
        let mut from_record = Vec::new();
        let mut from_decoded = Vec::new();
        let mut scratch: Vec<u32> = Vec::new();

        encode_blob_payload_from_record(&cb, pwr.blob_record(0), 0, &mut from_record)
            .expect("encode from record");
        let decoded = pwr.decode_blob_into(0, &mut scratch).expect("decode 0").to_vec();
        encode_blob_payload(&cb, &decoded, &mut from_decoded).expect("encode decoded");

        assert_eq!(from_record, from_decoded);
    }

    // Helper: build a PerWayRcs directly from slice-of-slices.
    fn make_per_way_rcs(blobs: &[&[u32]]) -> PerWayRcs {
        let sidecar = make_sidecar_bytes(blobs);
        parse_per_way_refcount_sidecar_bytes(&sidecar, blobs.len()).expect("make_per_way_rcs")
    }

    #[test]
    fn router_build_happy_path() {
        // 4 blobs:
        //   blob 0: 2 ways [2,1] refs — fully-contained in worker 0
        //   blob 1: 1 way [3] refs   — fully-contained in worker 1
        //   blob 2: 1 way [2] refs   — straddler (Both)
        //   blob 3: 0 ways           — zero-ref, no manifest, no straddler
        let coords_b0: &[(i32, i32)] = &[(10, 20), (30, 40), (50, 60)];
        let coords_b1: &[(i32, i32)] = &[(100, 200), (300, 400), (500, 600)];
        let coords_b2: &[(i32, i32)] = &[(1, 2), (3, 4)];

        let rcs_b0: &[u32] = &[2, 1];
        let rcs_b1: &[u32] = &[3];
        let rcs_b2: &[u32] = &[2];

        let per_way_rcs = make_per_way_rcs(&[rcs_b0, rcs_b1, rcs_b2, &[]]);

        let tmp_dir = tempfile::tempdir().expect("tempdir");

        // Build worker 0 tmp file (blob 0).
        let w0_path = tmp_dir.path().join("payloads-W0");
        let mut encoded_b0: Vec<u8> = Vec::new();
        encode_blob_payload(&make_coord_bytes(coords_b0), rcs_b0, &mut encoded_b0).expect("enc b0");
        std::fs::write(&w0_path, &encoded_b0).expect("write w0");

        // Build worker 1 tmp file (blob 1).
        let w1_path = tmp_dir.path().join("payloads-W1");
        let mut encoded_b1: Vec<u8> = Vec::new();
        encode_blob_payload(&make_coord_bytes(coords_b1), rcs_b1, &mut encoded_b1).expect("enc b1");
        std::fs::write(&w1_path, &encoded_b1).expect("write w1");

        let worker_manifests: Vec<Vec<ManifestEntry>> = vec![
            vec![ManifestEntry { blob_idx: 0, byte_offset: 0, byte_length: encoded_b0.len() as u64 }],
            vec![ManifestEntry { blob_idx: 1, byte_offset: 0, byte_length: encoded_b1.len() as u64 }],
        ];

        // Straddler blob 2: split raw slot bytes at an arbitrary midpoint.
        let raw_b2 = make_coord_bytes(coords_b2);
        let split = 8; // first coord (8 bytes) in left half, second in right half
        let left_bytes = raw_b2[..split].to_vec();
        let right_bytes = raw_b2[split..].to_vec();

        let straddler_slots: Vec<std::sync::Mutex<Option<StraddlerSlot>>> = (0..4)
            .map(|i| {
                if i == 2 {
                    std::sync::Mutex::new(Some(StraddlerSlot::Both {
                        left: left_bytes.clone(),
                        right: right_bytes.clone(),
                    }))
                } else {
                    std::sync::Mutex::new(None)
                }
            })
            .collect();

        let (router, stats) = build_blob_location_router(
            &per_way_rcs,
            worker_manifests,
            &[w0_path, w1_path],
            straddler_slots,
        )
        .expect("router build");
        assert_eq!(stats.num_way_blobs, 4);
        assert_eq!(stats.num_straddlers, 1);
        assert_eq!(stats.num_worker, 2);
        assert_eq!(stats.num_empty, 1);

        let mut buf: Vec<u8> = Vec::new();

        router.pread_blob_payload(0, &mut buf).expect("pread 0");
        assert_eq!(reconstruct_coords(&buf, rcs_b0), coords_b0);

        router.pread_blob_payload(1, &mut buf).expect("pread 1");
        assert_eq!(reconstruct_coords(&buf, rcs_b1), coords_b1);

        router.pread_blob_payload(2, &mut buf).expect("pread 2");
        assert_eq!(reconstruct_coords(&buf, rcs_b2), coords_b2);

        // blob 3: zero refs — payload is empty.
        router.pread_blob_payload(3, &mut buf).expect("pread 3");
        assert!(buf.is_empty(), "zero-ref blob payload must be empty");
    }

    #[test]
    fn router_build_missing_half_error() {
        let per_way_rcs = make_per_way_rcs(&[&[1]]);
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let w0_path = tmp_dir.path().join("payloads-W0");
        std::fs::write(&w0_path, b"").expect("write w0");

        let straddler_slots: Vec<std::sync::Mutex<Option<StraddlerSlot>>> = vec![
            std::sync::Mutex::new(Some(StraddlerSlot::Left(vec![0u8; 8]))),
        ];

        let result = build_blob_location_router(
            &per_way_rcs,
            vec![vec![]],
            &[w0_path],
            straddler_slots,
        );
        let err = match result {
            Ok(_) => panic!("expected Err for missing right half"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing right half"), "error message: {msg}");
    }
}
