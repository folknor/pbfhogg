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
//! Stage 4 knows ref_count per way from parsing the way blob's refs.
//! For each way, consume `2*ref_count` varints, unzigzag, accumulate running
//! lat/lon. No per-way framing bytes.

use std::io::{Seek as _, Write as _};
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

/// Stats from `finalize_coord_payloads`.
#[derive(Debug)]
pub(super) struct FinalizeStats {
    pub output_bytes: u64,
    pub num_way_blobs: u64,
    pub num_straddlers: u64,
    pub finalize_ms: u64,
    pub read_ms: u64,
    pub encode_ms: u64,
    pub write_ms: u64,
}

/// Flat per-way ref-count table.
///
/// Uses a single allocation instead of `Vec<Vec<u32>>` to avoid allocator
/// fragmentation at planet scale (~5 GB → 4.7 GB flat, cleaner cache access).
pub(super) struct PerWayRcs {
    flat: Vec<u32>,
    offsets: Vec<usize>, // len == num_blobs + 1; offsets[num_blobs] == flat.len()
}

impl PerWayRcs {
    pub(super) fn blob(&self, blob_idx: usize) -> &[u32] {
        &self.flat[self.offsets[blob_idx]..self.offsets[blob_idx + 1]]
    }

    pub(super) fn num_blobs(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
}

/// Parse the per-way refcount sidecar bytes into a flat `PerWayRcs`.
///
/// Extracted as a pure function so both the file loader and tests can call it.
pub(super) fn parse_per_way_refcount_sidecar_bytes(
    data: &[u8],
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    let mut cursor = protohoggr::Cursor::new(data);
    let mut flat: Vec<u32> = Vec::new();
    let mut offsets: Vec<usize> = Vec::with_capacity(num_way_blobs + 1);
    offsets.push(0);

    for blob_idx in 0..num_way_blobs {
        let num_ways = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let num_ways_usize = num_ways as usize;
        for way_idx in 0..num_ways_usize {
            let rc = cursor
                .read_varint()
                .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
            #[allow(clippy::cast_possible_truncation)]
            flat.push(rc as u32);
        }
        offsets.push(flat.len());
    }

    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way refcount sidecar has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }

    Ok(PerWayRcs { flat, offsets })
}

/// Load the per-way refcount sidecar into a flat `PerWayRcs`.
///
/// Avoids `Vec<Vec<u32>>` allocator fragmentation at planet scale.
pub(super) fn load_per_way_refcount_sidecar_flat(
    path: &Path,
    num_way_blobs: usize,
) -> Result<PerWayRcs> {
    let data = std::fs::read(path)
        .map_err(|e| format!("read per-way refcount sidecar: {e}"))?;
    parse_per_way_refcount_sidecar_bytes(&data, num_way_blobs)
}

