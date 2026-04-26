//! Byte-level adversarial fixture primitives.
//!
//! Helpers for individual tests to inject malformed bytes into known-good
//! PBF files: reversed/overshooting indexdata ranges, truncated varints in
//! relation memids, DenseNodes with adversarial granularity. Without these
//! primitives every test would hand-roll wire-format manipulation.
//!
//! Tests construct a known-good PBF via the existing fixture writers
//! (`write_test_pbf`, `write_multi_block_test_pbf`), then call one of:
//!
//! - [`mutate_blob_header_indexdata`] - alter the BlobHeader.indexdata bytes
//! - [`mutate_blob_payload`] - alter the OSMData PrimitiveBlock bytes
//! - [`truncate_to`] - chop the file at a byte boundary (T04 sweep)
//!
//! Internally we walk the wire format with a tiny varint reader. We never
//! import pbfhogg internals - these helpers stay viable across rewrites.
//!
//! ## PBF layout (reference)
//!
//! ```text
//! Stream = Frame*
//! Frame  = [u32 BE length L] [L bytes BlobHeader] [datasize bytes Blob]
//!
//! BlobHeader (protobuf)
//!   field 1, string  type           e.g. "OSMHeader" or "OSMData"
//!   field 2, bytes   indexdata      pbfhogg-specific, optional
//!   field 3, int32   datasize       length of the Blob bytes that follow
//!
//! Blob (protobuf)
//!   field 2, int32   raw_size       decompressed size, only when compressed
//!   oneof data
//!     field 1, bytes raw            uncompressed
//!     field 3, bytes zlib_data      zlib-compressed
//!     field 5, bytes zstd_data      zstd-compressed
//! ```
//!
//! When a payload mutation lands we re-emit the Blob as raw (uncompressed)
//! and rebuild the BlobHeader so the frame stays self-consistent.

use std::io::Read;

use flate2::read::ZlibDecoder;

/// Byte ranges of one frame in a PBF file.
///
/// Returned by [`locate_blobs`]. All offsets are absolute into the input
/// buffer, never overlapping ranges across frames.
#[derive(Debug, Clone, Copy)]
pub struct BlobLocation {
    /// First byte of the 4-byte big-endian length prefix.
    pub frame_start: usize,
    /// First byte of the BlobHeader protobuf (after the length prefix).
    pub header_start: usize,
    /// One past the last byte of the BlobHeader.
    pub header_end: usize,
    /// First byte of the Blob protobuf (== `header_end`).
    pub blob_start: usize,
    /// One past the last byte of the Blob.
    pub blob_end: usize,
}

/// Walk the PBF byte stream and return every frame's range.
///
/// Stops at the first malformed frame instead of panicking - tests that
/// truncate the file rely on this. The returned vector contains only
/// frames that were fully readable.
pub fn locate_blobs(pbf: &[u8]) -> Vec<BlobLocation> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < pbf.len() {
        if pbf.len() - pos < 4 {
            break;
        }
        let header_len = u32::from_be_bytes([
            pbf[pos],
            pbf[pos + 1],
            pbf[pos + 2],
            pbf[pos + 3],
        ]) as usize;
        let header_start = pos + 4;
        let Some(header_end) = header_start.checked_add(header_len) else {
            break;
        };
        if header_end > pbf.len() {
            break;
        }
        let datasize = match parse_datasize(&pbf[header_start..header_end]) {
            Some(d) => d as usize,
            None => break,
        };
        let blob_start = header_end;
        let Some(blob_end) = blob_start.checked_add(datasize) else {
            break;
        };
        if blob_end > pbf.len() {
            break;
        }
        out.push(BlobLocation {
            frame_start: pos,
            header_start,
            header_end,
            blob_start,
            blob_end,
        });
        pos = blob_end;
    }
    out
}

/// Mutate the BlobHeader.indexdata bytes of frame `blob_idx`.
///
/// The closure receives the raw indexdata bytes (an empty `Vec` if the
/// blob has no indexdata field) and may rewrite them freely. Use this
/// to inject reversed `min_id`/`max_id` ranges or overshooting lengths.
///
/// Returns a new PBF buffer with the change applied. The frame length
/// prefix is recomputed; the Blob payload and BlobHeader.datasize are
/// preserved verbatim. If the closure produces an empty vector and the
/// original had no indexdata field, the field stays absent; otherwise an
/// indexdata field of the new length is emitted.
pub fn mutate_blob_header_indexdata(
    pbf: &[u8],
    blob_idx: usize,
    f: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    let blobs = locate_blobs(pbf);
    let target = *blobs.get(blob_idx).expect("blob_idx out of range");

    let header = &pbf[target.header_start..target.header_end];
    let blob_type = read_bytes_field(header, 1).expect("BlobHeader.type missing");
    let datasize = parse_datasize(header).expect("BlobHeader.datasize missing");
    let original_indexdata = locate_indexdata(header).map(|(s, e)| header[s..e].to_vec());
    let mut indexdata = original_indexdata.clone().unwrap_or_default();
    f(&mut indexdata);
    let indexdata_for_emit = if indexdata.is_empty() && original_indexdata.is_none() {
        None
    } else {
        Some(indexdata.as_slice())
    };
    let new_header = build_blob_header(&blob_type, indexdata_for_emit, datasize);

    splice_frame(
        pbf,
        target,
        &new_header,
        &pbf[target.blob_start..target.blob_end],
    )
}

