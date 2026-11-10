//! Way-ref-only wire-format scanner for extracting way node references from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction - no string table parsing,
//! no group_ranges allocation. Only extracts way IDs and their node ref lists.
//!
//! Used by passes that only need `way.id()` + `way.refs()`, such as ALTW
//! pass 0 (`collect_way_referenced_node_ids`).
//!
//! # Known limitations
//!
//! - **Way groups only.** Parses PrimitiveGroup field 3 (Way). Other element
//!   types (nodes, relations) in the same group are skipped.
//! - **Sorted PBF assumption.** Relies on indexdata `ElementKind::Way` for blob
//!   filtering. Mixed-type blobs in unsorted PBFs could be mislabeled.

use crate::ElementKind;
use crate::element_kind::KindSet;
use crate::error::Result;

/// Extract way IDs and their node refs from decompressed PrimitiveBlock bytes.
///
/// For each way, calls `callback(way_id, &refs)` where refs is the decoded
/// node ID list, including an empty list for zero-ref ways. Uses a
/// caller-provided `refs_buf` to avoid per-way allocation.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn scan_way_refs(
    decompressed: &[u8],
    refs_buf: &mut Vec<i64>,
    group_starts: &mut Vec<(usize, usize)>,
    mut callback: impl FnMut(i64, &[i64]),
) -> Result<KindSet> {
    use crate::read::wire::{Cursor, WIRE_LEN};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    group_starts.clear();
    let mut kinds = KindSet::default();

    // Parse PrimitiveBlock top-level: only collect group offsets.
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            _ => {
                cursor.skip_field(wire_type)?;
            }
        }
    }

    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);

        // PrimitiveGroup field 3 = Way (repeated).
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 3 && wire_type == WIRE_LEN {
                kinds.add(ElementKind::Way);
                let way_data = gcursor.read_len_delimited()?;
                parse_way_refs(way_data, refs_buf, &mut callback)?;
            } else {
                skip_recording_kind(&mut gcursor, field, wire_type, &mut kinds)?;
            }
        }
    }

    Ok(kinds)
}

/// Skip one non-way group field, recording the kind it holds under the
/// crate's `KindSet` rule: a plain node, a dense message with at least one
/// ID, a relation. Anything else (changeset groups, unknown fields, a
/// mistyped tag) adds nothing. Reads the dense message only to see whether
/// it carries an ID, which only a foreign-kind blob reaches.
pub(crate) fn skip_recording_kind(
    gcursor: &mut crate::read::wire::Cursor<'_>,
    field: u32,
    wire_type: u32,
    kinds: &mut KindSet,
) -> Result<()> {
    use crate::read::wire::{WIRE_LEN, WireDenseNodes};

    if wire_type != WIRE_LEN {
        gcursor.skip_field(wire_type)?;
        return Ok(());
    }
    match field {
        1 => {
            kinds.add(ElementKind::Node);
            gcursor.skip_field(wire_type)?;
        }
        2 => {
            if !WireDenseNodes::parse(gcursor.read_len_delimited()?)?
                .id_data
                .is_empty()
            {
                kinds.add(ElementKind::Node);
            }
        }
        4 => {
            kinds.add(ElementKind::Relation);
            gcursor.skip_field(wire_type)?;
        }
        _ => gcursor.skip_field(wire_type)?,
    }
    Ok(())
}

/// Delta-decode a packed `Way.refs` payload into `refs_buf`.
///
/// Signed deltas reconstruct modulo 2^64, matching WayRefIter and the writer.
/// For example, absolute refs MIN then MAX encode as MIN then -1; rejecting
/// the second addition would refuse output our own writer can produce.
///
/// This scanner bypasses `PrimitiveBlock` construction, so the array is
/// validated here, during the decode: each delta is read with the fallible
/// `Cursor::read_varint` and its error propagated. `PackedSint64Iter`, which
/// this used to iterate, wraps the same decoder but turns its error into an
/// end of iteration, so an array ending mid-varint or holding a varint wider
/// than 64 bits gave the way a silently short ref list.
fn accumulate_refs(refs_data: &[u8], refs_buf: &mut Vec<i64>) -> Result<()> {
    use crate::read::wire::Cursor;

    let mut cursor = Cursor::new(refs_data);
    let mut cum: i64 = 0;
    while !cursor.is_empty() {
        let raw = cursor.read_varint().map_err(|_| refs_err())?;
        cum = cum.wrapping_add(protohoggr::zigzag_decode_64(raw));
        refs_buf.push(cum);
    }
    Ok(())
}

