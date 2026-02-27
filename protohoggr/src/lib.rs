//! Zero-copy protobuf wire-format primitives.
//!
//! Provides varint encoding/decoding, zigzag encoding/decoding, a cursor-based
//! decoder, field-level encoders, and packed repeated field helpers. No external
//! dependencies — pure Rust, no std beyond default.

#![deny(clippy::unwrap_used)]

use std::fmt;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// An error that occurs when decoding protobuf wire format.
#[derive(Debug)]
pub struct WireError {
    /// Static error message describing the failure.
    pub msg: &'static str,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wire format error: {}", self.msg)
    }
}

impl std::error::Error for WireError {}

/// A type alias for `Result<T, WireError>`.
pub type WireResult<T> = Result<T, WireError>;

#[cold]
fn wire_error(msg: &'static str) -> WireError {
    WireError { msg }
}

// ---------------------------------------------------------------------------
// Wire-format constants
// ---------------------------------------------------------------------------

/// Protobuf wire type: variable-length integer (LEB128).
pub const WIRE_VARINT: u32 = 0;

/// Protobuf wire type: 64-bit fixed.
pub const WIRE_64BIT: u32 = 1;

/// Protobuf wire type: length-delimited (bytes, strings, submessages, packed repeated).
pub const WIRE_LEN: u32 = 2;

/// Protobuf wire type: 32-bit fixed.
pub const WIRE_32BIT: u32 = 5;

// ---------------------------------------------------------------------------
// Cursor — zero-copy reader over a byte slice
// ---------------------------------------------------------------------------

/// Zero-copy cursor for reading protobuf wire format from a byte slice.
#[derive(Clone, Debug)]
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Read a varint (LEB128). Fast path for single-byte values.
    #[inline]
    pub fn read_varint(&mut self) -> WireResult<u64> {
        if self.pos >= self.data.len() {
            return Err(wire_error("unexpected end of input reading varint"));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        if b < 0x80 {
            return Ok(u64::from(b));
        }
        self.read_varint_slow(u64::from(b & 0x7F))
    }

    #[cold]
    fn read_varint_slow(&mut self, mut val: u64) -> WireResult<u64> {
        let mut shift: u32 = 7;
        loop {
            if self.pos >= self.data.len() {
                return Err(wire_error("truncated varint"));
            }
            let b = self.data[self.pos];
            self.pos += 1;
            val |= u64::from(b & 0x7F) << shift;
            if b < 0x80 {
                return Ok(val);
            }
            shift += 7;
            if shift >= 64 {
                return Err(wire_error("varint too long"));
            }
        }
    }

    #[inline]
    pub fn read_varint_u32(&mut self) -> WireResult<u32> {
        #[allow(clippy::cast_possible_truncation)]
        Ok(self.read_varint()? as u32)
    }

    #[inline]
    pub fn read_varint_i64(&mut self) -> WireResult<i64> {
        #[allow(clippy::cast_possible_wrap)]
        Ok(self.read_varint()? as i64)
    }

    #[inline]
    pub fn read_sint64(&mut self) -> WireResult<i64> {
        let v = self.read_varint()?;
        Ok(zigzag_decode_64(v))
    }

    #[inline]
    pub fn read_sint32(&mut self) -> WireResult<i32> {
        let v = self.read_varint()?;
        Ok(zigzag_decode_32(v))
    }

    /// Read a (field_number, wire_type) tag. Returns None at EOF.
    #[inline]
    pub fn read_tag(&mut self) -> WireResult<Option<(u32, u32)>> {
        if self.is_empty() {
            return Ok(None);
        }
        let v = self.read_varint_u32()?;
        Ok(Some((v >> 3, v & 0x7)))
    }

    /// Read a length-delimited field, returning the sub-slice.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn read_len_delimited(&mut self) -> WireResult<&'a [u8]> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.data.len() {
            return Err(wire_error("length-delimited field extends past end"));
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    /// Skip a varint by scanning for the terminating byte (MSB=0)
    /// without decoding the value.
    #[inline]
    pub fn skip_varint(&mut self) -> WireResult<()> {
        loop {
            if self.pos >= self.data.len() {
                return Err(wire_error("unexpected end of input skipping varint"));
            }
            let b = self.data[self.pos];
            self.pos += 1;
            if b < 0x80 {
                return Ok(());
            }
        }
    }

    /// Skip an unknown field given its wire type.
    #[inline]
    pub fn skip_field(&mut self, wire_type: u32) -> WireResult<()> {
        match wire_type {
            WIRE_VARINT => {
                self.skip_varint()?;
            }
            WIRE_64BIT => {
                if self.pos + 8 > self.data.len() {
                    return Err(wire_error("unexpected end of input skipping 64-bit"));
                }
                self.pos += 8;
            }
            WIRE_LEN => {
                self.read_len_delimited()?;
            }
            WIRE_32BIT => {
                if self.pos + 4 > self.data.len() {
                    return Err(wire_error("unexpected end of input skipping 32-bit"));
                }
                self.pos += 4;
            }
            _ => return Err(wire_error("unknown wire type")),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Zigzag decode
// ---------------------------------------------------------------------------

#[inline]
pub fn zigzag_decode_64(v: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let signed = (v >> 1) as i64;
    #[allow(clippy::cast_possible_wrap)]
    let sign = -((v & 1) as i64);
    signed ^ sign
}

#[inline]
pub fn zigzag_decode_32(v: u64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let v32 = v as u32;
    #[allow(clippy::cast_possible_wrap)]
    let signed = (v32 >> 1) as i32;
    #[allow(clippy::cast_possible_wrap)]
    let sign = -((v32 & 1) as i32);
    signed ^ sign
}

// ---------------------------------------------------------------------------
// Packed field iterators
// ---------------------------------------------------------------------------

/// Base packed varint iterator. Yields raw u64 values.
#[derive(Clone)]
pub struct PackedIter<'a> {
    cursor: Cursor<'a>,
}

impl<'a> PackedIter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(data),
        }
    }

    #[inline]
    pub fn empty() -> Self {
        Self {
            cursor: Cursor::new(&[]),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cursor.is_empty()
    }

    #[inline]
    pub fn remaining_bytes(&self) -> usize {
        self.cursor.remaining()
    }
}

