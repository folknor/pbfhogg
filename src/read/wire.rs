//! Zero-copy protobuf wire-format decoder for OSM PrimitiveBlock messages.
//!
//! Replaces `protobuf` crate's generated parser for the hot read path.
//! All types borrow from the decompressed buffer — no Vec allocations for
//! packed repeated fields. The write path and blob envelope parsing stay
//! on the `protobuf` crate unchanged.


use crate::error::{new_wire_error, Result};

// ---------------------------------------------------------------------------
// Wire-format constants
// ---------------------------------------------------------------------------

const WIRE_VARINT: u32 = 0;
const WIRE_64BIT: u32 = 1;
const WIRE_LEN: u32 = 2;
const WIRE_32BIT: u32 = 5;

// ---------------------------------------------------------------------------
// Cursor — zero-copy reader over a byte slice
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct Cursor<'a> {
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
    pub fn read_varint(&mut self) -> Result<u64> {
        if self.pos >= self.data.len() {
            return Err(new_wire_error("unexpected end of input reading varint"));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        if b < 0x80 {
            return Ok(u64::from(b));
        }
        self.read_varint_slow(u64::from(b & 0x7F))
    }

    #[cold]
    fn read_varint_slow(&mut self, mut val: u64) -> Result<u64> {
        let mut shift: u32 = 7;
        loop {
            if self.pos >= self.data.len() {
                return Err(new_wire_error("truncated varint"));
            }
            let b = self.data[self.pos];
            self.pos += 1;
            val |= u64::from(b & 0x7F) << shift;
            if b < 0x80 {
                return Ok(val);
            }
            shift += 7;
            if shift >= 64 {
                return Err(new_wire_error("varint too long"));
            }
        }
    }

    #[inline]
    pub fn read_varint_u32(&mut self) -> Result<u32> {
        #[allow(clippy::cast_possible_truncation)]
        Ok(self.read_varint()? as u32)
    }

    #[inline]
    pub fn read_varint_i64(&mut self) -> Result<i64> {
        #[allow(clippy::cast_possible_wrap)]
        Ok(self.read_varint()? as i64)
    }

    #[inline]
    pub fn read_sint64(&mut self) -> Result<i64> {
        let v = self.read_varint()?;
        Ok(zigzag_decode_64(v))
    }

    #[inline]
    #[allow(dead_code)] // API completeness — symmetric with read_sint64
    pub fn read_sint32(&mut self) -> Result<i32> {
        let v = self.read_varint()?;
        Ok(zigzag_decode_32(v))
    }

    /// Read a (field_number, wire_type) tag. Returns None at EOF.
    #[inline]
    pub fn read_tag(&mut self) -> Result<Option<(u32, u32)>> {
        if self.is_empty() {
            return Ok(None);
        }
        let v = self.read_varint_u32()?;
        Ok(Some((v >> 3, v & 0x7)))
    }

    /// Read a length-delimited field, returning the sub-slice.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn read_len_delimited(&mut self) -> Result<&'a [u8]> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.data.len() {
            return Err(new_wire_error("length-delimited field extends past end"));
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    /// Skip a varint by scanning for the terminating byte (MSB=0)
    /// without decoding the value.
    #[inline]
    pub fn skip_varint(&mut self) -> Result<()> {
        loop {
            if self.pos >= self.data.len() {
                return Err(new_wire_error("unexpected end of input skipping varint"));
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
    pub fn skip_field(&mut self, wire_type: u32) -> Result<()> {
        match wire_type {
            WIRE_VARINT => {
                self.skip_varint()?;
            }
            WIRE_64BIT => {
                if self.pos + 8 > self.data.len() {
                    return Err(new_wire_error("unexpected end of input skipping 64-bit"));
                }
                self.pos += 8;
            }
            WIRE_LEN => {
                self.read_len_delimited()?;
            }
            WIRE_32BIT => {
                if self.pos + 4 > self.data.len() {
                    return Err(new_wire_error("unexpected end of input skipping 32-bit"));
                }
                self.pos += 4;
            }
            _ => return Err(new_wire_error("unknown wire type")),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Zigzag decode
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn zigzag_decode_64(v: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let signed = (v >> 1) as i64;
    #[allow(clippy::cast_possible_wrap)]
    let sign = -((v & 1) as i64);
    signed ^ sign
}

#[inline]
fn zigzag_decode_32(v: u64) -> i32 {
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
pub(crate) struct PackedIter<'a> {
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
    #[allow(dead_code)] // API completeness
    pub fn is_empty(&self) -> bool {
        self.cursor.is_empty()
    }

    #[inline]
    #[allow(dead_code)] // API completeness
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
pub(crate) struct PackedSint64Iter<'a>(PackedIter<'a>);

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
pub(crate) struct PackedSint32Iter<'a>(PackedIter<'a>);

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
pub(crate) struct PackedInt64Iter<'a>(PackedIter<'a>);

impl<'a> PackedInt64Iter<'a> {
    #[inline]
    #[allow(dead_code)] // API completeness — symmetric with PackedSint64Iter::new
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
pub(crate) struct PackedInt32Iter<'a>(PackedIter<'a>);

impl<'a> PackedInt32Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }

    #[inline]
    #[allow(dead_code)] // API completeness
    pub fn empty() -> Self {
        Self(PackedIter::empty())
    }

    #[inline]
    #[allow(dead_code)] // API completeness
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    #[allow(dead_code)] // API completeness
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
pub(crate) struct PackedUint32Iter<'a>(PackedIter<'a>);

impl<'a> PackedUint32Iter<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self(PackedIter::new(data))
    }

    #[inline]
    #[allow(dead_code)] // API completeness
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
pub(crate) struct PackedBoolIter<'a>(PackedIter<'a>);

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
// WireStringTable — zero-copy indexed string table
// ---------------------------------------------------------------------------

/// String table storing buffer-relative offsets instead of `&[u8]` slices.
///
/// Offsets are relative to the decompressed buffer, not the StringTable message,
/// because `PrimitiveBlock` in `block.rs` transmutes `WireBlock<'a>` to
/// `WireBlock<'static>` for self-referential ownership — storing slices directly
/// would create dangling references after the lifetime erasure.
#[derive(Clone, Debug)]
pub(crate) struct WireStringTable<'a> {
    buffer: &'a [u8],
    entries: Box<[(u32, u32)]>, // (offset, length) relative to buffer start
}