/// Error for a `Way.refs` array that ends mid-varint or holds a varint wider
/// than 64 bits. Out of line: only a corrupt block reaches it.
#[cold]
#[inline(never)]
fn refs_err() -> crate::error::Error {
    crate::error::new_wire_error("way refs array holds a truncated or overlong varint")
}

/// Parse a single Way message and extract id + refs.
///
/// Routes through [`WireWay::parse`] rather than hand-rolling a third Way
/// parser. That matters for more than duplication: `wire.rs` accepts a packed
/// repeated field in *either* legal protobuf wire form (a length-delimited
/// packed segment, or individually tagged varints), and this scanner used to
/// match `WIRE_LEN` only. On a spec-legal unpacked encoding the old code saw
/// `refs` as absent, invoked no callback at all, and the caller concluded the
/// way had no node references - a silently truncated node index, not an error.
/// See `reference/truncation-handling.md`, "Which wire forms of a packed field
/// count as present".
fn parse_way_refs(
    way_data: &[u8],
    refs_buf: &mut Vec<i64>,
    callback: &mut impl FnMut(i64, &[i64]),
) -> Result<()> {
    use crate::read::wire::WireWay;

    let way = WireWay::parse(way_data)?;
    // Emit every Way, including zero-ref ways: the external location join
    // records one refcount per callback and consumes one per Way at assembly.
    refs_buf.clear();
    accumulate_refs(way.refs_data, refs_buf)?;
    callback(way.id, refs_buf);

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Deltas `MIN` then `-1` (zigzag `u64::MAX`, then 1): refs `MIN`, `MAX`.
    #[test]
    fn scanned_refs_wrap_like_the_writer_and_element_iterator() {
        let deltas = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 1];
        let mut refs = Vec::new();
        accumulate_refs(&deltas, &mut refs).unwrap();
        assert_eq!(refs, [i64::MIN, i64::MAX]);
    }

    /// A refs array the decoder cannot consume in full is refused, not cut
    /// short: one ending mid-varint, and one whose tenth byte carries payload
    /// past bit 63. Each follows a valid ref, which the old iterator kept
    /// while dropping the rest with no error.
    #[test]
    fn malformed_refs_are_refused_not_truncated() {
        let mut overlong = vec![0x02u8];
        overlong.extend_from_slice(&[0x80; 9]);
        overlong.push(0x02);
        for refs_data in [&[0x02u8, 0x80][..], &overlong] {
            let err = accumulate_refs(refs_data, &mut Vec::new())
                .expect_err("a malformed refs array must be refused");
            assert!(
                err.to_string().contains("way refs array"),
                "expected the refs guard. Got: {err}"
            );
        }
    }

    /// PrimitiveBlock holding one PrimitiveGroup holding one Way. `refs` is
    /// written packed when `packed`, unpacked (one varint-typed field 8 per
    /// delta) otherwise. Both spellings are legal protobuf.
    fn way_block(way_id: i64, refs: &[i64], packed: bool) -> Vec<u8> {
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
        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }

        let mut way = Vec::new();
        way.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(way_id as u64, &mut way);

        let mut deltas = Vec::new();
        let mut prev = 0_i64;
        for &r in refs {
            deltas.push(zigzag(r.wrapping_sub(prev)));
            prev = r;
        }
        if packed {
            let mut seg = Vec::new();
            for &d in &deltas {
                varint(d, &mut seg);
            }
            way.push(0x42); // field 8, LEN
            varint(seg.len() as u64, &mut way);
            way.extend_from_slice(&seg);
        } else {
            for &d in &deltas {
                way.push(0x40); // field 8, VARINT
                varint(d, &mut way);
            }
        }

        let mut group = Vec::new();
        group.push(0x1A); // field 3 (ways), LEN
        varint(way.len() as u64, &mut group);
        group.extend_from_slice(&way);

        let mut block = Vec::new();
        block.push(0x12); // PrimitiveBlock field 2, LEN
        varint(group.len() as u64, &mut block);
        block.extend_from_slice(&group);
        block
    }

    fn collect(block: &[u8]) -> Vec<(i64, Vec<i64>)> {
        let mut out = Vec::new();
        let mut refs_buf = Vec::new();
        let mut group_starts = Vec::new();
        scan_way_refs(block, &mut refs_buf, &mut group_starts, |id, refs| {
            out.push((id, refs.to_vec()));
        })
        .unwrap();
        out
    }

    /// A group of `(field, payload)` length-delimited fields; every payload
    /// here is under 128 bytes, so each length is one byte.
    fn group_of(fields: &[(u8, &[u8])]) -> Vec<u8> {
        let mut group = Vec::new();
        for &(field, payload) in fields {
            group.push((field << 3) | 2);
            group.push(u8::try_from(payload.len()).expect("short payload"));
            group.extend_from_slice(payload);
        }
        group
    }

    fn block_of(groups: &[Vec<u8>]) -> Vec<u8> {
        let mut block = Vec::new();
        for group in groups {
            block.push(0x12);
            block.push(u8::try_from(group.len()).expect("short group"));
            block.extend_from_slice(group);
        }
        block
    }

    fn set(kinds: &[ElementKind]) -> KindSet {
        let mut set = KindSet::default();
        for &kind in kinds {
            set.add(kind);
        }
        set
    }

    /// The way walker records the kinds under the shared `KindSet` rule: a
    /// way, a plain node, a dense message with an ID and a relation count;
    /// empty groups, an empty dense message and changeset groups do not.
    #[test]
    fn recorded_kinds_follow_the_shared_rule() {
        let way = [0x08, 0x01];
        let node = [0x08, 0x02];
        let relation = [0x08, 0x03];
        let dense_with_ids = [0x0A, 0x01, 0x02]; // DenseNodes.id = [zigzag 1]
        let dense_empty_ids = [0x0A, 0x00];
        let changeset = [0x08, 0x01];

        let cases: Vec<(&str, Vec<Vec<u8>>, KindSet)> = vec![
            ("no groups", vec![], KindSet::default()),
            ("empty group", vec![group_of(&[])], KindSet::default()),
            (
                "empty dense message",
                vec![group_of(&[(2, &[])])],
                KindSet::default(),
            ),
            (
                "dense message with an empty id array",
                vec![group_of(&[(2, &dense_empty_ids)])],
                KindSet::default(),
            ),
            (
                "changeset group",
                vec![group_of(&[(5, &changeset)])],
                KindSet::default(),
            ),
            (
                "ways",
                vec![group_of(&[(3, &way)])],
                set(&[ElementKind::Way]),
            ),
            (
                "plain node",
                vec![group_of(&[(1, &node)])],
                set(&[ElementKind::Node]),
            ),
            (
                "dense with ids",
                vec![group_of(&[(2, &dense_with_ids)])],
                set(&[ElementKind::Node]),
            ),
            (
                "relation",
                vec![group_of(&[(4, &relation)])],
                set(&[ElementKind::Relation]),
            ),
            (
                "foreign kinds beside ways and across groups",
                vec![
                    group_of(&[(3, &way), (1, &node), (5, &changeset)]),
                    group_of(&[(4, &relation), (2, &dense_with_ids)]),
                ],
                set(&[ElementKind::Node, ElementKind::Way, ElementKind::Relation]),
            ),
        ];
        for (what, groups, want) in cases {
            let block = block_of(&groups);
            let got = scan_way_refs(&block, &mut Vec::new(), &mut Vec::new(), |_, _| {})
                .unwrap_or_else(|err| panic!("{what}: scan_way_refs: {err}"));
            assert_eq!(got, want, "{what}: scan_way_refs");
        }

        let foreign = block_of(&[group_of(&[(3, &way), (4, &relation)])]);
        let kinds = scan_way_refs(&foreign, &mut Vec::new(), &mut Vec::new(), |_, _| {}).unwrap();
        assert_eq!(
            kinds.foreign_to(ElementKind::Way),
            Some(ElementKind::Relation)
        );
    }

    #[test]
    fn empty_refs_emit_a_way_and_clear_the_previous_refs() {
        let mut block = way_block(10, &[], false);
        block.extend(way_block(11, &[7], true));
        block.extend(way_block(12, &[], true));
        assert_eq!(
            collect(&block),
            vec![(10, vec![]), (11, vec![7]), (12, vec![])]
        );
    }

    #[test]
    fn a_single_unpacked_ref_is_not_read_as_no_refs() {
        // The old matcher took `(8, WIRE_LEN)` only, so an unpacked `refs`
        // fell to `skip_field`, `refs_data` stayed `None`, and the callback
        // never fired: the caller concluded the way referenced no nodes and
        // the node index came out silently short. A single unpacked varint is
        // byte-identical to a one-element packed segment.
        let unpacked = collect(&way_block(42, &[7], false));
        assert_eq!(
            unpacked,
            vec![(42, vec![7])],
            "an unpacked Way.refs must decode to the same refs a packed one does"
        );

        // Passing counterpart in the same shape: the packed spelling of the
        // identical way must give the identical answer, so this is not
        // satisfied by a scanner that emits refs unconditionally.
        assert_eq!(collect(&way_block(42, &[7], true)), unpacked);
    }

    #[test]
    fn a_way_with_no_refs_yields_an_empty_callback() {
        assert_eq!(
            collect(&way_block(42, &[], true)),
            vec![(42, vec![])],
            "a zero-ref way is reported with an empty refs list"
        );
    }
}
