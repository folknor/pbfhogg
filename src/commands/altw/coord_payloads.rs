//! Blob-ordered delta-varint coord payload format (prototype).
//!
//! Transforms the existing `coord_slots` file (8 bytes per slot, indexed by
//! global slot_pos) into a compressed `coord_payloads` file consumed per
//! way blob. The hypothesis is that per-way delta-encoded varints compress
//! the 37 GB (Europe) / 99 GB (planet) flat coord_slots by 3-4×, reducing
//! stage 4's I/O-bound coord-read wall by a proportional amount.
//!
//! Validated against: `notes/altw-optimization-history.md` "Stage 4
//! bottleneck isolated 2026-04-14" — coord read is 720 MB/s × 37 GB / 6
//! workers ≈ 51 s wall; compressing to ~10 GB projects ~17 s wall read.
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
//! coord_slots starting at `slot_start + cursor`. Emit `2*N` zigzag-varints:
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

const _: () = {
    assert!(COORD_SLOT_SIZE == 8);
};

/// Results of the transform pass, for measurement.
pub(super) struct TransformStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub num_way_blobs: u64,
    pub num_ways: u64,
    pub num_refs: u64,
    pub transform_ms: u64,
    pub read_ms: u64,
    pub encode_ms: u64,
    pub write_ms: u64,
    pub sidecar_parse_ms: u64,
}