impl Iterator for PackedIter<'_> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.cursor.is_empty() {
            return None;
        }
        // Packed field data is trusted to contain complete varints since it was
        // length-delimited by the outer message. If somehow truncated, we stop
        // iteration rather than propagating errors through the Iterator trait.
        self.cursor.read_varint().ok()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.cursor.remaining();
        // Each varint is 1-10 bytes, so:
        // min elements = remaining / 10 (all 10-byte varints)
        // max elements = remaining (all 1-byte varints)
        (remaining / 10, Some(remaining))
    }
}

// --- Typed wrappers ---

#[derive(Clone)]
pub struct PackedSint64Iter<'a>(PackedIter<'a>);

impl<'a> PackedSint64Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }

    #[inline]
    pub fn empty() -> Self {
        Self(PackedIter::empty())
    }
}

impl Iterator for PackedSint64Iter<'_> {
    type Item = i64;

    #[inline]
    fn next(&mut self) -> Option<i64> {
        self.0.next().map(zigzag_decode_64)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct PackedSint32Iter<'a>(PackedIter<'a>);

impl<'a> PackedSint32Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }
}

impl Iterator for PackedSint32Iter<'_> {
    type Item = i32;

    #[inline]
    fn next(&mut self) -> Option<i32> {
        self.0.next().map(zigzag_decode_32)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct PackedInt64Iter<'a>(PackedIter<'a>);

impl<'a> PackedInt64Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }
}

impl Iterator for PackedInt64Iter<'_> {
    type Item = i64;

    #[inline]
    fn next(&mut self) -> Option<i64> {
        #[allow(clippy::cast_possible_wrap)]
        self.0.next().map(|v| v as i64)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct PackedInt32Iter<'a>(PackedIter<'a>);

impl<'a> PackedInt32Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }

    #[inline]
    pub fn empty() -> Self {
        Self(PackedIter::empty())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub fn remaining_bytes(&self) -> usize {
        self.0.remaining_bytes()
    }
}

impl Iterator for PackedInt32Iter<'_> {
    type Item = i32;

    #[inline]
    fn next(&mut self) -> Option<i32> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        self.0.next().map(|v| v as i32)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct PackedUint32Iter<'a>(PackedIter<'a>);

impl<'a> PackedUint32Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }

    #[inline]
    pub fn empty() -> Self {
        Self(PackedIter::empty())
    }
}

impl Iterator for PackedUint32Iter<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        #[allow(clippy::cast_possible_truncation)]
        self.0.next().map(|v| v as u32)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone)]
pub struct PackedBoolIter<'a>(PackedIter<'a>);

impl<'a> PackedBoolIter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }
}

