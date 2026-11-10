//! The geocode-relevant way store: the scratch spine between the way
//! extraction pass (Pass 1.5) and the way resolver (Pass 2b).
//!
//! Pass 1.5 reads every way blob once, validates it as block construction
//! does, classifies each way and appends one record per geocode-relevant
//! way (street, building address, interpolation or admin member) to its
//! worker's scratch file, while it fills the referenced-node set. Pass 2b
//! then resolves coordinates from these records instead of decoding the way
//! blobs a second time.
//!
//! # Order
//!
//! Records are grouped into one segment per scheduled way blob, in the
//! blob's way order. The segment table is indexed by way-schedule position,
//! never by worker or by physical append order, and the resolver produces one
//! result per scheduled blob (an empty blob included), so the output, and the
//! string pool's interning order, are those of a blob-by-blob decode.
//!
//! # Record layout
//!
//! Scratch only, never a reader format; every field is written by this
//! module and read back only by it:
//!
//! ```text
//! flags      u8   ROLE_* bits
//! values     u8   VALUE_* bits: which tag values follow
//! way_id     zigzag varint
//! value*     varint length + bytes, in VALUE_* bit order
//! refs       varint length + the way's validated packed `refs` bytes,
//!            unchanged (zigzag deltas, decoded with wrapping arithmetic)
//! ```
//!
//! Every ref of the way is kept, including negative refs, refs above the
//! node-ID bound, repeats and a closing vertex, so missing-ref accounting,
//! interpolation completeness and building centroids are computed exactly
//! as from the block.

use super::Result;
use crate::error::Error;

pub(super) const ROLE_STREET: u8 = 1;
pub(super) const ROLE_BUILDING_ADDR: u8 = 1 << 1;
pub(super) const ROLE_INTERP: u8 = 1 << 2;
pub(super) const ROLE_ADMIN: u8 = 1 << 3;

/// Tag value slots, in record order.
pub(super) const VALUE_NAME: usize = 0;
pub(super) const VALUE_HOUSENUMBER: usize = 1;
pub(super) const VALUE_STREET: usize = 2;
pub(super) const VALUE_PLACE: usize = 3;
pub(super) const VALUE_POSTCODE: usize = 4;
pub(super) const VALUE_INTERPOLATION: usize = 5;
pub(super) const VALUE_SLOTS: usize = 6;

/// The tag values a way's roles read: the street name for a street, the
/// address for a building, the parent and mode for an interpolation.
pub(super) fn needed_values(roles: u8) -> u8 {
    let mut mask = 0_u8;
    if roles & ROLE_STREET != 0 {
        mask |= 1 << VALUE_NAME;
    }
    if roles & ROLE_BUILDING_ADDR != 0 {
        mask |= (1 << VALUE_HOUSENUMBER)
            | (1 << VALUE_STREET)
            | (1 << VALUE_PLACE)
            | (1 << VALUE_POSTCODE);
    }
    if roles & ROLE_INTERP != 0 {
        mask |= (1 << VALUE_STREET) | (1 << VALUE_PLACE) | (1 << VALUE_INTERPOLATION);
    }
    mask
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    protohoggr::encode_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Append one record. `values` holds the way's tag values as validated
/// string-table bytes; only those `needed_values(roles)` selects are kept.
pub(super) fn encode(
    out: &mut Vec<u8>,
    way_id: i64,
    roles: u8,
    values: &[Option<&[u8]>; VALUE_SLOTS],
    refs: &[u8],
) {
    let needed = needed_values(roles);
    let mut present = 0_u8;
    for (slot, value) in values.iter().enumerate() {
        if value.is_some() && needed & (1 << slot) != 0 {
            present |= 1 << slot;
        }
    }
    out.push(roles);
    out.push(present);
    protohoggr::encode_varint(out, protohoggr::zigzag_encode_64(way_id));
    for (slot, value) in values.iter().enumerate() {
        if present & (1 << slot) != 0
            && let Some(value) = value
        {
            put_bytes(out, value);
        }
    }
    put_bytes(out, refs);
}

/// One record read back.
pub(super) struct ExtractedWay<'a> {
    pub(super) way_id: i64,
    pub(super) roles: u8,
    pub(super) values: [Option<&'a str>; VALUE_SLOTS],
    /// The way's packed `refs` bytes, as validated in its blob.
    pub(super) refs: &'a [u8],
}

impl ExtractedWay<'_> {
    pub(super) fn has(&self, role: u8) -> bool {
        self.roles & role != 0
    }
}