/// Load the per-way refcount sidecar emitted by stage 1 pass A. Returns
/// `per_way_rcs[blob_idx]` = `Vec<u32>` of per-way ref counts for way blob
/// `blob_idx` (schedule-order).
pub(super) fn load_per_way_refcount_sidecar(
    path: &Path,
    num_way_blobs: usize,
) -> Result<Vec<Vec<u32>>> {
    let data = std::fs::read(path)
        .map_err(|e| format!("read per-way refcount sidecar: {e}"))?;
    let mut cursor = protohoggr::Cursor::new(&data);
    let mut result: Vec<Vec<u32>> = Vec::with_capacity(num_way_blobs);
    for blob_idx in 0..num_way_blobs {
        let num_ways = cursor
            .read_varint()
            .map_err(|e| format!("per-way sidecar blob {blob_idx} num_ways: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let num_ways_usize = num_ways as usize;
        let mut rcs: Vec<u32> = Vec::with_capacity(num_ways_usize);
        for way_idx in 0..num_ways_usize {
            let rc = cursor
                .read_varint()
                .map_err(|e| format!("per-way sidecar blob {blob_idx} way {way_idx}: {e}"))?;
            #[allow(clippy::cast_possible_truncation)]
            rcs.push(rc as u32);
        }
        result.push(rcs);
    }
    if cursor.remaining() != 0 {
        return Err(format!(
            "per-way refcount sidecar has {} trailing bytes",
            cursor.remaining()
        )
        .into());
    }
    Ok(result)
}

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

/// Transform `coord_slots` → `coord_payloads`.
///
/// `way_slot_starts[blob_idx]` is the starting slot position for way blob
/// `blob_idx`. Blob's slot range is
/// `[way_slot_starts[blob_idx], way_slot_starts[blob_idx+1])` (last uses
/// `total_slots`).
///
/// `per_way_rcs[blob_idx][way_idx]` ref count for way in blob.
/// Per-blob sum must equal the blob's slot range length.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn transform_coord_slots_to_payloads(
    coord_slots_path: &Path,
    per_way_refcount_sidecar_path: &Path,
    coord_payloads_path: &Path,
    way_slot_starts: &[u64],
    total_slots: u64,
) -> Result<TransformStats> {
    use std::os::unix::fs::FileExt as _;

    crate::debug::emit_marker("COORD_PAYLOADS_TRANSFORM_START");

    if way_slot_starts.is_empty() {
        return Err("way_slot_starts is empty".into());
    }

    let num_way_blobs = way_slot_starts.len();
    let mut stats = TransformStats {
        input_bytes: total_slots * COORD_SLOT_SIZE as u64,
        output_bytes: 0,
        num_way_blobs: num_way_blobs as u64,
        num_ways: 0,
        num_refs: 0,
        transform_ms: 0,
        read_ms: 0,
        encode_ms: 0,
        write_ms: 0,
        sidecar_parse_ms: 0,
    };
    let t_all = std::time::Instant::now();

    let t_sidecar = std::time::Instant::now();
    let per_way_rcs = load_per_way_refcount_sidecar(per_way_refcount_sidecar_path, num_way_blobs)?;
    stats.sidecar_parse_ms = t_sidecar.elapsed().as_millis() as u64;

    // Validate sidecar totals match slot starts (per blob).
    for (blob_idx, rcs) in per_way_rcs.iter().enumerate() {
        let blob_ref_count: u64 = rcs.iter().map(|&r| u64::from(r)).sum();
        let expected = if blob_idx + 1 < num_way_blobs {
            way_slot_starts[blob_idx + 1] - way_slot_starts[blob_idx]
        } else {
            total_slots - way_slot_starts[blob_idx]
        };
        if blob_ref_count != expected {
            return Err(format!(
                "per-way sidecar blob {blob_idx} sum={blob_ref_count} vs slot-range={expected}"
            )
            .into());
        }
    }

    let coord_file = std::fs::File::open(coord_slots_path)
        .map_err(|e| format!("open coord_slots: {e}"))?;
    let output_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(coord_payloads_path)
        .map_err(|e| format!("create coord_payloads: {e}"))?;

    // Header + offset table occupy the first `header_size` bytes; payload
    // follows. We seek past the header, write payload sequentially through
    // a BufWriter, then rewind and write the header + offsets via pwrite.
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
        .map_err(|e| format!("seek past header: {e}"))?;

    let mut coord_buf: Vec<u8> = Vec::new();
    let mut payload_buf: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut payload_pos: u64 = 0;

    for blob_idx in 0..num_way_blobs {
        let slot_start = way_slot_starts[blob_idx];
        let blob_ref_count = if blob_idx + 1 < num_way_blobs {
            way_slot_starts[blob_idx + 1] - way_slot_starts[blob_idx]
        } else {
            total_slots - way_slot_starts[blob_idx]
        };
        let coord_byte_len = (blob_ref_count as usize) * COORD_SLOT_SIZE;
        let rcs = &per_way_rcs[blob_idx];

        let t_read = std::time::Instant::now();
        coord_buf.resize(coord_byte_len, 0);
        if coord_byte_len > 0 {
            coord_file
                .read_exact_at(&mut coord_buf, slot_start * COORD_SLOT_SIZE as u64)
                .map_err(|e| format!("read coord_slots blob {blob_idx}: {e}"))?;
        }
        stats.read_ms += t_read.elapsed().as_millis() as u64;

        for &rc in rcs {
            stats.num_ways += 1;
            stats.num_refs += u64::from(rc);
        }

        let t_enc = std::time::Instant::now();
        payload_buf.clear();
        encode_blob_payload(&coord_buf, rcs, &mut payload_buf)
            .map_err(|e| format!("blob {blob_idx}: {e}"))?;
        stats.encode_ms += t_enc.elapsed().as_millis() as u64;

        let t_wr = std::time::Instant::now();
        output_writer
            .write_all(&payload_buf)
            .map_err(|e| format!("write coord_payloads blob {blob_idx}: {e}"))?;
        stats.write_ms += t_wr.elapsed().as_millis() as u64;

        payload_pos += payload_buf.len() as u64;
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
    output_file
        .write_all_at(&header_buf, 0)
        .map_err(|e| format!("pwrite coord_payloads header: {e}"))?;
    output_file
        .sync_data()
        .map_err(|e| format!("sync coord_payloads: {e}"))?;

    stats.transform_ms = t_all.elapsed().as_millis() as u64;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("coord_payloads_transform_ms", stats.transform_ms as i64);
        crate::debug::emit_counter("coord_payloads_read_ms", stats.read_ms as i64);
        crate::debug::emit_counter("coord_payloads_encode_ms", stats.encode_ms as i64);
        crate::debug::emit_counter("coord_payloads_write_ms", stats.write_ms as i64);
        crate::debug::emit_counter("coord_payloads_sidecar_parse_ms", stats.sidecar_parse_ms as i64);
        crate::debug::emit_counter("coord_payloads_input_bytes", stats.input_bytes as i64);
        crate::debug::emit_counter("coord_payloads_output_bytes", stats.output_bytes as i64);
        crate::debug::emit_counter("coord_payloads_num_way_blobs", stats.num_way_blobs as i64);
        crate::debug::emit_counter("coord_payloads_num_ways", stats.num_ways as i64);
        crate::debug::emit_counter("coord_payloads_num_refs", stats.num_refs as i64);
    }
    crate::debug::emit_marker("COORD_PAYLOADS_TRANSFORM_END");

    Ok(stats)
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
}