impl<'a> WireStringTable<'a> {
    #[hotpath::measure]
    fn parse(data: &'a [u8], buffer: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        // Count not available before scanning — Vec::new() with amortized push is fine.
        let mut entries = Vec::new();
        while let Some((field, wire_type)) = cursor.read_tag()? {
            if field == 1 && wire_type == WIRE_LEN {
                let bytes = cursor.read_len_delimited()?;
                // Store buffer-relative offset (not a slice) — see WireStringTable doc.
                let offset = bytes.as_ptr() as usize - buffer.as_ptr() as usize;
                #[allow(clippy::cast_possible_truncation)]
                entries.push((offset as u32, bytes.len() as u32));
            } else {
                cursor.skip_field(wire_type)?;
            }
        }
        Ok(Self {
            buffer,
            entries: entries.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&'a [u8]> {
        self.entries.get(index).map(|&(off, len)| {
            &self.buffer[off as usize..off as usize + len as usize]
        })
    }
}

// ---------------------------------------------------------------------------
// WireBlock — parsed PrimitiveBlock
// ---------------------------------------------------------------------------

/// Parsed PrimitiveBlock — the root message of each data blob.
///
/// All fields borrow from `buffer` (the decompressed blob bytes). Group data
/// and string table entries are stored as buffer-relative `(offset, length)`
/// pairs rather than slices, because `PrimitiveBlock` in `block.rs` transmutes
/// `WireBlock<'a>` to `WireBlock<'static>` for self-referential ownership.
/// The `buffer` reference is reconstituted at access time via `group()` and
/// `WireStringTable::get()`.
#[derive(Debug)]
pub(crate) struct WireBlock<'a> {
    buffer: &'a [u8],
    pub stringtable: WireStringTable<'a>,
    pub group_ranges: Box<[(u32, u32)]>, // (offset, length) relative to buffer start
    pub granularity: i32,
    pub lat_offset: i64,
    pub lon_offset: i64,
    pub date_granularity: i32,
}

impl<'a> WireBlock<'a> {
    #[hotpath::measure]
    pub fn parse(buffer: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(buffer);
        let mut stringtable_data: Option<&'a [u8]> = None;
        // Count not available before scanning — Vec::new() with amortized push is fine.
        let mut group_ranges = Vec::new();
        let mut granularity: i32 = 100;
        let mut lat_offset: i64 = 0;
        let mut lon_offset: i64 = 0;
        let mut date_granularity: i32 = 1000;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                // StringTable stringtable = 1
                (1, WIRE_LEN) => {
                    stringtable_data = Some(cursor.read_len_delimited()?);
                }
                // repeated PrimitiveGroup primitivegroup = 2
                (2, WIRE_LEN) => {
                    let data = cursor.read_len_delimited()?;
                    // Store buffer-relative offset (not a slice) — see WireBlock doc.
                    let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    group_ranges.push((offset as u32, data.len() as u32));
                }
                // int32 granularity = 17
                (17, WIRE_VARINT) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    {
                        granularity = cursor.read_varint()? as i32;
                    }
                }
                // int64 lat_offset = 19
                (19, WIRE_VARINT) => {
                    lat_offset = cursor.read_varint_i64()?;
                }
                // int64 lon_offset = 20
                (20, WIRE_VARINT) => {
                    lon_offset = cursor.read_varint_i64()?;
                }
                // int32 date_granularity = 18
                (18, WIRE_VARINT) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    {
                        date_granularity = cursor.read_varint()? as i32;
                    }
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        let st_data = stringtable_data.unwrap_or(&[]);
        let stringtable = WireStringTable::parse(st_data, buffer)?;

