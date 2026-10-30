//! The overlap-run sweep: one run's entries merged into type-then-ID order
//! by a k-way merge over borrowed cursors, re-encoded, framed on the sweep's
//! own worker and streamed to the write loop in byte-bounded chunks.
//!
//! # Shape
//!
//! Each entry of the run, once the merge reaches it, becomes a [`Cursor`]
//! over its decoded block. An ordinary entry is read lazily, element by
//! element, straight out of the block; an entry flagged internally unsorted
//! or restructured is first collected into a vector of borrowed element
//! views of the run's kind, stably sorted by OSM ID. A small heap orders the
//! cursors by the OSM ID of their next element. No element is copied into an
//! owned form on the common path: survivors are re-encoded from the borrowed
//! view, and duplicates are dropped after validation.
//!
//! The frontier rule is the one an owned-element heap sweep would use: before
//! the merge opens entry `j` it emits every element whose ID sorts strictly
//! below `j`'s first ID, since no entry from `j` on can hold such an element;
//! once every entry is open it drains. An ID is emitted as one group: every
//! element of that ID is pulled from every cursor before any is written, the
//! group is ordered by version, and under dedupe one element per version
//! survives. The survivor is the copy from the earliest input, then the
//! earliest frame in that input, then the earliest in that frame
//! ([`Member::tie`]); the group is written in (version, tie) order, so the
//! output order is (OSM ID, version), as before.
//!
//! # Validation
//!
//! A dropped duplicate is validated as a written element would be: its user
//! name, and a relation's member roles, are resolved, so a malformed loser
//! fails the run exactly where a malformed survivor does. An entry placed by
//! its believed indexdata claim is held to it: an element of a foreign kind
//! is refused when the block is opened, and the count and exact numeric
//! range are checked when the cursor is exhausted, which every cursor is
//! before its run ends. A lazily read cursor must also not descend
//! in OSM order: the merge emits its elements in the order it reads them, so
//! a descent would be written out of order. A failed run fails the command,
//! whose output is published only on success.
//!
//! # Memory
//!
//! A cursor pins its decoded block while it has elements left. A block that
//! is mostly consumed but keeps a sparse tail far ahead (a layout `sort`
//! accepts) would pin the whole block for a handful of elements, so when the
//! run's pinned bytes pass [`SWEEP_PINNED_BYTES`] the sweep demotes every
//! cursor with less than an eighth of its elements left: the rest of the
//! block is validated, copied into owned elements and the block released.
//! The scan is amortized (at most once per half budget of growth). A dense
//! giant component still holds its live overlap, as any in-memory merge must.
//!
//! Each borrowed element carries an [`Rc`] of the block it borrows from, so a
//! block lives exactly as long as the cursor reading it or any element taken
//! from it; an exhausted cursor releases its block at once.
//!
//! Output: every block the builder fills is framed (compressed) here, on the
//! sweep's worker, and frames are packed into chunks of at most
//! [`OVERLAP_RUN_CHUNK_BYTES`] of capacity (a single larger frame travels
//! alone), sent through the run's channel of
//! [`OVERLAP_RUN_CHANNEL_DEPTH`] messages. A run therefore holds at most
//! that many chunks queued plus the one it is building, whatever its size.
//!
//! [`SWEEP_PINNED_BYTES`]: crate::tunables::SWEEP_PINNED_BYTES
//! [`OVERLAP_RUN_CHUNK_BYTES`]: crate::tunables::OVERLAP_RUN_CHUNK_BYTES
//! [`OVERLAP_RUN_CHANNEL_DEPTH`]: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::rc::Rc;

