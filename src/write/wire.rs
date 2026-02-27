//! Wire-format encoding primitives for direct protobuf serialization.
//!
//! Mirrors the read-side [`wire`](crate::read::wire) decoding primitives.
//! Used by [`BlockBuilder`](super::block_builder::BlockBuilder) to encode
//! ways, relations, dense nodes, and header blocks directly to protobuf
//! wire format without intermediate allocations.

/// Protobuf wire type: variable-length integer (LEB128).
const WIRE_VARINT: u32 = 0;

/// Protobuf wire type: length-delimited (bytes, strings, submessages, packed repeated).
const WIRE_LEN: u32 = 2;

// ---------------------------------------------------------------------------
// Core varint / zigzag encoding
// ---------------------------------------------------------------------------

/// Encode a `u64` as a variable-length integer (LEB128) into `buf`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Zigzag-encode a signed 64-bit integer for `sint64` fields.
///
/// Maps: 0 → 0, -1 → 1, 1 → 2, -2 → 3, 2 → 4, …
/// Inverse of `zigzag_decode_64` in `src/read/wire.rs`.
#[inline]
#[allow(clippy::cast_sign_loss)]
pub(crate) fn zigzag_encode_64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Zigzag-encode a signed 32-bit integer for `sint32` fields.
#[inline]
#[allow(clippy::cast_sign_loss)]
pub(crate) fn zigzag_encode_32(v: i32) -> u64 {
    ((v << 1) ^ (v >> 31)) as u64
}

// ---------------------------------------------------------------------------
// Field-level encoders
//
// Tags are encoded via `encode_tag()` which handles field numbers > 15
// (e.g. HeaderBlock fields 16, 17, 32, 33, 34).
// ---------------------------------------------------------------------------

/// Encode a varint field. Skips if `value == 0` (matches prost default-skipping).
///
/// For proto `int64`, `uint64`, `uint32` field types.
#[inline]
#[cfg(test)]
#[allow(dead_code, clippy::cast_possible_truncation)]
pub(crate) fn encode_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, value);
    }
}

/// Encode a field tag (field_number, wire_type) as a varint.
///
/// For field numbers ≤ 15 the tag fits in a single byte; larger field
/// numbers produce 2+ byte varints. Handles all valid field numbers.
#[inline]
fn encode_tag(buf: &mut Vec<u8>, field: u32, wire_type: u32) {
    encode_varint(buf, u64::from(field << 3 | wire_type));
}

/// Encode an `int64` field. Skips if `value == 0`.
///
/// Negative `i64` values encode as 10-byte varints (sign-extension),
/// matching prost's behavior for `int64` (NOT zigzag-encoded).
#[inline]
#[allow(clippy::cast_sign_loss)]
pub(crate) fn encode_int64_field(buf: &mut Vec<u8>, field: u32, value: i64) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, value as u64);
    }
}

/// Encode an `int32` field. Skips if `value == 0`.
///
/// Negative `i32` sign-extends to `i64` before varint encoding, producing
/// 10-byte varints. This matches prost's behavior for `int32` fields.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn encode_int32_field(buf: &mut Vec<u8>, field: u32, value: i32) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        // Sign-extend i32 → i64 → u64 for correct negative encoding
        encode_varint(buf, value as i64 as u64);
    }
}

/// Encode a `uint32` field. Skips if `value == 0`.
#[inline]
#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_uint32_field(buf: &mut Vec<u8>, field: u32, value: u32) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, u64::from(value));
    }
}

/// Encode a `bool` field. Skips if `value == false`.
#[inline]
#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_bool_field(buf: &mut Vec<u8>, field: u32, value: bool) {
    if value {
        encode_tag(buf, field, WIRE_VARINT);
        buf.push(1);
    }
}

/// Encode a length-delimited field (bytes, submessage, packed repeated).
///
/// Skips if `data` is empty (matches prost behavior for empty repeated fields).
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    if !data.is_empty() {
        encode_tag(buf, field, WIRE_LEN);
        encode_varint(buf, data.len() as u64);
        buf.extend_from_slice(data);
    }
}

