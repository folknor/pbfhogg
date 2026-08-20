//! OSM PrimitiveBlock wire-format message parsers.
//!
//! Uses [`protohoggr`] for the generic protobuf primitives (Cursor, packed
//! iterators, wire constants). This module adds the OSM-specific message
//! parsers: PrimitiveBlock, PrimitiveGroup, Node, Way, Relation, DenseNodes,
//! DenseInfo, Info, and StringTable.

use crate::error::{Result, new_wire_error};
pub(crate) use protohoggr::{
    Cursor, PackedBoolIter, PackedInt32Iter, PackedSint32Iter, PackedSint64Iter, PackedUint32Iter,
    WIRE_LEN, WIRE_VARINT,
};

/// Record one occurrence of a packed repeated field into `slot`.
///
/// Protobuf permits a packed repeated field to be written in either wire form:
/// length-delimited (one packed segment, which is what every OSM writer in
/// existence emits) or varint (an individually tagged, unpacked value).
/// libosmium accepts both forms for every packed field on the OSM read path -
/// see the paired `length_delimited` / `varint` cases throughout
/// `research/libosmium/include/osmium/io/detail/pbf_decoder.hpp` - and so do we.
/// A single unpacked value is byte-identical to a one-element packed segment,
/// so it is captured with no copy: `read_raw_field` hands back exactly the
/// varint's own bytes.
///
/// Before this helper existed, only `WIRE_LEN` was matched. An unpacked field
/// fell through to `skip_field` and the slot stayed empty, so an unpacked
/// `DenseNodes.keys_vals` decoded as "this block has no tags at all" with no
/// diagnostic - the silent-data-loss class this parser exists to prevent,
/// reachable through a spec-legal encoding.
///
/// The spec also permits a packed field to appear as *several* occurrences,
/// whose semantics are concatenation of every occurrence in field order. A
/// concatenation cannot be expressed as one borrowed slice, and the read path
/// is built on borrowed slices for a measured reason - so we do not support it.
/// We do not silently keep the last occurrence either, which is what this
/// parser used to do and what libosmium still does (`ids = ...` plainly
/// overwrites): that drops element data with no diagnostic. Instead a second
/// non-empty occurrence is a hard error naming the field. Empty occurrences are
/// ignored in either position, since an empty segment contributes nothing to a
/// concatenation - so the only rejected shape is two occurrences that both
/// carry data, which no known writer produces.
#[inline]
fn take_packed<'a>(
    cursor: &mut Cursor<'a>,
    wire_type: u32,
    slot: &mut &'a [u8],
    dup_msg: &'static str,
) -> Result<()> {
    let bytes = if wire_type == WIRE_LEN {
        cursor.read_len_delimited()?
    } else {
        cursor.read_raw_field(WIRE_VARINT)?
    };
    if bytes.is_empty() {
        return Ok(());
    }
    if !slot.is_empty() {
        return Err(new_wire_error(dup_msg));
    }
    *slot = bytes;
    Ok(())
}

// Per-field duplicate-occurrence messages. Each names exactly one field of one
// message, so a test asserting on one of these cannot be satisfied by any other
// guard in the decoder.
const DUP_NODE_KEYS: &str = "Node.keys appears twice with data; packed field split across segments";
const DUP_NODE_VALS: &str = "Node.vals appears twice with data; packed field split across segments";
const DUP_WAY_KEYS: &str = "Way.keys appears twice with data; packed field split across segments";
const DUP_WAY_VALS: &str = "Way.vals appears twice with data; packed field split across segments";
const DUP_WAY_REFS: &str = "Way.refs appears twice with data; packed field split across segments";
const DUP_WAY_LAT: &str = "Way.lat appears twice with data; packed field split across segments";
const DUP_WAY_LON: &str = "Way.lon appears twice with data; packed field split across segments";
const DUP_REL_KEYS: &str =
    "Relation.keys appears twice with data; packed field split across segments";
const DUP_REL_VALS: &str =
    "Relation.vals appears twice with data; packed field split across segments";
const DUP_REL_ROLES: &str =
    "Relation.roles_sid appears twice with data; packed field split across segments";
const DUP_REL_MEMIDS: &str =
    "Relation.memids appears twice with data; packed field split across segments";
