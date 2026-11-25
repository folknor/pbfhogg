//! Cut an encoded block that the writer would refuse as too large into
//! blocks that each fit, between elements, on its wire bytes.
//!
//! A block can pass the blob payload limit while every input blob it came
//! from framed: renumber's id splice widens varints,
//! add-locations-to-ways splices coordinates into every way, a command
//! that packs records from many input blobs into one builder cut by count
//! (the `sort` / `cat --dedupe` overlap sweep) can gather more bytes than
//! one blob holds, and the history `time-filter` re-encodes each input
//! blob's survivors, plus the version of an object straddling its end,
//! under a string table and metadata encoding of its own. A block carrying a field-5
//! way-member bitmap is also cut to keep its BlobHeader under its cap (see
//! [`fit_scanned`]). The block's own bytes are the
//! record of what it holds, so nothing is retained to recover it: the cut
//! reads the finished block, plans its pieces, and assembles them one at a
//! time. A block that fits is never touched beyond one length comparison;
//! the cut runs only for one over the limit.
//!
//! # Which limit
//!
//! The writer refuses a payload over the raw limit and a Blob body at or
//! over the datasize cap, and a compressed body can be larger than its
//! payload. [`Limits`] holds the raw limit and the largest payload whose
//! worst-case body still fits (the "sure" limit; the same for an
//! uncompressed blob). A block within the sure limit frames as it is; a
//! larger one is compressed once with the writer's own encoder
//! (`write::encoded_body_len`) to decide. The writer's refusal stays the
//! authority: should a trial and the writer ever disagree, the writer
//! refuses, it never writes a wrong blob.
//!
//! # What a piece holds: the first cut
//!
//! The first cut repeats the block's whole context in every piece: the
//! string table ahead of the groups and any top-level fields (granularity,
//! date granularity, offsets) after them, and copies every item's bytes
//! unchanged. That is the layout both renumber's rewriters and
//! `BlockBuilder` write. The groups are cut between items. An item is an
//! element (a `DenseNodes` message holding at least one node, a plain
//! `Node`, a `Way`, a `Relation`) or a filler: an empty `DenseNodes`, the
//! empty body of a group with no fields, or any other group field. A
//! `DenseNodes` message is one item here. Every piece holds at least one
//! element, so it has a real index, and fillers stay once, in order, on
//! whichever side of a cut makes the pieces fit.
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
//! bound with that property, so it can miss a partition the bound
//! over-counts. When neither finds a partition the block goes to the writer
//! whole, which refuses it with its typed size error.
//!
//! # A cut block's indexes
//!
//! Renumber numbers its pieces itself (`renumber/split.rs`). Every other
//! caller goes through [`fit_scanned`], which gives each piece the index and
//! tagdata the blob scan (`blob_meta::scan_block_ids_and_tags`, the scan
//! `cat` indexes an unindexed blob with) reads from the piece's own bytes:
//! its kind, id range, count and node bbox, and its tag keys, with no
//! tagdata for a piece that holds no tag, as the builder writes it.

mod reencode;

use protohoggr::{Cursor, WIRE_LEN};

use crate::blob_meta::BlobIndex;
use crate::block_builder::OwnedBlock;
use crate::error::Error;
use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_MESSAGE_SIZE};
use crate::write::compression::Compression;

/// The sizes a block is held to under one compression. See the module
/// documentation, "Which limit".
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    compression: Compression,
    /// The largest payload the writer accepts: the raw limit, inclusive,
    /// and for an uncompressed blob also what keeps the body under the cap.
    raw: usize,
    /// The largest payload whose worst-case body is under the cap.
    sure: usize,
}

