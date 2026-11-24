//! Cut a rewritten block that outgrew the blob payload limit into pieces.
//!
//! Renumber rewrites each input blob into one block by splicing new IDs
//! into its wire bytes, and the splice can grow a block: a way or relation
//! id of 1 is a one-byte varint where a large or negative new id takes up to
//! ten, a plain node's id likewise, and a way ref or relation member that
//! alternates between a live element and an orphan named outside the new
//! range turns one-byte deltas into ten-byte ones. A block that fit the
//! limit on input can therefore come out several times over it. The writer
//! refuses a payload past the limit, so such a block is cut here, between
//! elements, into blocks that each fit and carry their own index.
//!
//! The check is one length comparison per rewritten blob; the cut itself
//! runs only for a block over the limit, after the rewrite, on bytes the
//! rewriter just wrote, so the per-element rewrite loops are untouched.
//!
//! # What a piece holds
//!
//! Every piece repeats the rewritten block's whole context: the string table
//! ahead of the groups and the retained top-level fields (granularity, date
//! granularity, offsets) after them (the rewriters write exactly that
//! layout). The groups are cut between items. An item is an element (a
//! `DenseNodes` message holding at least one node, a plain `Node`, a `Way`,
//! a `Relation`) or a filler: an empty `DenseNodes`, the empty body of a
//! group with no fields, or any other group field. The rewriters drop every
//! non-element group field before the cut, so that last kind reaches this
//! module only from a caller that keeps one; the cut handles it all the same. A `DenseNodes` message is indivisible (cutting it means
//! rebasing every delta-coded column at a node boundary). Every piece holds
//! at least one element, so it has a real index, and fillers stay once, in
//! order, on whichever side of a cut makes the pieces fit.
//!
//! # Which cuts
//!
//! The cut points are found by an exact search, not greedily: for each end
//! point `j` the size limit bounds a piece's start from below and the
//! must-hold-an-element rule bounds it from above, and `j` is reachable when
//! a reachable cut lies between the two. A greedy fill can spend the element
//! a later filler needed to share a piece with and refuse a block that has a
//! legal partition. When no partition exists - one element with the copied
//! context is over the limit - the block goes to the writer whole, which
//! refuses it with its typed size error.

use protohoggr::{Cursor, WIRE_LEN};

use crate::blob_meta::BlobIndex;
use crate::error::Error;
use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_MESSAGE_SIZE};
use crate::write::compression::Compression;

/// The largest rewritten block the writer frames under `compression`. The
/// payload limit is inclusive; an uncompressed payload is also the body, with
/// its field tag and length ahead of it, and the body must be strictly
/// under `MAX_BLOB_DATASIZE`. A compressed body can still expand past that
/// after this check; the writer refuses such a block, typed.
pub(super) fn payload_limit(compression: &Compression) -> usize {
    let raw_cap = usize::try_from(MAX_BLOB_MESSAGE_SIZE).unwrap_or(usize::MAX);
    match compression {
        Compression::None => {
            let body_cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap_or(usize::MAX);
            // Blob field 1 (`raw`): one tag byte, the length varint, the bytes.
            let mut p = body_cap.saturating_sub(1 + 10);
            while 1 + varint_len(p + 1) + p + 1 < body_cap {
                p += 1;
            }
            p.min(raw_cap)
        }
        Compression::Zlib(_) | Compression::Zstd(_) => raw_cap,
    }
}

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

/// `block` with its `index`, as one or more blocks that each fit `limit`.
/// A block within the limit comes back unchanged.
pub(super) fn fit(
    block: Vec<u8>,
    index: BlobIndex,
    numbering: Numbering,
    limit: usize,
) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
    if block.len() <= limit {
        return Ok(vec![(block, index)]);
    }
    cut(block, index, numbering, limit)
}

