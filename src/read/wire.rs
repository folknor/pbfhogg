//! OSM PrimitiveBlock wire-format message parsers.
//!
//! Uses [`protohoggr`] for the generic protobuf primitives (Cursor, packed
//! iterators, wire constants). This module adds the OSM-specific message
//! parsers: PrimitiveBlock, PrimitiveGroup, Node, Way, Relation, DenseNodes,
//! DenseInfo, Info, and StringTable.

pub(crate) use protohoggr::{
    Cursor, PackedBoolIter, PackedInt32Iter, PackedSint32Iter, PackedSint64Iter,
    PackedUint32Iter, WIRE_LEN, WIRE_VARINT,
};
use crate::error::Result;

// ---------------------------------------------------------------------------
// WireStringTable — zero-copy indexed string table
// ---------------------------------------------------------------------------

/// String table storing buffer-relative offsets instead of `&[u8]` slices.
///
/// Offsets are relative to the decompressed buffer, not the StringTable message,
/// because `PrimitiveBlock` in `block.rs` transmutes `WireBlock<'a>` to
/// `WireBlock<'static>` for self-referential ownership — storing slices directly
/// would create dangling references after the lifetime erasure.
///
/// Two storage modes:
/// - **Boxed** (`entries` is `Some`): traditional heap-allocated entry array.
///   Used by `PrimitiveBlock::new(Bytes)` for backward compatibility.
/// - **Inline** (`entries` is `None`, `entries_offset`/`entries_count` set):
///   entries are appended as raw LE bytes in the decompressed buffer itself.
///   Zero separate heap allocation. Used by `PrimitiveBlock::from_vec()`.
///   Eliminates cross-thread Box alloc/free retention that caused 25+ GB OOM.
#[derive(Clone, Debug)]
pub(crate) struct WireStringTable<'a> {
    buffer: &'a [u8],
    /// Boxed entries for backward-compat path. None when using inline storage.
    entries: Option<Box<[(u32, u32)]>>,
    /// Byte offset into buffer where inline (u32, u32) LE entries start.
    entries_offset: u32,
    /// Number of inline entries.
    entries_count: u32,
}

impl<'a> WireStringTable<'a> {
    /// Parse string table entries into a heap-allocated Box (legacy path).
    #[hotpath::measure]
    fn parse(data: &'a [u8], buffer: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        let mut entries = Vec::new();
        while let Some((field, wire_type)) = cursor.read_tag()? {
            if field == 1 && wire_type == WIRE_LEN {
                let bytes = cursor.read_len_delimited()?;
                let offset = bytes.as_ptr() as usize - buffer.as_ptr() as usize;
                #[allow(clippy::cast_possible_truncation)]
                entries.push((offset as u32, bytes.len() as u32));
            } else {
                cursor.skip_field(wire_type)?;
            }
        }
        Ok(Self {
            buffer,
            entries: Some(entries.into_boxed_slice()),
            entries_offset: 0,
            entries_count: 0,
        })
    }

    /// Scan string table entries from protobuf data, collecting into caller's Vec.
    /// Used by the inline path: caller appends the Vec contents to the buffer afterward.
    fn scan_entries(data: &[u8], buffer: &[u8], out: &mut Vec<(u32, u32)>) -> Result<()> {
        let mut cursor = Cursor::new(data);
        out.clear();
        while let Some((field, wire_type)) = cursor.read_tag()? {
            if field == 1 && wire_type == WIRE_LEN {
                let bytes = cursor.read_len_delimited()?;
                let offset = bytes.as_ptr() as usize - buffer.as_ptr() as usize;
                #[allow(clippy::cast_possible_truncation)]
                out.push((offset as u32, bytes.len() as u32));
            } else {
                cursor.skip_field(wire_type)?;
            }
        }
        Ok(())
    }

    /// Create an inline WireStringTable that reads entries from the buffer itself.
    fn inline(buffer: &'a [u8], entries_offset: u32, entries_count: u32) -> Self {
        Self { buffer, entries: None, entries_offset, entries_count }
    }

