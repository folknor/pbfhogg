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
//! A cursor pins its decoded block while it has elements left. Its charge
//! against the run's budget is the block's protobuf payload length plus, for
//! a sorted-views cursor, the allocated capacity of its view vector (one
//! [`Element`] per element of the block, which at planet density is several
//! times the payload). A block that
//! is mostly consumed but keeps a sparse tail far ahead (a layout `sort`
//! accepts) would pin the whole block for a handful of elements, so when the
//! run's charged bytes pass [`SWEEP_PINNED_BYTES`] the sweep demotes every
//! cursor with less than an eighth of its elements left: the rest of the
//! block is validated, copied into owned elements and the block (and any
//! views) released. The scan is amortized (at most once per half budget of
//! growth). A dense giant component still holds its live overlap, as any
//! in-memory merge must.
//!
//! The charge is an accounting of what cursors hold, not an allocation
//! census. It leaves out a decoded block's inline string and group entries
//! appended past its payload, the stable sort's scratch while a sorted-views
//! cursor opens, the owned tails demotion leaves, the group being emitted
//! (whose elements keep their block alive after the cursor has released it)
//! and one cursor slot per opened entry.
//!
//! The group is held whole, uncharged and unbounded: every copy of one ID
//! is gathered before any is written, because a cursor delivers an ID's
//! copies in whatever version order its block holds them (`sort` accepts a
//! blob whose versions of one ID descend), so ordering by version needs
//! them all. Demotion cannot free a group member. Ordinary groups are a
//! few elements; only adversarial input (`sort` of many copies of one ID)
//! or an object of very many versions grows one. Removing the hold would
//! take a merge keyed on (ID, version, tie) with every cursor delivering
//! that order, which means an eager (ID, version) pass over every overlap
//! blob before its cursor opens (or sorted views for any blob that fails
//! it) - a second pass over normal input to bound what normal input never
//! reaches. That merge would still hold live cursor blocks and views.
//!
//! Each borrowed element carries an [`Rc`] of the block it borrows from, so a
//! block lives exactly as long as the cursor reading it or any element taken
//! from it; an exhausted cursor releases its block at once.
//!
//! Output: every block the builder fills is framed (compressed) here, on the
//! sweep's worker, and frames are packed into chunks of at most
//! [`OVERLAP_RUN_CHUNK_BYTES`] of capacity, sent through the run's channel
//! of [`OVERLAP_RUN_CHANNEL_DEPTH`] messages. A frame larger than a chunk
//! (a block of large elements under `Compression::None`, or of
//! incompressible ones, up to the 32 MiB format limit) is framed straight
//! into pieces of at most a chunk, packed like whole frames: the write loop
//! takes a chunk as bytes in order and no frame boundary is read after
//! framing. So every chunk is within the budget whatever the block, and a
//! run holds at most `depth` chunks queued plus the one it is building,
//! whatever its size. A sweep blocked on its full channel while sending a
//! large frame's pieces still holds the pieces not yet sent and the group's
//! filled blocks not yet framed: worker-local, one frame and one group at
//! most, and only on the workers sweeping, never per issued run. A block is
//! shipped as soon as the builder fills it, even inside a group, so
//! completed output waits on the sweep's side for one block at most.
//!
//! # Resource counters
//!
//! Each run reports what it held ([`RunCounts`]), and `write_plan` emits the
//! figures over every run once its pool has finished:
//! `overlap_run_pinned_high_water_bytes` (the largest charge any run reached,
//! taken as each cursor opens, before any demotion it triggers),
//! `overlap_run_demoted_cursors`, `overlap_run_chunks` (chunks sent),
//! `overlap_run_chunk_high_water_bytes` (the largest summed capacity of the
//! frames and frame pieces of a chunk sent, the final partial one included)
//! and
//! `overlap_run_staged_high_water_bytes` (the largest completed output held
//! on the sweep's side before shipping: payload lengths of the filled blocks
//! plus the capacities of the chunk being packed, sampled as each ship
//! begins, so it leaves out the pieces of the frame being packed). They
//! are what the resource tests in `crates/lib/tests/merge_pbf.rs` assert, in place of a
//! host's resident memory.
//!
//! [`SWEEP_PINNED_BYTES`]: crate::tunables::SWEEP_PINNED_BYTES
//! [`OVERLAP_RUN_CHUNK_BYTES`]: crate::tunables::OVERLAP_RUN_CHUNK_BYTES
//! [`OVERLAP_RUN_CHANNEL_DEPTH`]: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::rc::Rc;