/// One group field's item, as byte ranges into the block.
struct Item {
    group: usize,
    /// The whole field, tag included.
    field: (usize, usize),
    /// The field's payload, for reading a relation id back.
    payload: (usize, usize),
    /// Elements this item holds; 0 for a filler.
    count: u64,
}

impl Item {
    fn len(&self) -> usize {
        self.field.1 - self.field.0
    }
}

/// The rewritten block, taken apart.
struct Layout {
    /// Bytes ahead of the first group and after the last.
    prefix: (usize, usize),
    suffix: (usize, usize),
    items: Vec<Item>,
    /// Per group, its first item and one past its last.
    groups: Vec<(usize, usize)>,
}

#[cold]
#[inline(never)]
fn cut(
    block: Vec<u8>,
    index: BlobIndex,
    numbering: Numbering,
    limit: usize,
) -> crate::Result<Vec<(Vec<u8>, BlobIndex)>> {
    let layout = layout(&block)?;
    let Some(cuts) = partition(&layout, limit) else {
        return Ok(vec![(block, index)]);
    };
    let mut pieces = Vec::with_capacity(cuts.len().saturating_sub(1));
    let mut preceding: u64 = 0;
    for w in cuts.windows(2) {
        let (i, j) = (w[0], w[1]);
        let bytes = assemble(&block, &layout, i, j);
        let items = &layout.items[i..j];
        let count: u64 = items.iter().map(|it| it.count).sum();
        let piece_index = match numbering {
            Numbering::ReadBack => relation_index(&block, items, count)?,
            Numbering::From(first) => {
                // A piece's first id lies `preceding` positions past the
                // block's first, in the start's direction.
                let base = super::wire_rewrite::offset_id(first, preceding)?;
                super::blob_index(index.kind, base, count, index.bbox)?
            }
        };
        preceding = preceding
            .checked_add(count)
            .ok_or_else(|| Error::internal("renumber: piece count overflow"))?;
        pieces.push((bytes, piece_index));
    }
    if preceding != index.count {
        return Err(Error::internal(format!(
            "renumber: block cut into pieces holding {preceding} elements, the block held {}",
            index.count
        )));
    }
    Ok(pieces)
}

/// A relation piece's index, from the new ids its relations carry.
fn relation_index(block: &[u8], items: &[Item], count: u64) -> crate::Result<BlobIndex> {
    let mut range: Option<(i64, i64)> = None;
    for item in items.iter().filter(|it| it.count > 0) {
        let id = super::wire_rewrite::relation_id(&block[item.payload.0..item.payload.1])?;
        range = Some(match range {
            None => (id, id),
            Some((lo, hi)) => (lo.min(id), hi.max(id)),
        });
    }
    let (min_id, max_id) =
        range.ok_or_else(|| Error::internal("renumber: a relation piece without a relation"))?;
    Ok(BlobIndex {
        kind: crate::ElementKind::Relation,
        min_id,
        max_id,
        count,
        bbox: None,
    })
}

fn wire_err(e: protohoggr::WireError) -> Error {
    Error::internal_with("renumber: re-reading a rewritten block", e)
}

/// Take a rewritten block apart. Its layout is the rewriters' own: the
/// string table, the groups, the retained top-level fields. A top-level
/// field between groups is a rewriter bug.
fn layout(block: &[u8]) -> crate::Result<Layout> {
    let mut items = Vec::new();
    let mut groups = Vec::new();
    let mut first_group: Option<usize> = None;
    let mut last_group_end = 0usize;
    let mut cursor = Cursor::new(block);
    loop {
        let tag_start = cursor.position();
        let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? else {
            break;
        };
        if (field, wire_type) != (2, WIRE_LEN) {
            cursor.skip_field(wire_type).map_err(wire_err)?;
            continue;
        }
        // A group must follow the previous group directly: anything between
        // them would be neither prefix nor suffix.
        if first_group.is_some() && tag_start != last_group_end {
            return Err(Error::internal("renumber: a block field among its groups"));
        }
        let group = cursor.read_len_delimited().map_err(wire_err)?;
        let body_start = cursor.position() - group.len();
        first_group.get_or_insert(tag_start);
        last_group_end = cursor.position();
        let g = groups.len();
        let first_item = items.len();
        group_items(group, body_start, g, &mut items)?;
        if items.len() == first_item {
            items.push(Item {
                group: g,
                field: (body_start, body_start),
                payload: (body_start, body_start),
                count: 0,
            });
        }
        groups.push((first_item, items.len()));
    }
    let first_group =
        first_group.ok_or_else(|| Error::internal("renumber: a rewritten block without groups"))?;
    Ok(Layout {
        prefix: (0, first_group),
        suffix: (last_group_end, block.len()),
        items,
        groups,
    })
}