impl Limits {
    pub(crate) fn new(compression: Compression) -> Self {
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

    /// A block at most this long frames without a trial.
    pub(crate) fn sure(&self) -> usize {
        self.sure
    }

    /// The largest payload the writer accepts.
    pub(crate) fn raw(&self) -> usize {
        self.raw
    }

    /// Whether the writer frames `bytes`: within the raw limit, and its body,
    /// encoded as the writer encodes it, under the cap.
    pub(crate) fn frames(&self, bytes: &[u8]) -> crate::Result<bool> {
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

/// One piece of a cut block, before its index.
pub(crate) struct Piece {
    pub(crate) bytes: Vec<u8>,
    /// Elements it holds, at least one.
    pub(crate) count: u64,
}

/// One group field's item, as byte ranges into the block.
pub(crate) struct Item {
    group: usize,
    /// The group field number: 1 `Node`, 2 `DenseNodes`, 3 `Way`,
    /// 4 `Relation`; 0 for a filler.
    pub(crate) field_num: u32,
    /// The whole field, tag included.
    pub(crate) field: (usize, usize),
    /// The field's payload.
    pub(crate) payload: (usize, usize),
    /// Elements this item holds; 0 for a filler.
    pub(crate) count: u64,
}

impl Item {
    pub(crate) fn len(&self) -> usize {
        self.field.1 - self.field.0
    }
}

/// A block, taken apart.
pub(crate) struct Layout {
    /// Bytes ahead of the first group and after the last.
    pub(crate) prefix: (usize, usize),
    pub(crate) suffix: (usize, usize),
    pub(crate) items: Vec<Item>,
    /// Per group, its first item and one past its last.
    pub(crate) groups: Vec<(usize, usize)>,
}

fn wire_err(e: protohoggr::WireError) -> Error {
    Error::internal_with("block cut: re-reading an encoded block", e)
}

/// Take a block apart. Its layout is the one this crate's encoders write:
/// the string table, the groups, any top-level fields after them. A
/// top-level field between groups is an encoder bug.
pub(crate) fn layout(block: &[u8]) -> crate::Result<Layout> {
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
            return Err(Error::internal("block cut: a block field among its groups"));
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
        first_group.ok_or_else(|| Error::internal("block cut: an encoded block without groups"))?;
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

/// Nodes in a `DenseNodes`: the varints of its packed id field.
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

pub(crate) fn varint_len(mut v: usize) -> usize {
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
/// `counts`, such that every piece `[c_k, c_k+1)` fits `limit` by `size`,
/// holds at most `max_count` elements and holds an element, or `None` when
/// no such cut exists. `size(i, j)` must be monotone in both ends; it is
/// called with both ends non-decreasing from one call to the next, `i < j`.
/// The element count is monotone too, so both bounds move the same lower
/// end before a predecessor is chosen.
fn partition(
    counts: &[u64],
    mut size: impl FnMut(usize, usize) -> usize,
    limit: usize,
    max_count: u64,
) -> Option<Vec<usize>> {
    let n = counts.len();
    let mut held = Vec::with_capacity(n + 1);
    held.push(0u64);
    for &c in counts {
        held.push(held[held.len() - 1].saturating_add(c));
    }
    let over = |i: usize, j: usize, size: &mut dyn FnMut(usize, usize) -> usize| {
        held[j] - held[i] > max_count || size(i, j) > limit
    };
    // `pred[j]`: the cut a piece ending at reachable cut `j` starts from.
    let mut pred: Vec<Option<usize>> = vec![None; n + 1];
    let mut reachable: Vec<usize> = vec![0];
    let mut lo = 0usize;
    let mut last_element: Option<usize> = None;
    for j in 1..=n {
        if counts[j - 1] > 0 {
            last_element = Some(j - 1);
        }
        while lo < j && over(lo, j, &mut size) {
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

/// The block bytes of the first-cut piece holding items `[i, j)`.
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

/// A planned cut: which pieces, not yet assembled. Each piece is built
/// only when asked for, so a caller can hold one at a time.
pub(crate) struct Plan<'b> {
    how: How<'b>,
    cuts: Vec<usize>,
    limit: usize,
}

enum How<'b> {
    /// The first cut: items copied beside the whole context.
    Whole { block: &'b [u8], layout: &'b Layout },
    /// The re-encoding cut.
    Reencode(reencode::Cut<'b>),
}

impl Plan<'_> {
    /// How many pieces.
    pub(crate) fn len(&self) -> usize {
        self.cuts.len().saturating_sub(1)
    }

    /// Piece `k`, in block order.
    pub(crate) fn piece(&self, k: usize) -> crate::Result<Piece> {
        let (i, j) = (self.cuts[k], self.cuts[k + 1]);
        match &self.how {
            How::Whole { block, layout } => Ok(Piece {
                bytes: assemble(block, layout, i, j),
                count: layout.items[i..j].iter().map(|it| it.count).sum(),
            }),
            How::Reencode(cut) => cut.piece(i, j, self.limit),
        }
    }
}

/// A plan cutting `block` (taken apart as `layout`) into pieces of at most
/// `limit` bytes and `max_count` elements: the first cut where it finds a
/// partition, else the re-encoding cut, else `None`.
pub(crate) fn plan<'b>(
    block: &'b [u8],
    layout: &'b Layout,
    limit: usize,
    max_count: u64,
) -> Option<Plan<'b>> {
    let counts: Vec<u64> = layout.items.iter().map(|it| it.count).collect();
    let sizes = Sizes::new(layout);
    if let Some(cuts) = partition(&counts, |i, j| sizes.size(i, j), limit, max_count) {
        return Some(Plan {
            how: How::Whole { block, layout },
            cuts,
            limit,
        });
    }
    let (cut, cuts) = reencode::plan(block, layout, limit, max_count)?;
    Some(Plan {
        how: How::Reencode(cut),
        cuts,
        limit,
    })
}

/// Hand `block` to `emit` as it is when the writer frames it under
/// `limits`, and otherwise as the pieces of a cut, in order, one at a time,
/// each with the index and tagdata scanned from its own bytes (see the
/// module documentation). `emit`'s second argument is what the caller
/// holds besides the block it is given: 0 for a block passed whole, the
/// cut block's length for a piece. A block the cut finds no fitting plan
/// for goes to `emit` whole and the writer decides: it refuses with its
/// typed size error a block over a hard limit, and frames one the cut's
/// conservative bounds only suspected; a plan is chosen and checked before
/// its first piece is emitted.
///
/// A block carrying a `pbfhogg.WayMembers-v1` payload (BlobHeader field 5,
/// one bit per way in file order) is held to one more bound: the header
/// must stay under its cap, and the bitmap cannot be dropped, so a block
/// with more than [`MAX_WAY_MEMBER_WAYS`] ways is cut even when its payload
/// fits. Each piece's ways are a contiguous run of the block's, in order,
/// so each piece carries the bits of its own run, repacked from bit zero.
/// The pin bitmaps (way field 20) sit inside the way messages and travel
/// with their bytes.
pub(crate) fn fit_scanned(
    block: OwnedBlock,
    limits: &Limits,
    mut emit: impl FnMut(OwnedBlock, usize) -> crate::Result<()>,
) -> crate::Result<()> {
    let count_fits = block.way_members.is_none() || block.index.count <= MAX_WAY_MEMBER_WAYS;
    if count_fits && limits.frames(&block.bytes)? {
        return emit(block, 0);
    }
    fit_scanned_cold(block, limits, &mut emit)
}

/// The most ways a block carrying a field-5 payload may hold. The
/// BlobHeader must stay strictly under `MAX_BLOB_HEADER_SIZE` (64 KiB), and
/// besides the bitmap it holds the `OSMData` type field (9 bytes), the
/// datasize field (at most 6), the indexdata field (44) and the field-5
/// head (tag, length varint, version, count varint: at most 8); tagdata is
/// dropped by the writer when it does not fit. 128 bytes of the cap are
/// kept for those, the rest is bitmap.
pub(crate) const MAX_WAY_MEMBER_WAYS: u64 =
    (crate::read::blob_wire::MAX_BLOB_HEADER_SIZE - 128) * 8;

/// A block's own field-5 payload, read back: its bitmap, after checking it
/// is version 1, counts `count` ways, and is `ceil(count / 8)` bytes with
/// no bit set past the last way. The payload is this crate's, so anything
/// else is an encoder bug.
fn way_member_bits(payload: &[u8], count: u64) -> crate::Result<&[u8]> {
    let bad = |why: &str| {
        Err(Error::internal(format!(
            "block cut: way members payload {why}"
        )))
    };
    if payload.first() != Some(&1) {
        return bad("is not version 1");
    }
    let mut cursor = Cursor::new(&payload[1..]);
    let declared = cursor.read_varint().map_err(wire_err)?;
    if declared != count {
        return bad("counts other than the block's ways");
    }
    let bits = &payload[1 + cursor.position()..];
    if usize::try_from(count.div_ceil(8)) != Ok(bits.len()) {
        return bad("has a bitmap of the wrong length");
    }
    if count % 8 != 0 && bits.last().is_some_and(|&b| b >> (count % 8) != 0) {
        return bad("sets a bit past the last way");
    }
    Ok(bits)
}

/// The field-5 payload for ways `[start, start + count)` of `bits`.
fn way_member_slice(bits: &[u8], start: u64, count: u64) -> Vec<u8> {
    let mut payload = vec![1];
    protohoggr::encode_varint(&mut payload, count);
    let first = payload.len();
    payload.resize(first + usize::try_from(count.div_ceil(8)).unwrap_or(0), 0);
    for k in 0..count {
        let at = start + k;
        let set = usize::try_from(at / 8)
            .ok()
            .and_then(|byte| bits.get(byte))
            .is_some_and(|&b| b >> (at % 8) & 1 == 1);
        if set {
            let k = usize::try_from(k).unwrap_or(usize::MAX);
            payload[first + k / 8] |= 1 << (k % 8);
        }
    }
    payload
}

#[cold]
#[inline(never)]
fn fit_scanned_cold(
    block: OwnedBlock,
    limits: &Limits,
    emit: &mut dyn FnMut(OwnedBlock, usize) -> crate::Result<()>,
) -> crate::Result<()> {
    let chosen = cut(
        &block.bytes,
        block.index.kind,
        block.index.count,
        block.way_members.as_deref(),
        limits,
        emit,
    )?;
    if chosen { Ok(()) } else { emit(block, 0) }
}

/// Fit every block of `blocks` in place (see [`fit_scanned`]): the vector
/// is left untouched when every block frames, and otherwise rebuilt from
/// the first block that does not, its pieces in its place. For a worker
/// task that returns its owned blocks for a consumer to write, so the cut
/// runs on the worker and a failure inside it is located with the task's
/// input blob. A block no plan fits stays whole, for the writer to refuse.
pub(crate) fn fit_blocks(blocks: &mut Vec<OwnedBlock>, limits: &Limits) -> crate::Result<()> {
    let mut first = None;
    for (k, block) in blocks.iter().enumerate() {
        let count_fits = block.way_members.is_none() || block.index.count <= MAX_WAY_MEMBER_WAYS;
        if !(count_fits && limits.frames(&block.bytes)?) {
            first = Some(k);
            break;
        }
    }
    let Some(first) = first else {
        return Ok(());
    };
    let rest = blocks.split_off(first);
    for block in rest {
        fit_scanned(block, limits, |piece, _| {
            blocks.push(piece);
            Ok(())
        })?;
    }
    Ok(())
}

/// Cut a block lent as bytes (a `BlockBuilder::lend_encoded` block, which
/// carries no field-5 payload) into pieces that each frame under `limits`,
/// handing each to `emit` with its scanned index and tagdata. Returns
/// `false`, having emitted nothing, when no plan fits; the caller then
/// keeps the writer's refusal.
#[cold]
#[inline(never)]
pub(crate) fn cut_lent(
    bytes: &[u8],
    kind: crate::ElementKind,
    count: u64,
    limits: &Limits,
    emit: &mut dyn FnMut(OwnedBlock) -> crate::Result<()>,
) -> crate::Result<bool> {
    cut(bytes, kind, count, None, limits, &mut |piece, _| {
        emit(piece)
    })
}

/// The cut behind [`fit_scanned`] and [`cut_lent`], over borrowed bytes:
/// `true` when a plan was chosen and every piece emitted, `false` (nothing
/// emitted) when none fits. `emit`'s second argument is the cut block's
/// length, as [`fit_scanned`] passes it.
pub(crate) fn cut(
    bytes: &[u8],
    kind: crate::ElementKind,
    count: u64,
    way_members: Option<&[u8]>,
    limits: &Limits,
    emit: &mut dyn FnMut(OwnedBlock, usize) -> crate::Result<()>,
) -> crate::Result<bool> {
    let (bits, max_count) = match way_members {
        Some(payload) => {
            if kind != crate::ElementKind::Way {
                return Err(Error::internal(
                    "block cut: a block of other elements carries way members",
                ));
            }
            (Some(way_member_bits(payload, count)?), MAX_WAY_MEMBER_WAYS)
        }
        None => (None, u64::MAX),
    };
    let held = bytes.len();
    let layout = layout(bytes)?;
    let Some(plan) = choose(bytes, &layout, limits, max_count)? else {
        return Ok(false);
    };
    let mut total = 0u64;
    for k in 0..plan.len() {
        let piece = plan.piece(k)?;
        let members = bits.map(|bits| way_member_slice(bits, total, piece.count));
        total += piece.count;
        emit(scanned(piece, kind, members)?, held)?;
    }
    if total != count {
        return Err(Error::internal(format!(
            "block cut: a block of {count} elements was cut into pieces holding {total}"
        )));
    }
    Ok(true)
}

/// The plan every piece of which frames: one at the sure limit, which
/// needs no trial; else one at the raw limit whose every piece a trial
/// passes; else `None`.
fn choose<'b>(
    block: &'b [u8],
    layout: &'b Layout,
    limits: &Limits,
    max_count: u64,
) -> crate::Result<Option<Plan<'b>>> {
    if let Some(plan) = plan(block, layout, limits.sure, max_count) {
        return Ok(Some(plan));
    }
    if limits.sure >= limits.raw {
        return Ok(None);
    }
    let Some(plan) = plan(block, layout, limits.raw, max_count) else {
        return Ok(None);
    };
    for k in 0..plan.len() {
        if !limits.frames(&plan.piece(k)?.bytes)? {
            return Ok(None);
        }
    }
    Ok(Some(plan))
}

/// A piece with the index and tagdata its own bytes scan to, and its slice
/// of the block's field-5 payload.
fn scanned(
    piece: Piece,
    kind: crate::ElementKind,
    way_members: Option<Vec<u8>>,
) -> crate::Result<OwnedBlock> {
    let (index, tags) = crate::blob_meta::scan_block_ids_and_tags(&piece.bytes)?;
    let index: BlobIndex =
        index.ok_or_else(|| Error::internal("block cut: a piece no single index describes"))?;
    if index.kind != kind || index.count != piece.count {
        return Err(Error::internal(format!(
            "block cut: a piece of {} {} scanned as {} {}",
            piece.count,
            kind.as_str(),
            index.count,
            index.kind.as_str()
        )));
    }
    let tagdata = tags.filter(|t| !t.keys_empty()).and_then(|t| t.serialize());
    Ok(OwnedBlock {
        bytes: piece.bytes,
        index,
        tagdata,
        way_members,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::block_builder::{BlockBuilder, MemberData};

    #[test]
    fn the_uncompressed_limit_leaves_the_body_under_the_cap() {
        let limits = Limits::new(Compression::None);
        let p = limits.raw;
        assert_eq!(limits.sure, p);
        let cap = usize::try_from(MAX_BLOB_DATASIZE).unwrap();
        assert!(1 + varint_len(p) + p < cap);
        assert!(1 + varint_len(p + 1) + p + 1 >= cap);
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

    /// Limits at a small byte size, uncompressed, so a test block of a few
    /// kilobytes is over them.
    fn small(limit: usize) -> Limits {
        Limits {
            compression: Compression::None,
            raw: limit,
            sure: limit,
        }
    }

    fn fit_all(block: OwnedBlock, limits: &Limits) -> Vec<(OwnedBlock, usize)> {
        let mut out = Vec::new();
        fit_scanned(block, limits, |b, held| {
            out.push((b, held));
            Ok(())
        })
        .unwrap();
        out
    }

    fn relations(values: &[(i64, usize)]) -> OwnedBlock {
        let mut bb = BlockBuilder::new();
        for &(id, len) in values {
            let value = "v".repeat(len) + &id.to_string();
            let members = [MemberData {
                id: crate::MemberId::Node(id * 10),
                role: "outer",
            }];
            bb.add_relation(id, [("name", value.as_str())], &members, None);
        }
        bb.take_owned().unwrap().unwrap()
    }

    fn decoded(bytes: &[u8]) -> Vec<String> {
        crate::PrimitiveBlock::from_vec(bytes.to_vec())
            .unwrap()
            .elements()
            .map(|e| match e {
                crate::Element::Relation(r) => format!(
                    "r{} {:?} {:?}",
                    r.id(),
                    r.tags().collect::<Vec<_>>(),
                    r.members()
                        .map(|m| (m.id, m.role().unwrap().to_owned()))
                        .collect::<Vec<_>>()
                ),
                crate::Element::DenseNode(n) => format!(
                    "n{} {} {} {:?}",
                    n.id(),
                    n.decimicro_lat().unwrap(),
                    n.decimicro_lon().unwrap(),
                    n.tags().collect::<Vec<_>>()
                ),
                _ => String::from("another element kind"),
            })
            .collect()
    }

    #[test]
    fn a_block_that_frames_is_handed_on_unchanged() {
        let block = relations(&[(1, 10), (2, 10)]);
        let bytes = block.bytes.clone();
        let out = fit_all(block, &small(bytes.len()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.bytes, bytes);
        assert_eq!(out[0].1, 0);
    }

    #[test]
    fn an_oversized_block_is_cut_into_scanned_pieces_in_order() {
        let block = relations(&[(5, 300), (7, 300), (9, 300), (12, 300)]);
        let whole = decoded(&block.bytes);
        let held = block.bytes.len();
        let limit = held / 2;
        let out = fit_all(block, &small(limit));
        assert!(out.len() >= 2, "{} pieces", out.len());
        let mut seen = Vec::new();
        let mut last_max = i64::MIN;
        for (piece, h) in &out {
            assert_eq!(*h, held);
            assert!(piece.bytes.len() <= limit);
            assert_eq!(piece.index.kind, crate::ElementKind::Relation);
            assert!(piece.index.min_id > last_max);
            last_max = piece.index.max_id;
            let (index, _) = crate::blob_meta::scan_block_ids_and_tags(&piece.bytes).unwrap();
            assert_eq!(index, Some(piece.index));
            assert!(piece.tagdata.is_some(), "a tagged piece carries tagdata");
            seen.extend(decoded(&piece.bytes));
        }
        assert_eq!(seen, whole);
    }

    #[test]
    fn fit_blocks_cuts_only_what_does_not_frame_and_keeps_the_order() {
        let a = relations(&[(1, 10), (2, 10)]);
        let b = relations(&[(5, 300), (7, 300), (9, 300), (12, 300)]);
        let c = relations(&[(20, 10)]);
        let limit = b.bytes.len() / 2;
        let whole: Vec<String> = [&a, &b, &c]
            .iter()
            .flat_map(|x| decoded(&x.bytes))
            .collect();
        let (a_bytes, c_bytes) = (a.bytes.clone(), c.bytes.clone());
        let mut fitting = vec![relations(&[(1, 10)]), relations(&[(2, 10)])];
        let untouched: Vec<Vec<u8>> = fitting.iter().map(|x| x.bytes.clone()).collect();
        fit_blocks(&mut fitting, &small(limit)).unwrap();
        assert_eq!(
            fitting.iter().map(|x| x.bytes.clone()).collect::<Vec<_>>(),
            untouched
        );

        let mut blocks = vec![a, b, c];
        fit_blocks(&mut blocks, &small(limit)).unwrap();
        assert!(blocks.len() >= 4, "{} blocks", blocks.len());
        assert_eq!(blocks[0].bytes, a_bytes);
        assert_eq!(blocks.last().unwrap().bytes, c_bytes);
        assert!(blocks.iter().all(|x| x.bytes.len() <= limit));
        let seen: Vec<String> = blocks.iter().flat_map(|x| decoded(&x.bytes)).collect();
        assert_eq!(seen, whole);
    }

    #[test]
    fn node_pieces_carry_their_own_bbox() {
        let mut bb = BlockBuilder::new();
        let pad = "x".repeat(200);
        for k in 0..8i64 {
            let lat = i32::try_from(k * 1000).unwrap();
            let value = format!("{pad}{k}");
            bb.add_node(k + 1, lat, -lat, [("name", value.as_str())], None);
        }
        let block = bb.take_owned().unwrap().unwrap();
        let whole = decoded(&block.bytes);
        let limit = block.bytes.len() / 3;
        let out = fit_all(block, &small(limit));
        assert!(out.len() >= 3);
        let mut seen = Vec::new();
        for (piece, _) in &out {
            let bbox = piece.index.bbox.unwrap();
            let (lo, hi) = (piece.index.min_id - 1, piece.index.max_id - 1);
            let lo = i32::try_from(lo * 1000).unwrap();
            let hi = i32::try_from(hi * 1000).unwrap();
            assert_eq!(bbox, crate::blob_meta::BlobBbox::new(lo, hi, -hi, -lo));
            seen.extend(decoded(&piece.bytes));
        }
        assert_eq!(seen, whole);
    }

    #[test]
    fn a_block_with_no_fitting_plan_is_handed_on_whole() {
        let block = relations(&[(1, 10), (2, 2000)]);
        let bytes = block.bytes.clone();
        let out = fit_all(block, &small(1000));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.bytes, bytes);
        assert_eq!(out[0].1, 0);
    }

    #[test]
    fn a_block_of_other_elements_carrying_way_members_is_refused_at_the_cut() {
        let mut block = relations(&[(1, 300), (2, 300)]);
        block.way_members = Some(way_member_slice(&[0], 0, 2));
        let limit = block.bytes.len() / 2;
        let err = fit_scanned(block, &small(limit), |_, _| Ok(()))
            .err()
            .unwrap();
        assert!(err.to_string().contains("way members"), "{err}");
    }

    /// A field-5 payload whose bit `k` is `member(k)`.
    fn members_payload(count: u64, member: impl Fn(u64) -> bool) -> Vec<u8> {
        let mut bits = vec![0u8; usize::try_from(count.div_ceil(8)).unwrap()];
        for k in 0..count {
            if member(k) {
                bits[(k / 8) as usize] |= 1 << (k % 8);
            }
        }
        way_member_slice(&bits, 0, count)
    }

    /// The membership bits of a sequence of pieces, concatenated.
    fn piece_bits(out: &[(OwnedBlock, usize)]) -> Vec<bool> {
        let mut seen = Vec::new();
        for (piece, _) in out {
            let payload = piece.way_members.as_deref().unwrap();
            let bits = way_member_bits(payload, piece.index.count).unwrap();
            for k in 0..piece.index.count {
                seen.push(bits[(k / 8) as usize] >> (k % 8) & 1 == 1);
            }
        }
        seen
    }

    #[test]
    fn an_oversized_way_block_carries_each_piece_its_own_member_bits() {
        let mut bb = BlockBuilder::new();
        let pad = "w".repeat(200);
        for id in 1..=11i64 {
            let value = format!("{pad}{id}");
            bb.add_way(
                id,
                [("name", value.as_str())],
                &[id * 10, id * 10 + 1],
                None,
            );
        }
        let mut block = bb.take_owned().unwrap().unwrap();
        let member = |k: u64| k % 3 == 0 || k == 10;
        block.way_members = Some(members_payload(11, member));
        let limit = block.bytes.len() / 4;
        let out = fit_all(block, &small(limit));
        assert!(out.len() >= 4, "{} pieces", out.len());
        assert!(
            out.iter().any(|(p, _)| p.index.count % 8 != 0),
            "a piece boundary that is not byte-aligned"
        );
        let want: Vec<bool> = (0..11).map(member).collect();
        assert_eq!(piece_bits(&out), want);
    }

    /// A way block of `count` bare ways, ids 1..=count, in one group.
    fn bare_ways(count: u64) -> Vec<u8> {
        let mut group = Vec::new();
        let mut way = Vec::new();
        for id in 1..=count {
            way.clear();
            protohoggr::encode_tag(&mut way, 1, protohoggr::WIRE_VARINT);
            protohoggr::encode_varint(&mut way, id);
            protohoggr::encode_tag(&mut group, 3, WIRE_LEN);
            protohoggr::encode_varint(&mut group, way.len() as u64);
            group.extend_from_slice(&way);
        }
        let mut block = Vec::new();
        // An empty string table: one empty string.
        protohoggr::encode_tag(&mut block, 1, WIRE_LEN);
        protohoggr::encode_varint(&mut block, 2);
        protohoggr::encode_tag(&mut block, 1, WIRE_LEN);
        protohoggr::encode_varint(&mut block, 0);
        protohoggr::encode_tag(&mut block, 2, WIRE_LEN);
        protohoggr::encode_varint(&mut block, group.len() as u64);
        block.extend_from_slice(&group);
        block
    }

    fn frames_header(piece: &OwnedBlock) -> bool {
        let mut buf = Vec::new();
        crate::write::encode_blob_header_into(
            "OSMData",
            i32::MAX,
            Some(&piece.index.serialize()),
            None,
            piece.way_members.as_deref(),
            &mut buf,
        )
        .is_ok()
    }

    /// The header bound alone: a body far under the payload limit whose
    /// bitmap would not fit one BlobHeader is cut by way count.
    #[test]
    fn a_block_whose_bitmap_overflows_the_header_is_cut_by_way_count() {
        let count = MAX_WAY_MEMBER_WAYS + 1001;
        let bytes = bare_ways(count);
        let (index, _) = crate::blob_meta::scan_block_ids_and_tags(&bytes).unwrap();
        let member = |k: u64| k % 7 == 2;
        let block = OwnedBlock {
            bytes,
            index: index.unwrap(),
            tagdata: None,
            way_members: Some(members_payload(count, member)),
        };
        assert!(
            !frames_header(&block),
            "the fixture must overflow the header"
        );
        let limits = Limits::new(Compression::None);
        assert!(limits.frames(&block.bytes).unwrap());
        let out = fit_all(block, &limits);
        assert!(out.len() >= 2, "{} pieces", out.len());
        for (piece, _) in &out {
            assert!(piece.index.count <= MAX_WAY_MEMBER_WAYS);
            assert!(frames_header(piece));
        }
        let want: Vec<bool> = (0..count).map(member).collect();
        assert_eq!(piece_bits(&out), want);
    }

    /// The ceiling is safe: a header at it, with the widest datasize and
    /// indexdata, frames.
    #[test]
    fn a_header_at_the_way_ceiling_frames() {
        let piece = OwnedBlock {
            bytes: Vec::new(),
            index: BlobIndex {
                kind: crate::ElementKind::Way,
                min_id: i64::MIN,
                max_id: i64::MAX,
                count: MAX_WAY_MEMBER_WAYS,
                bbox: None,
            },
            tagdata: None,
            way_members: Some(members_payload(MAX_WAY_MEMBER_WAYS, |_| true)),
        };
        assert!(frames_header(&piece));
    }
}
