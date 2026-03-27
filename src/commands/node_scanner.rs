//! Node-only wire-format scanner for extracting (id, lat, lon) from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction entirely — no string table parsing,
//! no group_ranges allocation, no UTF-8 validation. This eliminates the cross-thread
//! alloc/free retention problem that causes 25+ GB heap accumulation at Europe/planet
//! scale when using the pipelined reader.
//!
//! Used by:
//! - External join stage 2 (merge-join with sorted bucket pairs)
//! - ALTW dense pass 1 (node index build)
//!
//! See `notes/cross-pipeline-optimization-plan.md` for the full list of retrofit targets.

use super::Result;

/// Compact node coordinate tuple. 16 bytes — id (i64) + lat (i32) + lon (i32).
#[derive(Clone, Copy)]
pub(crate) struct NodeTuple {
    pub id: i64,
    pub lat: i32,
    pub lon: i32,
}

/// Extract (id, lat, lon) tuples from decompressed PrimitiveBlock bytes.
///
/// Zero heap allocations per block — reads wire format inline, appends to caller's Vec.
/// The caller owns the Vec and can clear+reuse it across blocks.
///
/// Only parses DenseNodes (field 2 in PrimitiveGroup). Non-dense Node messages
/// (field 1) are skipped — all modern PBFs use dense encoding exclusively.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn extract_node_tuples(
    decompressed: &[u8],
    out: &mut Vec<NodeTuple>,
) -> Result<()> {
    use crate::read::wire::{Cursor, WireDenseNodes, PackedSint64Iter, WIRE_LEN, WIRE_VARINT};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    let mut granularity: i64 = 100;
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (1, WIRE_LEN) => { cursor.skip_field(wire_type)?; }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            (17, WIRE_VARINT) => { granularity = cursor.read_varint()? as i64; }
            (19, WIRE_VARINT) => { lat_offset = cursor.read_varint_i64()?; }
            (20, WIRE_VARINT) => { lon_offset = cursor.read_varint_i64()?; }
            _ => { cursor.skip_field(wire_type)?; }
        }
    }

    for &(off, len) in &group_starts {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);
        let mut dense_data: Option<&[u8]> = None;
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 2 && wire_type == WIRE_LEN {
                dense_data = Some(gcursor.read_len_delimited()?);
                break;
            }
            gcursor.skip_field(wire_type)?;
        }

        let Some(dd) = dense_data else { continue };
        let dense = WireDenseNodes::parse(dd)?;

        let mut ids = PackedSint64Iter::new(dense.id_data);
        let mut lats = PackedSint64Iter::new(dense.lat_data);
        let mut lons = PackedSint64Iter::new(dense.lon_data);
        let mut cum_id: i64 = 0;
        let mut cum_lat: i64 = 0;
        let mut cum_lon: i64 = 0;

        while let (Some(did), Some(dlat), Some(dlon)) = (ids.next(), lats.next(), lons.next()) {
            cum_id += did;
            cum_lat += dlat;
            cum_lon += dlon;
            out.push(NodeTuple {
                id: cum_id,
                lat: ((lat_offset + granularity * cum_lat) / 100) as i32,
                lon: ((lon_offset + granularity * cum_lon) / 100) as i32,
            });
        }
    }

    Ok(())
}