/// A scratch record the store cannot read back is a pbfhogg bug or a
/// scratch file changed under the run, never input.
#[cold]
#[inline(never)]
fn corrupt() -> Error {
    Error::internal("geocode way store: a scratch record does not decode")
}

/// The records of one segment, in the order they were written.
pub(super) struct SegmentRecords<'a> {
    cursor: crate::read::wire::Cursor<'a>,
}

impl<'a> SegmentRecords<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: crate::read::wire::Cursor::new(bytes),
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        self.cursor.read_len_delimited().map_err(|_| corrupt())
    }

    fn byte(&mut self) -> Result<u8> {
        let v = self.cursor.read_varint().map_err(|_| corrupt())?;
        // A flag byte below 0x80 is its own one-byte varint.
        u8::try_from(v)
            .ok()
            .filter(|b| *b < 0x80)
            .ok_or_else(corrupt)
    }

    /// The next record, or `None` at the end of the segment.
    pub(super) fn next_way(&mut self) -> Result<Option<ExtractedWay<'a>>> {
        if self.cursor.is_empty() {
            return Ok(None);
        }
        let roles = self.byte()?;
        let present = self.byte()?;
        let way_id =
            protohoggr::zigzag_decode_64(self.cursor.read_varint().map_err(|_| corrupt())?);
        let mut values: [Option<&'a str>; VALUE_SLOTS] = [None; VALUE_SLOTS];
        for (slot, value) in values.iter_mut().enumerate() {
            if present & (1 << slot) != 0 {
                let bytes = self.bytes()?;
                // Extraction stored bytes of a string table that passed the
                // block's UTF-8 check; the check here costs one pass over
                // tag values and keeps the store free of `unsafe`.
                *value = Some(std::str::from_utf8(bytes).map_err(|_| corrupt())?);
            }
        }
        let refs = self.bytes()?;
        Ok(Some(ExtractedWay {
            way_id,
            roles,
            values,
            refs,
        }))
    }
}

/// Where one way blob's segment lies: its worker's file and byte range.
#[derive(Clone, Copy)]
pub(super) struct Segment {
    pub(super) file: usize,
    pub(super) offset: u64,
    pub(super) len: u64,
}

/// The store once extraction has joined: one read-only mapping per worker
/// file, and each scheduled way blob's segment (`None` for a blob with no
/// geocode-relevant way).
pub(super) struct WayStore {
    maps: Vec<Option<memmap2::Mmap>>,
    segments: Vec<Option<Segment>>,
    paths: Vec<crate::scratch::ScratchPath>,
}

impl WayStore {
    /// Map the worker files, flushed and closed by their writers.
    pub(super) fn open(
        paths: Vec<crate::scratch::ScratchPath>,
        segments: Vec<Option<Segment>>,
    ) -> Result<Self> {
        let mut maps = Vec::with_capacity(paths.len());
        for path in &paths {
            let file = crate::scratch::open_file(path)?;
            if file.metadata()?.len() == 0 {
                maps.push(None);
                continue;
            }
            // SAFETY: the extraction workers have joined and dropped their
            // writers, so nothing writes this run-private scratch file again;
            // it is removed only by `remove`, which consumes the store and
            // with it the mapping.
            let map = unsafe { memmap2::Mmap::map(file.as_file()) }.map_err(|e| {
                Error::from(e).context(format!("map geocode way store {}", path.display()))
            })?;
            maps.push(Some(map));
        }
        Ok(Self {
            maps,
            segments,
            paths,
        })
    }

