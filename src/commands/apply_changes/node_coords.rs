//! Checked node-coordinate extraction for `--locations-on-ways`.
//!
//! Under `--locations-on-ways` every base node blob is decompressed so the
//! worker pool can collect the coordinates OSC ways reference. This module
//! is that extractor. It reads the decompressed `PrimitiveBlock` bytes
//! directly (no string table, no `PrimitiveBlock` construction) and covers
//! both node encodings the format allows:
//!
//! - **DenseNodes**: the DenseNodes message of each group, through
//!   `WireGroup::dense` - the accessor the element iterator uses, which
//!   refuses a group that repeats it.
//! - **Plain `Node` messages**: every `Node` of each group, through
//!   `WireGroup::nodes` - again the element iterator's accessor.
//!
//! Using the iterator's own accessors, in its order (a group's dense nodes,
//! then its plain nodes), makes the extracted node set the same set the
//! rewrite path sees and writes, and gives a duplicated id within one blob
//! the same winner the iterator would.
//!
//! All coordinate scaling is checked: granularity must be positive and at
//! most one degree per unit, `offset + granularity * raw` must not overflow
//! `i64`, and the decimicrodegree result must fit in `i32`. Dense delta
//! accumulation is modular, as in the decoder and the writer, so IDs that
//! wrap extract as the values every reader yields; a coordinate sum that
//! wraps lands at least 2^63 from the previous, accepted one, outside the
//! window the checked scaling accepts, so it is refused as before. Every
//! decoded node is validated before the caller decides whether it needs it,
//! so a corrupt coordinate is an error even on a node no OSC way references.
//! Group framing errors that the sub-message iterator cannot report are
//! caught by the strict `PrimitiveBlock` parse that always follows this
//! extraction on the same bytes.
//!
//! The wire scanner in `crate::scan::node` used to serve this role. It reads
//! only DenseNodes and errors on plain `Node` messages; at the time it also
//! stopped reading a group at its DenseNodes message, so a plain `Node` after
//! it in the same group was skipped without an error, and apply-changes
//! swallowed its errors, dropping every coordinate in such a blob without a
//! word.

use crate::error::{Error, Result, new_wire_error};
use crate::read::wire::{
    Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT, WireDenseNodes, WireGroup, WireNode,
};

/// Upper bound on `PrimitiveBlock.granularity`: one degree (1e9
/// nanodegrees) per raw unit. Nothing real comes close, and the bound keeps
/// `granularity * raw` inside `i64` for any plausible delta.
const MAX_GRANULARITY: i64 = 1_000_000_000;

fn overflow_err() -> Error {
    new_wire_error(
        "node coordinate arithmetic overflowed i64; the blob's granularity, \
         offset, or coordinate values are corrupt",
    )
}

/// `(offset + granularity * raw) / 100`, checked, narrowed to the
/// decimicrodegree `i32` the rest of the codebase carries.
fn decimicro(offset: i64, granularity: i64, raw: i64) -> Result<i32> {
    let nano = granularity
        .checked_mul(raw)
        .and_then(|scaled| offset.checked_add(scaled))
        .ok_or_else(overflow_err)?;
    i32::try_from(nano / 100).map_err(|_| {
        new_wire_error(
            "node coordinate does not fit in an i32 decimicrodegree; the blob's \
             granularity, offset, or coordinate values are corrupt",
        )
    })
}

fn checked_granularity(raw: u64) -> Result<i64> {
    let granularity = i64::try_from(raw).ok().filter(|g| *g > 0).ok_or_else(|| {
        new_wire_error(
            "PrimitiveBlock.granularity must be a positive int32; got a value that \
             is zero, negative, or too large",
        )
    })?;
    if granularity > MAX_GRANULARITY {
        return Err(new_wire_error(
            "PrimitiveBlock.granularity exceeds one degree per unit (1e9 nanodegrees); \
             refusing to decode coordinates from it",
        ));
    }
    Ok(granularity)
}