/// Encode a length-delimited field unconditionally (even if empty).
///
/// Used for StringTable entry 0 (the required empty string).
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_bytes_field_always(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_tag(buf, field, WIRE_LEN);
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

// ---------------------------------------------------------------------------
// Packed repeated field helpers
// ---------------------------------------------------------------------------

/// Encode a packed repeated `uint32` field.
///
/// Clears `scratch`, encodes all values as varints into it, then writes
/// the packed field (tag + length + content) to `buf`. Skips if empty.
#[cfg(test)]
pub(crate) fn encode_packed_uint32(
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    field: u32,
    values: &[u32],
) {
    if values.is_empty() {
        return;
    }
    scratch.clear();
    for &v in values {
        encode_varint(scratch, u64::from(v));
    }
    encode_bytes_field(buf, field, scratch);
}

/// Encode a packed repeated `int32` field.
///
/// Negative values sign-extend to 10-byte varints (matching prost).
#[allow(clippy::cast_sign_loss)]
pub(crate) fn encode_packed_int32(
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    field: u32,
    values: &[i32],
) {
    if values.is_empty() {
        return;
    }
    scratch.clear();
    for &v in values {
        encode_varint(scratch, v as i64 as u64);
    }
    encode_bytes_field(buf, field, scratch);
}

/// Encode a packed repeated `sint64` field (zigzag + varint).
///
/// Used for DenseNodes id/lat/lon and DenseInfo timestamp/changeset.
pub(crate) fn encode_packed_sint64(
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    field: u32,
    values: &[i64],
) {
    if values.is_empty() {
        return;
    }
    scratch.clear();
    for &v in values {
        encode_varint(scratch, zigzag_encode_64(v));
    }
    encode_bytes_field(buf, field, scratch);
}

/// Encode a packed repeated `sint32` field (zigzag + varint).
///
/// Used for DenseInfo uid/user_sid.
pub(crate) fn encode_packed_sint32(
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    field: u32,
    values: &[i32],
) {
    if values.is_empty() {
        return;
    }
    scratch.clear();
    for &v in values {
        encode_varint(scratch, zigzag_encode_32(v));
    }
    encode_bytes_field(buf, field, scratch);
}

/// Encode a packed repeated `bool` field.
///
/// Used for DenseInfo visible.
pub(crate) fn encode_packed_bool(
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    field: u32,
    values: &[bool],
) {
    if values.is_empty() {
        return;
    }
    scratch.clear();
    for &v in values {
        encode_varint(scratch, u64::from(v));
    }
    encode_bytes_field(buf, field, scratch);
}

/// Encode a `sint64` field, always written even if zero.
///
/// Used for HeaderBBox required fields where zero is a valid value.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_sint64_field_always(buf: &mut Vec<u8>, field: u32, value: i64) {
    encode_tag(buf, field, WIRE_VARINT);
    encode_varint(buf, zigzag_encode_64(value));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- varint encoding --

    #[test]
    fn varint_single_byte() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf, [0x00]);

        buf.clear();
        encode_varint(&mut buf, 1);
        assert_eq!(buf, [0x01]);

        buf.clear();
        encode_varint(&mut buf, 127);
        assert_eq!(buf, [0x7f]);
    }

    #[test]
    fn varint_multi_byte() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        encode_varint(&mut buf, 300);
        assert_eq!(buf, [0xac, 0x02]);

        buf.clear();
        encode_varint(&mut buf, 16384);
        assert_eq!(buf, [0x80, 0x80, 0x01]);
    }

    #[test]
    fn varint_max() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, u64::MAX);
        assert_eq!(buf.len(), 10);
        assert_eq!(
            buf,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    // -- zigzag encoding --

    #[test]
    fn zigzag_64_known_values() {
        assert_eq!(zigzag_encode_64(0), 0);
        assert_eq!(zigzag_encode_64(-1), 1);
        assert_eq!(zigzag_encode_64(1), 2);
        assert_eq!(zigzag_encode_64(-2), 3);
        assert_eq!(zigzag_encode_64(2), 4);
        assert_eq!(zigzag_encode_64(i64::MIN), u64::MAX);
        assert_eq!(zigzag_encode_64(i64::MAX), u64::MAX - 1);
    }

    #[test]
    fn zigzag_32_known_values() {
        assert_eq!(zigzag_encode_32(0), 0);
        assert_eq!(zigzag_encode_32(-1), 1);
        assert_eq!(zigzag_encode_32(1), 2);
        assert_eq!(zigzag_encode_32(-2), 3);
    }

    /// Cross-validate encode/decode roundtrip.
    ///
    /// Uses the same formula as `zigzag_decode_64` in `src/read/wire.rs`
    /// (which is private, so we inline the formula here).
    #[test]
    fn zigzag_roundtrip() {
        #[allow(clippy::cast_possible_wrap)]
        fn decode(v: u64) -> i64 {
            let signed = (v >> 1) as i64;
            let sign = -((v & 1) as i64);
            signed ^ sign
        }

        for v in [
            0i64,
            1,
            -1,
            2,
            -2,
            100,
            -100,
            1_000_000,
            -1_000_000,
            i64::MAX,
            i64::MIN,
        ] {
            let encoded = zigzag_encode_64(v);
            assert_eq!(decode(encoded), v, "roundtrip failed for {v}");
        }
    }

    // -- field-level encoders --

    #[test]
    fn int64_field_skip_zero() {
        let mut buf = Vec::new();
        encode_int64_field(&mut buf, 1, 0);
        assert!(buf.is_empty(), "should skip zero value");
    }

    #[test]
    fn int64_field_positive() {
        let mut buf = Vec::new();
        encode_int64_field(&mut buf, 1, 5001);
        // tag: (1 << 3) | 0 = 0x08
        assert_eq!(buf[0], 0x08);
        // decode varint: 5001
        let rest = &buf[1..];
        let mut val: u64 = 0;
        for (i, &b) in rest.iter().enumerate() {
            val |= u64::from(b & 0x7f) << (7 * i);
            if b < 0x80 {
                assert_eq!(val, 5001);
                break;
            }
        }
    }

    #[test]
    fn int32_field_negative_sign_extends() {
        // Negative int32 should produce 10-byte varint (sign extension to i64)
        let mut buf = Vec::new();
        encode_int32_field(&mut buf, 1, -1);
        // tag (1 byte) + 10-byte varint for -1
        assert_eq!(buf.len(), 11);
    }

    #[test]
    fn uint32_field() {
        let mut buf = Vec::new();
        encode_uint32_field(&mut buf, 5, 42);
        // tag: (5 << 3) | 0 = 0x28, value: 42 = 0x2a
        assert_eq!(buf, [0x28, 0x2a]);
    }

    #[test]
    fn bool_field_false_skipped() {
        let mut buf = Vec::new();
        encode_bool_field(&mut buf, 6, false);
        assert!(buf.is_empty());
    }

    #[test]
    fn bool_field_true() {
        let mut buf = Vec::new();
        encode_bool_field(&mut buf, 6, true);
        // tag: (6 << 3) | 0 = 0x30, value: 1
        assert_eq!(buf, [0x30, 0x01]);
    }

    // -- bytes / submessage fields --

    #[test]
    fn bytes_field_skip_empty() {
        let mut buf = Vec::new();
        encode_bytes_field(&mut buf, 1, &[]);
        assert!(buf.is_empty(), "should skip empty data");
    }

    #[test]
    fn bytes_field_always_includes_empty() {
        let mut buf = Vec::new();
        encode_bytes_field_always(&mut buf, 1, &[]);
        // tag: (1 << 3) | 2 = 0x0a, length: 0x00
        assert_eq!(buf, [0x0a, 0x00]);
    }

    #[test]
    fn bytes_field_with_data() {
        let mut buf = Vec::new();
        encode_bytes_field(&mut buf, 1, b"hello");
        // tag: 0x0a, length: 5, then "hello"
        assert_eq!(&buf[..2], &[0x0a, 0x05]);
        assert_eq!(&buf[2..], b"hello");
    }

    // -- packed repeated fields --

    #[test]
    fn packed_uint32_values() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        encode_packed_uint32(&mut buf, &mut scratch, 2, &[1, 2, 3]);
        // tag: (2 << 3) | 2 = 0x12, length: 3, values: 0x01, 0x02, 0x03
        assert_eq!(buf, [0x12, 0x03, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn packed_uint32_empty() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        encode_packed_uint32(&mut buf, &mut scratch, 2, &[]);
        assert!(buf.is_empty(), "should skip empty packed field");
    }

    #[test]
    fn packed_int32_negative() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        encode_packed_int32(&mut buf, &mut scratch, 8, &[-1]);
        // tag: (8 << 3) | 2 = 0x42, length: 10 (negative int32 = 10-byte varint)
        assert_eq!(buf[0], 0x42);
        assert_eq!(buf[1], 0x0a); // length = 10
        assert_eq!(buf.len(), 12); // 1 tag + 1 length + 10 varint bytes
    }
}