        Ok(Self {
            buffer,
            stringtable,
            group_ranges: group_ranges.into_boxed_slice(),
            granularity,
            lat_offset,
            lon_offset,
            date_granularity,
        })
    }

    #[inline]
    pub fn group(&self, index: usize) -> &'a [u8] {
        let (off, len) = self.group_ranges[index];
        &self.buffer[off as usize..off as usize + len as usize]
    }
}

// ---------------------------------------------------------------------------
// WireGroup — lazy PrimitiveGroup scanner
// ---------------------------------------------------------------------------

/// Lazy PrimitiveGroup scanner — yields raw sub-message bytes by element type.
///
/// Does not parse eagerly; each accessor (`nodes()`, `dense()`, `ways()`,
/// `relations()`) scans the group's wire format for the target field number.
/// A PrimitiveGroup contains exactly one element type per the OSM PBF spec.
pub(crate) struct WireGroup<'a> {
    data: &'a [u8],
}

impl<'a> WireGroup<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn nodes(&self) -> WireMessageIter<'a> {
        WireMessageIter::new(self.data, 1)
    }

    pub fn dense(&self) -> Result<Option<&'a [u8]>> {
        let mut cursor = Cursor::new(self.data);
        while let Some((field, wire_type)) = cursor.read_tag()? {
            if field == 2 && wire_type == WIRE_LEN {
                return Ok(Some(cursor.read_len_delimited()?));
            }
            cursor.skip_field(wire_type)?;
        }
        Ok(None)
    }

    pub fn ways(&self) -> WireMessageIter<'a> {
        WireMessageIter::new(self.data, 3)
    }

    pub fn relations(&self) -> WireMessageIter<'a> {
        WireMessageIter::new(self.data, 4)
    }
}

// ---------------------------------------------------------------------------
// WireMessageIter — yields sub-message byte slices for a given field number
// ---------------------------------------------------------------------------

/// Iterator over length-delimited sub-messages matching a specific field number.
///
/// Used by `WireGroup` to yield raw bytes for each Node, Way, or Relation
/// message within a PrimitiveGroup. Skips non-matching fields.
pub(crate) struct WireMessageIter<'a> {
    cursor: Cursor<'a>,
    target_field: u32,
}

impl<'a> WireMessageIter<'a> {
    fn new(data: &'a [u8], target_field: u32) -> Self {
        Self {
            cursor: Cursor::new(data),
            target_field,
        }
    }

    pub fn empty() -> Self {
        Self {
            cursor: Cursor::new(&[]),
            target_field: 0,
        }
    }
}

impl<'a> Iterator for WireMessageIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let (field, wire_type) = self.cursor.read_tag().ok()??;
            if field == self.target_field && wire_type == WIRE_LEN {
                return self.cursor.read_len_delimited().ok();
            }
            if self.cursor.skip_field(wire_type).is_err() {
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-element wire types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct WireNode<'a> {
    pub id: i64,
    pub lat: i64,
    pub lon: i64,
    pub keys_data: &'a [u8],
    pub vals_data: &'a [u8],
    pub info_data: Option<&'a [u8]>,
}