use super::{BlobEntry, Frame, RunCounts, element_id, read_block};
use crate::blob_meta::IdTally;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::error::Result;
use crate::osm_id::{OsmIdKey, osm_id_cmp, osm_id_key};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata, read_dense_node,
    read_node, read_relation, read_way, relation_members_into, write_single_node_local,
    write_single_relation_local, write_single_way_local,
};
use crate::read::block::BlockElementsIter;
use crate::read::input::InputFile;
use crate::write::metrics::WriterMetrics;
use crate::writer::{Compression, frame_blob_pipelined};
use crate::{Element, ElementKind, PrimitiveBlock};

/// One run's chunk of framed output blobs, written in order.
pub(super) type Chunk = Vec<Vec<u8>>;

/// One overlap run's sweep, on its worker.
pub(super) struct Sweep<'a, 'e> {
    pub(super) entries: &'a [BlobEntry],
    pub(super) frames: &'a [Frame],
    pub(super) sources: &'a [InputFile],
    pub(super) kind: ElementKind,
    pub(super) dedupe: bool,
    pub(super) cancel: &'a crate::host::Recorder,
    pub(super) emitter: &'a crate::ordered_pool::Emitter<'e, Chunk, RunCounts>,
    pub(super) compression: &'a Compression,
    pub(super) metrics: &'a WriterMetrics,
}

// ---------------------------------------------------------------------------
// Items: borrowed or owned elements
// ---------------------------------------------------------------------------

/// A borrowed element and the block it borrows from.
///
/// The lifetime of `elem` is erased to `'static`; it really borrows the
/// block `owner` keeps alive. Field order matters: `elem` is declared, and so
/// dropped, first. A `Held` never leaves this module, and nothing hands out
/// the element with a lifetime longer than a borrow of the `Held`.
struct Held {
    elem: Element<'static>,
    _owner: Rc<PrimitiveBlock>,
}

/// One element of the run's kind, as the merge carries it.
enum Item {
    Held(Held),
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

impl Item {
    fn id(&self) -> i64 {
        match self {
            Item::Held(h) => element_id(&h.elem),
            Item::Node(n) => n.id,
            Item::Way(w) => w.id,
            Item::Relation(r) => r.id,
        }
    }

    /// The identity version: absent metadata (and the dense -1 sentinel) is
    /// 0, the mapping the owned conversions use.
    fn version(&self) -> i32 {
        match self {
            Item::Held(h) => borrowed_version(&h.elem),
            Item::Node(n) => n.metadata.as_ref().map_or(0, |m| m.version),
            Item::Way(w) => w.metadata.as_ref().map_or(0, |m| m.version),
            Item::Relation(r) => r.metadata.as_ref().map_or(0, |m| m.version),
        }
    }

