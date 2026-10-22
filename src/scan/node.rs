//! Node-only wire-format scanner for extracting (id, lat, lon) from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction entirely - no string table parsing,
//! no group_ranges allocation, no UTF-8 validation. This eliminates the cross-thread
//! alloc/free retention problem that causes 25+ GB heap accumulation at Europe/planet
//! scale when using the pipelined reader.
//!
//! Used by:
//! - External join stage 2 (inline variant for interleaved merge-join)
//! - ALTW sparse pass 1 (node index build)
//!
//! Dense and plain nodes share checked coordinate scaling. Dense-only groups
//! retain their single group walk; a second walk is needed only when a group
//! contains plain nodes, to preserve the element iterator's dense-first order.
//!
//! # Known limitations
//!
//! - **Sorted PBF assumption.** The indexdata-based blob skip (`ElemKind::Node` check)
//!   relies on each blob containing exactly one element type, which is guaranteed by
//!   `Sort.Type_then_ID`. Mixed-type blobs in unsorted PBFs could be mislabeled by
//!   indexdata, causing nodes in a mislabeled blob to be skipped.

use crate::error::Result;

/// Compact node coordinate tuple. 16 bytes - id (i64) + lat (i32) + lon (i32).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeTuple {
    pub id: i64,
    pub lat: i32,
    pub lon: i32,
}

/// Error for coordinate scaling (`offset + granularity * raw`) that ran past
/// `i64`.
fn overflow_err() -> crate::error::Error {
    crate::error::new_wire_error(
        "node coordinate arithmetic overflowed i64; the blob's \
         granularity, offset, or coordinate deltas are corrupt",
    )
}

/// `(offset + granularity * raw) / 100`, in checked arithmetic, narrowed to
/// the decimicrodegree `i32` the rest of the codebase carries.
///
/// The narrowing is checked rather than truncating: `as i32` on an
/// out-of-range value silently produces a plausible-looking coordinate
/// somewhere else on Earth, which is exactly the class of silent corruption
/// this scanner must not manufacture from untrusted metadata.
fn decimicro(offset: i64, granularity: i64, raw: i64) -> Result<i32> {
    let nano = granularity
        .checked_mul(raw)
        .and_then(|scaled| offset.checked_add(scaled))
        .ok_or_else(overflow_err)?;
    i32::try_from(nano / 100).map_err(|_| {
        crate::error::new_wire_error(
            "node coordinate does not fit in an i32 decimicrodegree; \
             the blob's granularity, offset, or deltas are corrupt",
        )
    })
}

/// Extract (id, lat, lon) tuples from decompressed PrimitiveBlock bytes.
///
/// Zero heap allocations per block - reads wire format inline, appends to caller's Vec.
/// The caller owns the Vec and can clear+reuse it across blocks.
///
/// Reads both node encodings, dense first within each group.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn extract_node_tuples(
    decompressed: &[u8],
    out: &mut Vec<NodeTuple>,
    group_starts: &mut Vec<(usize, usize)>,
) -> Result<()> {
    extract_node_coords(decompressed, group_starts, |id, lat, lon| {
        out.push(NodeTuple { id, lat, lon });
    })
}