use super::{BlobEntry, Frame, FrameBuf, RunCounts, Sources, element_id, read_block};
use crate::blob_meta::IdTally;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::error::Result;
use crate::osm_id::{OsmIdKey, osm_id_cmp, osm_id_key};
use crate::owned::{
    OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata,
    read_dense_node, read_node, read_relation, read_way, write_single_node_local,
    write_single_relation_local, write_single_way_local,
};
use crate::read::block::BlockElementsIter;
use crate::write::metrics::WriterMetrics;
use crate::writer::{Compression, frame_blob_pipelined_pieces};
use crate::{Element, ElementKind, PrimitiveBlock};

/// One run's chunk of framed output, written in order: whole frames, or the
/// pieces of one larger than a chunk. Its buffers are bytes in order, not
/// one frame each.
pub(super) type Chunk = Vec<Vec<u8>>;

/// One overlap run's sweep, on its worker.
pub(super) struct Sweep<'a, 'e> {
    pub(super) entries: &'a [BlobEntry],
    pub(super) frames: &'a [Frame],
    pub(super) sources: &'a Sources,
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
            Item::Node(n) => n
                .metadata
                .as_ref()
                .map_or(0, OwnedMetadata::version_or_zero),
            Item::Way(w) => w
                .metadata
                .as_ref()
                .map_or(0, OwnedMetadata::version_or_zero),
            Item::Relation(r) => r
                .metadata
                .as_ref()
                .map_or(0, OwnedMetadata::version_or_zero),
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
    /// Bytes this cursor is charged for holding: its block's payload, plus
    /// a sorted feed's view vector (0 once released; see the module docs).
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
    /// The frame being packed, in pieces (reused, so it grows only when a frame needs more pieces than any before).
    pieces: Vec<Vec<u8>>,
    refs: Vec<i64>,
    compression: &'s Compression,
    metrics: &'s WriterMetrics,
    /// What the run reports (see the module docs, "Resource counters").
    chunks: u64,
    chunk_high_water: usize,
    staged_high_water: usize,
}