/// Mutate the decompressed OSMData/OSMHeader payload of frame `blob_idx`.
///
/// The closure receives the raw protobuf bytes of the inner message
/// (PrimitiveBlock for OSMData blobs, HeaderBlock for OSMHeader blobs)
/// and may rewrite them freely. Use this to truncate varints inside a
/// relation's memids field or splice in adversarial DenseNodes
/// granularity values.
///
/// Returns a new PBF buffer with the change applied. The frame is
/// re-emitted with the payload as a raw (uncompressed) Blob, and the
/// BlobHeader.datasize and frame length prefix are recomputed. The
/// indexdata field is preserved verbatim - tests that need to mutate
/// it use [`mutate_blob_header_indexdata`].
pub fn mutate_blob_payload(
    pbf: &[u8],
    blob_idx: usize,
    f: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    let blobs = locate_blobs(pbf);
    let target = *blobs.get(blob_idx).expect("blob_idx out of range");

    let blob = &pbf[target.blob_start..target.blob_end];
    let mut payload = decompress_blob(blob).expect("decompress fixture blob");
    f(&mut payload);
    let new_blob = emit_blob_raw(&payload);

    let header = &pbf[target.header_start..target.header_end];
    let blob_type = read_bytes_field(header, 1).expect("BlobHeader.type missing");
    let indexdata = locate_indexdata(header).map(|(s, e)| header[s..e].to_vec());
    let new_size = u32::try_from(new_blob.len()).expect("blob fits in u32 datasize");
    let new_header = build_blob_header(&blob_type, indexdata.as_deref(), new_size);

    splice_frame(pbf, target, &new_header, &new_blob)
}

/// Chop the PBF at a byte offset.
///
/// Returns `pbf[..len.min(pbf.len())]` as an owned vector. Used by the
/// truncation sweep (T04) to drive every command against every truncation
/// boundary of a known-good fixture.
pub fn truncate_to(pbf: &[u8], len: usize) -> Vec<u8> {
    pbf[..len.min(pbf.len())].to_vec()
}

// ---------------------------------------------------------------------------
// Internal helpers (varint, protobuf scan, blob (de)compression).
// ---------------------------------------------------------------------------

fn splice_frame(pbf: &[u8], target: BlobLocation, new_header: &[u8], new_blob: &[u8]) -> Vec<u8> {
    let header_len_be = u32::try_from(new_header.len())
        .expect("BlobHeader fits in u32")
        .to_be_bytes();
    let mut out =
        Vec::with_capacity(pbf.len() + new_header.len() + new_blob.len() - (target.blob_end - target.frame_start));
    out.extend_from_slice(&pbf[..target.frame_start]);
    out.extend_from_slice(&header_len_be);
    out.extend_from_slice(new_header);
    out.extend_from_slice(new_blob);
    out.extend_from_slice(&pbf[target.blob_end..]);
    out
}

fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if i >= 10 {
            return None;
        }
        value |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        let byte = u8::try_from(value & 0x7F).expect("masked to 7 bits") | 0x80;
        out.push(byte);
        value >>= 7;
    }
    out.push(u8::try_from(value).expect("final byte fits in u8"));
}

/// Scan a protobuf message and return the next field's tag bytes consumed
/// alongside any wire-typed body. Returns `(field_number, wire_type, body_end_pos)`
/// for field, advancing `pos` past the body.
fn skip_field(buf: &[u8], pos: &mut usize, wire: u64) -> Option<()> {
    match wire {
        0 => {
            let (_, n) = read_varint(&buf[*pos..])?;
            *pos += n;
        }
        1 => *pos = pos.checked_add(8)?,
        2 => {
            let (len, n) = read_varint(&buf[*pos..])?;
            *pos += n;
            let len_us = usize::try_from(len).ok()?;
            *pos = pos.checked_add(len_us)?;
        }
        5 => *pos = pos.checked_add(4)?,
        _ => return None,
    }
    if *pos > buf.len() {
        return None;
    }
    Some(())
}

