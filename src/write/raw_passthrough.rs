//! Raw group passthrough: assemble PrimitiveBlock protobuf from raw components.
//!
//! For groups where all elements are selected, copy the raw protobuf group bytes
//! instead of decoding + re-encoding via BlockBuilder. The StringTable is copied
//! whole. Only partial-match groups need full decode + re-encode.
//!
//! See `notes/raw-group-passthrough.md` for the design.

use protohoggr::{encode_bytes_field_always, encode_varint};

/// Assemble a PrimitiveBlock protobuf from raw StringTable bytes,
/// raw/re-encoded group bytes, and scalar fields.
///
/// The output is the serialized PrimitiveBlock message (not framed — the caller
/// wraps it via `PbfWriter::write_primitive_block` or similar).
///
/// # Arguments
/// - `stringtable`: raw StringTable protobuf bytes (field 1 submessage content)
/// - `groups`: list of raw PrimitiveGroup protobuf bytes (field 2 submessage content each)
/// - `granularity`: field 17 (default 100)
/// - `lat_offset`: field 19 (default 0)
/// - `lon_offset`: field 20 (default 0)
/// - `date_granularity`: field 18 (default 1000)
#[allow(clippy::cast_sign_loss)]
#[allow(dead_code)] // Scaffolding for future per-group raw passthrough
pub(crate) fn frame_raw_block(
    stringtable: &[u8],
    groups: &[&[u8]],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
    date_granularity: i32,
    out: &mut Vec<u8>,
) {
    out.clear();

    // Field 1: StringTable (len-delimited)
    encode_bytes_field_always(out, 1, stringtable);

    // Field 2: PrimitiveGroup (repeated, len-delimited)
    for group in groups {
        encode_bytes_field_always(out, 2, group);
    }

    // Field 17: granularity (varint, default 100)
    if granularity != 100 {
        encode_varint(out, (17 << 3) as u64); // field 17, wire type 0
        encode_varint(out, granularity as u64);
    }

    // Field 18: date_granularity (varint, default 1000)
    if date_granularity != 1000 {
        encode_varint(out, (18 << 3) as u64);
        encode_varint(out, date_granularity as u64);
    }

    // Field 19: lat_offset (varint, default 0)
    if lat_offset != 0 {
        encode_varint(out, (19 << 3) as u64);
        encode_varint(out, lat_offset as u64);
    }

    // Field 20: lon_offset (varint, default 0)
    if lon_offset != 0 {
        encode_varint(out, (20 << 3) as u64);
        encode_varint(out, lon_offset as u64);
    }
}