const DUP_REL_TYPES: &str =
    "Relation.types appears twice with data; packed field split across segments";
const DUP_DENSE_ID: &str =
    "DenseNodes.id appears twice with data; packed field split across segments";
const DUP_DENSE_LAT: &str =
    "DenseNodes.lat appears twice with data; packed field split across segments";
const DUP_DENSE_LON: &str =
    "DenseNodes.lon appears twice with data; packed field split across segments";
const DUP_DENSE_KV: &str =
    "DenseNodes.keys_vals appears twice with data; packed field split across segments";
const DUP_DI_VERSION: &str =
    "DenseInfo.version appears twice with data; packed field split across segments";
const DUP_DI_TIMESTAMP: &str =
    "DenseInfo.timestamp appears twice with data; packed field split across segments";
const DUP_DI_CHANGESET: &str =
    "DenseInfo.changeset appears twice with data; packed field split across segments";
const DUP_DI_UID: &str =
    "DenseInfo.uid appears twice with data; packed field split across segments";
const DUP_DI_USER_SID: &str =
    "DenseInfo.user_sid appears twice with data; packed field split across segments";
const DUP_DI_VISIBLE: &str =
    "DenseInfo.visible appears twice with data; packed field split across segments";

// ---------------------------------------------------------------------------
// WireStringTable - zero-copy indexed string table
// ---------------------------------------------------------------------------

/// String table storing buffer-relative offsets as inline LE bytes in the
/// decompressed buffer. Zero separate heap allocations.
///
/// Offsets are relative to the decompressed buffer, not the StringTable message,
/// because `PrimitiveBlock` in `block.rs` transmutes `WireBlock<'a>` to
/// `WireBlock<'static>` for self-referential ownership.
///
/// The (u32, u32) entry pairs (offset, length) are appended to the buffer
/// after the protobuf data by `WireBlock::parse_and_inline`. This eliminates
/// the cross-thread Box alloc/free retention that caused 25+ GB OOM at
/// Europe/planet scale with the pipelined reader.
#[derive(Clone, Debug)]
pub(crate) struct WireStringTable<'a> {
    buffer: &'a [u8],
    /// Byte offset into buffer where inline (u32, u32) LE entries start.
    entries_offset: u32,
    /// Number of inline entries.
    entries_count: u32,
}

impl<'a> WireStringTable<'a> {
    /// Scan string table entries from protobuf data, collecting into caller's Vec.
    /// Caller appends the Vec contents to the buffer afterward.
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

