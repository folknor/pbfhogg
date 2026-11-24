//! Cut a rewritten block that outgrew the blob payload limit into pieces.
//!
//! Renumber rewrites each input blob into one block by splicing new IDs
//! into its wire bytes, and the splice can grow a block: a way or relation
//! id of 1 is a one-byte varint where a large or negative new id takes up to
//! ten, a plain node's id likewise, and a way ref or relation member that
//! alternates between a live element and an orphan named outside the new
//! range turns one-byte deltas into ten-byte ones. A block that fit the
//! limit on input can therefore come out several times over it. The writer
//! refuses a payload past the limit, so such a block is cut, between
//! elements, into blocks that each fit and carry their own index.
//!
//! The check is one length comparison per rewritten blob; the cut itself
//! runs only for a block over the limit, after the rewrite, on bytes the
//! rewriter just wrote, so the per-element rewrite loops are untouched.
//!
//! The cut is the shared wire cut, `commands/split.rs`, whose module
//! documentation says which limit, what a piece holds and which cuts it
//! finds. Renumber tries it at the raw limit first, then, when a piece's
//! body expands past the cap, at the sure limit. What this module adds is
//! renumber's numbering: every piece carries the whole string table only
//! when the first cut applies, and its index comes from the positions its
//! new ids were given by (nodes and ways) or is read back from its
//! relations, never from a blob scan, so one input blob stays one result
//! whose totals the stage checks.

use crate::blob_meta::BlobIndex;
use crate::commands::split::{Limits, Piece, layout, plan};
use crate::error::Error;

/// How a rewritten block's elements got their new ids, so each piece can be
/// given its own index.
#[derive(Clone, Copy)]
pub(super) enum Numbering {
    /// By position from this first id (the blob's base), in the start's
    /// direction: nodes and ways.
    From(i64),
    /// Read back from the elements: relations.
    ReadBack,
}

/// `block` with its `index`, as one or more blocks the writer frames under
/// `limits`. A block that frames comes back unchanged; one that cannot be
/// cut comes back whole, for the writer to refuse.
pub(super) fn fit(
    block: Vec<u8>,
    index: BlobIndex,
    numbering: Numbering,
    limits: &Limits,
) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
    if limits.frames(&block)? {
        return Ok(vec![(block, index)]);
    }
    fit_cold(block, index, numbering, limits)
}

#[cold]
#[inline(never)]
fn fit_cold(
    block: Vec<u8>,
    index: BlobIndex,
    numbering: Numbering,
    limits: &Limits,
) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
    // No partition at the raw limit means none at the smaller sure limit.
    let Some(pieces) = cut(&block, &index, numbering, limits.raw())? else {
        return Ok(vec![(block, index)]);
    };
    let mut all_frame = true;
    for (bytes, _) in &pieces {
        if !limits.frames(bytes)? {
            all_frame = false;
            break;
        }
    }
    if all_frame || limits.sure() >= limits.raw() {
        return Ok(pieces);
    }
    // A piece's body expands past the cap: cut where no body can.
    Ok(cut(&block, &index, numbering, limits.sure())?.unwrap_or(pieces))
}

/// A block's bytes with its index.
type Indexed = (Vec<u8>, BlobIndex);

/// Fold one relation id into a piece's range.
fn widen(range: &mut Option<(i64, i64)>, id: i64) {
    *range = Some(match *range {
        None => (id, id),
        Some((lo, hi)) => (lo.min(id), hi.max(id)),
    });
}

/// `block` cut into pieces of at most `limit` bytes, each with its index, or
/// `None` when neither cut finds a partition.
fn cut(
    block: &[u8],
    index: &BlobIndex,
    numbering: Numbering,
    limit: usize,
) -> crate::Result<Option<Vec<Indexed>>> {
    let layout = layout(block)?;
    let Some(plan) = plan(block, &layout, limit) else {
        return Ok(None);
    };
    let mut pieces = Vec::with_capacity(plan.len());
    for k in 0..plan.len() {
        pieces.push(plan.piece(k)?);
    }
    index_pieces(pieces, index, numbering).map(Some)
}

/// The smallest and largest relation id a relation piece holds.
fn relation_range(piece: &[u8]) -> crate::Result<Option<(i64, i64)>> {
    let layout = layout(piece)?;
    let mut range = None;
    for item in layout.items.iter().filter(|it| it.field_num == 4) {
        let id = super::wire_rewrite::relation_id(&piece[item.payload.0..item.payload.1])?;
        widen(&mut range, id);
    }
    Ok(range)
}