fn parse_datasize(header: &[u8]) -> Option<u32> {
    let mut pos = 0;
    while pos < header.len() {
        let (tag, np) = read_varint(&header[pos..])?;
        let field = tag >> 3;
        let wire = tag & 7;
        pos += np;
        if field == 3 && wire == 0 {
            let (val, _) = read_varint(&header[pos..])?;
            return u32::try_from(val).ok();
        }
        skip_field(header, &mut pos, wire)?;
    }
    None
}

fn locate_indexdata(header: &[u8]) -> Option<(usize, usize)> {
    let mut pos = 0;
    while pos < header.len() {
        let (tag, np) = read_varint(&header[pos..])?;
        let field = tag >> 3;
        let wire = tag & 7;
        pos += np;
        if field == 2 && wire == 2 {
            let (len, n) = read_varint(&header[pos..])?;
            let start = pos + n;
            let len_us = usize::try_from(len).ok()?;
            let end = start.checked_add(len_us)?;
            return Some((start, end));
        }
        skip_field(header, &mut pos, wire)?;
    }
    None
}

fn read_bytes_field(msg: &[u8], target_field: u64) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos < msg.len() {
        let (tag, np) = read_varint(&msg[pos..])?;
        let field = tag >> 3;
        let wire = tag & 7;
        pos += np;
        if field == target_field && wire == 2 {
            let (len, n) = read_varint(&msg[pos..])?;
            let start = pos + n;
            let len_us = usize::try_from(len).ok()?;
            let end = start.checked_add(len_us)?;
            return Some(msg[start..end].to_vec());
        }
        skip_field(msg, &mut pos, wire)?;
    }
    None
}

fn decompress_blob(blob: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 0;
    let mut data: Option<(u64, Vec<u8>)> = None;
    while pos < blob.len() {
        let (tag, np) = read_varint(&blob[pos..]).ok_or("bad blob tag")?;
        let field = tag >> 3;
        let wire = tag & 7;
        pos += np;
        match (field, wire) {
            (1, 2) | (3, 2) | (5, 2) => {
                let (len, n) = read_varint(&blob[pos..]).ok_or("bad blob data length")?;
                pos += n;
                let len_us = usize::try_from(len).map_err(|e| e.to_string())?;
                let end = pos.checked_add(len_us).ok_or("blob data overflow")?;
                if end > blob.len() {
                    return Err("blob data range exceeds blob".into());
                }
                data = Some((field, blob[pos..end].to_vec()));
                pos = end;
            }
            _ => {
                skip_field(blob, &mut pos, wire).ok_or("malformed blob")?;
            }
        }
    }
    let (field, bytes) = data.ok_or("blob has no data field")?;
    match field {
        1 => Ok(bytes),
        3 => {
            let mut out = Vec::new();
            ZlibDecoder::new(&bytes[..])
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            Ok(out)
        }
        5 => zstd::stream::decode_all(&bytes[..]).map_err(|e| e.to_string()),
        other => Err(format!("unsupported Blob data field {other}")),
    }
}

fn emit_blob_raw(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x0A);
    write_varint(&mut out, u64::try_from(payload.len()).expect("payload len fits in u64"));
    out.extend_from_slice(payload);
    out
}

fn build_blob_header(blob_type: &[u8], indexdata: Option<&[u8]>, datasize: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x0A);
    write_varint(&mut out, u64::try_from(blob_type.len()).expect("type len fits in u64"));
    out.extend_from_slice(blob_type);
    if let Some(ix) = indexdata {
        out.push(0x12);
        write_varint(&mut out, u64::try_from(ix.len()).expect("indexdata len fits in u64"));
        out.extend_from_slice(ix);
    }
    out.push(0x18);
    write_varint(&mut out, u64::from(datasize));
    out
}

// Minimal sanity test for the helpers themselves. Larger smoke coverage
// lives in `tests/fixture_helpers.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for &v in &[0_u64, 1, 127, 128, 255, 16_384, 1_u64 << 32, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let (got, n) = read_varint(&buf).expect("decode");
            assert_eq!(got, v);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn truncate_to_caps_to_len() {
        let bytes = vec![1, 2, 3, 4, 5];
        assert_eq!(truncate_to(&bytes, 3), vec![1, 2, 3]);
        assert_eq!(truncate_to(&bytes, 99), bytes);
        assert_eq!(truncate_to(&bytes, 0), Vec::<u8>::new());
    }
}