impl Iterator for PackedBoolIter<'_> {
    type Item = bool;

    #[inline]
    fn next(&mut self) -> Option<bool> {
        self.0.next().map(|v| v != 0)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

// ---------------------------------------------------------------------------
// Encoding: varint / zigzag
// ---------------------------------------------------------------------------

/// Encode a `u64` as a variable-length integer (LEB128) into `buf`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Zigzag-encode a signed 64-bit integer for `sint64` fields.
///
/// Maps: 0 → 0, -1 → 1, 1 → 2, -2 → 3, 2 → 4, …
#[inline]
#[allow(clippy::cast_sign_loss)]
pub fn zigzag_encode_64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Zigzag-encode a signed 32-bit integer for `sint32` fields.
#[inline]
#[allow(clippy::cast_sign_loss)]
pub fn zigzag_encode_32(v: i32) -> u64 {
    ((v << 1) ^ (v >> 31)) as u64
}

// ---------------------------------------------------------------------------
// Field-level encoders
// ---------------------------------------------------------------------------

/// Encode a field tag (field_number, wire_type) as a varint.
#[inline]
pub fn encode_tag(buf: &mut Vec<u8>, field: u32, wire_type: u32) {
    encode_varint(buf, u64::from(field << 3 | wire_type));
}

/// Encode a varint field. Skips if `value == 0`.
#[inline]
pub fn encode_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, value);
    }
}

/// Encode an `int64` field. Skips if `value == 0`.
///
/// Negative `i64` values encode as 10-byte varints (sign-extension).
#[inline]
#[allow(clippy::cast_sign_loss)]
pub fn encode_int64_field(buf: &mut Vec<u8>, field: u32, value: i64) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, value as u64);
    }
}

/// Encode an `int32` field. Skips if `value == 0`.
///
/// Negative `i32` sign-extends to `i64` before varint encoding,
/// producing 10-byte varints.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn encode_int32_field(buf: &mut Vec<u8>, field: u32, value: i32) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, value as i64 as u64);
    }
}

/// Encode a `uint32` field. Skips if `value == 0`.
#[inline]
pub fn encode_uint32_field(buf: &mut Vec<u8>, field: u32, value: u32) {
    if value != 0 {
        encode_tag(buf, field, WIRE_VARINT);
        encode_varint(buf, u64::from(value));
    }
}

/// Encode a `bool` field. Skips if `value == false`.
#[inline]
pub fn encode_bool_field(buf: &mut Vec<u8>, field: u32, value: bool) {
    if value {
        encode_tag(buf, field, WIRE_VARINT);
        buf.push(1);
    }
}

/// Encode a length-delimited field (bytes, submessage, packed repeated).
///
/// Skips if `data` is empty.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    if !data.is_empty() {
        encode_tag(buf, field, WIRE_LEN);
        encode_varint(buf, data.len() as u64);
        buf.extend_from_slice(data);
    }
}