impl<'a> WireNode<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut id: i64 = 0;
        let mut lat: i64 = 0;
        let mut lon: i64 = 0;
        let mut keys_data: &[u8] = &[];
        let mut vals_data: &[u8] = &[];
        let mut info_data: Option<&[u8]> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_VARINT) => id = cursor.read_sint64()?,
                (2, WIRE_LEN) => keys_data = cursor.read_len_delimited()?,
                (3, WIRE_LEN) => vals_data = cursor.read_len_delimited()?,
                (4, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_VARINT) => lat = cursor.read_sint64()?,
                (9, WIRE_VARINT) => lon = cursor.read_sint64()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(Self {
            id,
            lat,
            lon,
            keys_data,
            vals_data,
            info_data,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WireWay<'a> {
    pub id: i64,
    pub keys_data: &'a [u8],
    pub vals_data: &'a [u8],
    pub refs_data: &'a [u8],
    pub lat_data: &'a [u8],
    pub lon_data: &'a [u8],
    pub info_data: Option<&'a [u8]>,
}

impl<'a> WireWay<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut id: i64 = 0;
        let mut keys_data: &[u8] = &[];
        let mut vals_data: &[u8] = &[];
        let mut refs_data: &[u8] = &[];
        let mut lat_data: &[u8] = &[];
        let mut lon_data: &[u8] = &[];
        let mut info_data: Option<&[u8]> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_VARINT) => id = cursor.read_varint_i64()?,
                (2, WIRE_LEN) => keys_data = cursor.read_len_delimited()?,
                (3, WIRE_LEN) => vals_data = cursor.read_len_delimited()?,
                (4, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN) => refs_data = cursor.read_len_delimited()?,
                (9, WIRE_LEN) => lat_data = cursor.read_len_delimited()?,
                (10, WIRE_LEN) => lon_data = cursor.read_len_delimited()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(Self {
            id,
            keys_data,
            vals_data,
            refs_data,
            lat_data,
            lon_data,
            info_data,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WireRelation<'a> {
    pub id: i64,
    pub keys_data: &'a [u8],
    pub vals_data: &'a [u8],
    pub roles_sid_data: &'a [u8],
    pub memids_data: &'a [u8],
    pub types_data: &'a [u8],
    pub info_data: Option<&'a [u8]>,
}

impl<'a> WireRelation<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut id: i64 = 0;
        let mut keys_data: &[u8] = &[];
        let mut vals_data: &[u8] = &[];
        let mut roles_sid_data: &[u8] = &[];
        let mut memids_data: &[u8] = &[];
        let mut types_data: &[u8] = &[];
        let mut info_data: Option<&[u8]> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_VARINT) => id = cursor.read_varint_i64()?,
                (2, WIRE_LEN) => keys_data = cursor.read_len_delimited()?,
                (3, WIRE_LEN) => vals_data = cursor.read_len_delimited()?,
                (4, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN) => roles_sid_data = cursor.read_len_delimited()?,
                (9, WIRE_LEN) => memids_data = cursor.read_len_delimited()?,
                (10, WIRE_LEN) => types_data = cursor.read_len_delimited()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(Self {
            id,
            keys_data,
            vals_data,
            roles_sid_data,
            memids_data,
            types_data,
            info_data,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WireDenseNodes<'a> {
    pub id_data: &'a [u8],
    pub lat_data: &'a [u8],
    pub lon_data: &'a [u8],
    pub keys_vals_data: &'a [u8],
    pub info_data: Option<&'a [u8]>,
}

impl<'a> WireDenseNodes<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut id_data: &[u8] = &[];
        let mut lat_data: &[u8] = &[];
        let mut lon_data: &[u8] = &[];
        let mut keys_vals_data: &[u8] = &[];
        let mut info_data: Option<&[u8]> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => id_data = cursor.read_len_delimited()?,
                (5, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN) => lat_data = cursor.read_len_delimited()?,
                (9, WIRE_LEN) => lon_data = cursor.read_len_delimited()?,
                (10, WIRE_LEN) => keys_vals_data = cursor.read_len_delimited()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(Self {
            id_data,
            lat_data,
            lon_data,
            keys_vals_data,
            info_data,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WireDenseInfo<'a> {
    pub version_data: &'a [u8],
    pub timestamp_data: &'a [u8],
    pub changeset_data: &'a [u8],
    pub uid_data: &'a [u8],
    pub user_sid_data: &'a [u8],
    pub visible_data: &'a [u8],
}

impl<'a> WireDenseInfo<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut version_data: &[u8] = &[];
        let mut timestamp_data: &[u8] = &[];
        let mut changeset_data: &[u8] = &[];
        let mut uid_data: &[u8] = &[];
        let mut user_sid_data: &[u8] = &[];
        let mut visible_data: &[u8] = &[];

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => version_data = cursor.read_len_delimited()?,
                (2, WIRE_LEN) => timestamp_data = cursor.read_len_delimited()?,
                (3, WIRE_LEN) => changeset_data = cursor.read_len_delimited()?,
                (4, WIRE_LEN) => uid_data = cursor.read_len_delimited()?,
                (5, WIRE_LEN) => user_sid_data = cursor.read_len_delimited()?,
                (6, WIRE_LEN) => visible_data = cursor.read_len_delimited()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(Self {
            version_data,
            timestamp_data,
            changeset_data,
            uid_data,
            user_sid_data,
            visible_data,
        })
    }
}

