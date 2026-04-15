//! Relation-member-node wire-format scanner for PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction when the caller only needs the set
//! of node IDs referenced by relation members.
//!
//! Used by ALTW's optional relation-member preservation pass.
//!
//! # Known limitations
//!
//! - **Relation groups only.** Parses PrimitiveGroup field 4 (Relation). Other
//!   element types in the same group are skipped.
//! - **Sorted PBF assumption.** Callers filter blobs by indexdata kind before
//!   invoking this scanner.

use super::Result;

/// Extract all non-negative node member IDs from a decompressed PrimitiveBlock.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn scan_relation_member_node_ids(
    decompressed: &[u8],
    group_starts: &mut Vec<(usize, usize)>,
    member_node_ids: &mut crate::commands::id_set_dense::IdSetDense,
) -> Result<()> {
    use crate::read::wire::{Cursor, WIRE_LEN};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    group_starts.clear();

    // Parse PrimitiveBlock top-level: only collect PrimitiveGroup offsets.
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            _ => cursor.skip_field(wire_type)?,
        }
    }

    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);

        // PrimitiveGroup field 4 = Relation (repeated).
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 4 && wire_type == WIRE_LEN {
                let relation_data = gcursor.read_len_delimited()?;
                parse_relation_member_node_ids(relation_data, member_node_ids)?;
            } else {
                gcursor.skip_field(wire_type)?;
            }
        }
    }

    Ok(())
}

fn parse_relation_member_node_ids(
    relation_data: &[u8],
    member_node_ids: &mut crate::commands::id_set_dense::IdSetDense,
) -> Result<()> {
    use crate::read::wire::{Cursor, PackedInt32Iter, PackedSint64Iter, WIRE_LEN};

    let mut cursor = Cursor::new(relation_data);
    let mut memids_data: &[u8] = &[];
    let mut types_data: &[u8] = &[];

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (9, WIRE_LEN) => memids_data = cursor.read_len_delimited()?,
            (10, WIRE_LEN) => types_data = cursor.read_len_delimited()?,
            _ => cursor.skip_field(wire_type)?,
        }
    }

    let mut current_member_id: i64 = 0;
    let mut member_id_deltas = PackedSint64Iter::new(memids_data);
    let mut member_types = PackedInt32Iter::new(types_data);

    while let (Some(delta), Some(member_type)) = (member_id_deltas.next(), member_types.next()) {
        current_member_id += delta;
        if member_type == 0 && current_member_id >= 0 {
            member_node_ids.set(current_member_id);
        }
    }

    Ok(())
}
