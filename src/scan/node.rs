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
//! Skipping construction also skips `validate_groups_strict`, so the dense
//! coordinate checks it makes (equal-length, well-formed `id`/`lat`/`lon`
//! arrays) are made here instead, during the decode itself; see
//! `scan_dense_coords`.
//!
//! # Known limitations
//!
//! - **Sorted PBF assumption.** The indexdata-based blob skip (`ElementKind::Node` check)
//!   relies on each blob containing exactly one element type, which is guaranteed by
//!   `Sort.Type_then_ID`. Mixed-type blobs in unsorted PBFs could be mislabeled by
//!   indexdata, causing nodes in a mislabeled blob to be skipped.

use crate::ElementKind;
use crate::element_kind::KindSet;
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

/// Error for a dense `id` array that ends mid-varint or holds a varint wider
/// than 64 bits. Out of line: only a corrupt block reaches it.
#[cold]
#[inline(never)]
fn dense_id_err() -> crate::error::Error {
    crate::error::new_wire_error("dense group id array holds a truncated or overlong varint")
}

/// Error for a dense `lat` or `lon` array that is shorter than the `id`
/// array or holds a truncated or overlong varint. A failed read cannot tell
/// those apart cheaply (a truncated final varint also leaves the cursor
/// empty), and both mean the same thing here: the group's coordinate
/// arrays do not describe its ids.
#[cold]
#[inline(never)]
fn dense_coord_err() -> crate::error::Error {
    crate::error::new_wire_error(
        "dense group lat/lon array is shorter than its id array or holds a \
         truncated or overlong varint",
    )
}

/// Error for a dense `lat` or `lon` array with values left over after the
/// last id.
#[cold]
#[inline(never)]
fn dense_mismatch_err() -> crate::error::Error {
    crate::error::new_wire_error("dense group has mismatched id/lat/lon array lengths")
}

/// Decode one dense group's `id`, `lat` and `lon` arrays in lockstep into
/// `sink`, refusing any group whose arrays are not three complete varint
/// streams of equal length.
///
/// This scanner bypasses `PrimitiveBlock` construction, so the checks
/// `validate_groups_strict` makes on these three arrays have to happen here.
/// They cost no extra pass: each value is read with `Cursor::read_varint`,
/// the same decoder `PackedSint64Iter` wraps, but its error is propagated
/// instead of being turned into an end of iteration. A successful read
/// consumes exactly one complete varint, so `n` successful lockstep reads
/// followed by three empty cursors is exactly "three well-formed arrays of
/// `n` values each":
///
/// - an array that ends mid-varint, or holds a varint past 64 bits, fails
///   its read;
/// - a `lat` or `lon` array shorter than `id` fails a read on an empty
///   cursor;
/// - a `lat` or `lon` array longer than `id` is left non-empty.
///
/// The previous `while let (Some(..), Some(..), Some(..))` over three
/// `PackedSint64Iter`s stopped at the shortest array and at the first
/// malformed varint, so such a group scanned to fewer nodes with no error
/// and the node index built from it silently missed them.
///
/// Failure is streaming, not atomic: nodes before the malformed value have
/// already reached `sink` when the error returns. Every caller propagates
/// the error and abandons the operation, so that partial output is never
/// used.
///
/// Only the coordinate streams are validated. `keys_vals` and `denseinfo`
/// are never read by this scanner, so their contents cannot change its
/// output, and they are not checked here.
#[inline]
fn scan_dense_coords(
    dense: &crate::read::wire::WireDenseNodes<'_>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
    sink: &mut impl FnMut(i64, i32, i32),
) -> Result<()> {
    use crate::read::wire::Cursor;
    use protohoggr::zigzag_decode_64;

    let mut ids = Cursor::new(dense.id_data);
    let mut lats = Cursor::new(dense.lat_data);
    let mut lons = Cursor::new(dense.lon_data);
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
    while !ids.is_empty() {
        let did = zigzag_decode_64(ids.read_varint().map_err(|_| dense_id_err())?);
        let dlat = zigzag_decode_64(lats.read_varint().map_err(|_| dense_coord_err())?);
        let dlon = zigzag_decode_64(lons.read_varint().map_err(|_| dense_coord_err())?);
        cum_id = cum_id.wrapping_add(did);
        cum_lat = cum_lat.wrapping_add(dlat);
        cum_lon = cum_lon.wrapping_add(dlon);
        sink(
            cum_id,
            decimicro(lat_offset, granularity, cum_lat)?,
            decimicro(lon_offset, granularity, cum_lon)?,
        );
    }
    if !lats.is_empty() || !lons.is_empty() {
        return Err(dense_mismatch_err());
    }
    Ok(())
}