impl Output<'_> {
    /// Encode one surviving element.
    fn encode(&mut self, item: &Item) -> Result<()> {
        match item {
            Item::Held(h) => match &h.elem {
                Element::DenseNode(dn) => {
                    crate::commands::ensure_node_capacity_local(&mut self.bb, &mut self.filled)?;
                    let meta = dense_node_metadata(dn)?;
                    self.bb.add_node_dense(
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
                    self.bb.add_node_partial(
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
                    self.bb
                        .add_way_partial(w.id(), w.tags(), &self.refs, meta.as_ref());
                }
                Element::Relation(r) => {
                    crate::commands::ensure_relation_capacity_local(
                        &mut self.bb,
                        &mut self.filled,
                    )?;
                    let mut members: Vec<MemberData<'_>> = Vec::new();
                    r.members_into(&mut members)?;
                    let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                    self.bb
                        .add_relation_partial(r.id(), r.tags(), &members, meta.as_ref());
                }
            },
            Item::Node(n) => write_single_node_local(n, &mut self.bb, &mut self.filled)?,
            Item::Way(w) => write_single_way_local(w, &mut self.bb, &mut self.filled)?,
            Item::Relation(r) => write_single_relation_local(r, &mut self.bb, &mut self.filled)?,
        }
        Ok(())
    }

    /// Frame every filled block into the chunk, sending full chunks.
    ///
    /// A frame larger than a chunk arrives in pieces of at most a chunk
    /// each, packed like whole frames, so no chunk sent exceeds
    /// [`OVERLAP_RUN_CHUNK_BYTES`] of capacity whatever the block: the
    /// writer takes a chunk as bytes in order and never reads a frame
    /// boundary.
    ///
    /// [`OVERLAP_RUN_CHUNK_BYTES`]: crate::tunables::OVERLAP_RUN_CHUNK_BYTES
    fn ship(&mut self, emitter: &crate::ordered_pool::Emitter<'_, Chunk, RunCounts>) -> Result<()> {
        let filled: usize = self.filled.iter().map(|b| b.bytes.len()).sum();
        self.staged_high_water = self
            .staged_high_water
            .max(filled.saturating_add(self.chunk_bytes));
        let limit = crate::tunables::OVERLAP_RUN_CHUNK_BYTES;
        for block in std::mem::take(&mut self.filled) {
            frame_owned(block, self.compression, self.metrics, &mut self.pieces)?;
            let mut pieces = std::mem::take(&mut self.pieces);
            for piece in pieces.drain(..) {
                let cap = piece.capacity();
                if self.chunk_bytes > 0 && self.chunk_bytes + cap > limit {
                    self.send(emitter)?;
                }
                self.chunk.push(piece);
                self.chunk_bytes += cap;
                if self.chunk_bytes >= limit {
                    self.send(emitter)?;
                }
            }
            self.pieces = pieces;
        }
        Ok(())
    }

    fn send(&mut self, emitter: &crate::ordered_pool::Emitter<'_, Chunk, RunCounts>) -> Result<()> {
        let bytes = std::mem::take(&mut self.chunk_bytes);
        let chunk = std::mem::take(&mut self.chunk);
        if chunk.is_empty() {
            return Ok(());
        }
        self.chunks += 1;
        self.chunk_high_water = self.chunk_high_water.max(bytes);
        emitter.send(chunk)
    }
}

/// Frame one encoded block to complete wire bytes, on this thread, appended
/// to `pieces` in buffers of at most [`OVERLAP_RUN_CHUNK_BYTES`] (one buffer
/// unless the frame is larger).
///
/// [`OVERLAP_RUN_CHUNK_BYTES`]: crate::tunables::OVERLAP_RUN_CHUNK_BYTES
fn frame_owned(
    owned: OwnedBlock,
    compression: &Compression,
    metrics: &WriterMetrics,
    pieces: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let OwnedBlock {
        bytes,
        index,
        tagdata,
        way_members,
    } = owned;
    const PIECE: std::num::NonZeroUsize =
        match std::num::NonZeroUsize::new(crate::tunables::OVERLAP_RUN_CHUNK_BYTES) {
            Some(piece) => piece,
            None => panic!("OVERLAP_RUN_CHUNK_BYTES must be non-zero"),
        };
    let indexdata = index.serialize();
    frame_blob_pipelined_pieces(
        &bytes,
        compression,
        Some(indexdata.as_slice()),
        tagdata.as_deref(),
        way_members.as_deref(),
        metrics,
        PIECE,
        pieces,
    )
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
            r.members_into(&mut members)?;
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
    /// The group's dedupe index, once it is large (see [`Sweep::insert`]).
    versions: HashMap<i32, usize>,
    visits: usize,
    /// What the run reports (see the module docs, "Resource counters").
    pinned_high_water: usize,
    demoted: u64,
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
    pub(super) fn run(&self, frame_buf: &mut FrameBuf) -> Result<RunCounts> {
        let mut counts = RunCounts::default();
        let mut out = Output {
            bb: BlockBuilder::new(),
            filled: Vec::new(),
            chunk: Vec::new(),
            chunk_bytes: 0,
            pieces: Vec::new(),
            refs: Vec::new(),
            compression: self.compression,
            metrics: self.metrics,
            chunks: 0,
            chunk_high_water: 0,
            staged_high_water: 0,
        };
        let mut merge = Merge {
            cursors: Vec::with_capacity(self.entries.len()),
            heap: BinaryHeap::new(),
            pinned: 0,
            next_scan: crate::tunables::SWEEP_PINNED_BYTES,
            group: Vec::new(),
            versions: HashMap::new(),
            visits: 0,
            pinned_high_water: 0,
            demoted: 0,
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
            merge.pinned_high_water = merge.pinned_high_water.max(merge.pinned);
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
        counts.pinned_high_water = merge.pinned_high_water;
        counts.demoted = merge.demoted;
        counts.chunks = out.chunks;
        counts.chunk_high_water = out.chunk_high_water;
        counts.staged_high_water = out.staged_high_water;
        Ok(counts)
    }

    /// Read and decode one entry's frame and open a cursor on it.
    fn open(&self, entry: &BlobEntry, frame: &Frame, frame_buf: &mut FrameBuf) -> Result<Cursor> {
        let source = &self.sources[frame.source];
        let block = read_block(source, frame, frame_buf, self.cancel)?;
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
        let mut pinned = block.decompressed_size();
        let block = Rc::new(block);
        let (feed, total) = if sorted_views {
            // Sized up front from the block's own count (every kind, so a
            // restructured block over-reserves by its other kinds), so the
            // vector is allocated once and its capacity is what it holds.
            let mut views: Vec<Element<'static>> =
                Vec::with_capacity(usize::try_from(block.element_count()).unwrap_or(0));
            views.extend(
                block
                    .elements()
                    .filter(|e| of_kind(e, kind))
                    // SAFETY: the views live in this cursor's feed, declared
                    // before `owner`, which holds the block; each view taken
                    // out carries its own clone of the `Rc`.
                    .map(|e| unsafe { erase_elem(e) }),
            );
            views.sort_by(|a, b| osm_id_cmp(element_id(a), element_id(b)));
            let total = views.len() as u64;
            // The vector stays allocated until the feed ends or is demoted,
            // however far its iterator has advanced.
            pinned = pinned.saturating_add(
                views
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Element<'static>>()),
            );
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
            self.insert(&mut merge.group, &mut merge.versions, member, counts)?;
        }
        // The index points into the unsorted group; sorting invalidates it.
        merge.versions.clear();
        // A large group's sort and encode get their own checks; an ordinary
        // one is covered by the pull checks around it.
        let large = merge.group.len() >= crate::tunables::SWEEP_CHECKPOINT_REMOVALS;
        if large {
            self.checkpoint()?;
        }
        merge.group.sort_by_key(|m| (m.version, m.tie));
        if large {
            self.checkpoint()?;
        }
        let mut encoded = 0usize;
        for member in merge.group.drain(..) {
            out.encode(&member.item)
                .map_err(|e| self.locate(member.tie.0, member.tie.1, e))?;
            counts.elements += 1;
            // Ship each block as it fills, so a group spanning several
            // blocks stages one at a time (see the module docs, "Memory").
            if !out.filled.is_empty() {
                out.ship(self.emitter)?;
            }
            encoded += 1;
            if encoded >= crate::tunables::SWEEP_CHECKPOINT_REMOVALS {
                encoded = 0;
                self.checkpoint()?;
            }
        }
        Ok(())
    }

    /// Add one element to the group; under dedupe keep one per version, the
    /// one with precedence, validating and counting the other.
    ///
    /// The kept copy of a version is found by a scan of the group while it
    /// is small, and through `versions` (version to group slot, built once
    /// the group reaches [`SWEEP_GROUP_INDEX_THRESHOLD`]) after, so a group
    /// of k versions costs linear time, not quadratic. A replacement keeps
    /// its slot, so the index stays valid until the group is sorted.
    ///
    /// [`SWEEP_GROUP_INDEX_THRESHOLD`]: crate::tunables::SWEEP_GROUP_INDEX_THRESHOLD
    fn insert(
        &self,
        group: &mut Vec<Member>,
        versions: &mut HashMap<i32, usize>,
        member: Member,
        counts: &mut RunCounts,
    ) -> Result<()> {
        if !self.dedupe {
            group.push(member);
            return Ok(());
        }
        let found = if versions.is_empty() {
            group.iter().position(|m| m.version == member.version)
        } else {
            versions.get(&member.version).copied()
        };
        if let Some(slot) = found {
            let kept = &mut group[slot];
            let loser = if member.tie < kept.tie {
                std::mem::replace(kept, member)
            } else {
                member
            };
            validate_loser(&loser.item).map_err(|e| self.locate(loser.tie.0, loser.tie.1, e))?;
            counts.duplicates += 1;
            return Ok(());
        }
        if !versions.is_empty() {
            versions.insert(member.version, group.len());
        }
        group.push(member);
        if versions.is_empty() && group.len() >= crate::tunables::SWEEP_GROUP_INDEX_THRESHOLD {
            versions.extend(group.iter().enumerate().map(|(slot, m)| (m.version, slot)));
        }
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
                merge.demoted += 1;
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

    /// The resource tests in `crates/lib/tests/merge_pbf.rs` (the planet-density and
    /// large-frame ones)
    /// size their fixtures against these values, which they cannot read: a
    /// change here must resize them.
    #[test]
    fn the_resource_tests_are_sized_against_these_constants() {
        assert_eq!(
            crate::tunables::OVERLAP_RUN_CHUNK_BYTES,
            2 << 20,
            "resize CHUNK_BYTES and the self-merge and large-frame fixtures in tests/merge_pbf.rs"
        );
        assert_eq!(
            crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH,
            4,
            "resize CHANNEL_DEPTH and the self-merge fixture in tests/merge_pbf.rs"
        );
        assert_eq!(
            crate::tunables::SWEEP_PINNED_BYTES,
            64 << 20,
            "resize PINNED_BYTES and the sparse-tail fixture in tests/merge_pbf.rs"
        );
    }

    /// A sorted-views cursor is charged for its view vector as well as its
    /// block, and releases both when it ends.
    #[test]
    fn a_sorted_cursor_is_charged_for_its_views() {
        let ids: Vec<i64> = (1..=100).rev().collect();
        let block = node_block(&ids);
        let payload = block.decompressed_size();
        let mut cursor = Cursor::open(block, ElementKind::Node, None, true, (0, 0)).expect("open");
        assert_eq!(
            cursor.pinned,
            payload + 100 * std::mem::size_of::<Element<'static>>()
        );
        drain(&mut cursor).expect("drain");
        assert_eq!(cursor.pinned, 0, "released once exhausted");
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