    /// Create a WireStringTable that reads entries from the buffer itself.
    fn new(buffer: &'a [u8], entries_offset: u32, entries_count: u32) -> Self {
        Self {
            buffer,
            entries_offset,
            entries_count,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries_count as usize
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&'a [u8]> {
        if index >= self.entries_count as usize {
            return None;
        }
        let base = self.entries_offset as usize + index * 8;
        if base + 8 > self.buffer.len() {
            return None;
        }
        let off = u32::from_le_bytes([
            self.buffer[base],
            self.buffer[base + 1],
            self.buffer[base + 2],
            self.buffer[base + 3],
        ]);
        let len = u32::from_le_bytes([
            self.buffer[base + 4],
            self.buffer[base + 5],
            self.buffer[base + 6],
            self.buffer[base + 7],
        ]);
        Some(&self.buffer[off as usize..off as usize + len as usize])
    }
}

// ---------------------------------------------------------------------------
// WireBlock - parsed PrimitiveBlock
// ---------------------------------------------------------------------------

/// Parsed PrimitiveBlock - the root message of each data blob.
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
        let mut st_scratch = Vec::new();
        let mut gr_scratch = Vec::new();
        Self::parse_and_inline_with_scratch(buf, &mut st_scratch, &mut gr_scratch)
    }

    /// Like [`parse_and_inline`] but reuses caller-provided scratch buffers
    /// to avoid per-block allocation. The scratch Vecs are cleared at the start
    /// and may grow to accommodate the block's string table and group entries.
    /// Between blocks, the capacity is retained - no malloc/free after the
    /// first few blocks.
    #[hotpath::measure]
    #[allow(clippy::cast_possible_truncation)]
    pub fn parse_and_inline_with_scratch(
        buf: &mut Vec<u8>,
        st_entries: &mut Vec<(u32, u32)>,
        group_entries: &mut Vec<(u32, u32)>,
    ) -> Result<WireBlockMeta> {
        let proto_len = buf.len() as u32;

        // Phase 1: scan protobuf, collect into reusable scratch Vecs.
        st_entries.clear();
        group_entries.clear();
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
                        {
                            granularity = cursor.read_varint()? as i32;
                        }
                    }
                    (19, WIRE_VARINT) => {
                        lat_offset = cursor.read_varint_i64()?;
                    }
                    (20, WIRE_VARINT) => {
                        lon_offset = cursor.read_varint_i64()?;
                    }
                    (18, WIRE_VARINT) => {
                        #[allow(clippy::cast_possible_wrap)]
                        {
                            date_granularity = cursor.read_varint()? as i32;
                        }
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
            WireStringTable::scan_entries(st_data, buffer, st_entries)?;
        }
        // Phase 1 temp Vecs are still alive but immutable borrow of buf is released.

        // Phase 2: append entries as raw LE bytes to the buffer.
        // Pre-reserve exact capacity to avoid reallocation during extend.
        let append_size = (st_entries.len() + group_entries.len()) * 8;
        buf.reserve(append_size);

        let st_inline_offset = buf.len() as u32;
        let st_inline_count = st_entries.len() as u32;
        for &(off, len) in st_entries.iter() {
            buf.extend_from_slice(&off.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }

        let gr_inline_offset = buf.len() as u32;
        let gr_inline_count = group_entries.len() as u32;
        for &(off, len) in group_entries.iter() {
            buf.extend_from_slice(&off.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }

        // Scratch Vecs retain capacity for the next block (caller reuses them).
        #[allow(clippy::cast_possible_truncation)]
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
            stringtable: WireStringTable::new(buffer, meta.st_inline_offset, meta.st_inline_count),
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
        self.group_ranges_count as usize
    }

    #[inline]
    pub fn group(&self, index: usize) -> &'a [u8] {
        let base = self.group_ranges_offset as usize + index * 8;
        let off = u32::from_le_bytes([
            self.buffer[base],
            self.buffer[base + 1],
            self.buffer[base + 2],
            self.buffer[base + 3],
        ]);
        let len = u32::from_le_bytes([
            self.buffer[base + 4],
            self.buffer[base + 5],
            self.buffer[base + 6],
            self.buffer[base + 7],
        ]);
        &self.buffer[off as usize..off as usize + len as usize]
    }
}

/// Metadata from `WireBlock::parse_and_inline`. Stored on the stack - no heap.
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
// WireGroup - lazy PrimitiveGroup scanner
// ---------------------------------------------------------------------------

/// Lazy PrimitiveGroup scanner - yields raw sub-message bytes by element type.
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
// WireMessageIter - yields sub-message byte slices for a given field number
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
                (2, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut keys_data, DUP_NODE_KEYS)?;
                }
                (3, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut vals_data, DUP_NODE_VALS)?;
                }
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
    pub pins_data: Option<&'a [u8]>,
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
        let mut pins_data: Option<&[u8]> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_VARINT) => id = cursor.read_varint_i64()?,
                (2, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut keys_data, DUP_WAY_KEYS)?;
                }
                (3, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut vals_data, DUP_WAY_VALS)?;
                }
                (4, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut refs_data, DUP_WAY_REFS)?;
                }
                (9, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut lat_data, DUP_WAY_LAT)?;
                }
                (10, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut lon_data, DUP_WAY_LON)?;
                }
                (20, WIRE_LEN) => pins_data = Some(cursor.read_len_delimited()?),
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
            pins_data,
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
                (2, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut keys_data, DUP_REL_KEYS)?;
                }
                (3, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut vals_data, DUP_REL_VALS)?;
                }
                (4, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut roles_sid_data, DUP_REL_ROLES)?;
                }
                (9, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut memids_data, DUP_REL_MEMIDS)?;
                }
                (10, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut types_data, DUP_REL_TYPES)?;
                }
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

#[derive(Clone, Copy, Debug)]
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
                (1, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut id_data, DUP_DENSE_ID)?;
                }
                (5, WIRE_LEN) => info_data = Some(cursor.read_len_delimited()?),
                (8, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut lat_data, DUP_DENSE_LAT)?;
                }
                (9, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut lon_data, DUP_DENSE_LON)?;
                }
                (10, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut keys_vals_data, DUP_DENSE_KV)?;
                }
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