/// The items of one group whose body starts at `offset` in the block.
fn group_items(group: &[u8], offset: usize, g: usize, items: &mut Vec<Item>) -> crate::Result<()> {
    let mut cursor = Cursor::new(group);
    loop {
        let start = cursor.position();
        let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? else {
            break;
        };
        let (payload, count) = if wire_type == WIRE_LEN && (1..=4).contains(&field) {
            let data = cursor.read_len_delimited().map_err(wire_err)?;
            let payload_start = cursor.position() - data.len();
            let count = if field == 2 { dense_count(data)? } else { 1 };
            ((payload_start, cursor.position()), count)
        } else {
            cursor.skip_field(wire_type).map_err(wire_err)?;
            ((start, cursor.position()), 0)
        };
        items.push(Item {
            group: g,
            field: (offset + start, offset + cursor.position()),
            payload: (offset + payload.0, offset + payload.1),
            count,
        });
    }
    Ok(())
}

/// Nodes in a rewritten `DenseNodes`: the varints of its packed id field.
fn dense_count(dense: &[u8]) -> crate::Result<u64> {
    let mut count: u64 = 0;
    let mut cursor = Cursor::new(dense);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? {
        if (field, wire_type) == (1, WIRE_LEN) {
            let ids = cursor.read_len_delimited().map_err(wire_err)?;
            let mut ids = Cursor::new(ids);
            while ids.remaining() > 0 {
                ids.read_varint().map_err(wire_err)?;
                count += 1;
            }
        } else {
            cursor.skip_field(wire_type).map_err(wire_err)?;
        }
    }
    Ok(count)
}

fn varint_len(mut v: usize) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// A group field holding `inner` bytes: its one-byte tag, length, body.
fn group_field_len(inner: usize) -> usize {
    1 + varint_len(inner) + inner
}

/// Exact sizes of the pieces a layout can be cut into.
struct Sizes<'a> {
    layout: &'a Layout,
    context: usize,
    /// Prefix sums of item lengths.
    bytes: Vec<usize>,
    /// Prefix sums of whole group field lengths.
    whole: Vec<usize>,
}

impl<'a> Sizes<'a> {
    fn new(layout: &'a Layout) -> Self {
        let mut bytes = Vec::with_capacity(layout.items.len() + 1);
        bytes.push(0);
        for item in &layout.items {
            bytes.push(bytes[bytes.len() - 1] + item.len());
        }
        let mut whole = Vec::with_capacity(layout.groups.len() + 1);
        whole.push(0);
        for &(a, b) in &layout.groups {
            whole.push(whole[whole.len() - 1] + group_field_len(bytes[b] - bytes[a]));
        }
        let context = (layout.prefix.1 - layout.prefix.0) + (layout.suffix.1 - layout.suffix.0);
        Self {
            layout,
            context,
            bytes,
            whole,
        }
    }

    /// The encoded size of a piece holding items `[i, j)`, `i < j`.
    fn size(&self, i: usize, j: usize) -> usize {
        let gi = self.layout.items[i].group;
        let gj = self.layout.items[j - 1].group;
        if gi == gj {
            return self.context + group_field_len(self.bytes[j] - self.bytes[i]);
        }
        let first_end = self.layout.groups[gi].1;
        let last_start = self.layout.groups[gj].0;
        self.context
            + group_field_len(self.bytes[first_end] - self.bytes[i])
            + (self.whole[gj] - self.whole[gi + 1])
            + group_field_len(self.bytes[j] - self.bytes[last_start])
    }
}