/// Each piece's index: positional from the block's first id, or read back
/// from its relations.
fn index_pieces(
    pieces: Vec<Piece>,
    index: &BlobIndex,
    numbering: Numbering,
) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
    let mut out = Vec::with_capacity(pieces.len());
    let mut preceding: u64 = 0;
    for piece in pieces {
        let piece_index = match numbering {
            Numbering::ReadBack => {
                let (min_id, max_id) = relation_range(&piece.bytes)?.ok_or_else(|| {
                    Error::internal("renumber: a relation piece without a relation")
                })?;
                BlobIndex {
                    kind: crate::ElementKind::Relation,
                    min_id,
                    max_id,
                    count: piece.count,
                    bbox: None,
                }
            }
            Numbering::From(first) => {
                // A piece's first id lies `preceding` positions past the
                // block's first, in the start's direction.
                let base = super::wire_rewrite::offset_id(first, preceding)?;
                super::blob_index(index.kind, base, piece.count, index.bbox)?
            }
        };
        preceding = preceding
            .checked_add(piece.count)
            .ok_or_else(|| Error::internal("renumber: piece count overflow"))?;
        out.push((piece.bytes, piece_index));
    }
    if preceding != index.count {
        return Err(Error::internal(format!(
            "renumber: block cut into pieces holding {preceding} elements, the block held {}",
            index.count
        )));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn field(num: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from(num << 3 | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    /// A way message with id `id` (int64) and `pad` bytes of field 2 keys.
    fn way(id: i64, pad: usize) -> Vec<u8> {
        let mut w = Vec::new();
        w.push(0x08);
        varint(id as u64, &mut w);
        field(2, &vec![1u8; pad], &mut w);
        w
    }

    fn relation(id: i64, pad: usize) -> Vec<u8> {
        way(id, pad)
    }

    /// A block: a string table, groups of `(field, message)` items, and a
    /// trailing granularity field.
    fn block(groups: &[Vec<(u32, Vec<u8>)>]) -> Vec<u8> {
        let mut b = Vec::new();
        field(1, b"\x0a\x00", &mut b);
        for g in groups {
            let mut body = Vec::new();
            for (num, msg) in g {
                field(*num, msg, &mut body);
            }
            field(2, &body, &mut b);
        }
        b.extend_from_slice(&[0x88, 0x01, 0x64]);
        b
    }

    /// `fit` with a bare byte limit: the block unchanged within it, else
    /// cut, else whole.
    fn fit_at(
        block: Vec<u8>,
        index: BlobIndex,
        numbering: Numbering,
        limit: usize,
    ) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
        if block.len() <= limit {
            return Ok(vec![(block, index)]);
        }
        Ok(match cut(&block, &index, numbering, limit)? {
            Some(pieces) => pieces,
            None => vec![(block, index)],
        })
    }

    fn index(kind: crate::ElementKind, min_id: i64, max_id: i64, count: u64) -> BlobIndex {
        BlobIndex {
            kind,
            min_id,
            max_id,
            count,
            bbox: None,
        }
    }

    /// Every piece decodes, holds an element, fits, and the pieces' items
    /// concatenate to the block's.
    fn check(block: &[u8], pieces: &[(Vec<u8>, BlobIndex)], limit: usize) {
        let whole = layout(block).unwrap();
        let mut seen = Vec::new();
        for (bytes, idx) in pieces {
            assert!(bytes.len() <= limit, "{} > {limit}", bytes.len());
            assert!(idx.count > 0);
            let l = layout(bytes).unwrap();
            assert_eq!(&bytes[..l.prefix.1], &block[..whole.prefix.1]);
            assert_eq!(&bytes[l.suffix.0..], &block[whole.suffix.0..]);
            for it in &l.items {
                seen.push(bytes[it.field.0..it.field.1].to_vec());
            }
        }
        let expected: Vec<Vec<u8>> = whole
            .items
            .iter()
            .filter(|it| it.len() > 0)
            .map(|it| block[it.field.0..it.field.1].to_vec())
            .collect();
        seen.retain(|s| !s.is_empty());
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_block_within_the_limit_is_returned_unchanged() {
        let b = block(&[vec![(3, way(1, 10)), (3, way(2, 10))]]);
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), b.len()).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, b);
    }

    #[test]
    fn ways_are_cut_with_positional_indexes_both_directions() {
        let ways: Vec<(u32, Vec<u8>)> = (0..6).map(|k| (3, way(k, 40))).collect();
        let b = block(&[ways]);
        let limit = b.len() / 2;
        // (first id, block min, block max, the pieces' expected ranges)
        let up = vec![(100, 101), (102, 103), (104, 105)];
        let down = vec![(-101, -100), (-103, -102), (-105, -104)];
        for (first, min, max, expected) in [(100, 100, 105, up), (-100, -105, -100, down)] {
            let idx = index(crate::ElementKind::Way, min, max, 6);
            let pieces = fit_at(b.clone(), idx, Numbering::From(first), limit).unwrap();
            check(&b, &pieces, limit);
            let ranges: Vec<(i64, i64)> =
                pieces.iter().map(|(_, p)| (p.min_id, p.max_id)).collect();
            assert_eq!(ranges, expected);
        }
    }

    #[test]
    fn relation_pieces_take_their_range_from_their_relations() {
        let rels: Vec<(u32, Vec<u8>)> = [7, 3, 9, 1]
            .iter()
            .map(|&id| (4, relation(id, 40)))
            .collect();
        let b = block(&[rels]);
        let limit = b.len() / 2 + 4;
        let idx = index(crate::ElementKind::Relation, 1, 9, 4);
        let pieces = fit_at(b.clone(), idx, Numbering::ReadBack, limit).unwrap();
        check(&b, &pieces, limit);
        let ranges: Vec<(i64, i64)> = pieces.iter().map(|(_, p)| (p.min_id, p.max_id)).collect();
        assert_eq!(ranges, vec![(3, 7), (1, 9)]);
    }

    /// `A, B, filler, C`: greedy packs `A B`, then `filler C` does not fit;
    /// `A | B filler | C` does.
    #[test]
    fn a_filler_moves_to_whichever_side_lets_the_pieces_fit() {
        let filler = (9, vec![0u8; 30]);
        let b = block(&[vec![
            (3, way(1, 26)),
            (3, way(2, 46)),
            filler,
            (3, way(3, 76)),
        ]]);
        let context = 4 + 3;
        let limit = context + 3 + 84;
        let idx = index(crate::ElementKind::Way, 1, 3, 3);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), limit).unwrap();
        assert_eq!(pieces.len(), 3);
        check(&b, &pieces, limit);
    }

    #[test]
    fn trailing_fillers_and_empty_groups_never_make_an_element_free_piece() {
        let b = block(&[
            vec![(3, way(1, 40)), (9, vec![0u8; 5])],
            vec![],
            vec![(3, way(2, 40))],
            vec![(9, vec![0u8; 5])],
        ]);
        let limit = b.len() - 20;
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), limit).unwrap();
        assert_eq!(pieces.len(), 2);
        check(&b, &pieces, limit);
    }

    #[test]
    fn an_element_that_cannot_fit_leaves_the_block_whole() {
        let b = block(&[vec![(3, way(1, 10)), (3, way(2, 200))]]);
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), 100).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, b);
    }

    #[test]
    fn dense_groups_count_their_nodes_and_an_empty_one_is_a_filler() {
        let mut dense = Vec::new();
        field(1, &[0x02, 0x02, 0x02], &mut dense);
        let mut empty = Vec::new();
        field(1, &[], &mut empty);
        let b = block(&[vec![(2, dense.clone())], vec![(2, empty)], vec![(2, dense)]]);
        let idx = index(crate::ElementKind::Node, 1, 6, 6);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), b.len() - 1).unwrap();
        assert_eq!(pieces.len(), 2);
        check(&b, &pieces, b.len() - 1);
        assert_eq!(
            pieces
                .iter()
                .map(|(_, p)| (p.min_id, p.max_id))
                .collect::<Vec<_>>(),
            vec![(1, 3), (4, 6)]
        );
    }

    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn packed(values: impl IntoIterator<Item = u64>) -> Vec<u8> {
        let mut out = Vec::new();
        for v in values {
            varint(v, &mut out);
        }
        out
    }

    /// A block with this string table, groups of `(field, message)` items
    /// and a trailing granularity field.
    fn block_with(table: &[&[u8]], groups: &[Vec<(u32, Vec<u8>)>]) -> Vec<u8> {
        let mut body = Vec::new();
        for s in table {
            field(1, s, &mut body);
        }
        let mut b = Vec::new();
        field(1, &body, &mut b);
        for g in groups {
            let mut gb = Vec::new();
            for (num, msg) in g {
                field(*num, msg, &mut gb);
            }
            field(2, &gb, &mut b);
        }
        b.extend_from_slice(&[0x88, 0x01, 0x64]);
        b
    }

    /// Every element of a block as the reader decodes it.
    fn decoded(bytes: &[u8]) -> Vec<String> {
        let block = crate::PrimitiveBlock::from_vec(bytes.to_vec()).unwrap();
        block
            .elements()
            .map(|e| match e {
                crate::Element::DenseNode(n) => format!(
                    "n{} {} {} {:?} {:?}",
                    n.id(),
                    n.nano_lat_exact(),
                    n.nano_lon_exact(),
                    n.tags().collect::<Vec<_>>(),
                    n.info().map(|i| (
                        i.version(),
                        i.milli_timestamp(),
                        i.changeset(),
                        i.uid(),
                        i.user().unwrap().to_owned()
                    )),
                ),
                crate::Element::Way(w) => format!(
                    "w{} {:?} {:?} {:?}",
                    w.id(),
                    w.refs().collect::<Vec<_>>(),
                    w.tags().collect::<Vec<_>>(),
                    w.info().user().map(|u| u.unwrap().to_owned()),
                ),
                crate::Element::Relation(r) => format!(
                    "r{} {:?} {:?}",
                    r.id(),
                    r.tags().collect::<Vec<_>>(),
                    r.members()
                        .map(|m| m.role().unwrap().to_owned())
                        .collect::<Vec<_>>(),
                ),
                crate::Element::Node(n) => {
                    format!("p{} {:?}", n.id(), n.tags().collect::<Vec<_>>())
                }
            })
            .collect()
    }

    fn assert_same_content(block: &[u8], pieces: &[(Vec<u8>, BlobIndex)], limit: usize) {
        let mut seen = Vec::new();
        for (bytes, idx) in pieces {
            assert!(bytes.len() <= limit, "{} > {limit}", bytes.len());
            let d = decoded(bytes);
            assert_eq!(d.len() as u64, idx.count);
            seen.extend(d);
        }
        assert_eq!(seen, decoded(block));
    }

    /// One `DenseNodes` of `n` nodes with every column, tags on most nodes
    /// and an unterminated last run.
    fn dense_message(n: i64) -> Vec<u8> {
        let mut d = Vec::new();
        field(
            1,
            &packed((0..n).map(|k| zz(if k == 0 { 1000 } else { 1 }))),
            &mut d,
        );
        let mut info = Vec::new();
        field(1, &packed((0..n).map(|_| 1)), &mut info);
        field(
            2,
            &packed((0..n).map(|k| zz(if k == 0 { 1_600_000_000 } else { 60 }))),
            &mut info,
        );
        field(
            3,
            &packed((0..n).map(|k| zz(if k == 0 { 90_000 } else { k % 3 }))),
            &mut info,
        );
        field(
            4,
            &packed((0..n).map(|k| zz(if k == 0 { 7 } else { 0 }))),
            &mut info,
        );
        field(
            5,
            &packed((0..n).map(|k| zz(if k == 0 { 4 } else { 0 }))),
            &mut info,
        );
        field(5, &info, &mut d);
        field(
            8,
            &packed((0..n).map(|k| zz(if k == 0 { 556_000_000 } else { 37 - k }))),
            &mut d,
        );
        field(
            9,
            &packed((0..n).map(|k| zz(if k == 0 { 125_000_000 } else { k * 11 }))),
            &mut d,
        );
        let mut kv = Vec::new();
        for k in 0..n {
            if k % 4 != 1 {
                kv.extend([1, 2 + (k % 2) as u64]);
            }
            if k + 1 < n {
                kv.push(0);
            }
        }
        field(10, &packed(kv), &mut d);
        d
    }

    const DENSE_TABLE: [&[u8]; 5] = [b"", b"k", b"va", b"vb", b"alice"];

    #[test]
    fn one_dense_group_is_cut_between_nodes_and_decodes_the_same() {
        let b = block_with(&DENSE_TABLE, &[vec![(2, dense_message(300))]]);
        let idx = index(crate::ElementKind::Node, 1000, 1299, 300);
        let limit = b.len() / 3;
        let pieces = fit_at(b.clone(), idx, Numbering::From(1000), limit).unwrap();
        assert!(pieces.len() >= 3, "{} pieces", pieces.len());
        assert_same_content(&b, &pieces, limit);
        let mut next = 1000;
        for (bytes, p) in &pieces {
            assert_eq!(p.min_id, next);
            next = p.max_id + 1;
            // Each piece is one group holding one dense message.
            let l = layout(bytes).unwrap();
            assert_eq!(l.groups.len(), 1);
        }
        assert_eq!(next, 1300);
    }

    /// The string table is the obstacle: each way fits only with its own
    /// strings, so each piece carries only those.
    #[test]
    fn a_way_that_fits_only_with_its_own_strings_gets_a_compact_table() {
        let a = vec![b'a'; 300];
        let c = vec![b'c'; 300];
        let table: [&[u8]; 4] = [b"", b"name", &a, &c];
        let way = |id: u64, val: u64| {
            let mut w = Vec::new();
            w.push(0x08);
            varint(id, &mut w);
            field(2, &[1], &mut w);
            field(3, &[val as u8], &mut w);
            let mut info = Vec::new();
            info.extend([0x08, 0x01, 0x28]);
            varint(val, &mut info);
            field(4, &info, &mut w);
            field(8, &packed([zz(5), zz(1)]), &mut w);
            w
        };
        let b = block_with(&table, &[vec![(3, way(1, 2)), (3, way(2, 3))]]);
        let limit = 400;
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), limit).unwrap();
        assert_eq!(pieces.len(), 2);
        assert_same_content(&b, &pieces, limit);
    }

    #[test]
    fn a_relation_role_is_remapped_and_its_range_read_back() {
        let big = vec![b'r'; 300];
        let other = vec![b'o'; 300];
        let table: [&[u8]; 4] = [b"", b"x", &big, &other];
        let rel = |id: u64, role: u64| {
            let mut r = Vec::new();
            r.push(0x08);
            varint(id, &mut r);
            field(8, &[role as u8], &mut r);
            field(9, &packed([zz(10)]), &mut r);
            field(10, &[1], &mut r);
            r
        };
        let b = block_with(&table, &[vec![(4, rel(9, 2)), (4, rel(4, 3))]]);
        let idx = index(crate::ElementKind::Relation, 4, 9, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::ReadBack, 400).unwrap();
        assert_eq!(pieces.len(), 2);
        assert_same_content(&b, &pieces, 400);
        let ranges: Vec<(i64, i64)> = pieces.iter().map(|(_, p)| (p.min_id, p.max_id)).collect();
        assert_eq!(ranges, vec![(9, 9), (4, 4)]);
    }

    /// A field the cut does not know may hold a string index it cannot
    /// remap, so no piece gets a compact table and the block stays whole.
    #[test]
    fn an_unknown_element_field_keeps_the_whole_table() {
        let a = vec![b'a'; 300];
        let c = vec![b'c'; 300];
        let table: [&[u8]; 4] = [b"", b"name", &a, &c];
        let way = |id: u64, val: u8, extra: bool| {
            let mut w = vec![0x08, id as u8];
            field(2, &[1], &mut w);
            field(3, &[val], &mut w);
            if extra {
                field(15, &[2], &mut w);
            }
            w
        };
        let b = block_with(&table, &[vec![(3, way(1, 2, true)), (3, way(2, 3, false))]]);
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1), 400).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, b);
    }

    #[test]
    fn a_dense_group_with_a_compact_table_decodes_the_same() {
        let big = vec![b'z'; 2000];
        let table: [&[u8]; 6] = [b"", b"k", b"va", b"vb", b"alice", &big];
        let b = block_with(&table, &[vec![(2, dense_message(300))]]);
        // The unused large entry alone is over the limit.
        let limit = 1500;
        let idx = index(crate::ElementKind::Node, 1000, 1299, 300);
        let pieces = fit_at(b.clone(), idx, Numbering::From(1000), limit).unwrap();
        assert!(pieces.len() >= 2);
        assert_same_content(&b, &pieces, limit);
    }
}