#[derive(Clone, Copy, Debug)]
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
                (1, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut version_data, DUP_DI_VERSION)?;
                }
                (2, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(
                        &mut cursor,
                        wire_type,
                        &mut timestamp_data,
                        DUP_DI_TIMESTAMP,
                    )?;
                }
                (3, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(
                        &mut cursor,
                        wire_type,
                        &mut changeset_data,
                        DUP_DI_CHANGESET,
                    )?;
                }
                (4, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut uid_data, DUP_DI_UID)?;
                }
                (5, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut user_sid_data, DUP_DI_USER_SID)?;
                }
                (6, WIRE_LEN | WIRE_VARINT) => {
                    take_packed(&mut cursor, wire_type, &mut visible_data, DUP_DI_VISIBLE)?;
                }
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

/// Parsed Info sub-message - all scalars, no byte references needed.
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
// Unit tests - OSM-specific parsers only. Primitive tests live in protohoggr.
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

    #[test]
    fn wire_way_parses_field_20_pins() {
        // field 1 (id) varint = 1; field 8 (refs) len=1 [0x00];
        // field 20 (pins) tag = (20<<3)|2 = 162 -> varint [0xA2, 0x01], len=1 [0xFF]
        let with_pins = [
            0x08, 0x01, // field 1: id = 1
            0x42, 0x01, 0x00, // field 8: refs = [0x00]
            0xA2, 0x01, 0x01, 0xFF, // field 20: pins = [0xFF]
        ];
        let way = WireWay::parse(&with_pins).unwrap();
        assert_eq!(way.pins_data, Some([0xFFu8].as_slice()));

        let without_pins = [0x08, 0x01, 0x42, 0x01, 0x00];
        let way = WireWay::parse(&without_pins).unwrap();
        assert_eq!(way.pins_data, None);
    }

    // -----------------------------------------------------------------------
    // Packed repeated fields: unpacked wire form, and duplicate occurrences.
    // -----------------------------------------------------------------------

    /// Tag byte for a single-byte field number and wire type.
    fn tag(field: u32, wire_type: u32) -> u8 {
        #[allow(clippy::cast_possible_truncation)]
        {
            ((field << 3) | wire_type) as u8
        }
    }

    #[test]
    fn dense_keys_vals_written_unpacked_is_not_silently_empty() {
        // DenseNodes with id packed [0x02] and keys_vals written UNPACKED as a
        // single varint-typed field 10 rather than a packed segment. Both forms
        // are legal protobuf; libosmium accepts both. Before take_packed the
        // varint form fell through to skip_field and keys_vals decoded as
        // empty, i.e. "no tags in this block" with no diagnostic.
        let unpacked = [
            tag(1, WIRE_LEN),
            0x01,
            0x02, // id = packed [1]
            tag(10, WIRE_VARINT),
            0x07, // keys_vals = 7, unpacked
        ];
        let dn = WireDenseNodes::parse(&unpacked).unwrap();
        assert_eq!(
            dn.keys_vals_data,
            [0x07u8].as_slice(),
            "unpacked keys_vals must decode to the same one-element varint stream \
             a packed segment would produce, not to an empty slice"
        );

        // The packed spelling of the identical value produces identical bytes.
        let packed = [tag(1, WIRE_LEN), 0x01, 0x02, tag(10, WIRE_LEN), 0x01, 0x07];
        let dn_packed = WireDenseNodes::parse(&packed).unwrap();
        assert_eq!(dn_packed.keys_vals_data, dn.keys_vals_data);
    }

    #[test]
    fn way_refs_written_unpacked_is_not_silently_empty() {
        let unpacked = [
            0x08,
            0x01, // id = 1
            tag(8, WIRE_VARINT),
            0x04, // refs = 4, unpacked
        ];
        let way = WireWay::parse(&unpacked).unwrap();
        assert_eq!(way.refs_data, [0x04u8].as_slice());
    }

    #[test]
    fn dense_keys_vals_in_two_data_bearing_segments_is_a_named_error() {
        let two_segments = [
            tag(1, WIRE_LEN),
            0x01,
            0x02,
            tag(10, WIRE_LEN),
            0x01,
            0x07, // keys_vals segment 1
            tag(10, WIRE_LEN),
            0x01,
            0x09, // keys_vals segment 2
        ];
        let err = WireDenseNodes::parse(&two_segments).unwrap_err();
        // Assert on the field-specific text. Only the keys_vals duplicate guard
        // can produce it: every other duplicate guard names a different field,
        // and a generic truncation or tag error names neither.
        assert!(
            err.to_string()
                .contains("DenseNodes.keys_vals appears twice"),
            "expected the keys_vals duplicate guard, got: {err}"
        );
    }

    #[test]
    fn duplicate_guard_names_the_field_that_actually_repeated() {
        // Same shape as above but on DenseNodes.lat. If the guard were shared
        // or the message generic, this test and the keys_vals one could not
        // both pass.
        let two_segments = [tag(8, WIRE_LEN), 0x01, 0x02, tag(8, WIRE_LEN), 0x01, 0x04];
        let err = WireDenseNodes::parse(&two_segments).unwrap_err();
        assert!(
            err.to_string().contains("DenseNodes.lat appears twice"),
            "expected the lat duplicate guard, got: {err}"
        );
    }

    #[test]
    fn an_empty_packed_segment_does_not_shadow_a_later_one() {
        // Concatenation semantics: an empty occurrence contributes nothing, so
        // empty-then-data and data-then-empty must both yield the data, not an
        // error and not the empty slice.
        let empty_first = [
            tag(10, WIRE_LEN),
            0x00, // keys_vals = empty segment
            tag(10, WIRE_LEN),
            0x01,
            0x07,
        ];
        assert_eq!(
            WireDenseNodes::parse(&empty_first).unwrap().keys_vals_data,
            [0x07u8].as_slice()
        );

        let empty_last = [tag(10, WIRE_LEN), 0x01, 0x07, tag(10, WIRE_LEN), 0x00];
        assert_eq!(
            WireDenseNodes::parse(&empty_last).unwrap().keys_vals_data,
            [0x07u8].as_slice()
        );
    }

    #[test]
    fn dense_info_accepts_the_unpacked_form() {
        let unpacked = [
            tag(1, WIRE_VARINT),
            0x03, // version = 3, unpacked
            tag(2, WIRE_LEN),
            0x01,
            0x08, // timestamp = packed [8]
        ];
        let di = WireDenseInfo::parse(&unpacked).unwrap();
        assert_eq!(di.version_data, [0x03u8].as_slice());
        assert_eq!(di.timestamp_data, [0x08u8].as_slice());
    }

    #[test]
    fn node_and_relation_packed_fields_accept_the_unpacked_form() {
        let node = [
            0x08,
            0x02, // id (sint64) = 1
            tag(2, WIRE_VARINT),
            0x05, // keys = 5, unpacked
            tag(3, WIRE_VARINT),
            0x06, // vals = 6, unpacked
        ];
        let n = WireNode::parse(&node).unwrap();
        assert_eq!(n.keys_data, [0x05u8].as_slice());
        assert_eq!(n.vals_data, [0x06u8].as_slice());

        let rel = [
            0x08,
            0x01,
            tag(9, WIRE_VARINT),
            0x0A, // memids = 10, unpacked
            tag(10, WIRE_VARINT),
            0x01, // types = 1, unpacked
        ];
        let r = WireRelation::parse(&rel).unwrap();
        assert_eq!(r.memids_data, [0x0Au8].as_slice());
        assert_eq!(r.types_data, [0x01u8].as_slice());
    }

    #[test]
    fn multi_byte_unpacked_varint_keeps_all_its_bytes() {
        // read_raw_field must hand back the whole varint, not just the first
        // byte: 300 encodes as 0xAC 0x02.
        let unpacked = [tag(10, WIRE_VARINT), 0xAC, 0x02];
        let dn = WireDenseNodes::parse(&unpacked).unwrap();
        assert_eq!(dn.keys_vals_data, [0xACu8, 0x02].as_slice());
        let mut it = PackedInt32Iter::new(dn.keys_vals_data);
        assert_eq!(it.next(), Some(300));
        assert_eq!(it.next(), None);
    }
}