/// Shared checked coordinate scanner; caller-owned group scratch is reused.
pub(crate) fn extract_node_coords(
    decompressed: &[u8],
    group_starts: &mut Vec<(usize, usize)>,
    mut sink: impl FnMut(i64, i32, i32),
) -> Result<()> {
    use crate::read::wire::{
        Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT, WireDenseNodes, WireGroup, WireNode,
    };

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    let mut granularity: i64 = 100;
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;
    group_starts.clear();

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                cursor.skip_field(wire_type)?;
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            (17, WIRE_VARINT) => {
                // `granularity` is attacker-controlled metadata: it comes off
                // the wire before any element is decoded and is then
                // multiplied by every node's accumulated delta below. An
                // unvalidated read (`as i64`) turns a varint above
                // `i64::MAX` into a negative granularity and a huge one into
                // a multiplication that panics in debug and wraps in
                // release. Bound it here, the way
                // `blob_meta/scan_ids.rs::scan_dense_nodes` already does for
                // the same three fields. The spec's unit is nanodegrees, so
                // a full circle is 3.6e11 nanodegrees; anything above one
                // degree per raw unit is not a real producer and the
                // upper bound keeps `granularity * cum` inside i64 for any
                // plausible delta.
                let raw = cursor.read_varint()?;
                granularity = i64::try_from(raw).ok().filter(|g| *g > 0).ok_or_else(|| {
                    crate::error::new_wire_error(
                        "PrimitiveBlock.granularity must be a positive int32; \
                         got a value that is zero, negative, or too large",
                    )
                })?;
                if granularity > 1_000_000_000 {
                    return Err(crate::error::new_wire_error(
                        "PrimitiveBlock.granularity exceeds one degree per unit \
                         (1e9 nanodegrees); refusing to decode coordinates from it",
                    ));
                }
            }
            (19, WIRE_VARINT) => {
                lat_offset = cursor.read_varint_i64()?;
            }
            (20, WIRE_VARINT) => {
                lon_offset = cursor.read_varint_i64()?;
            }
            _ => {
                cursor.skip_field(wire_type)?;
            }
        }
    }

    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);
        let mut dense_data: Option<&[u8]> = None;
        let mut has_plain = false;
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => {
                    has_plain = true;
                    gcursor.skip_field(wire_type)?;
                }
                (2, WIRE_LEN) => {
                    // A repeated DenseNodes would merge in protobuf; the
                    // decoder refuses it (`WireGroup::dense`), and reading
                    // only the first would silently drop the rest.
                    if dense_data.is_some() {
                        return Err(crate::error::new_wire_error(
                            crate::read::wire::DUP_GROUP_DENSE,
                        ));
                    }
                    dense_data = Some(gcursor.read_len_delimited()?);
                }
                _ => {
                    gcursor.skip_field(wire_type)?;
                }
            }
        }

        if let Some(dd) = dense_data {
            let dense = WireDenseNodes::parse(dd)?;

            let mut ids = PackedSint64Iter::new(dense.id_data);
            let mut lats = PackedSint64Iter::new(dense.lat_data);
            let mut lons = PackedSint64Iter::new(dense.lon_data);
            let mut cum_id: i64 = 0;
            let mut cum_lat: i64 = 0;
            let mut cum_lon: i64 = 0;

            // Deltas sum modulo 2^64, as the decoder and the writer do, so a
            // block whose IDs wrap (the writer encodes `i64::MIN` then
            // `i64::MAX` as deltas `MIN`, `-1`) scans to the IDs every reader
            // yields. A wrapped coordinate cannot slip through: a sum that wraps
            // lands at least 2^63 away from the previous, accepted coordinate,
            // far outside the window `decimicro` accepts for any granularity and
            // offset, so it is refused exactly where a checked sum was.
            while let (Some(did), Some(dlat), Some(dlon)) = (ids.next(), lats.next(), lons.next()) {
                cum_id = cum_id.wrapping_add(did);
                cum_lat = cum_lat.wrapping_add(dlat);
                cum_lon = cum_lon.wrapping_add(dlon);
                sink(
                    cum_id,
                    decimicro(lat_offset, granularity, cum_lat)?,
                    decimicro(lon_offset, granularity, cum_lon)?,
                );
            }
        }
        if has_plain {
            for data in WireGroup::new(group_data).nodes() {
                let node = WireNode::parse(data)?;
                sink(
                    node.id,
                    decimicro(lat_offset, granularity, node.lat)?,
                    decimicro(lon_offset, granularity, node.lon)?,
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{NodeTuple, extract_node_tuples};

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            #[allow(clippy::cast_possible_truncation)]
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    #[allow(clippy::cast_sign_loss)]
    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from((field << 3) | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn packed(field: u32, deltas: &[i64], out: &mut Vec<u8>) {
        let mut seg = Vec::new();
        for &d in deltas {
            varint(zz(d), &mut seg);
        }
        len_delimited(field, &seg, out);
    }

    fn scalar(field: u32, value: i64, out: &mut Vec<u8>) {
        varint(u64::from(field << 3), out);
        #[allow(clippy::cast_sign_loss)]
        varint(value as u64, out);
    }

    /// A PrimitiveBlock with one DenseNodes group, optionally carrying
    /// explicit `granularity` (17) / `lat_offset` (19) / `lon_offset` (20).
    /// Values are deltas, exactly as they sit on the wire.
    fn dense_block(ids: &[i64], lats: &[i64], lons: &[i64], scalars: &[(u32, i64)]) -> Vec<u8> {
        let mut dense = Vec::new();
        packed(1, ids, &mut dense);
        packed(8, lats, &mut dense);
        packed(9, lons, &mut dense);

        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);

        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        for &(field, value) in scalars {
            scalar(field, value, &mut block);
        }
        block
    }

    fn scan(block: &[u8]) -> super::Result<Vec<NodeTuple>> {
        let mut out = Vec::new();
        let mut groups = Vec::new();
        extract_node_tuples(block, &mut out, &mut groups)?;
        Ok(out)
    }

    #[test]
    fn plain_and_mixed_groups_use_checked_scaling_and_dense_first_order() {
        let mut plain = Vec::new();
        scalar(1, 14, &mut plain); // zigzag ID 7
        scalar(8, 6, &mut plain); // raw latitude 3
        scalar(9, 7, &mut plain); // raw longitude -4
        let mut dense = Vec::new();
        packed(1, &[1], &mut dense);
        packed(8, &[2], &mut dense);
        packed(9, &[5], &mut dense);
        let mut group = Vec::new();
        len_delimited(1, &plain, &mut group);
        len_delimited(2, &dense, &mut group);
        len_delimited(1, &plain, &mut group);
        let mut block = Vec::new();
        len_delimited(2, &group, &mut block);
        scalar(17, 1000, &mut block);
        scalar(19, 100, &mut block);
        scalar(20, -100, &mut block);
        let got = scan(&block).unwrap();
        let coords: Vec<_> = got.iter().map(|n| (n.id, n.lat, n.lon)).collect();
        assert_eq!(coords, [(1, 21, 49), (7, 31, -41), (7, 31, -41)]);

        let mut plain_only = Vec::new();
        len_delimited(1, &plain, &mut plain_only);
        let mut block = Vec::new();
        len_delimited(2, &plain_only, &mut block);
        assert_eq!(scan(&block).unwrap()[0].id, 7);
        scalar(19, i64::MAX, &mut block);
        assert!(scan(&block).is_err());
    }

    /// Baseline: the default framing decodes to the decimicrodegrees the
    /// rest of the codebase carries. Asserted on the decoded integers
    /// themselves, not on a round trip through an encoder.
    #[test]
    fn default_framing_decodes_to_decimicrodegrees() {
        let tuples = scan(&dense_block(&[1], &[550_000_000], &[120_000_000], &[])).unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(
            (tuples[0].id, tuples[0].lat, tuples[0].lon),
            (1, 550_000_000, 120_000_000)
        );
    }

    /// `granularity` arrives from the wire before any element is decoded and
    /// is then multiplied by every accumulated delta. Zero is nonsensical,
    /// and a varint above `i64::MAX` used to become a negative granularity
    /// via a bare `as i64` - both then flowed into an unchecked
    /// multiplication that panics in debug and wraps in release.
    #[test]
    fn hostile_granularity_is_refused_not_multiplied() {
        for granularity in [0_i64, 4_000_000_000, i64::MAX] {
            let block = dense_block(&[1], &[1], &[1], &[(17, granularity)]);
            let err =
                scan(&block).expect_err("a granularity outside the sane range must be refused");
            assert!(
                err.to_string().contains("granularity"),
                "expected the granularity guard specifically. Got: {err}"
            );
        }

        // -1 written as a varint is ten 0xFF-ish bytes, which `as i64` turned
        // into a negative granularity. Same guard must catch it.
        let err = scan(&dense_block(&[1], &[1], &[1], &[(17, -1)]))
            .expect_err("a negative granularity must be refused");
        assert!(
            err.to_string().contains("granularity"),
            "expected the granularity guard specifically. Got: {err}"
        );

        // Passing counterpart: a legal non-default granularity still decodes,
        // so the guard is a bound and not a ban. At 1000 nanodegrees per unit
        // a raw 55_000_000 is 55 degrees = 550_000_000 decimicrodegrees.
        let tuples = scan(&dense_block(
            &[1],
            &[55_000_000],
            &[12_000_000],
            &[(17, 1000)],
        ))
        .unwrap();
        assert_eq!((tuples[0].lat, tuples[0].lon), (550_000_000, 120_000_000));
    }

    /// The delta accumulators sum modulo 2^64, like the decoder's and the
    /// writer's: IDs that wrap scan to the same values every reader yields,
    /// while a coordinate far enough out to wrap is still refused, by the
    /// checked scaling.
    #[test]
    fn id_deltas_wrap_and_wild_coordinates_are_refused() {
        // Deltas MIN then -1: the writer's encoding of IDs MIN then MAX.
        let tuples = scan(&dense_block(&[i64::MIN, -1], &[0, 0], &[0, 0], &[])).unwrap();
        let ids: Vec<i64> = tuples.iter().map(|t| t.id).collect();
        assert_eq!(ids, [i64::MIN, i64::MAX]);

        let err = scan(&dense_block(&[1, 2], &[i64::MAX, i64::MAX], &[0, 0], &[]))
            .expect_err("a coordinate outside the scalable range must be refused");
        assert!(
            err.to_string().contains("overflow"),
            "expected an overflow guard. Got: {err}"
        );
    }

    /// A coordinate that does not fit an i32 decimicrodegree used to be
    /// truncated by `as i32` into a plausible coordinate somewhere else on
    /// Earth. `lat_offset` reaches that from metadata alone.
    #[test]
    fn out_of_range_coordinate_is_refused_not_truncated() {
        let block = dense_block(&[1], &[0], &[0], &[(19, 900_000_000_000)]);
        let err = scan(&block).expect_err("an unrepresentable coordinate must be refused");
        assert!(
            err.to_string().contains("does not fit in an i32"),
            "expected the narrowing guard specifically. Got: {err}"
        );
    }
}