/// Extract (id, lat, lon) tuples from decompressed PrimitiveBlock bytes.
///
/// Zero heap allocations per block - reads wire format inline, appends to caller's Vec.
/// The caller owns the Vec and can clear+reuse it across blocks.
///
/// Reads both node encodings, dense first within each group. Returns the
/// element kinds the block holds, as [`extract_node_coords`] does.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn extract_node_tuples(
    decompressed: &[u8],
    out: &mut Vec<NodeTuple>,
    group_starts: &mut Vec<(usize, usize)>,
) -> Result<KindSet> {
    extract_node_coords(decompressed, group_starts, |id, lat, lon| {
        out.push(NodeTuple { id, lat, lon });
    })
}

/// Shared checked coordinate scanner; caller-owned group scratch is reused.
///
/// Returns the kinds the block holds, recorded from the group field tags
/// this walk reads anyway under the crate's `KindSet` rule: a plain node, a
/// dense message with at least one ID, a way, a relation. Empty groups, an
/// empty dense message and changeset groups add nothing. A caller with an
/// indexdata kind claim compares it with `KindSet::foreign_to`; others may
/// ignore the set.
pub(crate) fn extract_node_coords(
    decompressed: &[u8],
    group_starts: &mut Vec<(usize, usize)>,
    mut sink: impl FnMut(i64, i32, i32),
) -> Result<KindSet> {
    use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT, WireDenseNodes, WireGroup, WireNode};

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
                if granularity > crate::tunables::MAX_NODE_GRANULARITY {
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

    let mut kinds = KindSet::default();
    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);
        let mut dense_data: Option<&[u8]> = None;
        let mut has_plain = false;
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => {
                    has_plain = true;
                    kinds.add(ElementKind::Node);
                    gcursor.skip_field(wire_type)?;
                }
                (3, WIRE_LEN) => {
                    kinds.add(ElementKind::Way);
                    gcursor.skip_field(wire_type)?;
                }
                (4, WIRE_LEN) => {
                    kinds.add(ElementKind::Relation);
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
            if !dense.id_data.is_empty() {
                kinds.add(ElementKind::Node);
            }
            scan_dense_coords(&dense, granularity, lat_offset, lon_offset, &mut sink)?;
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

    Ok(kinds)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ElementKind, KindSet, NodeTuple, extract_node_tuples};

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

    /// A PrimitiveBlock with one DenseNodes group whose `id` (1), `lat` (8)
    /// and `lon` (9) packed payloads are given as raw bytes, so a test can
    /// write a truncated or overlong varint.
    fn dense_block_raw(ids: &[u8], lats: &[u8], lons: &[u8]) -> Vec<u8> {
        let mut dense = Vec::new();
        len_delimited(1, ids, &mut dense);
        len_delimited(8, lats, &mut dense);
        len_delimited(9, lons, &mut dense);
        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        let mut block = Vec::new();
        len_delimited(2, &group, &mut block);
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

    fn kinds_of(block: &[u8]) -> KindSet {
        extract_node_tuples(block, &mut Vec::new(), &mut Vec::new()).unwrap()
    }

    fn set(kinds: &[ElementKind]) -> KindSet {
        let mut set = KindSet::default();
        for &kind in kinds {
            set.add(kind);
        }
        set
    }

    fn block_of_groups(groups: &[Vec<u8>]) -> Vec<u8> {
        let mut block = Vec::new();
        for group in groups {
            len_delimited(2, group, &mut block);
        }
        block
    }

    /// The recorded kind set follows the shared `KindSet` rule exactly: a
    /// plain node, a dense message with an ID, a way and a relation count;
    /// empty groups, an empty dense message and changeset groups do not.
    #[test]
    fn recorded_kinds_follow_the_shared_rule() {
        let mut node_payload = Vec::new();
        scalar(1, 14, &mut node_payload);
        let mut dense_with_ids = Vec::new();
        packed(1, &[1], &mut dense_with_ids);
        packed(8, &[0], &mut dense_with_ids);
        packed(9, &[0], &mut dense_with_ids);
        let mut dense_empty_arrays = Vec::new();
        packed(1, &[], &mut dense_empty_arrays);
        packed(8, &[], &mut dense_empty_arrays);
        packed(9, &[], &mut dense_empty_arrays);

        let group_with = |field: u32, payload: &[u8]| {
            let mut group = Vec::new();
            len_delimited(field, payload, &mut group);
            group
        };

        assert_eq!(kinds_of(&[]), KindSet::default());
        assert_eq!(
            kinds_of(&block_of_groups(&[Vec::new()])),
            KindSet::default(),
            "an empty group adds nothing"
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(2, &[])])),
            KindSet::default(),
            "an empty dense message adds nothing"
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(2, &dense_empty_arrays)])),
            KindSet::default(),
            "a dense message with empty arrays adds nothing"
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(5, &[0x08, 0x01])])),
            KindSet::default(),
            "a changeset group adds nothing"
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(2, &dense_with_ids)])),
            set(&[ElementKind::Node])
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(1, &node_payload)])),
            set(&[ElementKind::Node])
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(3, &[0x08, 0x01])])),
            set(&[ElementKind::Way])
        );
        assert_eq!(
            kinds_of(&block_of_groups(&[group_with(4, &[0x08, 0x01])])),
            set(&[ElementKind::Relation])
        );

        // A foreign kind in a node group, and kinds across groups.
        let mut mixed = group_with(2, &dense_with_ids);
        len_delimited(3, &[0x08, 0x01], &mut mixed);
        let kinds = kinds_of(&block_of_groups(&[mixed, group_with(4, &[0x08, 0x01])]));
        assert_eq!(
            kinds,
            set(&[ElementKind::Node, ElementKind::Way, ElementKind::Relation])
        );
        assert_eq!(kinds.foreign_to(ElementKind::Node), Some(ElementKind::Way));
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

    /// BUG-183: the dense loop used to zip three infallible packed iterators
    /// and stop at the shortest, so every shape below scanned to fewer nodes
    /// with `Ok`. Each malformed group now fails, and each refusal names the
    /// array at fault. Zigzag `0x02` is delta 1.
    #[test]
    fn malformed_dense_coordinate_arrays_are_refused_not_truncated() {
        let ten_byte_overflow: Vec<u8> = [0x80; 9].iter().copied().chain([0x02]).collect();
        let mut overflow_tail = vec![0x02];
        overflow_tail.extend_from_slice(&ten_byte_overflow);
        // (what, id bytes, lat bytes, lon bytes, expected error substring)
        type Case<'a> = (&'a str, &'a [u8], &'a [u8], &'a [u8], &'a str);
        let cases: [Case<'_>; 10] = [
            (
                "lat tenth byte overflows",
                &[2],
                &ten_byte_overflow,
                &[0],
                "lat/lon array",
            ),
            (
                "lon tenth byte overflows",
                &[2],
                &[0],
                &ten_byte_overflow,
                "lat/lon array",
            ),
            (
                "lat shorter than id",
                &[2, 2],
                &[2],
                &[2, 2],
                "lat/lon array is shorter",
            ),
            (
                "lon shorter than id",
                &[2, 2],
                &[2, 2],
                &[2],
                "lat/lon array is shorter",
            ),
            (
                "lat longer than id",
                &[2],
                &[2, 2],
                &[2],
                "mismatched id/lat/lon",
            ),
            ("lat without ids", &[], &[2], &[], "mismatched id/lat/lon"),
            // Equal terminal-byte counts, so only the truncation is wrong.
            (
                "id ends mid-varint",
                &[2, 0x80],
                &[2],
                &[2],
                "id array holds a truncated",
            ),
            (
                "lon ends mid-varint",
                &[2, 2],
                &[2, 2],
                &[2, 0x80],
                "lat/lon array is shorter",
            ),
            (
                "id tenth byte overflows",
                &ten_byte_overflow,
                &[0],
                &[0],
                "id array",
            ),
            // All three arrays end in the same overflowing varint: no length
            // disagreement is left to notice, only the decode failure.
            (
                "every array overflows last",
                &overflow_tail,
                &overflow_tail,
                &overflow_tail,
                "id array",
            ),
        ];
        for (what, ids, lats, lons, want) in cases {
            let err = scan(&dense_block_raw(ids, lats, lons))
                .expect_err(&format!("{what}: a malformed dense group must be refused"));
            assert!(
                err.to_string().contains(want),
                "{what}: expected {want:?}. Got: {err}"
            );
        }

        // Well-formed counterparts: the checks are bounds, not bans. A legal
        // ten-byte varint (u64::MAX, zigzag delta i64::MIN) decodes, and
        // three empty arrays are a group of zero nodes.
        let ten_byte_max: Vec<u8> = [0xff; 9].iter().copied().chain([0x01]).collect();
        let tuples = scan(&dense_block_raw(&ten_byte_max, &[0], &[0])).unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].id, i64::MIN);
        assert!(scan(&dense_block_raw(&[], &[], &[])).unwrap().is_empty());
        let tuples = scan(&dense_block_raw(&[2, 2], &[2, 2], &[2, 2])).unwrap();
        let ids: Vec<i64> = tuples.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 2]);
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
