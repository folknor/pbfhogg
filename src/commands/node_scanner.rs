//! Node-only wire-format scanner for extracting (id, lat, lon) from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction entirely — no string table parsing,
//! no group_ranges allocation, no UTF-8 validation. This eliminates the cross-thread
//! alloc/free retention problem that causes 25+ GB heap accumulation at Europe/planet
//! scale when using the pipelined reader.
//!
//! Used by:
//! - External join stage 2 (inline variant for interleaved merge-join)
//! - ALTW dense pass 1 (node index build)
//! - ALTW sparse pass 1 (node index build)
//!
//! # Known limitations
//!
//! - **DenseNodes only.** Only parses PrimitiveGroup field 2 (DenseNodes). Non-dense
//!   Node messages (field 1) are silently skipped. All modern PBF writers (osmium,
//!   pbfhogg, Planetiler, osm2pgsql) use dense encoding exclusively. Pre-2012 PBFs
//!   or hand-crafted test files may use non-dense nodes — those would produce missing
//!   coordinates without error.
//!
//! - **Sorted PBF assumption.** The indexdata-based blob skip (`ElemKind::Node` check)
//!   relies on each blob containing exactly one element type, which is guaranteed by
//!   `Sort.Type_then_ID`. Mixed-type blobs in unsorted PBFs could be mislabeled by
//!   indexdata, causing nodes in a mislabeled blob to be skipped.
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

#[derive(Clone, Copy)]
pub(crate) struct DenseNodeScanMeta {
    pub granularity: i64,
    pub lat_offset: i64,
    pub lon_offset: i64,
}

/// Scan PrimitiveBlock metadata and collect PrimitiveGroup byte ranges that
/// may contain DenseNodes. Returns the block-level coordinate metadata needed
/// to decode the delta streams later.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn scan_dense_group_ranges(
    decompressed: &[u8],
    group_starts: &mut Vec<(usize, usize)>,
) -> Result<DenseNodeScanMeta> {
    use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    let mut granularity: i64 = 100;
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;
    group_starts.clear();

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

    Ok(DenseNodeScanMeta {
        granularity,
        lat_offset,
        lon_offset,
    })
}

/// Visit `(id, lat, lon)` tuples from DenseNodes groups without
/// materializing a `Vec<NodeTuple>`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn visit_dense_node_tuples<F>(
    decompressed: &[u8],
    group_starts: &[(usize, usize)],
    meta: DenseNodeScanMeta,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(i64, i32, i32),
{
    use crate::read::wire::{Cursor, PackedSint64Iter, WireDenseNodes, WIRE_LEN};

    let buffer = decompressed;

    for &(off, len) in group_starts {
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
            visit(
                cum_id,
                ((meta.lat_offset + meta.granularity * cum_lat) / 100) as i32,
                ((meta.lon_offset + meta.granularity * cum_lon) / 100) as i32,
            );
        }
    }

    Ok(())
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
    group_starts: &mut Vec<(usize, usize)>,
) -> Result<()> {
    out.clear();
    let meta = scan_dense_group_ranges(decompressed, group_starts)?;
    visit_dense_node_tuples(decompressed, group_starts, meta, |id, lat, lon| {
        out.push(NodeTuple { id, lat, lon });
    })
}