/// Parsed Info sub-message — all scalars, no byte references needed.
#[derive(Clone, Debug, Default)]
pub(crate) struct WireInfo {
    pub version: Option<i32>,
    pub timestamp: Option<i64>,
    pub changeset: Option<i64>,
    pub uid: Option<i32>,
    pub user_sid: Option<i32>,
    pub visible: Option<bool>,
}

impl WireInfo {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut info = Self::default();

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                (1, WIRE_VARINT) => info.version = Some(cursor.read_varint()? as i32),
                (2, WIRE_VARINT) => info.timestamp = Some(cursor.read_varint_i64()?),
                (3, WIRE_VARINT) => info.changeset = Some(cursor.read_varint_i64()?),
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                (4, WIRE_VARINT) => info.uid = Some(cursor.read_varint()? as i32),
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                (5, WIRE_VARINT) => info.user_sid = Some(cursor.read_varint()? as i32),
                (6, WIRE_VARINT) => info.visible = Some(cursor.read_varint()? != 0),
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(info)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn varint_single_byte() {
        let mut c = Cursor::new(&[0x00]);
        assert_eq!(c.read_varint().unwrap(), 0);

        let mut c = Cursor::new(&[0x01]);
        assert_eq!(c.read_varint().unwrap(), 1);

        let mut c = Cursor::new(&[0x7F]);
        assert_eq!(c.read_varint().unwrap(), 127);
    }

    #[test]
    fn varint_multi_byte() {
        // 300 = 0xAC 0x02
        let mut c = Cursor::new(&[0xAC, 0x02]);
        assert_eq!(c.read_varint().unwrap(), 300);

        // 150 = 0x96 0x01
        let mut c = Cursor::new(&[0x96, 0x01]);
        assert_eq!(c.read_varint().unwrap(), 150);
    }

    #[test]
    fn varint_truncated() {
        let mut c = Cursor::new(&[0x80]); // continuation bit set, no more bytes
        assert!(c.read_varint().is_err());
    }

    #[test]
    fn varint_empty() {
        let mut c = Cursor::new(&[]);
        assert!(c.read_varint().is_err());
    }

    #[test]
    fn zigzag_roundtrip() {
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

    #[test]
    fn packed_sint64_iter() {
        // zigzag(1)=2, zigzag(-1)=1, zigzag(0)=0
        let data = [0x02, 0x01, 0x00];
        let vals: Vec<i64> = PackedSint64Iter::new(&data).collect();
        assert_eq!(vals, vec![1, -1, 0]);
    }

    #[test]
    fn packed_uint32_iter() {
        let data = [0x00, 0x01, 0x7F, 0xAC, 0x02]; // 0, 1, 127, 300
        let vals: Vec<u32> = PackedUint32Iter::new(&data).collect();
        assert_eq!(vals, vec![0, 1, 127, 300]);
    }

    #[test]
    fn packed_bool_iter() {
        let data = [0x01, 0x00, 0x01];
        let vals: Vec<bool> = PackedBoolIter::new(&data).collect();
        assert_eq!(vals, vec![true, false, true]);
    }

    #[test]
    fn read_tag_and_len_delimited() {
        // field=1, wire_type=2 (LEN), length=3, data=[0x41, 0x42, 0x43]
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
        // varint field: tag(field=1, varint) + value 300
        let data = [0x08, 0xAC, 0x02];
        let mut c = Cursor::new(&data);
        let (_, wt) = c.read_tag().unwrap().unwrap();
        c.skip_field(wt).unwrap();
        assert!(c.is_empty());
    }

    #[test]
    fn wire_info_parse() {
        // version=5 (field 1, varint 5)
        // uid=42 (field 4, varint 42)
        // visible=true (field 6, varint 1)
        let data = [
            0x08, 0x05, // field 1, varint, value=5
            0x20, 0x2A, // field 4, varint, value=42
            0x30, 0x01, // field 6, varint, value=1
        ];
        let info = WireInfo::parse(&data).unwrap();
        assert_eq!(info.version, Some(5));
        assert_eq!(info.uid, Some(42));
        assert_eq!(info.visible, Some(true));
        assert_eq!(info.timestamp, None);
        assert_eq!(info.changeset, None);
        assert_eq!(info.user_sid, None);
    }
}