    #[inline]
    pub fn len(&self) -> usize {
        if let Some(ref e) = self.entries {
            e.len()
        } else {
            self.entries_count as usize
        }
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&'a [u8]> {
        if let Some(ref entries) = self.entries {
            // Boxed path
            entries.get(index).map(|&(off, len)| {
                &self.buffer[off as usize..off as usize + len as usize]
            })
        } else {
            // Inline path: read (u32, u32) from buffer at entries_offset + index * 8
            if index >= self.entries_count as usize { return None; }
            let base = self.entries_offset as usize + index * 8;
            if base + 8 > self.buffer.len() { return None; }
            let off = u32::from_le_bytes([
                self.buffer[base], self.buffer[base + 1],
                self.buffer[base + 2], self.buffer[base + 3],
            ]);
            let len = u32::from_le_bytes([
                self.buffer[base + 4], self.buffer[base + 5],
                self.buffer[base + 6], self.buffer[base + 7],
            ]);
            Some(&self.buffer[off as usize..off as usize + len as usize])
        }
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
    /// Group ranges: boxed for legacy path, inline offset+count for inline path.
    group_ranges_box: Option<Box<[(u32, u32)]>>,
    group_ranges_offset: u32,
    group_ranges_count: u32,
    pub granularity: i32,
    pub lat_offset: i64,
    pub lon_offset: i64,
    pub date_granularity: i32,
    /// Length of the original protobuf data (before inline entries were appended).
    pub proto_len: u32,
}

impl<'a> WireBlock<'a> {
    /// Parse a PrimitiveBlock using heap-allocated string table and group ranges (legacy path).
    /// Used by `PrimitiveBlock::new(Bytes)` for backward compatibility.
    #[hotpath::measure]
    pub fn parse(buffer: &'a [u8]) -> Result<Self> {
        let mut cursor = Cursor::new(buffer);
        let mut stringtable_data: Option<&'a [u8]> = None;
        let mut group_ranges = Vec::new();
        let mut granularity: i32 = 100;
        let mut lat_offset: i64 = 0;
        let mut lon_offset: i64 = 0;
        let mut date_granularity: i32 = 1000;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => {
                    stringtable_data = Some(cursor.read_len_delimited()?);
                }
                (2, WIRE_LEN) => {
                    let data = cursor.read_len_delimited()?;
                    let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    group_ranges.push((offset as u32, data.len() as u32));
                }
                (17, WIRE_VARINT) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    { granularity = cursor.read_varint()? as i32; }
                }
                (19, WIRE_VARINT) => { lat_offset = cursor.read_varint_i64()?; }
                (20, WIRE_VARINT) => { lon_offset = cursor.read_varint_i64()?; }
                (18, WIRE_VARINT) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    { date_granularity = cursor.read_varint()? as i32; }
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        let st_data = stringtable_data.unwrap_or(&[]);
        let stringtable = WireStringTable::parse(st_data, buffer)?;

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            buffer,
            stringtable,
            group_ranges_box: Some(group_ranges.into_boxed_slice()),
            group_ranges_offset: 0,
            group_ranges_count: 0,
            granularity,
            lat_offset,
            lon_offset,
            date_granularity,
            proto_len: buffer.len() as u32,
        })
    }

    /// Parse and inline: scan protobuf, then append string table entries and group
    /// ranges as raw LE bytes to the buffer. Zero separate heap allocations.
    ///
    /// The buffer is extended in-place. After this call, the buffer layout is:
    /// `[protobuf data] [st entries: (u32,u32) × N as LE bytes] [group ranges: (u32,u32) × M as LE bytes]`
    ///
    /// Temp Vecs used during scanning are allocated and freed on the calling thread
    /// (no cross-thread retention). Only the buffer itself crosses thread boundaries.
    #[hotpath::measure]
    #[allow(clippy::cast_possible_truncation)]
    pub fn parse_and_inline(buf: &mut Vec<u8>) -> Result<WireBlockMeta> {
        let proto_len = buf.len() as u32;

        // Phase 1: scan protobuf, collect into temp local Vecs (same-thread alloc/free).
        let mut st_entries: Vec<(u32, u32)> = Vec::new();
        let mut group_entries: Vec<(u32, u32)> = Vec::new();
        let mut granularity: i32 = 100;
        let mut lat_offset: i64 = 0;
        let mut lon_offset: i64 = 0;
        let mut date_granularity: i32 = 1000;
        let mut stringtable_offset: usize = 0;
        let mut stringtable_len: usize = 0;

        {
            let buffer: &[u8] = buf;
            let mut cursor = Cursor::new(buffer);

            while let Some((field, wire_type)) = cursor.read_tag()? {
                match (field, wire_type) {
                    (1, WIRE_LEN) => {
                        let data = cursor.read_len_delimited()?;
                        stringtable_offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                        stringtable_len = data.len();
                    }
                    (2, WIRE_LEN) => {
                        let data = cursor.read_len_delimited()?;
                        let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                        group_entries.push((offset as u32, data.len() as u32));
                    }
                    (17, WIRE_VARINT) => {
                        #[allow(clippy::cast_possible_wrap)]
                        { granularity = cursor.read_varint()? as i32; }
                    }
                    (19, WIRE_VARINT) => { lat_offset = cursor.read_varint_i64()?; }
                    (20, WIRE_VARINT) => { lon_offset = cursor.read_varint_i64()?; }
                    (18, WIRE_VARINT) => {
                        #[allow(clippy::cast_possible_wrap)]
                        { date_granularity = cursor.read_varint()? as i32; }
                    }
                    _ => cursor.skip_field(wire_type)?,
                }
            }

            // Scan string table entries from the stringtable submessage.
            let st_data = if stringtable_len > 0 {
                &buffer[stringtable_offset..stringtable_offset + stringtable_len]
            } else {
                &[]
            };
            WireStringTable::scan_entries(st_data, buffer, &mut st_entries)?;
        }
        // Phase 1 temp Vecs are still alive but immutable borrow of buf is released.

        // Phase 2: append entries as raw LE bytes to the buffer.
        let st_inline_offset = buf.len() as u32;
        let st_inline_count = st_entries.len() as u32;
        for &(off, len) in &st_entries {
            buf.extend_from_slice(&off.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }

        let gr_inline_offset = buf.len() as u32;
        let gr_inline_count = group_entries.len() as u32;
        for &(off, len) in &group_entries {
            buf.extend_from_slice(&off.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }

        // Phase 1 temp Vecs drop here — same-thread free, no cross-thread retention.
        Ok(WireBlockMeta {
            proto_len,
            st_inline_offset,
            st_inline_count,
            gr_inline_offset,
            gr_inline_count,
            granularity,
            lat_offset,
            lon_offset,
            date_granularity,
        })
    }

    /// Construct a WireBlock from inline metadata + the extended buffer.
    pub fn from_inline(buffer: &'a [u8], meta: &WireBlockMeta) -> Self {
        Self {
            buffer,
            stringtable: WireStringTable::inline(buffer, meta.st_inline_offset, meta.st_inline_count),
            group_ranges_box: None,
            group_ranges_offset: meta.gr_inline_offset,
            group_ranges_count: meta.gr_inline_count,
            granularity: meta.granularity,
            lat_offset: meta.lat_offset,
            lon_offset: meta.lon_offset,
            date_granularity: meta.date_granularity,
            proto_len: meta.proto_len,
        }
    }

    /// Number of primitive groups in this block.
    #[inline]
    pub fn group_count(&self) -> usize {
        if let Some(ref b) = self.group_ranges_box {
            b.len()
        } else {
            self.group_ranges_count as usize
        }
    }

    #[inline]
    pub fn group(&self, index: usize) -> &'a [u8] {
        if let Some(ref b) = self.group_ranges_box {
            let (off, len) = b[index];
            &self.buffer[off as usize..off as usize + len as usize]
        } else {
            let base = self.group_ranges_offset as usize + index * 8;
            let off = u32::from_le_bytes([
                self.buffer[base], self.buffer[base + 1],
                self.buffer[base + 2], self.buffer[base + 3],
            ]);
            let len = u32::from_le_bytes([
                self.buffer[base + 4], self.buffer[base + 5],
                self.buffer[base + 6], self.buffer[base + 7],
            ]);
            &self.buffer[off as usize..off as usize + len as usize]
        }
    }
}

/// Metadata from `WireBlock::parse_and_inline`. Stored on the stack — no heap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WireBlockMeta {
    pub proto_len: u32,
    pub st_inline_offset: u32,
    pub st_inline_count: u32,
    pub gr_inline_offset: u32,
    pub gr_inline_count: u32,
    pub granularity: i32,
    pub lat_offset: i64,
    pub lon_offset: i64,
    pub date_granularity: i32,
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

    #[inline]
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

    #[inline]
    pub fn ways(&self) -> WireMessageIter<'a> {
        WireMessageIter::new(self.data, 3)
    }

    #[inline]
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

    #[inline]
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

        // Osmosis writes -1 for version and changeset when metadata is absent
        // (protobuf default is 0). Map these sentinels to None so downstream
        // code treats them as genuinely absent rather than real values.
        if info.version == Some(-1) {
            info.version = None;
        }
        if info.changeset == Some(-1) {
            info.changeset = None;
        }

        Ok(info)
    }
}

// ---------------------------------------------------------------------------
// Unit tests — OSM-specific parsers only. Primitive tests live in protohoggr.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn wire_info_parse() {
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