    /// The segment of way-schedule position `position`, empty when that blob
    /// holds no geocode-relevant way.
    pub(super) fn segment(&self, position: usize) -> Result<&[u8]> {
        let Some(slot) = self.segments.get(position) else {
            return Err(Error::internal(format!(
                "geocode way store has no slot for way blob {position}"
            )));
        };
        let Some(seg) = slot else {
            return Ok(&[]);
        };
        let map = self
            .maps
            .get(seg.file)
            .and_then(Option::as_ref)
            .ok_or_else(|| Error::internal("geocode way store segment names an unmapped file"))?;
        let start = usize::try_from(seg.offset).map_err(|_| corrupt())?;
        let len = usize::try_from(seg.len).map_err(|_| corrupt())?;
        map.get(start..start.checked_add(len).ok_or_else(corrupt)?)
            .ok_or_else(corrupt)
    }

    /// Bytes held, for the progress line.
    pub(super) fn bytes(&self) -> u64 {
        self.segments.iter().flatten().map(|s| s.len).sum()
    }

    /// Unmap and delete the worker files.
    pub(super) fn remove(self, scratch: &crate::scratch::RunScratch) {
        drop(self.maps);
        for path in &self.paths {
            scratch.remove(path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_with_only_the_values_their_roles_read() {
        let mut out = Vec::new();
        let mut values: [Option<&[u8]>; VALUE_SLOTS] = [None; VALUE_SLOTS];
        values[VALUE_NAME] = Some(b"Main St");
        values[VALUE_HOUSENUMBER] = Some(b"");
        values[VALUE_INTERPOLATION] = Some(b"even");
        encode(&mut out, -5, ROLE_STREET | ROLE_ADMIN, &values, &[2, 4]);
        encode(&mut out, i64::MAX, ROLE_ADMIN, &values, &[]);
        let mut records = SegmentRecords::new(&out);
        let first = records.next_way().unwrap().unwrap();
        assert_eq!(first.way_id, -5);
        assert!(first.has(ROLE_STREET) && first.has(ROLE_ADMIN));
        assert!(!first.has(ROLE_INTERP));
        assert_eq!(first.values[VALUE_NAME], Some("Main St"));
        // Not read by a street or an admin way, so not stored.
        assert_eq!(first.values[VALUE_HOUSENUMBER], None);
        assert_eq!(first.values[VALUE_INTERPOLATION], None);
        assert_eq!(first.refs, [2, 4]);
        let second = records.next_way().unwrap().unwrap();
        assert_eq!(second.way_id, i64::MAX);
        assert_eq!(second.values, [None; VALUE_SLOTS]);
        assert!(second.refs.is_empty());
        assert!(records.next_way().unwrap().is_none());
    }

    #[test]
    fn a_present_empty_value_stays_present() {
        let mut out = Vec::new();
        let mut values: [Option<&[u8]>; VALUE_SLOTS] = [None; VALUE_SLOTS];
        values[VALUE_HOUSENUMBER] = Some(b"");
        values[VALUE_STREET] = Some(b"High St");
        encode(&mut out, 1, ROLE_BUILDING_ADDR, &values, &[2]);
        let way = SegmentRecords::new(&out).next_way().unwrap().unwrap();
        assert_eq!(way.values[VALUE_HOUSENUMBER], Some(""));
        assert_eq!(way.values[VALUE_STREET], Some("High St"));
        assert_eq!(way.values[VALUE_PLACE], None);
    }

    #[test]
    fn a_truncated_record_is_an_internal_error() {
        let mut out = Vec::new();
        encode(&mut out, 1, ROLE_STREET, &[None; VALUE_SLOTS], &[2, 4, 6]);
        out.pop();
        assert!(SegmentRecords::new(&out).next_way().is_err());
    }
}