/// Cut points `0 = c0 < c1 < ... = n` such that every piece `[c_k, c_k+1)`
/// fits `limit` and holds an element, or `None` when no such cut exists.
fn partition(layout: &Layout, limit: usize) -> Option<Vec<usize>> {
    let n = layout.items.len();
    let sizes = Sizes::new(layout);
    // `pred[j]`: the cut a piece ending at reachable cut `j` starts from.
    let mut pred: Vec<Option<usize>> = vec![None; n + 1];
    let mut reachable: Vec<usize> = vec![0];
    let mut lo = 0usize;
    let mut last_element: Option<usize> = None;
    for j in 1..=n {
        if layout.items[j - 1].count > 0 {
            last_element = Some(j - 1);
        }
        while lo < j && sizes.size(lo, j) > limit {
            lo += 1;
        }
        let Some(e) = last_element else { continue };
        if lo >= j || lo > e {
            continue;
        }
        // The earliest reachable cut at or after `lo`; it must be at most
        // `e`. The earliest makes the longest last piece; any would be legal.
        let at = reachable.partition_point(|&c| c < lo);
        let Some(&start) = reachable.get(at) else {
            continue;
        };
        if start <= e {
            pred[j] = Some(start);
            reachable.push(j);
        }
    }
    pred[n]?;
    let mut cuts = vec![n];
    let mut at = n;
    while at > 0 {
        at = pred[at]?;
        cuts.push(at);
    }
    cuts.reverse();
    Some(cuts)
}

/// The block bytes of the piece holding items `[i, j)`.
fn assemble(block: &[u8], layout: &Layout, i: usize, j: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&block[layout.prefix.0..layout.prefix.1]);
    let mut k = i;
    while k < j {
        let g = layout.items[k].group;
        let end = j.min(layout.groups[g].1);
        let inner: usize = layout.items[k..end].iter().map(Item::len).sum();
        protohoggr::encode_tag(&mut out, 2, WIRE_LEN);
        protohoggr::encode_varint(&mut out, inner as u64);
        for item in &layout.items[k..end] {
            out.extend_from_slice(&block[item.field.0..item.field.1]);
        }
        k = end;
    }
    out.extend_from_slice(&block[layout.suffix.0..layout.suffix.1]);
    out
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
        let pieces = fit(b.clone(), idx, Numbering::From(1), b.len()).unwrap();
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
            let pieces = fit(b.clone(), idx, Numbering::From(first), limit).unwrap();
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
        let pieces = fit(b.clone(), idx, Numbering::ReadBack, limit).unwrap();
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
        let pieces = fit(b.clone(), idx, Numbering::From(1), limit).unwrap();
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
        let pieces = fit(b.clone(), idx, Numbering::From(1), limit).unwrap();
        assert_eq!(pieces.len(), 2);
        check(&b, &pieces, limit);
    }

    #[test]
    fn an_element_that_cannot_fit_leaves_the_block_whole() {
        let b = block(&[vec![(3, way(1, 10)), (3, way(2, 200))]]);
        let idx = index(crate::ElementKind::Way, 1, 2, 2);
        let pieces = fit(b.clone(), idx, Numbering::From(1), 100).unwrap();
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
        let pieces = fit(b.clone(), idx, Numbering::From(1), b.len() - 1).unwrap();
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

    #[test]
    fn the_uncompressed_limit_leaves_the_body_under_the_cap() {
        let p = payload_limit(&Compression::None);
        let cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap();
        assert!(1 + varint_len(p) + p < cap);
        assert!(1 + varint_len(p + 1) + p + 1 >= cap);
    }
}
