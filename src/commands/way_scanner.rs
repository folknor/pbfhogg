//! Way-ref-only wire-format scanner for extracting way node references from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction — no string table parsing,
//! no group_ranges allocation. Only extracts way IDs and their node ref lists.
//!
//! Used by passes that only need `way.id()` + `way.refs()`:
//! - ALTW pass 0 (`collect_way_referenced_node_ids`)
//! - Geocode builder pass 1.5 (referenced node collection)
//!
//! # Known limitations
//!
//! - **Way groups only.** Parses PrimitiveGroup field 3 (Way). Other element
//!   types (nodes, relations) in the same group are skipped.
//! - **Sorted PBF assumption.** Relies on indexdata `ElemKind::Way` for blob
//!   filtering. Mixed-type blobs in unsorted PBFs could be mislabeled.

use super::Result;

/// Extract way IDs and their node refs from decompressed PrimitiveBlock bytes.
///
/// For each way, calls `callback(way_id, &refs)` where refs is the decoded
/// node ID list. Uses a caller-provided `refs_buf` to avoid per-way allocation.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn scan_way_refs(
    decompressed: &[u8],
    refs_buf: &mut Vec<i64>,
    mut callback: impl FnMut(i64, &[i64]),
) -> Result<()> {
    use crate::read::wire::{Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    // Parse PrimitiveBlock top-level: only collect group offsets.
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            _ => { cursor.skip_field(wire_type)?; }
        }
    }

    for &(off, len) in &group_starts {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);

        // PrimitiveGroup field 3 = Way (repeated).
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 3 && wire_type == WIRE_LEN {
                let way_data = gcursor.read_len_delimited()?;
                parse_way_refs(way_data, refs_buf, &mut callback)?;
            } else {
                gcursor.skip_field(wire_type)?;
            }
        }
    }

    Ok(())
}

/// Parse a single Way message and extract id + refs.
#[allow(clippy::cast_possible_wrap)]
fn parse_way_refs(
    way_data: &[u8],
    refs_buf: &mut Vec<i64>,
    callback: &mut impl FnMut(i64, &[i64]),
) -> Result<()> {
    use crate::read::wire::{Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT};

    let mut cursor = Cursor::new(way_data);
    let mut way_id: i64 = 0;
    let mut refs_data: Option<&[u8]> = None;

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (1, WIRE_VARINT) => { way_id = cursor.read_varint()? as i64; }
            (8, WIRE_LEN) => { refs_data = Some(cursor.read_len_delimited()?); }
            _ => { cursor.skip_field(wire_type)?; }
        }
    }

    if let Some(rd) = refs_data {
        refs_buf.clear();
        let mut cum: i64 = 0;
        for delta in PackedSint64Iter::new(rd) {
            cum += delta;
            refs_buf.push(cum);
        }
        callback(way_id, refs_buf);
    }

    Ok(())
}