/// Decode `(id, lat, lon)` for every node in a decompressed
/// `PrimitiveBlock`, dense and plain, and hand each one to `sink`.
///
/// `groups` is caller-owned scratch reused across blobs. Top-level block
/// fields may follow the groups on the wire, so the groups are located
/// first and decoded once granularity and offsets are known.
pub(super) fn extract_node_coords<F: FnMut(i64, i32, i32)>(
    decompressed: &[u8],
    groups: &mut Vec<(usize, usize)>,
    mut sink: F,
) -> Result<()> {
    let mut cursor = Cursor::new(decompressed);
    let mut granularity: i64 = 100;
    let mut lat_offset: i64 = 0;
    let mut lon_offset: i64 = 0;
    groups.clear();

    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                groups.push((offset, data.len()));
            }
            (17, WIRE_VARINT) => granularity = checked_granularity(cursor.read_varint()?)?,
            (19, WIRE_VARINT) => lat_offset = cursor.read_varint_i64()?,
            (20, WIRE_VARINT) => lon_offset = cursor.read_varint_i64()?,
            _ => cursor.skip_field(wire_type)?,
        }
    }

    for &(offset, len) in groups.as_slice() {
        let group = WireGroup::new(&decompressed[offset..offset + len]);

        if let Some(dense_data) = group.dense()? {
            let dense = WireDenseNodes::parse(dense_data)?;
            let mut ids = PackedSint64Iter::new(dense.id_data);
            let mut lats = PackedSint64Iter::new(dense.lat_data);
            let mut lons = PackedSint64Iter::new(dense.lon_data);
            let mut id: i64 = 0;
            let mut lat: i64 = 0;
            let mut lon: i64 = 0;
            while let (Some(did), Some(dlat), Some(dlon)) = (ids.next(), lats.next(), lons.next()) {
                // Modulo 2^64, as the decoder and writer sum deltas; see the
                // module docs for why a wrapped coordinate is still refused.
                id = id.wrapping_add(did);
                lat = lat.wrapping_add(dlat);
                lon = lon.wrapping_add(dlon);
                sink(
                    id,
                    decimicro(lat_offset, granularity, lat)?,
                    decimicro(lon_offset, granularity, lon)?,
                );
            }
        }

        for data in group.nodes() {
            let node = WireNode::parse(data)?;
            sink(
                node.id,
                decimicro(lat_offset, granularity, node.lat)?,
                decimicro(lon_offset, granularity, node.lon)?,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::extract_node_coords;

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

    fn packed_sint(field: u32, values: &[i64], out: &mut Vec<u8>) {
        let mut seg = Vec::new();
        for &v in values {
            varint(zz(v), &mut seg);
        }
        len_delimited(field, &seg, out);
    }

    fn sint(field: u32, value: i64, out: &mut Vec<u8>) {
        varint(u64::from(field << 3), out);
        varint(zz(value), out);
    }

    fn uint(field: u32, value: u64, out: &mut Vec<u8>) {
        varint(u64::from(field << 3), out);
        varint(value, out);
    }

    /// DenseNodes message body from absolute values (delta-encoded here).
    fn dense(nodes: &[(i64, i64, i64)]) -> Vec<u8> {
        let delta = |pick: fn(&(i64, i64, i64)) -> i64| {
            let mut prev = 0;
            nodes
                .iter()
                .map(|n| {
                    let v = pick(n);
                    let d = v - prev;
                    prev = v;
                    d
                })
                .collect::<Vec<_>>()
        };
        let mut out = Vec::new();
        packed_sint(1, &delta(|n| n.0), &mut out);
        packed_sint(8, &delta(|n| n.1), &mut out);
        packed_sint(9, &delta(|n| n.2), &mut out);
        out
    }

    /// Plain Node message body.
    fn plain(id: i64, lat: i64, lon: i64) -> Vec<u8> {
        let mut out = Vec::new();
        sint(1, id, &mut out);
        sint(8, lat, &mut out);
        sint(9, lon, &mut out);
        out
    }

    /// A PrimitiveBlock with an empty string table, the given group bodies
    /// and optional trailing granularity.
    fn block(groups: &[Vec<u8>], granularity: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        len_delimited(1, &[], &mut out);
        for g in groups {
            len_delimited(2, g, &mut out);
        }
        if let Some(g) = granularity {
            uint(17, g, &mut out);
        }
        out
    }

    fn extract(bytes: &[u8]) -> crate::error::Result<Vec<(i64, i32, i32)>> {
        let mut out = Vec::new();
        let mut groups = Vec::new();
        extract_node_coords(bytes, &mut groups, |id, lat, lon| out.push((id, lat, lon)))?;
        Ok(out)
    }

    #[test]
    fn dense_nodes_decode() {
        let mut group = Vec::new();
        len_delimited(2, &dense(&[(1, 10, 20), (5, -30, 40)]), &mut group);
        let got = extract(&block(&[group], None)).unwrap();
        assert_eq!(got, vec![(1, 10, 20), (5, -30, 40)]);
    }

    #[test]
    fn plain_nodes_decode() {
        let mut group = Vec::new();
        len_delimited(1, &plain(7, 100, -200), &mut group);
        len_delimited(1, &plain(9, 300, 400), &mut group);
        let got = extract(&block(&[group], None)).unwrap();
        assert_eq!(got, vec![(7, 100, -200), (9, 300, 400)]);
    }

    /// A plain Node after DenseNodes in one group is decoded, and the
    /// dense nodes come first, matching the element iterator's order.
    #[test]
    fn mixed_group_decodes_dense_then_plain() {
        let mut group = Vec::new();
        len_delimited(1, &plain(3, 1, 2), &mut group);
        len_delimited(2, &dense(&[(1, 10, 20)]), &mut group);
        len_delimited(1, &plain(4, 5, 6), &mut group);
        let got = extract(&block(&[group], None)).unwrap();
        assert_eq!(got, vec![(1, 10, 20), (3, 1, 2), (4, 5, 6)]);
    }

    /// Granularity scales plain-node coordinates and may follow the groups.
    #[test]
    fn granularity_after_groups_applies() {
        let mut group = Vec::new();
        len_delimited(1, &plain(1, 3, 4), &mut group);
        let got = extract(&block(&[group], Some(1000))).unwrap();
        assert_eq!(got, vec![(1, 30, 40)]);
    }

    #[test]
    fn zero_and_huge_granularity_are_rejected() {
        let mut group = Vec::new();
        len_delimited(1, &plain(1, 3, 4), &mut group);
        assert!(extract(&block(std::slice::from_ref(&group), Some(0))).is_err());
        assert!(extract(&block(&[group], Some(2_000_000_000))).is_err());
    }

    #[test]
    fn plain_coordinate_outside_i32_is_rejected() {
        let mut group = Vec::new();
        len_delimited(1, &plain(1, i64::from(i32::MAX) + 1, 0), &mut group);
        assert!(extract(&block(&[group], None)).is_err());
    }

    /// Dense deltas sum modulo 2^64, as the decoder does: IDs wrap to the
    /// values every reader yields, and a coordinate far enough out to wrap is
    /// refused by the checked scaling instead.
    #[test]
    fn dense_id_deltas_wrap_and_wild_coordinates_are_rejected() {
        let mut body = Vec::new();
        packed_sint(1, &[i64::MAX, 1], &mut body);
        packed_sint(8, &[0, 0], &mut body);
        packed_sint(9, &[0, 0], &mut body);
        let mut group = Vec::new();
        len_delimited(2, &body, &mut group);
        let got = extract(&block(&[group], None)).unwrap();
        assert_eq!(got, vec![(i64::MAX, 0, 0), (i64::MIN, 0, 0)]);

        let mut body = Vec::new();
        packed_sint(1, &[1, 1], &mut body);
        packed_sint(8, &[i64::MAX, 1], &mut body);
        packed_sint(9, &[0, 0], &mut body);
        let mut group = Vec::new();
        len_delimited(2, &body, &mut group);
        assert!(extract(&block(&[group], None)).is_err());
    }
}