/// Encode a length-delimited field unconditionally (even if empty).
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn encode_bytes_field_always(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_tag(buf, field, WIRE_LEN);
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Encode a `sint64` field, always written even if zero.
#[inline]
pub fn encode_sint64_field_always(buf: &mut Vec<u8>, field: u32, value: i64) {
    encode_tag(buf, field, WIRE_VARINT);
    encode_varint(buf, zigzag_encode_64(value));
}

// ---------------------------------------------------------------------------
// Packed repeated field helpers
// ---------------------------------------------------------------------------

/// Encode a packed repeated `uint32` field.
pub fn encode_packed_uint32(
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
#[allow(clippy::cast_sign_loss)]
pub fn encode_packed_int32(
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
pub fn encode_packed_sint64(
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
pub fn encode_packed_sint32(
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
pub fn encode_packed_bool(
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // -- decode: varint --

    #[test]
    fn decode_varint_single_byte() {
        let mut c = Cursor::new(&[0x00]);
        assert_eq!(c.read_varint().unwrap(), 0);

        let mut c = Cursor::new(&[0x01]);
        assert_eq!(c.read_varint().unwrap(), 1);

        let mut c = Cursor::new(&[0x7F]);
        assert_eq!(c.read_varint().unwrap(), 127);
    }

    #[test]
    fn decode_varint_multi_byte() {
        let mut c = Cursor::new(&[0xAC, 0x02]);
        assert_eq!(c.read_varint().unwrap(), 300);

        let mut c = Cursor::new(&[0x96, 0x01]);
        assert_eq!(c.read_varint().unwrap(), 150);
    }

    #[test]
    fn decode_varint_truncated() {
        let mut c = Cursor::new(&[0x80]);
        assert!(c.read_varint().is_err());
    }

    #[test]
    fn decode_varint_empty() {
        let mut c = Cursor::new(&[]);
        assert!(c.read_varint().is_err());
    }

    // -- decode: zigzag --

    #[test]
    fn zigzag_decode_roundtrip() {
        assert_eq!(zigzag_decode_64(0), 0);
        assert_eq!(zigzag_decode_64(1), -1);
        assert_eq!(zigzag_decode_64(2), 1);
        assert_eq!(zigzag_decode_64(3), -2);
        assert_eq!(zigzag_decode_64(4294967294), 2147483647);
        assert_eq!(zigzag_decode_64(4294967295), -2147483648);

        assert_eq!(zigzag_decode_32(0), 0);
        assert_eq!(zigzag_decode_32(1), -1);
        assert_eq!(zigzag_decode_32(2), 1);
        assert_eq!(zigzag_decode_32(3), -2);
    }

    // -- decode: packed iterators --

    #[test]
    fn packed_sint64_iter() {
        let data = [0x02, 0x01, 0x00];
        let vals: Vec<i64> = PackedSint64Iter::new(&data).collect();
        assert_eq!(vals, vec![1, -1, 0]);
    }

    #[test]
    fn packed_uint32_iter() {
        let data = [0x00, 0x01, 0x7F, 0xAC, 0x02];
        let vals: Vec<u32> = PackedUint32Iter::new(&data).collect();
        assert_eq!(vals, vec![0, 1, 127, 300]);
    }

    #[test]
    fn packed_bool_iter() {
        let data = [0x01, 0x00, 0x01];
        let vals: Vec<bool> = PackedBoolIter::new(&data).collect();
        assert_eq!(vals, vec![true, false, true]);
    }

    // -- decode: tag + len_delimited + skip --

    #[test]
    fn read_tag_and_len_delimited() {
        let data = [0x0A, 0x03, 0x41, 0x42, 0x43];
        let mut c = Cursor::new(&data);
        let (field, wt) = c.read_tag().unwrap().unwrap();
        assert_eq!(field, 1);
        assert_eq!(wt, WIRE_LEN);
        let bytes = c.read_len_delimited().unwrap();
        assert_eq!(bytes, b"ABC");
        assert!(c.is_empty());
    }

    #[test]
    fn skip_field_varint() {
        let data = [0x08, 0xAC, 0x02];
        let mut c = Cursor::new(&data);
        let (_, wt) = c.read_tag().unwrap().unwrap();
        c.skip_field(wt).unwrap();
        assert!(c.is_empty());
    }

    // -- encode: varint --

    #[test]
    fn encode_varint_single_byte() {
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
    fn encode_varint_multi_byte() {
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
    fn encode_varint_max() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, u64::MAX);
        assert_eq!(buf.len(), 10);
        assert_eq!(
            buf,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    // -- encode: zigzag --

    #[test]
    fn zigzag_encode_64_known_values() {
        assert_eq!(zigzag_encode_64(0), 0);
        assert_eq!(zigzag_encode_64(-1), 1);
        assert_eq!(zigzag_encode_64(1), 2);
        assert_eq!(zigzag_encode_64(-2), 3);
        assert_eq!(zigzag_encode_64(2), 4);
        assert_eq!(zigzag_encode_64(i64::MIN), u64::MAX);
        assert_eq!(zigzag_encode_64(i64::MAX), u64::MAX - 1);
    }

    #[test]
    fn zigzag_encode_32_known_values() {
        assert_eq!(zigzag_encode_32(0), 0);
        assert_eq!(zigzag_encode_32(-1), 1);
        assert_eq!(zigzag_encode_32(1), 2);
        assert_eq!(zigzag_encode_32(-2), 3);
    }

    #[test]
    fn zigzag_encode_decode_roundtrip() {
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
            assert_eq!(zigzag_decode_64(encoded), v, "roundtrip failed for {v}");
        }
    }

    // -- encode: field-level --

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
        assert_eq!(buf[0], 0x08);
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
        let mut buf = Vec::new();
        encode_int32_field(&mut buf, 1, -1);
        assert_eq!(buf.len(), 11);
    }

    #[test]
    fn uint32_field() {
        let mut buf = Vec::new();
        encode_uint32_field(&mut buf, 5, 42);
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
        assert_eq!(buf, [0x30, 0x01]);
    }

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
        assert_eq!(buf, [0x0a, 0x00]);
    }

    #[test]
    fn bytes_field_with_data() {
        let mut buf = Vec::new();
        encode_bytes_field(&mut buf, 1, b"hello");
        assert_eq!(&buf[..2], &[0x0a, 0x05]);
        assert_eq!(&buf[2..], b"hello");
    }

    // -- encode: packed repeated --

    #[test]
    fn packed_uint32_values() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        encode_packed_uint32(&mut buf, &mut scratch, 2, &[1, 2, 3]);
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
        assert_eq!(buf[0], 0x42);
        assert_eq!(buf[1], 0x0a);
        assert_eq!(buf.len(), 12);
    }
}
