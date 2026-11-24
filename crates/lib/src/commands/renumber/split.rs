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
//! # Which limit
//!
//! The writer refuses a payload over the raw limit and a Blob body at or
//! over the datasize cap, and a compressed body can be larger than its
//! payload. [`Limits`] holds the raw limit and the largest payload whose
//! worst-case body still fits (the "sure" limit; the same for an
//! uncompressed blob). A block within the sure limit is written as it is.
//! A larger one is compressed once here with the writer's own encoder
//! (`write::encoded_body_len`): if its body fits it is written whole,
//! otherwise it is cut at the raw limit and every piece over the sure limit
//! is compressed the same way; if one of those does not fit either, the
//! block is cut again at the sure limit, where no piece needs a trial. The
//! writer's refusal stays the authority: should a trial and the writer ever
//! disagree, the writer refuses, it never writes a wrong blob.
//!
//! # What a piece holds: the first cut
//!
//! The first cut repeats the rewritten block's whole context in every
//! piece: the string table ahead of the groups and the retained top-level
//! fields (granularity, date granularity, offsets) after them (the
//! rewriters write exactly that layout), and copies every item's bytes
//! unchanged. The groups are cut between items. An item is an element (a
//! `DenseNodes` message holding at least one node, a plain `Node`, a `Way`,
//! a `Relation`) or a filler: an empty `DenseNodes`, the empty body of a
//! group with no fields, or any other group field. The rewriters drop every
//! non-element group field before the cut, so that last kind reaches this
//! module only from a caller that keeps one; the cut handles it all the
//! same. A `DenseNodes` message is one item here. Every piece holds at
//! least one element, so it has a real index, and fillers stay once, in
//! order, on whichever side of a cut makes the pieces fit.
//!
//! When the first cut finds no partition, [`reencode`] tries again with
//! re-encoded pieces: dense groups cut between nodes and, where every
//! string reference can be read, a string table per piece holding only the
//! strings it uses. Its module documentation says what it can and cannot
//! cut.
//!
//! # Which cuts
//!
//! The cut points are found by an exact search, not greedily: for each end
//! point `j` the size limit bounds a piece's start from below and the
//! must-hold-an-element rule bounds it from above, and `j` is reachable when
//! a reachable cut lies between the two. A greedy fill can spend the element
//! a later filler needed to share a piece with and refuse a block that has a
//! legal partition. The search needs a piece's size to be monotone in both
//! its ends; the first cut's sizes are exact, the re-encoding cut's an upper
//! bound with that property. When neither finds a partition the block goes
//! to the writer whole, which refuses it with its typed size error.

mod reencode;

use protohoggr::{Cursor, WIRE_LEN};

use crate::blob_meta::BlobIndex;
use crate::error::Error;
use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_MESSAGE_SIZE};
use crate::write::compression::Compression;

/// The sizes a rewritten block is held to under one compression. See the
/// module documentation, "Which limit".
#[derive(Clone, Copy)]
pub(super) struct Limits {
    compression: Compression,
    /// The largest payload the writer accepts: the raw limit, inclusive,
    /// and for an uncompressed blob also what keeps the body under the cap.
    raw: usize,
    /// The largest payload whose worst-case body is under the cap.
    sure: usize,
}

impl Limits {
    pub(super) fn new(compression: Compression) -> Self {
        let raw_cap = usize::try_from(MAX_BLOB_MESSAGE_SIZE).unwrap_or(usize::MAX);
        let fits = |p: usize| {
            (crate::write::worst_case_body_len(p, &compression) as u64) < MAX_BLOB_DATASIZE
        };
        // The largest `p <= raw_cap` with `fits(p)`; `fits` is monotone.
        let (mut lo, mut hi) = (0usize, raw_cap);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let raw = match compression {
            // The body is the payload with a field head: the worst case is
            // the exact case.
            Compression::None => lo,
            Compression::Zlib(_) | Compression::Zstd(_) => raw_cap,
        };
        Self {
            compression,
            raw,
            sure: lo,
        }
    }

    /// A block at most this long is written without a trial.
    pub(super) fn sure(&self) -> usize {
        self.sure
    }