    /// Own a borrowed item (demotion); an owned one is returned unchanged.
    /// Resolves the user and roles, so it refuses what a written element
    /// would.
    fn into_owned(self) -> Result<Item> {
        let Item::Held(h) = self else {
            return Ok(self);
        };
        Ok(match &h.elem {
            Element::DenseNode(dn) => Item::Node(read_dense_node(dn)?),
            Element::Node(n) => Item::Node(read_node(n)?),
            Element::Way(w) => Item::Way(read_way(w)?),
            Element::Relation(r) => Item::Relation(read_relation(r)?),
        })
    }
}

fn borrowed_version(elem: &Element<'_>) -> i32 {
    match elem {
        Element::DenseNode(dn) => match dn.info() {
            Some(info) if info.version() != -1 => info.version(),
            _ => 0,
        },
        Element::Node(n) => n.info().version().unwrap_or(0),
        Element::Way(w) => w.info().version().unwrap_or(0),
        Element::Relation(r) => r.info().version().unwrap_or(0),
    }
}

fn of_kind(elem: &Element<'_>, kind: ElementKind) -> bool {
    matches!(
        (elem, kind),
        (Element::DenseNode(_) | Element::Node(_), ElementKind::Node)
            | (Element::Way(_), ElementKind::Way)
            | (Element::Relation(_), ElementKind::Relation)
    )
}

/// Erase a block iterator's lifetime.
///
/// # Safety
///
/// The caller keeps the block the iterator borrows alive (behind an `Rc`,
/// so at a stable address) for as long as the iterator and every element it
/// yields exist.
unsafe fn erase_iter(iter: BlockElementsIter<'_>) -> BlockElementsIter<'static> {
    // SAFETY: only the lifetime parameter changes; the caller upholds the
    // liveness contract above.
    unsafe { std::mem::transmute::<BlockElementsIter<'_>, BlockElementsIter<'static>>(iter) }
}

/// Erase an element's lifetime. Same contract as [`erase_iter`].
unsafe fn erase_elem(elem: Element<'_>) -> Element<'static> {
    // SAFETY: as for `erase_iter`.
    unsafe { std::mem::transmute::<Element<'_>, Element<'static>>(elem) }
}

// ---------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------

/// Where a cursor's elements come from.
enum Feed {
    /// Read in block order (an ordinary entry).
    Lazy(Box<BlockElementsIter<'static>>),
    /// Pre-sorted views (an internally unsorted or restructured entry).
    Sorted(std::vec::IntoIter<Element<'static>>),
    /// Owned after demotion, already validated.
    Owned(VecDeque<Item>),
    Done,
}

/// The claim an entry placed by its believed indexdata is held to, and the
/// order a lazily read cursor must keep.
struct Check {
    claim: Option<crate::blob_meta::BlobIndex>,
    tally: IdTally,
    /// Lazy feeds only: the previous ID, which the next must not sort below.
    last: Option<i64>,
    ordered: bool,
}

impl Check {
    fn visit(&mut self, id: i64) -> Result<()> {
        if self.ordered {
            if let Some(last) = self.last
                && osm_id_cmp(id, last).is_lt()
            {
                return Err(crate::error::new_unsorted(format!(
                    "the blob is internally out of order (id {id} after {last}) although \
                     the merge reads it in order"
                )));
            }
            self.last = Some(id);
        }
        if self.claim.is_some() {
            self.tally.add(id);
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        match &self.claim {
            Some(claim) => self.tally.refuse_inexact(claim),
            None => Ok(()),
        }
    }
}

struct Cursor {
    /// Declared before `owner`, so dropped before the block it borrows.
    feed: Feed,
    head: Option<Item>,
    owner: Option<Rc<PrimitiveBlock>>,
    check: Check,
    kind: ElementKind,
    /// (input, frame offset): where errors are located and what ranks this
    /// cursor's copies against other inputs'.
    source: usize,
    offset: u64,
    /// Ordinal of `head` within this cursor.
    occurrence: u64,
    /// Elements of the kind pulled so far, and the block's total.
    pulled: u64,
    total: u64,
    /// Decoded bytes this cursor pins (0 once released).
    pinned: usize,
}

impl Cursor {
    /// Pull the next element of the kind into `head`; release the block when
    /// the feed ends.
    fn advance(&mut self) -> Result<()> {
        loop {
            let next = match &mut self.feed {
                Feed::Lazy(iter) => iter.next(),
                Feed::Sorted(iter) => iter.next(),
                Feed::Owned(queue) => {
                    self.head = queue.pop_front();
                    self.occurrence += 1;
                    if self.head.is_none() {
                        self.feed = Feed::Done;
                    }
                    return Ok(());
                }
                Feed::Done => {
                    self.head = None;
                    return Ok(());
                }
            };
            let Some(elem) = next else {
                self.head = None;
                return self.release();
            };
            if !of_kind(&elem, self.kind) {
                continue;
            }
            self.check.visit(element_id(&elem))?;
            self.pulled += 1;
            self.occurrence += 1;
            let owner = self
                .owner
                .as_ref()
                .ok_or_else(|| crate::Error::internal("sweep cursor read without its block"))?;
            self.head = Some(Item::Held(Held {
                elem,
                _owner: Rc::clone(owner),
            }));
            return Ok(());
        }
    }

    /// The feed is exhausted: finish the claim check and drop the block
    /// (elements already taken keep it alive on their own).
    fn release(&mut self) -> Result<()> {
        self.feed = Feed::Done;
        self.owner = None;
        self.pinned = 0;
        self.check.finish()
    }

    /// Whether demoting would release a block mostly consumed.
    fn demotable(&self) -> bool {
        self.owner.is_some()
            && self.total.saturating_sub(self.pulled).saturating_mul(8) < self.total
    }

    /// Validate and own the rest of the feed, the head included, and
    /// release the block.
    fn demote(&mut self) -> Result<()> {
        let mut rest = VecDeque::new();
        loop {
            let next = match &mut self.feed {
                Feed::Lazy(iter) => iter.next(),
                Feed::Sorted(iter) => iter.next(),
                Feed::Owned(_) | Feed::Done => None,
            };
            let Some(elem) = next else { break };
            if !of_kind(&elem, self.kind) {
                continue;
            }
            self.check.visit(element_id(&elem))?;
            self.pulled += 1;
            let owner = self
                .owner
                .as_ref()
                .ok_or_else(|| crate::Error::internal("sweep cursor read without its block"))?;
            let held = Item::Held(Held {
                elem,
                _owner: Rc::clone(owner),
            });
            rest.push_back(held.into_owned()?);
        }
        if let Some(head) = self.head.take() {
            self.head = Some(head.into_owned()?);
        }
        self.release()?;
        self.feed = Feed::Owned(rest);
        Ok(())
    }
}

/// One element of the group being emitted.
struct Member {
    version: i32,
    /// (input, frame offset, occurrence in frame): lower wins a duplicate.
    tie: (usize, u64, u64),
    item: Item,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// The run's builder and the chunk it is filling.
struct Output<'s> {
    bb: BlockBuilder,
    filled: Vec<OwnedBlock>,
    chunk: Chunk,
    chunk_bytes: usize,
    refs: Vec<i64>,
    compression: &'s Compression,
    metrics: &'s WriterMetrics,
}

impl Output<'_> {
    /// Encode one surviving element.
    fn encode(&mut self, item: &Item) -> Result<()> {
        match item {
            Item::Held(h) => match &h.elem {
                Element::DenseNode(dn) => {
                    crate::commands::ensure_node_capacity_local(&mut self.bb, &mut self.filled)?;
                    let meta = dense_node_metadata(dn)?;
                    self.bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                Element::Node(n) => {
                    crate::commands::ensure_node_capacity_local(&mut self.bb, &mut self.filled)?;
                    let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                    self.bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                }
                Element::Way(w) => {
                    crate::commands::ensure_way_capacity_local(&mut self.bb, &mut self.filled)?;
                    let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                    self.refs.clear();
                    self.refs.extend(w.refs());
                    self.bb.add_way(w.id(), w.tags(), &self.refs, meta.as_ref());
                }
                Element::Relation(r) => {
                    crate::commands::ensure_relation_capacity_local(
                        &mut self.bb,
                        &mut self.filled,
                    )?;
                    let mut members: Vec<MemberData<'_>> = Vec::new();
                    relation_members_into(r, &mut members)?;
                    let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                    self.bb
                        .add_relation(r.id(), r.tags(), &members, meta.as_ref());
                }
            },
            Item::Node(n) => write_single_node_local(n, &mut self.bb, &mut self.filled)?,
            Item::Way(w) => write_single_way_local(w, &mut self.bb, &mut self.filled)?,
            Item::Relation(r) => write_single_relation_local(r, &mut self.bb, &mut self.filled)?,
        }
        Ok(())
    }

    /// Frame every filled block into the chunk, sending full chunks.
    fn ship(&mut self, emitter: &crate::ordered_pool::Emitter<'_, Chunk, RunCounts>) -> Result<()> {
        for block in std::mem::take(&mut self.filled) {
            let frame = frame_owned(block, self.compression, self.metrics)?;
            let cap = frame.capacity();
            let limit = crate::tunables::OVERLAP_RUN_CHUNK_BYTES;
            if self.chunk_bytes > 0 && self.chunk_bytes + cap > limit {
                self.send(emitter)?;
            }
            self.chunk.push(frame);
            self.chunk_bytes += cap;
            if self.chunk_bytes >= limit {
                self.send(emitter)?;
            }
        }
        Ok(())
    }

    fn send(&mut self, emitter: &crate::ordered_pool::Emitter<'_, Chunk, RunCounts>) -> Result<()> {
        self.chunk_bytes = 0;
        let chunk = std::mem::take(&mut self.chunk);
        if chunk.is_empty() {
            return Ok(());
        }
        emitter.send(chunk)
    }
}

/// Frame one encoded block to complete wire bytes, on this thread.
fn frame_owned(
    owned: OwnedBlock,
    compression: &Compression,
    metrics: &WriterMetrics,
) -> Result<Vec<u8>> {
    let OwnedBlock {
        bytes,
        index,
        tagdata,
        way_members,
    } = owned;
    let indexdata = index.serialize();
    Ok(frame_blob_pipelined(
        &bytes,
        compression,
        Some(indexdata.as_slice()),
        tagdata.as_deref(),
        way_members.as_deref(),
        metrics,
    )?
    .into_vec())
}

/// Validate a dropped duplicate as a written element would be: its user, and
/// a relation's roles, resolved. An owned item was validated when owned.
fn validate_loser(item: &Item) -> Result<()> {
    let Item::Held(h) = item else {
        return Ok(());
    };
    match &h.elem {
        Element::DenseNode(dn) => {
            dense_node_metadata(dn)?;
        }
        Element::Node(n) => {
            element_metadata(ElementKind::Node, n.id(), &n.info())?;
        }
        Element::Way(w) => {
            element_metadata(ElementKind::Way, w.id(), &w.info())?;
        }
        Element::Relation(r) => {
            element_metadata(ElementKind::Relation, r.id(), &r.info())?;
            let mut members: Vec<MemberData<'_>> = Vec::new();
            relation_members_into(r, &mut members)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The merge
// ---------------------------------------------------------------------------

/// The open cursors and the heap ordering them.
struct Merge {
    cursors: Vec<Option<Cursor>>,
    heap: BinaryHeap<Reverse<(OsmIdKey, usize)>>,
    pinned: usize,
    next_scan: usize,
    group: Vec<Member>,
    visits: usize,
}

impl Sweep<'_, '_> {
    /// The sweep's cooperative stop: the run cancelled, or abandoned (the
    /// write loop has returned, or a thread unwound), so nothing it still
    /// produces can be written.
    fn checkpoint(&self) -> Result<()> {
        self.cancel.check()?;
        if self.emitter.abandoned() {
            return Err(crate::Error::internal(
                "overlap run abandoned: its write loop is gone",
            ));
        }
        Ok(())
    }

    fn locate(&self, source: usize, offset: u64, e: crate::Error) -> crate::Error {
        self.sources[source].locate_at(offset, e)
    }

    /// Sweep the run: see the module docs.
    ///
    /// [`checkpoint`](Self::checkpoint) runs before each blob is read and
    /// after every [`SWEEP_CHECKPOINT_REMOVALS`] elements pulled (dropped
    /// duplicates included), so neither a long group nor the final drain
    /// runs unchecked.
    ///
    /// [`SWEEP_CHECKPOINT_REMOVALS`]: crate::tunables::SWEEP_CHECKPOINT_REMOVALS
    pub(super) fn run(&self, frame_buf: &mut Vec<u8>) -> Result<RunCounts> {
        let mut counts = RunCounts::default();
        let mut out = Output {
            bb: BlockBuilder::new(),
            filled: Vec::new(),
            chunk: Vec::new(),
            chunk_bytes: 0,
            refs: Vec::new(),
            compression: self.compression,
            metrics: self.metrics,
        };
        let mut merge = Merge {
            cursors: Vec::with_capacity(self.entries.len()),
            heap: BinaryHeap::new(),
            pinned: 0,
            next_scan: crate::tunables::SWEEP_PINNED_BYTES,
            group: Vec::new(),
            visits: 0,
        };
        for entry in self.entries {
            self.checkpoint()?;
            let below = osm_id_key(crate::osm_id::blob_osm_first_id(
                entry.index.min_id,
                entry.index.max_id,
            ));
            while merge
                .heap
                .peek()
                .is_some_and(|Reverse((key, _))| *key < below)
            {
                self.emit_group(&mut merge, &mut out, &mut counts)?;
                self.maybe_demote(&mut merge)?;
            }
            let frame = &self.frames[entry.frame];
            let cursor = self
                .open(entry, frame, frame_buf)
                .map_err(|e| self.locate(frame.source, frame.file_offset, e))?;
            let slot = merge.cursors.len();
            merge.pinned += cursor.pinned;
            if let Some(head) = &cursor.head {
                merge.heap.push(Reverse((osm_id_key(head.id()), slot)));
                merge.cursors.push(Some(cursor));
            } else {
                merge.cursors.push(None);
            }
            self.maybe_demote(&mut merge)?;
        }
        while !merge.heap.is_empty() {
            self.emit_group(&mut merge, &mut out, &mut counts)?;
            self.maybe_demote(&mut merge)?;
        }
        crate::commands::flush_local(&mut out.bb, &mut out.filled)?;
        out.ship(self.emitter)?;
        out.send(self.emitter)?;
        Ok(counts)
    }

    /// Read and decode one entry's frame and open a cursor on it.
    fn open(&self, entry: &BlobEntry, frame: &Frame, frame_buf: &mut Vec<u8>) -> Result<Cursor> {
        let source = &self.sources[frame.source];
        let block = read_block(source, frame, frame_buf)?;
        // A believed header claim routed this blob to this kind's sweep and
        // placed it in the run, so it is held to the claim (see the module
        // docs). A scanned entry's index came from the payload; a
        // restructured frame is read per kind on purpose.
        let believed = !entry.index_scanned;
        if believed {
            block.refuse_foreign_kind(Some(entry.index.kind))?;
        }
        Cursor::open(
            block,
            self.kind,
            believed.then_some(entry.index),
            entry.intra_unsorted || entry.restructured,
            (frame.source, frame.file_offset),
        )
    }
}

impl Cursor {
    /// A cursor over `block`'s elements of `kind`, its head pulled: read
    /// lazily, or through views sorted by OSM ID when `sorted_views`; held to
    /// `claim` when one is given.
    fn open(
        block: PrimitiveBlock,
        kind: ElementKind,
        claim: Option<crate::blob_meta::BlobIndex>,
        sorted_views: bool,
        (source, offset): (usize, u64),
    ) -> Result<Cursor> {
        let pinned = block.decompressed_size();
        let block = Rc::new(block);
        let (feed, total) = if sorted_views {
            let mut views: Vec<Element<'static>> = block
                .elements()
                .filter(|e| of_kind(e, kind))
                // SAFETY: the views live in this cursor's feed, declared
                // before `owner`, which holds the block; each view taken
                // out carries its own clone of the `Rc`.
                .map(|e| unsafe { erase_elem(e) })
                .collect();
            views.sort_by(|a, b| osm_id_cmp(element_id(a), element_id(b)));
            let total = views.len() as u64;
            (Feed::Sorted(views.into_iter()), total)
        } else {
            // SAFETY: as above, for the iterator.
            let iter = unsafe { erase_iter(block.elements()) };
            (Feed::Lazy(Box::new(iter)), block.element_count())
        };
        let mut cursor = Cursor {
            feed,
            head: None,
            owner: Some(block),
            check: Check {
                claim,
                tally: IdTally::default(),
                last: None,
                ordered: !sorted_views,
            },
            kind,
            source,
            offset,
            occurrence: 0,
            pulled: 0,
            total,
            pinned,
        };
        cursor.advance()?;
        Ok(cursor)
    }
}

impl Sweep<'_, '_> {
    /// Pull every element of the lowest ID from every cursor, order them by
    /// version (then input precedence), drop duplicates under dedupe, and
    /// encode the rest.
    fn emit_group(
        &self,
        merge: &mut Merge,
        out: &mut Output<'_>,
        counts: &mut RunCounts,
    ) -> Result<()> {
        let Some(&Reverse((key, _))) = merge.heap.peek() else {
            return Ok(());
        };
        merge.group.clear();
        while let Some(&Reverse((k, slot))) = merge.heap.peek()
            && k == key
        {
            merge.heap.pop();
            let cursor = merge.cursors[slot]
                .as_mut()
                .ok_or_else(|| crate::Error::internal("sweep heap names a closed cursor"))?;
            let item = cursor
                .head
                .take()
                .ok_or_else(|| crate::Error::internal("sweep heap names an empty cursor"))?;
            let tie = (cursor.source, cursor.offset, cursor.occurrence);
            let (source, offset) = (cursor.source, cursor.offset);
            let pinned_before = cursor.pinned;
            cursor
                .advance()
                .map_err(|e| self.locate(source, offset, e))?;
            merge.pinned -= pinned_before - cursor.pinned;
            if let Some(head) = &cursor.head {
                merge.heap.push(Reverse((osm_id_key(head.id()), slot)));
            } else {
                merge.cursors[slot] = None;
            }
            merge.visits += 1;
            if merge.visits >= crate::tunables::SWEEP_CHECKPOINT_REMOVALS {
                merge.visits = 0;
                self.checkpoint()?;
            }
            let member = Member {
                version: item.version(),
                tie,
                item,
            };
            self.insert(&mut merge.group, member, counts)?;
        }
        merge.group.sort_by_key(|m| (m.version, m.tie));
        for member in merge.group.drain(..) {
            out.encode(&member.item)
                .map_err(|e| self.locate(member.tie.0, member.tie.1, e))?;
            counts.elements += 1;
        }
        out.ship(self.emitter)
    }

    /// Add one element to the group; under dedupe keep one per version, the
    /// one with precedence, validating and counting the other.
    fn insert(
        &self,
        group: &mut Vec<Member>,
        member: Member,
        counts: &mut RunCounts,
    ) -> Result<()> {
        if self.dedupe
            && let Some(kept) = group.iter_mut().find(|m| m.version == member.version)
        {
            let loser = if member.tie < kept.tie {
                std::mem::replace(kept, member)
            } else {
                member
            };
            validate_loser(&loser.item).map_err(|e| self.locate(loser.tie.0, loser.tie.1, e))?;
            counts.duplicates += 1;
            return Ok(());
        }
        group.push(member);
        Ok(())
    }

    /// Demote mostly consumed cursors once the run's pinned bytes pass the
    /// budget; amortized to one scan per half budget of growth.
    fn maybe_demote(&self, merge: &mut Merge) -> Result<()> {
        if merge.pinned <= merge.next_scan {
            return Ok(());
        }
        for cursor in merge.cursors.iter_mut().flatten() {
            if cursor.demotable() {
                let (source, offset, pinned) = (cursor.source, cursor.offset, cursor.pinned);
                cursor
                    .demote()
                    .map_err(|e| self.locate(source, offset, e))?;
                merge.pinned -= pinned;
            }
        }
        let budget = crate::tunables::SWEEP_PINNED_BYTES;
        merge.next_scan = budget.max(merge.pinned.saturating_add(budget / 2));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node block holding `ids` in the given order, without metadata.
    fn node_block(ids: &[i64]) -> PrimitiveBlock {
        let mut bb = BlockBuilder::new();
        for &id in ids {
            bb.add_node(id, 1, 1, [], None);
        }
        let bytes = bb.take().expect("take").expect("block").to_vec();
        PrimitiveBlock::from_vec(bytes).expect("decode")
    }

    fn claim(count: u64, min_id: i64, max_id: i64) -> crate::blob_meta::BlobIndex {
        crate::blob_meta::BlobIndex {
            kind: ElementKind::Node,
            min_id,
            max_id,
            count,
            bbox: None,
        }
    }

    /// Every remaining ID of `cursor`, head first.
    fn drain(cursor: &mut Cursor) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        while let Some(head) = cursor.head.take() {
            ids.push(head.id());
            cursor.advance()?;
        }
        Ok(ids)
    }

    /// Demotion releases the block mid-read and the cursor goes on with the
    /// same elements, owned.
    #[test]
    fn a_demoted_cursor_releases_its_block_and_keeps_its_tail() {
        let ids: Vec<i64> = (1..=100).collect();
        let mut cursor =
            Cursor::open(node_block(&ids), ElementKind::Node, None, false, (0, 0)).expect("open");
        let mut seen = Vec::new();
        for _ in 0..95 {
            seen.push(cursor.head.take().expect("head").id());
            cursor.advance().expect("advance");
        }
        assert!(cursor.demotable(), "95 of 100 pulled is mostly consumed");
        cursor.demote().expect("demote");
        assert!(
            cursor.owner.is_none() && cursor.pinned == 0,
            "block released"
        );
        assert!(matches!(cursor.head, Some(Item::Node(_))), "head owned");
        seen.extend(drain(&mut cursor).expect("drain"));
        assert_eq!(seen, ids);
    }

    /// Demotion finishes the claim check against the original block, so a
    /// false count is still refused.
    #[test]
    fn demotion_still_refuses_an_inexact_claim() {
        let mut cursor = Cursor::open(
            node_block(&[1, 2, 3]),
            ElementKind::Node,
            Some(claim(4, 1, 3)),
            false,
            (0, 0),
        )
        .expect("open");
        assert!(
            cursor.demote().is_err(),
            "the claim says 4 elements, the block holds 3"
        );
    }

    /// An exact claim passes, and a lazily read block that descends is
    /// refused when the descent is read.
    #[test]
    fn a_lazy_cursor_checks_its_claim_and_its_order() {
        let mut honest = Cursor::open(
            node_block(&[1, 2, 3]),
            ElementKind::Node,
            Some(claim(3, 1, 3)),
            false,
            (0, 0),
        )
        .expect("open");
        assert_eq!(drain(&mut honest).expect("drain"), vec![1, 2, 3]);

        let mut wide = Cursor::open(
            node_block(&[2, 3]),
            ElementKind::Node,
            Some(claim(2, 1, 3)),
            false,
            (0, 0),
        )
        .expect("open");
        assert!(
            drain(&mut wide).is_err(),
            "a claimed range wider than the block is refused"
        );

        let mut descending = Cursor::open(
            node_block(&[1, 3, 2]),
            ElementKind::Node,
            None,
            false,
            (0, 0),
        )
        .expect("open");
        assert!(drain(&mut descending).is_err());
    }

    /// Sorted views reorder an internally unsorted block, keeping the
    /// occurrence order of equal IDs.
    #[test]
    fn sorted_views_order_an_unsorted_block() {
        let mut cursor = Cursor::open(
            node_block(&[3, -1, 2, 0]),
            ElementKind::Node,
            None,
            true,
            (0, 0),
        )
        .expect("open");
        assert_eq!(drain(&mut cursor).expect("drain"), vec![0, -1, 2, 3]);
    }
}