/// Finalize the integrated coord_payloads file after stage 3 workers complete.
///
/// Walks blobs 0..num_way_blobs in order. Straddler blobs are encoded from
/// their two raw-slot-byte halves; fully-contained blobs are pread from the
/// worker's temp file and written verbatim (already encoded by the worker).
/// Blobs with no manifest and no straddler must have zero ref count.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value, clippy::cast_possible_truncation)]
pub(super) fn finalize_coord_payloads(
    output_path: &Path,
    per_way_rcs: &PerWayRcs,
    worker_manifests: Vec<Vec<ManifestEntry>>,
    worker_tmp_paths: &[std::path::PathBuf],
    straddler_slots: Vec<std::sync::Mutex<Option<StraddlerSlot>>>,
) -> Result<FinalizeStats> {
    use std::os::unix::fs::FileExt as _;
    use std::time::Instant;

    crate::debug::emit_marker("COORD_PAYLOADS_FINALIZE_START");
    let t_all = Instant::now();

    let num_way_blobs = per_way_rcs.num_blobs();

    // Build blob_location: for each blob_idx, which (worker_id, ManifestEntry) holds it.
    // blob_location[i] = Some((worker_id, &ManifestEntry)) or None.
    let mut blob_location: Vec<Option<(usize, u64, u64)>> = vec![None; num_way_blobs];
    for (worker_id, manifest) in worker_manifests.iter().enumerate() {
        for entry in manifest {
            let idx = entry.blob_idx as usize;
            if idx >= num_way_blobs {
                return Err(format!(
                    "worker {worker_id} manifest entry blob_idx {idx} out of range (num_way_blobs={num_way_blobs})"
                ).into());
            }
            if blob_location[idx].is_some() {
                return Err(format!("blob {idx} appears in multiple worker manifests").into());
            }
            blob_location[idx] = Some((worker_id, entry.byte_offset, entry.byte_length));
        }
    }

    // Validate: a blob_idx cannot appear in both manifest and straddler_slots.
    for (idx, loc) in blob_location.iter().enumerate() {
        if loc.is_some() {
            let guard = straddler_slots[idx]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_some() {
                return Err(format!(
                    "blob {idx} appears in both a worker manifest and straddler staging"
                )
                .into());
            }
        }
    }

    // Open worker temp files for pread.
    let mut worker_files: Vec<std::fs::File> = Vec::with_capacity(worker_tmp_paths.len());
    for path in worker_tmp_paths {
        let f = std::fs::File::open(path)
            .map_err(|e| format!("open worker tmp {}: {e}", path.display()))?;
        worker_files.push(f);
    }

    let output_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output_path)
        .map_err(|e| format!("create coord_payloads: {e}"))?;

    let header_size: u64 = 16 + (num_way_blobs as u64 + 1) * 8;
    let mut blob_offsets: Vec<u64> = Vec::with_capacity(num_way_blobs + 1);
    blob_offsets.push(0);

    let mut output_writer = std::io::BufWriter::with_capacity(
        1024 * 1024,
        output_file
            .try_clone()
            .map_err(|e| format!("clone coord_payloads handle: {e}"))?,
    );
    output_writer
        .seek(std::io::SeekFrom::Start(header_size))
        .map_err(|e| format!("seek past header in coord_payloads: {e}"))?;

    let mut encode_scratch: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut read_scratch: Vec<u8> = Vec::new();
    let mut payload_pos: u64 = 0;
    let mut stats = FinalizeStats {
        output_bytes: 0,
        num_way_blobs: num_way_blobs as u64,
        num_straddlers: 0,
        finalize_ms: 0,
        read_ms: 0,
        encode_ms: 0,
        write_ms: 0,
    };

    for blob_idx in 0..num_way_blobs {
        let straddler_taken = straddler_slots[blob_idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        let bytes_written: usize = match straddler_taken {
            Some(StraddlerSlot::Both { left, right }) => {
                let mut coord_bytes = left;
                coord_bytes.extend_from_slice(&right);

                let t_enc = Instant::now();
                encode_scratch.clear();
                encode_blob_payload(&coord_bytes, per_way_rcs.blob(blob_idx), &mut encode_scratch)
                    .map_err(|e| format!("finalize straddler blob {blob_idx}: {e}"))?;
                #[allow(clippy::cast_possible_truncation)]
                { stats.encode_ms += t_enc.elapsed().as_millis() as u64; }

                let t_wr = Instant::now();
                output_writer
                    .write_all(&encode_scratch)
                    .map_err(|e| format!("write coord_payloads blob {blob_idx}: {e}"))?;
                #[allow(clippy::cast_possible_truncation)]
                { stats.write_ms += t_wr.elapsed().as_millis() as u64; }

                stats.num_straddlers += 1;
                encode_scratch.len()
            }
            Some(StraddlerSlot::Left(_)) => {
                return Err(format!(
                    "blob {blob_idx}: straddler missing right half"
                )
                .into());
            }
            Some(StraddlerSlot::Right(_)) => {
                return Err(format!(
                    "blob {blob_idx}: straddler missing left half"
                )
                .into());
            }
            None => {
                match blob_location[blob_idx] {
                    Some((worker_id, byte_offset, byte_length)) => {
                        #[allow(clippy::cast_possible_truncation)]
                        let len = byte_length as usize;
                        read_scratch.resize(len, 0);

                        let t_rd = Instant::now();
                        if len > 0 {
                            worker_files[worker_id]
                                .read_exact_at(&mut read_scratch, byte_offset)
                                .map_err(|e| format!("pread worker {worker_id} blob {blob_idx}: {e}"))?;
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        { stats.read_ms += t_rd.elapsed().as_millis() as u64; }

                        let t_wr = Instant::now();
                        output_writer
                            .write_all(&read_scratch)
                            .map_err(|e| format!("write coord_payloads blob {blob_idx}: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        { stats.write_ms += t_wr.elapsed().as_millis() as u64; }

                        len
                    }
                    None => {
                        // No manifest entry and no straddler staging. Valid
                        // only for a zero-ref blob; any non-zero rc here means
                        // stage 3 lost this blob (upstream bug).
                        let rcs = per_way_rcs.blob(blob_idx);
                        if rcs.iter().any(|&r| r != 0) {
                            return Err(format!(
                                "blob {blob_idx} has non-zero ref counts but no worker manifest \
                                 entry and no straddler staging — upstream bug"
                            ).into());
                        }
                        encode_scratch.clear();
                        encode_blob_payload(&[], rcs, &mut encode_scratch)
                            .map_err(|e| format!("encode zero-ref blob {blob_idx}: {e}"))?;

                        let t_wr = Instant::now();
                        output_writer
                            .write_all(&encode_scratch)
                            .map_err(|e| format!("write coord_payloads blob {blob_idx}: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        { stats.write_ms += t_wr.elapsed().as_millis() as u64; }

                        encode_scratch.len()
                    }
                }
            }
        };

        payload_pos += bytes_written as u64;
        blob_offsets.push(payload_pos);
    }

    output_writer
        .flush()
        .map_err(|e| format!("flush coord_payloads: {e}"))?;
    drop(output_writer);

    stats.output_bytes = header_size + payload_pos;

    let mut header_buf: Vec<u8> = Vec::with_capacity(header_size as usize);
    header_buf.extend_from_slice(&(num_way_blobs as u64).to_le_bytes());
    header_buf.extend_from_slice(&payload_pos.to_le_bytes());
    for &off in &blob_offsets {
        header_buf.extend_from_slice(&off.to_le_bytes());
    }
    debug_assert_eq!(header_buf.len() as u64, header_size);
    debug_assert_eq!(blob_offsets.len(), num_way_blobs + 1);

    output_file
        .write_all_at(&header_buf, 0)
        .map_err(|e| format!("pwrite coord_payloads header: {e}"))?;
    output_file
        .sync_data()
        .map_err(|e| format!("sync coord_payloads: {e}"))?;

    #[allow(clippy::cast_possible_truncation)]
    {
        stats.finalize_ms = t_all.elapsed().as_millis() as u64;
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_integrated_finalize_encode_ms", stats.encode_ms as i64);
        crate::debug::emit_counter("s3_integrated_finalize_read_ms", stats.read_ms as i64);
        crate::debug::emit_counter("s3_integrated_finalize_write_ms", stats.write_ms as i64);
        crate::debug::emit_counter("s3_integrated_output_bytes", stats.output_bytes as i64);
        crate::debug::emit_counter("s3_integrated_straddler_count", stats.num_straddlers as i64);
    }
    crate::debug::emit_marker("COORD_PAYLOADS_FINALIZE_END");

    Ok(stats)
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

/// Reader for `coord_payloads`. Holds the file handle + offset table in
/// memory (`~ 140 KB` at planet scale; trivial).
pub(super) struct CoordPayloadsReader {
    file: std::fs::File,
    /// Byte offset of blob i's payload within the file. blob_offsets.len()
    /// == num_way_blobs + 1; blob i's payload spans
    /// `[blob_offsets[i], blob_offsets[i+1])`.
    blob_offsets: Vec<u64>,
    payload_base: u64,
}

impl CoordPayloadsReader {
    pub(super) fn open(path: &Path, expected_num_blobs: usize) -> Result<Self> {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path)
            .map_err(|e| format!("open coord_payloads: {e}"))?;
        let mut hdr = [0u8; 16];
        file.read_exact(&mut hdr)
            .map_err(|e| format!("read coord_payloads header: {e}"))?;
        let num_way_blobs = u64::from_le_bytes([
            hdr[0], hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6], hdr[7],
        ]);
        let total_payload_bytes = u64::from_le_bytes([
            hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
        ]);
        #[allow(clippy::cast_possible_truncation)]
        let n = num_way_blobs as usize;
        if n != expected_num_blobs {
            return Err(format!(
                "coord_payloads num_way_blobs={n} != expected {expected_num_blobs}"
            )
            .into());
        }
        let mut offsets_bytes = vec![0u8; (n + 1) * 8];
        file.read_exact(&mut offsets_bytes)
            .map_err(|e| format!("read coord_payloads offsets: {e}"))?;
        let mut blob_offsets: Vec<u64> = Vec::with_capacity(n + 1);
        for chunk in offsets_bytes.chunks_exact(8) {
            blob_offsets.push(u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
                chunk[4], chunk[5], chunk[6], chunk[7],
            ]));
        }
        if blob_offsets[n] != total_payload_bytes {
            return Err(format!(
                "coord_payloads trailing offset {} != total_payload_bytes {}",
                blob_offsets[n], total_payload_bytes
            )
            .into());
        }
        let payload_base: u64 = 16 + ((n as u64) + 1) * 8;
        Ok(Self {
            file,
            blob_offsets,
            payload_base,
        })
    }

    /// Read blob `blob_idx`'s payload into `buf` (resized to exact length).
    pub(super) fn pread_blob_payload(&self, blob_idx: usize, buf: &mut Vec<u8>) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        let start = self.blob_offsets[blob_idx];
        let end = self.blob_offsets[blob_idx + 1];
        #[allow(clippy::cast_possible_truncation)]
        let len = (end - start) as usize;
        buf.resize(len, 0);
        if len > 0 {
            self.file
                .read_exact_at(buf, self.payload_base + start)
                .map_err(|e| format!("pread coord_payloads blob {blob_idx}: {e}"))?;
        }
        Ok(())
    }
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
        assert_eq!(pwr.num_blobs(), 2);
        assert_eq!(pwr.blob(0), &[3u32, 1u32]);
        assert_eq!(pwr.blob(1), &[2u32]);
    }

    #[test]
    fn parse_per_way_refcount_sidecar_bytes_empty_blob() {
        // blob 0: 0 ways; blob 1: 1 way with 1 ref
        let sidecar = make_sidecar_bytes(&[&[], &[1]]);
        let pwr = parse_per_way_refcount_sidecar_bytes(&sidecar, 2).expect("parse");
        assert_eq!(pwr.blob(0), &[] as &[u32]);
        assert_eq!(pwr.blob(1), &[1u32]);
    }

    // Helper: build a PerWayRcs directly from slice-of-slices.
    fn make_per_way_rcs(blobs: &[&[u32]]) -> PerWayRcs {
        let sidecar = make_sidecar_bytes(blobs);
        parse_per_way_refcount_sidecar_bytes(&sidecar, blobs.len()).expect("make_per_way_rcs")
    }

    #[test]
    fn finalize_coord_payloads_happy_path() {
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

        let output_path = tmp_dir.path().join("coord_payloads");
        let stats = finalize_coord_payloads(
            &output_path,
            &per_way_rcs,
            worker_manifests,
            &[w0_path, w1_path],
            straddler_slots,
        ).expect("finalize");
        assert_eq!(stats.num_way_blobs, 4);
        assert_eq!(stats.num_straddlers, 1);

        let reader = CoordPayloadsReader::open(&output_path, 4).expect("open reader");
        let mut buf: Vec<u8> = Vec::new();

        reader.pread_blob_payload(0, &mut buf).expect("pread 0");
        assert_eq!(reconstruct_coords(&buf, rcs_b0), coords_b0);

        reader.pread_blob_payload(1, &mut buf).expect("pread 1");
        assert_eq!(reconstruct_coords(&buf, rcs_b1), coords_b1);

        reader.pread_blob_payload(2, &mut buf).expect("pread 2");
        assert_eq!(reconstruct_coords(&buf, rcs_b2), coords_b2);

        // blob 3: zero refs — payload is empty.
        reader.pread_blob_payload(3, &mut buf).expect("pread 3");
        assert!(buf.is_empty(), "zero-ref blob payload must be empty");
    }

    #[test]
    fn finalize_coord_payloads_missing_half_error() {
        let per_way_rcs = make_per_way_rcs(&[&[1]]);
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let w0_path = tmp_dir.path().join("payloads-W0");
        std::fs::write(&w0_path, b"").expect("write w0");

        let straddler_slots: Vec<std::sync::Mutex<Option<StraddlerSlot>>> = vec![
            std::sync::Mutex::new(Some(StraddlerSlot::Left(vec![0u8; 8]))),
        ];

        let output_path = tmp_dir.path().join("coord_payloads");
        let result = finalize_coord_payloads(
            &output_path,
            &per_way_rcs,
            vec![vec![]],
            &[w0_path],
            straddler_slots,
        );
        assert!(result.is_err(), "expected Err for missing right half");
        let msg = format!("{}", result.expect_err("err"));
        assert!(msg.contains("missing right half"), "error message: {msg}");
    }
}