    /// Whether the writer frames `bytes`: within the raw limit, and its body,
    /// encoded as the writer encodes it, under the cap.
    fn frames(&self, bytes: &[u8]) -> crate::Result<bool> {
        if bytes.len() > self.raw {
            return Ok(false);
        }
        if bytes.len() <= self.sure {
            return Ok(true);
        }
        let body = crate::write::encoded_body_len(bytes, &self.compression)?;
        Ok((body as u64) < MAX_BLOB_DATASIZE)
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
    let Some(pieces) = cut(&block, &index, numbering, limits.raw)? else {
        return Ok(vec![(block, index)]);
    };
    let mut all_frame = true;
    for (bytes, _) in &pieces {
        if !limits.frames(bytes)? {
            all_frame = false;
            break;
        }
    }
    if all_frame || limits.sure >= limits.raw {
        return Ok(pieces);
    }
    // A piece's body expands past the cap: cut where no body can.
    Ok(cut(&block, &index, numbering, limits.sure)?.unwrap_or(pieces))
}

/// A block's bytes with its index.
type Indexed = (Vec<u8>, BlobIndex);

/// One piece of a cut block, before its index.
struct Piece {
    bytes: Vec<u8>,
    /// Elements it holds, at least one.
    count: u64,
    /// Smallest and largest relation id it holds, for a relation block.
    relation_ids: Option<(i64, i64)>,
}

/// Fold one relation id into a piece's range.
fn widen(range: &mut Option<(i64, i64)>, id: i64) {
    *range = Some(match *range {
        None => (id, id),
        Some((lo, hi)) => (lo.min(id), hi.max(id)),
    });
}

/// One group field's item, as byte ranges into the block.
struct Item {
    group: usize,
    /// The group field number: 1 `Node`, 2 `DenseNodes`, 3 `Way`,
    /// 4 `Relation`, anything else a filler.
    field_num: u32,
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

/// `block` cut into pieces of at most `limit` bytes, each with its index, or
/// `None` when neither cut finds a partition.
fn cut(
    block: &[u8],
    index: &BlobIndex,
    numbering: Numbering,
    limit: usize,
) -> crate::Result<Option<Vec<Indexed>>> {
    let layout = layout(block)?;
    let read_back = matches!(numbering, Numbering::ReadBack);
    let pieces = match whole_table_cut(block, &layout, limit, read_back)? {
        Some(pieces) => pieces,
        None => match reencode::cut(block, &layout, limit, read_back)? {
            Some(pieces) => pieces,
            None => return Ok(None),
        },
    };
    index_pieces(pieces, index, numbering).map(Some)
}

/// The first cut: every piece repeats the whole context and copies its
/// items' bytes.
fn whole_table_cut(
    block: &[u8],
    layout: &Layout,
    limit: usize,
    read_back: bool,
) -> crate::Result<Option<Vec<Piece>>> {
    let counts: Vec<u64> = layout.items.iter().map(|it| it.count).collect();
    let sizes = Sizes::new(layout);
    let Some(cuts) = partition(&counts, |i, j| sizes.size(i, j), limit) else {
        return Ok(None);
    };
    let mut pieces = Vec::with_capacity(cuts.len().saturating_sub(1));
    for w in cuts.windows(2) {
        let (i, j) = (w[0], w[1]);
        let items = &layout.items[i..j];
        let mut relation_ids = None;
        if read_back {
            for item in items.iter().filter(|it| it.count > 0) {
                let id = super::wire_rewrite::relation_id(&block[item.payload.0..item.payload.1])?;
                widen(&mut relation_ids, id);
            }
        }
        pieces.push(Piece {
            bytes: assemble(block, layout, i, j),
            count: items.iter().map(|it| it.count).sum(),
            relation_ids,
        });
    }
    Ok(Some(pieces))
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
                let (min_id, max_id) = piece.relation_ids.ok_or_else(|| {
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
                field_num: 0,
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
            field_num: if count > 0 { field } else { 0 },
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

/// Cut points `0 = c0 < c1 < ... = n` over items whose element counts are
/// `counts`, such that every piece `[c_k, c_k+1)` fits `limit` by `size`
/// and holds an element, or `None` when no such cut exists. `size(i, j)`
/// must be monotone in both ends; it is called with both ends
/// non-decreasing from one call to the next, `i < j`.
fn partition(
    counts: &[u64],
    mut size: impl FnMut(usize, usize) -> usize,
    limit: usize,
) -> Option<Vec<usize>> {
    let n = counts.len();
    // `pred[j]`: the cut a piece ending at reachable cut `j` starts from.
    let mut pred: Vec<Option<usize>> = vec![None; n + 1];
    let mut reachable: Vec<usize> = vec![0];
    let mut lo = 0usize;
    let mut last_element: Option<usize> = None;
    for j in 1..=n {
        if counts[j - 1] > 0 {
            last_element = Some(j - 1);
        }
        while lo < j && size(lo, j) > limit {
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

    #[test]
    fn the_uncompressed_limit_leaves_the_body_under_the_cap() {
        let limits = Limits::new(Compression::None);
        let p = limits.raw;
        assert_eq!(limits.sure, p);
        let cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap();
        assert!(1 + varint_len(p) + p < cap);
        assert!(1 + varint_len(p + 1) + p + 1 >= cap);
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

    #[test]
    fn a_compressed_sure_limit_covers_the_worst_case_body() {
        let cap = MAX_BLOB_DATASIZE;
        for compression in [
            Compression::Zlib(crate::write::compression::ZlibLevel::new(1).unwrap()),
            Compression::Zstd(crate::write::compression::ZstdLevel::new(3).unwrap()),
        ] {
            let limits = Limits::new(compression);
            assert_eq!(limits.raw, usize::try_from(MAX_BLOB_MESSAGE_SIZE).unwrap());
            assert!(limits.sure < limits.raw);
            let worst = |p| crate::write::worst_case_body_len(p, &compression) as u64;
            assert!(worst(limits.sure) < cap);
            assert!(worst(limits.sure + 1) >= cap);
        }
    }

    /// Incompressible bytes at the raw limit: within it, yet their body is
    /// over the cap, so the trial says no where the raw check says yes.
    #[test]
    fn the_trial_refuses_a_payload_whose_body_expands_past_the_cap() {
        let limits = Limits::new(Compression::Zlib(
            crate::write::compression::ZlibLevel::new(1).unwrap(),
        ));
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let noise: Vec<u8> = (0..limits.raw)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        assert!(!limits.frames(&noise).unwrap());
        assert!(limits.frames(&noise[..limits.sure]).unwrap());
    }
}
