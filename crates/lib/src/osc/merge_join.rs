//! Merge-joins for sorted PBF operations, used by `diff` and
//! `derive_changes`.
//!
//! [`block_pair_merge_phase`] is the blob-level merge over two indexed
//! inputs, run once per work unit (an OSM-order key [`Window`] of one kind
//! and the in-memory [`Frame`]s of both inputs that intersect it, read in
//! file order from each input's [`BlobCursor`] stream; see
//! `commands::diff::units`). [`stream_merge_phase`] is the
//! element-level merge over two [`StreamingBlocks`] cursors, for inputs
//! without indexdata. Both hand the caller borrowed elements through the
//! same [`BlockMergeAction`] variants to one [`MergeSink`] per command, so
//! an element prints the same way whichever path found it.

use crate::ElementKind;
use crate::commands::diff::view::{BlockView, MetadataCursor, Pair, Row, Rows};
use crate::osm_id::OsmIdKey;
use crate::{Element, PrimitiveBlock};

use crate::error::Result;

// ---------------------------------------------------------------------------
// StreamingBlocks - block-level cursor with stashing
// ---------------------------------------------------------------------------

/// Block source shared across all type phases of a streaming merge-join over
/// an input without indexdata.
///
/// Without indexdata a block's kind is known only once it is decoded, so
/// every block of the file is decompressed and parsed, whatever the caller
/// selects: that is what lets a block of an earlier kind turning up late be
/// refused instead of lost. What a caller does not select is classified and
/// dropped without converting its elements ([`StreamingBlocks::skip_kind`]).
/// A block of a later kind than the running phase is stashed for the phase
/// or skip that takes it next, which classifies it again.
pub(crate) struct StreamingBlocks {
    reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    /// Captured once; failures are located with the blob's true index and
    /// offset in this input.
    source: Option<std::sync::Arc<std::path::Path>>,
    /// Decompression buffers, shared with the other side of the merge; see
    /// [`MergeBuffers`].
    pool: MergeBuffers,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    stashed: Option<PrimitiveBlock>,
    /// `(blob index, offset)` of the block read last. The stash, when there
    /// is one, is that block, and so is any block being classified or
    /// converted, so errors about a block are located here.
    last_blob: (Option<u64>, Option<crate::blob::ByteOffset>),
    /// Kind ordinal and key of the last element converted, for the element
    /// order check of the selected kinds.
    last_element: Option<(u8, crate::osm_id::OsmIdKey)>,
}

impl StreamingBlocks {
    /// Create from a sequential BlobReader. Avoids PrimitiveBlock cross-thread
    /// alloc/free retention from the pipelined reader. Both sides of one
    /// merge take clones of the same `pool`.
    pub(crate) fn new_sequential<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        direct_io: bool,
        pool: MergeBuffers,
        host: &crate::host::Entered<'_>,
    ) -> crate::error::Result<Self> {
        let mut reader = crate::blob::BlobReader::open_in(source, direct_io, host)?;
        reader.set_parse_indexdata(true);
        reader.leading_header()?;
        let source = reader.source().cloned();
        Ok(Self {
            reader,
            source,
            pool,
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            stashed: None,
            last_blob: (None, None),
            last_element: None,
        })
    }

    fn next_block(&mut self) -> Result<Option<PrimitiveBlock>> {
        if let Some(b) = self.stashed.take() {
            return Ok(Some(b));
        }
        loop {
            let blob = match self.reader.next() {
                Some(r) => r?,
                None => return Ok(None),
            };
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                continue;
            }
            self.last_blob = (blob.blob_index(), blob.offset());
            return decode_blob(
                &blob,
                &self.pool,
                &mut self.st_scratch,
                &mut self.gr_scratch,
            )
            .map(Some)
            .map_err(|e| blob.locate_error(self.source.as_ref(), e));
        }
    }

    /// Locate an error at the block read last.
    #[cold]
    fn locate(&self, err: crate::Error) -> crate::Error {
        err.locate(crate::error::Location::new(
            self.source.as_ref(),
            self.last_blob.0,
            self.last_blob.1,
        ))
    }

    /// Read past every block of `kind`, a kind the caller excludes, up to
    /// the first block of a later kind (stashed) or the end of the file.
    ///
    /// Each block is classified by the element kinds it actually yields,
    /// never by [`PrimitiveBlock::block_type`], which reads only the first
    /// field of each group: a group starting with a way and holding a node
    /// after it classifies as ways, and dropping it as a way block would lose
    /// the node. So a block of `kind` alone is dropped; an empty one is
    /// neutral; a later kind ends the skip; an earlier kind, or a block
    /// holding several, is refused as unsorted - the same verdicts the
    /// selected phases give, so no element of a selected kind can leave the
    /// stream unseen.
    ///
    /// A dropped block is classified by the kinds its validation counted;
    /// its elements are never compared or printed, so element-level faults (a tag or role whose
    /// string index is out of range) and the order of elements within an
    /// excluded kind go unchecked: they cannot change an answer about the
    /// selected kinds. Decompression and block parsing still run
    /// on every block, because without indexdata they are the only way to
    /// know where a kind ends.
    pub(crate) fn skip_kind(&mut self, kind: ElementKind) -> Result<()> {
        loop {
            let Some(block) = self.next_block()? else {
                return Ok(());
            };
            let kinds = block.element_kinds();
            if kinds.is_empty() {
                continue;
            }
            match kinds.single() {
                Some(k) if k == kind => {}
                Some(k) if k > kind => {
                    self.stashed = Some(block);
                    return Ok(());
                }
                Some(k) => return Err(self.locate(unsorted_block(k, kind))),
                None => return Err(self.locate(mixed_block())),
            }
        }
    }

    /// The next block of the phase's `kind`, or `None` when this side's
    /// phase is over (end of file, or the first block of a later kind,
    /// stashed for the phase or skip that takes it next).
    ///
    /// Every block is classified by the element kinds it actually yields
    /// ([`PrimitiveBlock::element_kinds`]; see
    /// [`StreamingBlocks::skip_kind`] for why the block type cannot decide):
    /// no elements is skipped, `kind` alone is taken, a later kind is
    /// stashed and ends the phase, an earlier kind or several kinds is
    /// refused as unsorted. So a block of one kind is taken whatever its
    /// groups' first fields say (dense and plain node groups side by side,
    /// an empty dense group beside ways), and a block of several is refused
    /// before any of its elements reaches the merge.
    fn next_phase_block(&mut self, kind: ElementKind) -> Result<Option<PrimitiveBlock>> {
        loop {
            let Some(block) = self.next_block()? else {
                return Ok(None);
            };
            let kinds = block.element_kinds();
            if kinds.is_empty() {
                continue;
            }
            match kinds.single() {
                Some(k) if k == kind => return Ok(Some(block)),
                Some(k) if k > kind => {
                    self.stashed = Some(block);
                    return Ok(None);
                }
                Some(k) => return Err(self.locate(unsorted_block(k, kind))),
                None => return Err(self.locate(mixed_block())),
            }
        }
    }

    /// Check an element just pulled from this side's current block before
    /// the merge may look at it: it must be of the phase's `kind` (anything
    /// else is the mixed-block error), and it must not go backwards in OSM
    /// order within the kind. An equal id passes, as in the blob-order
    /// checks (duplicates are a separate concern). Without indexdata this is
    /// the only order evidence the stream has, and a merge over a backwards
    /// stream pairs the wrong elements.
    fn admit<'b>(&mut self, element: Element<'b>, kind: ElementKind) -> Result<Element<'b>> {
        if element_kind(&element) != kind {
            return Err(self.locate(mixed_block()));
        }
        let id = element_id(&element);
        let key = crate::osm_id::osm_id_key(id);
        let ordinal = kind.ordinal();
        if let Some((last_kind, last_key)) = self.last_element
            && last_kind == ordinal
            && key < last_key
        {
            return Err(self.locate(crate::error::new_unsorted(format!(
                "streaming merge-join: {} {} follows {} {}. The input's header claims \
                 Sort.Type_then_ID but its elements are not in canonical order.",
                kind.as_str(),
                id,
                kind.as_str(),
                crate::osm_id::osm_id_from_key(last_key),
            ))));
        }
        self.last_element = Some((ordinal, key));
        Ok(element)
    }
}

fn element_kind(element: &Element<'_>) -> ElementKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
        Element::Way(_) => ElementKind::Way,
        Element::Relation(_) => ElementKind::Relation,
    }
}

/// A block holding more than one kind, in a merge that requires one kind per
/// block. Consuming it in one phase would discard its other-kind elements for
/// good: the block is gone by the time the later phase for its kind runs, and
/// a partially-consumed block cannot be stashed. This used to be waved
/// through with a comment calling it "acceptable lossy behavior", which made
/// a mixed block silently shorten the diff.
fn mixed_block() -> crate::Error {
    crate::error::new_unsorted(
        "streaming merge-join: input contains a mixed-type block, so elements \
         of a later type would be consumed and discarded by an earlier phase. \
         This merge requires Sort.Type_then_ID input with one element type per \
         block.",
    )
}

/// A block of an earlier kind than the phase or skip running. It used to be
/// stashed like a later kind's block and then dropped when no later phase
/// wanted it, so a node block after the ways vanished from the comparison.
fn unsorted_block(found: ElementKind, phase: ElementKind) -> crate::Error {
    crate::error::new_unsorted(format!(
        "streaming merge-join: found a {} block during the {} phase. The input's header \
         claims Sort.Type_then_ID but its blobs are not in canonical order (nodes, then \
         ways, then relations).",
        found.as_str(),
        phase.as_str(),
    ))
}

// ---------------------------------------------------------------------------
// Element-level streaming merge (inputs without indexdata)
// ---------------------------------------------------------------------------

/// Pull the next element of one side, refilling its block as needed, and
/// admit it ([`StreamingBlocks::admit`]). `None` once this side's phase is
/// over, which also sets `ended` so the phase never asks this side again.
///
/// A macro rather than a function because the side's block and the
/// iterator borrowing it are separate locals of [`stream_merge_phase`]: the
/// block is replaced while the other side's iterator, borrowing the other
/// side's block, stays alive. On every path below, `$iter` and `$head` are
/// written before they are read again once `$block` is reassigned, which is
/// what lets the borrow checker see that no borrow of the old block
/// survives its replacement. The exhausted block is dropped before the next
/// one is decoded, so a side never holds two decoded blocks.
macro_rules! pull_head {
    ($src:expr, $block:ident, $iter:ident, $ended:ident, $kind:expr) => {
        loop {
            if let Some(it) = $iter.as_mut()
                && let Some(element) = it.next()
            {
                break Some($src.admit(element, $kind)?);
            }
            drop($block.take());
            $block = $src.next_phase_block($kind)?;
            $iter = $block.as_ref().map(PrimitiveBlock::elements);
            if $iter.is_none() {
                $ended = true;
                break None;
            }
        }
    };
}

/// Run one type phase of the element-level merge over two inputs without
/// indexdata, handing each element to `on_action` borrowed, through the
/// same [`BlockMergeAction`] element variants [`block_pair_merge_phase`]
/// emits (never the blob variants: without indexdata no block's range is
/// known before it is decoded).
///
/// Each side holds its current decoded block and one validated head
/// element. A head is pulled only when the side has none, and is admitted
/// (kind and order checks) before the merge compares it, so nothing the
/// checks would refuse can influence what is emitted. "No head" has two
/// states, needs-refill and phase-ended; once a side's phase has ended
/// (end of file, or a later kind's block stashed) it is never asked again in
/// this phase, so the stashed block is not reclassified per remaining
/// element of the other side.
///
/// Only the current block of each side is held; elements are compared and
/// printed straight from it, never converted to owned values. A failure
/// found mid-block (a wrong-kind element, a backwards id) surfaces after
/// the earlier elements of that block were handed to `on_action`; the run
/// still fails.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn stream_merge_phase(
    old_src: &mut StreamingBlocks,
    new_src: &mut StreamingBlocks,
    kind: ElementKind,
    sink: &mut impl MergeSink,
) -> Result<()> {
    let equality = sink.equality();
    let common_version = sink.details().common_version;
    let mut on_action = |action: BlockMergeAction<'_>| sink.record(kind, action);
    let mut old_block: Option<PrimitiveBlock> = None;
    let mut new_block: Option<PrimitiveBlock> = None;
    let mut old_iter: Option<crate::BlockElementsIter<'_>> = None;
    let mut new_iter: Option<crate::BlockElementsIter<'_>> = None;
    let mut old_head: Option<Element<'_>> = None;
    let mut new_head: Option<Element<'_>> = None;
    let mut old_ended = false;
    let mut new_ended = false;

    loop {
        if old_head.is_none() && !old_ended {
            old_head = pull_head!(old_src, old_block, old_iter, old_ended, kind);
        }
        if new_head.is_none() && !new_ended {
            new_head = pull_head!(new_src, new_block, new_iter, new_ended, kind);
        }
        match (old_head.take(), new_head.take()) {
            (None, None) => return Ok(()),
            (Some(o), None) => on_action(BlockMergeAction::ElementOldOnly(ChangeElement::Full(o)))?,
            (None, Some(n)) => on_action(BlockMergeAction::ElementNewOnly(ChangeElement::Full(n)))?,
            (Some(o), Some(n)) => match crate::osm_id::osm_id_cmp(element_id(&o), element_id(&n)) {
                std::cmp::Ordering::Less => {
                    on_action(BlockMergeAction::ElementOldOnly(ChangeElement::Full(o)))?;
                    new_head = Some(n);
                }
                std::cmp::Ordering::Greater => {
                    on_action(BlockMergeAction::ElementNewOnly(ChangeElement::Full(n)))?;
                    old_head = Some(o);
                }
                std::cmp::Ordering::Equal => {
                    if equality.equal(&o, &n)? {
                        on_action(BlockMergeAction::ElementEqual {
                            id: element_id(&o),
                            version: common_version.then(|| element_version(&o)).flatten(),
                        })?;
                    } else {
                        on_action(BlockMergeAction::ElementModified {
                            old: ChangeElement::Full(o),
                            new: ChangeElement::Full(n),
                        })?;
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Borrowed element equality - zero-alloc comparison via iterators
// ---------------------------------------------------------------------------

/// Extract ID from any Element variant.
pub(crate) fn element_id(e: &Element<'_>) -> i64 {
    match e {
        Element::DenseNode(dn) => dn.id(),
        Element::Node(n) => n.id(),
        Element::Way(w) => w.id(),
        Element::Relation(r) => r.id(),
    }
}

/// The version any Element variant claims, for printing (the text diff's
/// `vN`). A version below 1 is "no version" - 0 from pbfhogg's
/// `BlockBuilder` and libosmium, -1 from Osmosis - so it is `None`.
pub(crate) fn element_version(e: &Element<'_>) -> Option<i32> {
    match e {
        Element::DenseNode(dn) => dn.info().and_then(crate::DenseNodeInfo::claimed_version),
        Element::Node(n) => n.info().claimed_version(),
        Element::Way(w) => w.info().claimed_version(),
        Element::Relation(r) => r.info().claimed_version(),
    }
}

// ---------------------------------------------------------------------------
// Block-pair merge engine - blob-level comparison with borrowed elements
// ---------------------------------------------------------------------------

/// Actions emitted by both merges, to a [`MergeSink`].
pub(crate) enum BlockMergeAction<'a> {
    /// All elements in this blob are unchanged (count from indexdata).
    /// Emitted by the compressed-byte comparison when `skip_equal_blobs` is
    /// true and a fresh old/new blob pair has identical compressed bytes.
    /// `first` and `last` are the pair's first and last IDs in OSM order,
    /// from its claim, which is exact because a mixed-sign claim (whose
    /// first ID the range cannot name) is never skipped.
    BlobEqual { count: u64, first: i64, last: i64 },
    /// Every element of the run exists only in old.
    BlobOldOnly(BlockRun<'a>),
    /// Every element of the run exists only in new.
    BlobNewOnly(BlockRun<'a>),
    /// Single element unchanged. Only common text requests the old version.
    ElementEqual { id: i64, version: Option<i32> },
    /// Single element modified (different content, same ID).
    ElementModified {
        old: ChangeElement<'a>,
        new: ChangeElement<'a>,
    },
    /// Single element only in old.
    ElementOldOnly(ChangeElement<'a>),
    /// Single element only in new.
    ElementNewOnly(ChangeElement<'a>),
}

/// The material a sink requests for an element it will report.
#[derive(Clone, Copy)]
pub(crate) enum ElementDetail {
    Id,
    Version,
    Full,
}

#[derive(Clone, Copy)]
pub(crate) struct MergeDetails {
    pub(crate) common_version: bool,
    /// One detail per side, in old and new order.
    pub(crate) modified: [ElementDetail; 2],
    pub(crate) only: [ElementDetail; 2],
}

pub(crate) enum ChangeElement<'a> {
    Compact { id: i64, version: Option<i32> },
    Full(Element<'a>),
}

impl<'a> ChangeElement<'a> {
    pub(crate) fn id(&self) -> i64 {
        match self {
            Self::Compact { id, .. } => *id,
            Self::Full(element) => element_id(element),
        }
    }

    pub(crate) fn version(&self) -> Option<i32> {
        match self {
            Self::Compact { version, .. } => *version,
            Self::Full(element) => element_version(element),
        }
    }

    pub(crate) fn full(&self) -> Result<&Element<'a>> {
        match self {
            Self::Full(element) => Ok(element),
            Self::Compact { .. } => Err(crate::Error::internal("diff sink needs a full element")),
        }
    }
}

/// A single-sided run of a decoded block: the rest of its window part, as
/// the cursor the merge left it at.
pub(crate) struct BlockRun<'a> {
    cursor: ElementCursor<'a>,
    /// Elements in the run, counted from the decoded block (its window
    /// part less what the merge consumed), never the blob's claimed count.
    pub(crate) count: u64,
}

impl<'a> BlockRun<'a> {
    /// Walk only this window's remaining elements, starting at the
    /// cursor left by the merge. A sink that only counts never calls this.
    pub(crate) fn try_for_each(
        self,
        detail: ElementDetail,
        mut record: impl FnMut(ChangeElement<'a>) -> Result<()>,
    ) -> Result<()> {
        let mut cursor = self.cursor;
        if matches!(detail, ElementDetail::Full) {
            for slot in cursor.next..cursor.end {
                record(ChangeElement::Full(cursor.full_at(slot)?))?;
            }
            return Ok(());
        }
        while let Some(row) = cursor.take()? {
            record(cursor.change(&row, detail)?)?;
        }
        Ok(())
    }
}

/// A resumable position in a decoded block's window part.
///
/// The merge keeps one per side across its rounds, so a block that outlives
/// several of the peer's blocks is walked once. The comparison walk has
/// no full metadata. When output wants a full element, a separate forward
/// walk preserves the dense metadata delta state, including the prefix
/// before a clipped window. Neither walk restarts at a peer block boundary.
struct ElementCursor<'a> {
    block: &'a BlockState,
    view: BlockView<'a>,
    rows: Rows<'a>,
    metadata: MetadataCursor<'a>,
    full: Option<Box<crate::BlockElementsIter<'a>>>,
    full_next: usize,
    /// Leading elements still to pass over before the first yield.
    skip: usize,
    next: usize,
    end: usize,
    head: Option<Row<'a>>,
}

impl<'a> ElementCursor<'a> {
    fn new(block: &'a BlockState) -> Self {
        let view = block.view();
        Self {
            rows: view.rows(),
            block,
            view,
            metadata: MetadataCursor::default(),
            full: None,
            full_next: 0,
            skip: block.skip,
            next: block.skip,
            end: block.end,
            head: None,
        }
    }

    /// The next element, left in place.
    fn peek_id(&mut self) -> Result<Option<i64>> {
        if self.head.is_none() && self.next < self.end {
            let skip = std::mem::take(&mut self.skip);
            self.head = Some(self.rows.nth(skip).ok_or_else(short_comparison_block)?);
        }
        Ok(self.head.as_ref().map(|row| row.id))
    }

    /// Elements left, the peeked head included.
    fn len(&self) -> usize {
        self.end - self.next
    }

    fn take(&mut self) -> Result<Option<Row<'a>>> {
        if self.peek_id()?.is_none() {
            return Ok(None);
        }
        self.take_head().map(Some)
    }

    fn take_head(&mut self) -> Result<Row<'a>> {
        let row = self.head.take().ok_or_else(short_comparison_block)?;
        self.next += 1;
        Ok(row)
    }

    fn full_at(&mut self, slot: usize) -> Result<Element<'a>> {
        let skip = slot
            .checked_sub(self.full_next)
            .ok_or_else(|| crate::Error::internal("diff full-element cursor moved backwards"))?;
        let block = self.block;
        let iter = match self.full {
            Some(ref mut iter) => iter,
            None => self.full.insert(Box::new(block.full()?.elements())),
        };
        let element = iter.nth(skip).ok_or_else(short_comparison_block)?;
        self.full_next = slot + 1;
        Ok(element)
    }

    fn change(&mut self, row: &Row<'a>, detail: ElementDetail) -> Result<ChangeElement<'a>> {
        let version = match detail {
            ElementDetail::Id => None,
            ElementDetail::Version => row.version(&mut self.metadata),
            ElementDetail::Full => {
                let element = self.full_at(self.next - 1)?;
                if element_id(&element) != row.id {
                    return Err(crate::Error::internal(
                        "diff cursor element ID disagrees with its row",
                    ));
                }
                return Ok(ChangeElement::Full(element));
            }
        };
        Ok(ChangeElement::Compact {
            id: row.id,
            version,
        })
    }
}

fn short_comparison_block() -> crate::Error {
    crate::Error::internal("diff cursor ended before the validated block count")
}

/// Where a merge's actions go: diff's text and counts, or derive's OSC
/// fragments. One implementation per command serves every path, so an
/// element prints the same whichever merge found it. Generic rather than
/// `dyn` so the per-element call inlines.
pub(crate) trait MergeSink {
    /// By value: a single-sided run hands over the merge's cursor, so the
    /// sink walks it once from where the merge left it.
    fn record(&mut self, kind: ElementKind, action: BlockMergeAction<'_>) -> Result<()>;

    /// The equality a same-ID pair is judged "equal" or "modified" by. No
    /// default: every sink names its own (see [`crate::content::Equality`]).
    fn equality(&self) -> crate::content::Equality;

    /// The metadata and full elements the indexed cursor must prepare.
    fn details(&self) -> MergeDetails;
}

/// One OsmData blob's provenance and index metadata, from its frame header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobDesc {
    /// File-wide ordinal and frame start; provenance for errors, never used
    /// for scheduling.
    pub(crate) blob_index: u64,
    pub(crate) frame_offset: u64,
    pub(crate) index: crate::blob_meta::BlobIndex,
    /// The claimed last key of the blob before this one in file order, when
    /// that blob is of the same kind. A mixed-sign claim cannot name its
    /// first element, so the header guard cannot see such a blob start
    /// before its predecessor ends; decoding checks its actual first element
    /// against this.
    pub(crate) prev_last: Option<OsmIdKey>,
}

impl BlobDesc {
    /// The OSM-order key of the first element the claim allows: exact for a
    /// single-sign range, conservatively key 0 for a mixed-sign one.
    pub(crate) fn first_key(&self) -> OsmIdKey {
        crate::osm_id::blob_osm_first_key(self.index.min_id, self.index.max_id)
    }

    /// The OSM-order key of the last element the claim allows.
    pub(crate) fn last_key(&self) -> OsmIdKey {
        crate::osm_id::blob_osm_last_key(self.index.min_id, self.index.max_id)
    }
}

/// One data blob read from its input: its descriptor and its body (the raw
/// `Blob` envelope), shared by every work unit whose window it intersects,
/// so a blob crossing a cut is read and held once.
#[derive(Clone)]
pub(crate) struct Frame {
    pub(crate) desc: BlobDesc,
    pub(crate) body: std::sync::Arc<Vec<u8>>,
}

/// The OSM-order key interval `[start, end)` a work unit owns, each bound
/// `None` when unbounded. Keys, not raw IDs, so negative IDs window in the
/// order the merge orders them (`osm_id_cmp`), and every element falls in
/// exactly one unit of its kind.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub(crate) start: Option<OsmIdKey>,
    pub(crate) end: Option<OsmIdKey>,
}

impl Window {
    /// Whether a blob's claimed range reaches outside the window, so its
    /// decoded block must be clipped. Claim-based, which decoding has proved
    /// exact by then ([`hold_decoded_claim`]), with the conservative key 0
    /// for a mixed-sign range: a crossing claim costs a clip, never an
    /// element.
    fn crosses(self, desc: &BlobDesc) -> bool {
        self.start.is_some_and(|s| desc.first_key() < s)
            || self.end.is_some_and(|e| desc.last_key() >= e)
    }
}

/// A decoded block of one side and its window part. The merge reads it
/// through an [`ElementCursor`] borrowing it.
struct BlockState {
    bytes: Vec<u8>,
    strings: Vec<(u32, u32)>,
    groups: Vec<(u32, u32)>,
    scan: crate::read::block::BlockScan,
    full: std::cell::OnceCell<PrimitiveBlock>,
    /// Elements before the window part (an earlier unit owns them).
    skip: usize,
    /// Exclusive bound of the window part.
    end: usize,
    /// For a clipped block, the ID of the last element inside the window:
    /// the furthest the block can still offer this unit.
    clip_last: Option<i64>,
    index: crate::blob_meta::BlobIndex,
}

impl BlockState {
    /// Nothing of the block is left in the window.
    fn spent(&self) -> bool {
        self.skip >= self.end
    }

    /// A cursor at the start of the window part.
    fn cursor(&self) -> ElementCursor<'_> {
        ElementCursor::new(self)
    }

    fn view(&self) -> BlockView<'_> {
        BlockView::new(&self.bytes, &self.groups, &self.strings, self.scan.fields)
    }

    /// The full reader owns its backing buffer. Build it once, only when
    /// a writer requests a changed element, while the comparison walk
    /// continues to borrow the original validated bytes.
    fn full(&self) -> Result<&PrimitiveBlock> {
        if let Some(block) = self.full.get() {
            return Ok(block);
        }
        let block = PrimitiveBlock::from_vec(self.bytes.clone()).map_err(|err| {
            crate::Error::internal_with("diff could not rebuild a validated block", err)
        })?;
        Ok(self.full.get_or_init(|| block))
    }
}

/// The decompression buffers one unindexed streaming merge recycles, shared
/// by both of its sides and every phase.
///
/// Each decoded block holds its buffer and hands it back here when it drops
/// ([`PrimitiveBlock::from_vec_pooled_with_scratch`]), so the next decode on
/// either side reuses it instead of allocating a fresh, often megabyte-sized
/// one per block. One pool rather than one per side, so a buffer freed on
/// one side or in one phase serves the other. The idle buffers it retains
/// never outnumber the decoded blocks the merge has held at once (a current
/// block per side and a stashed one), within the pool's own count and
/// per-buffer capacity limits. A buffer whose decompression fails, or whose
/// parse fails before the block wraps it, is freed rather than returned; one
/// that fails validation after wrapping returns as usual. Everything runs on
/// the merge's one thread, so the pool's lock is never contended.
///
/// The indexed block-pair merge does not pool: its shard sides decompress
/// into a fresh buffer per block, as the parallel shard workers always did
/// (see [`BlockPairSide::decode`]).
pub(crate) type MergeBuffers = std::sync::Arc<crate::blob::DecompressPool>;

/// A fresh, empty [`MergeBuffers`] for one merge of `invocation`.
pub(crate) fn merge_buffers(invocation: &crate::host::Recorder) -> MergeBuffers {
    crate::blob::DecompressPool::new(invocation)
}

/// Decompress and parse a blob's block into a buffer from `pool`, which the
/// block returns there when it drops. Unlocated; the caller locates.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn decode_blob(
    blob: &crate::blob::Blob,
    pool: &MergeBuffers,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> crate::error::Result<PrimitiveBlock> {
    let mut buf = pool.get();
    blob.decompress_into(&mut buf)?;
    PrimitiveBlock::from_vec_pooled_with_scratch(buf, pool, st_scratch, gr_scratch)
}

/// Running canonical-order check over one file's blob sequence, in FILE
/// order.
///
/// Each input's [`BlobCursor`] runs it over every data blob as its header
/// arrives, of a selected kind or not. Without it a merge sees only kind
/// transitions, so blobs out of order within one kind gave a wrong answer
/// (50 created and 50 deleted on identical content) where another path
/// refused the file.
///
/// Sorted blob order is also a precondition of the unit planner, which cuts
/// each kind's stream at blob first keys assuming they ascend, and of the
/// merge, which decides from blob ranges which blobs overlap.
///
/// The ranges are compared by `osm_id` keys, not raw `i64`, so negative IDs
/// order the way `osm_id_cmp` orders them everywhere else, and only disorder
/// the ranges prove is refused (`OrderWatermark`): every valid layout passes,
/// mixed-sign ones included.
pub(crate) struct BlobOrderGuard {
    watermark: crate::osm_id::OrderWatermark,
}

impl BlobOrderGuard {
    pub(crate) fn new() -> Self {
        Self {
            watermark: crate::osm_id::OrderWatermark::default(),
        }
    }

    /// Observe the next blob in file order. Returns the error message a
    /// caller should surface, or `Ok(())`.
    pub(crate) fn observe(&mut self, index: &crate::blob_meta::BlobIndex) -> crate::Result<()> {
        if let Err(v) = self
            .watermark
            .observe(index.kind.ordinal(), index.min_id, index.max_id)
        {
            return Err(crate::error::new_unsorted(format!(
                "block-pair merge: a {} blob holding id {} follows {} blobs \
                 already reaching id {} earlier in this file. The input's header \
                 claims Sort.Type_then_ID but, according to its indexdata, its \
                 blobs are not in canonical order.",
                index.kind.as_str(),
                v.first_id,
                ElementKind::from_ordinal(v.previous_kind).map_or("unknown", ElementKind::as_str),
                v.previous_id,
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Block-pair merge engine - blob-level comparison with borrowed elements
// ---------------------------------------------------------------------------

/// Shadow-mode counters for block-pair merge classification.
///
/// Accumulated per shard and summed over every shard and phase of a run.
/// What fraction of blobs hit the byte-equal fast path, how many get
/// element-merged after decoding both sides, and how many are emitted as
/// entirely single-sided.
///
/// Element counts come from the runs' counts (no per-element increments),
/// so the overhead is a handful of u64 adds per blob.
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct BlockPairMergeStats {
    /// Blob pairs that matched via compressed-byte equality (no decode).
    pub pairs_byte_equal: u64,
    pub elements_byte_equal: u64,
    /// Overlapping blob pairs that required decoding both sides for element merge.
    pub pairs_overlapping_decoded: u64,
    /// Sum of both sides' element counts for overlapping pairs.
    pub elements_overlapping_decoded: u64,
    /// Runs emitted as BlobOldOnly (entirely single-sided, all code paths).
    /// Covers fast-path non-overlap, end-of-shard drain, and residuals.
    pub blobs_old_only: u64,
    pub elements_old_only: u64,
    /// Runs emitted as BlobNewOnly (entirely single-sided, all code paths).
    pub blobs_new_only: u64,
    pub elements_new_only: u64,
}

impl BlockPairMergeStats {
    /// Fold another shard's counters into these.
    pub(crate) fn add(&mut self, other: &Self) {
        self.pairs_byte_equal += other.pairs_byte_equal;
        self.elements_byte_equal += other.elements_byte_equal;
        self.pairs_overlapping_decoded += other.pairs_overlapping_decoded;
        self.elements_overlapping_decoded += other.elements_overlapping_decoded;
        self.blobs_old_only += other.blobs_old_only;
        self.elements_old_only += other.elements_old_only;
        self.blobs_new_only += other.blobs_new_only;
        self.elements_new_only += other.elements_new_only;
    }
}

/// The refusal of an OsmData blob without readable indexdata, on every
/// block-pair route (the header walk and the one-shard stream). Unlocated.
pub(crate) fn missing_index_error() -> crate::Error {
    crate::error::new_missing_indexdata(
        "block-pair merge requires readable indexdata on every data blob \
         (this blob has none, or its indexdata has an unknown version, \
         an unknown kind or an inverted id range)",
    )
}

/// One input of a block-pair merge, read as one sequential stream across
/// every kind, taking each header as it arrives and reading the bodies of
/// the selected kinds from the same stream.
///
/// There is no up-front walk: a whole-file header walk is a chain of
/// dependent `pread`s, one per blob, which cost about a quarter of a planet
/// diff's wall before any merge started. The stream is a command-input
/// [`crate::blob::BlobReader`], so it honours `--direct-io` and the host's
/// page-cache eviction, and refuses a partially indexed file through the
/// frame walk's own guard.
///
/// Every check is made as each header arrives, in file order: every data
/// header, of a selected kind or not, is observed once by
/// [`BlobOrderGuard`] and must carry readable indexdata; the first frame is
/// the OsmHeader the caller's front read already judged and is skipped
/// unread, as are frames of other types. [`Self::finish`] walks what no
/// kind consumed, so a defect past the last selected kind is still refused
/// before success. A defect late in a file is therefore reported after the
/// output written before it.
pub(crate) struct BlobCursor {
    reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    path: std::sync::Arc<std::path::Path>,
    order: BlobOrderGuard,
    /// The leading frame has been passed over.
    started: bool,
    /// Clean EOF was reached; never polled again.
    exhausted: bool,
    /// An observed data header whose payload is still pending in the
    /// reader: the next blob, of the running kind or a later one.
    lookahead: Option<BlobDesc>,
    /// The kind and claimed last key of the last data blob observed, which
    /// becomes the next blob's [`BlobDesc::prev_last`] when it is of that
    /// kind.
    last_of_kind: Option<(ElementKind, OsmIdKey)>,
}

impl BlobCursor {
    /// Open `source` as a command input, buffered or `O_DIRECT` per
    /// `direct_io`, under `host`'s eviction policy.
    pub(crate) fn open(
        source: crate::read::input::InputSource<'_>,
        direct_io: bool,
        host: &crate::host::Entered<'_>,
    ) -> Result<Self> {
        let path: std::sync::Arc<std::path::Path> = std::sync::Arc::from(source.path());
        let mut reader = crate::blob::BlobReader::open_in(source, direct_io, host)?;
        reader.set_parse_indexdata(true);
        Ok(Self {
            reader,
            path,
            order: BlobOrderGuard::new(),
            started: false,
            exhausted: false,
            lookahead: None,
            last_of_kind: None,
        })
    }

    #[cold]
    fn locate(&self, blob_index: u64, frame_offset: u64, err: crate::Error) -> crate::Error {
        err.locate(crate::error::Location::new(
            Some(&self.path),
            Some(blob_index),
            Some(crate::ByteOffset(frame_offset)),
        ))
    }

    /// Ensure a data header is pending in `lookahead`, unless at EOF.
    fn advance(&mut self) -> Result<()> {
        if self.lookahead.is_some() || self.exhausted {
            return Ok(());
        }
        loop {
            let Some(header) = self.reader.next_frame_header()? else {
                self.exhausted = true;
                return Ok(());
            };
            if !self.started {
                self.started = true;
                self.reader.skip_frame_payload()?;
                continue;
            }
            if !matches!(header.header.blob_type, crate::blob::BlobKind::OsmData) {
                self.reader.skip_frame_payload()?;
                continue;
            }
            // A walk from offset 0 that has not seeked always knows both.
            let frame_offset = header.id.start.unwrap_or(0);
            let blob_index = header.id.ordinal.unwrap_or(0);
            let index = header
                .header
                .indexdata
                .as_ref()
                .and_then(|data| crate::blob_meta::BlobIndex::deserialize(data));
            let Some(index) = index else {
                // The frame is completed first, as the walk does, so a
                // truncated payload or a partially indexed file is that
                // error rather than this one.
                self.reader.skip_frame_payload()?;
                return Err(self.locate(blob_index, frame_offset, missing_index_error()));
            };
            if let Err(e) = self.order.observe(&index) {
                return Err(self.locate(blob_index, frame_offset, e));
            }
            let prev_last = self
                .last_of_kind
                .filter(|(kind, _)| *kind == index.kind)
                .map(|(_, key)| key);
            self.last_of_kind = Some((
                index.kind,
                crate::osm_id::blob_osm_last_key(index.min_id, index.max_id),
            ));
            self.lookahead = Some(BlobDesc {
                blob_index,
                frame_offset,
                index,
                prev_last,
            });
            return Ok(());
        }
    }

    /// Pass over blobs of kinds before `kind` (excluded by the caller's type
    /// filter; the order guard has seen them) and say whether the next blob
    /// is of `kind`. After it, the cursor is at EOF or holds a blob of
    /// `kind` or a later kind.
    pub(crate) fn reaches(&mut self, kind: ElementKind) -> Result<bool> {
        loop {
            self.advance()?;
            match self.lookahead {
                None => return Ok(false),
                Some(desc) if desc.index.kind.ordinal() < kind.ordinal() => {
                    self.lookahead = None;
                    self.reader.skip_frame_payload()?;
                }
                Some(desc) => return Ok(desc.index.kind == kind),
            }
        }
    }

    /// The descriptor of the next blob of `kind`, its body not yet read;
    /// `None` once the next blob is of a later kind or the file ends.
    pub(crate) fn peek(&mut self, kind: ElementKind) -> Result<Option<BlobDesc>> {
        if !self.reaches(kind)? {
            return Ok(None);
        }
        Ok(self.lookahead)
    }

    /// The next blob of `kind` with its body (the raw `Blob` envelope);
    /// `None` once the next blob is of a later kind or the file ends.
    pub(crate) fn next_of(&mut self, kind: ElementKind) -> Result<Option<Frame>> {
        if !self.reaches(kind)? {
            return Ok(None);
        }
        let Some(desc) = self.lookahead.take() else {
            return Err(crate::Error::internal(
                "block-pair stream: no pending blob after reaching its kind",
            ));
        };
        let mut body = Vec::new();
        self.reader.read_frame_payload(&mut body)?;
        Ok(Some(Frame {
            desc,
            body: std::sync::Arc::new(body),
        }))
    }

    /// The path this cursor reads, for locating errors.
    pub(crate) fn path(&self) -> &std::sync::Arc<std::path::Path> {
        &self.path
    }

    /// Walk the rest of the file, headers checked and bodies skipped.
    pub(crate) fn finish(&mut self) -> Result<()> {
        loop {
            self.advance()?;
            if self.lookahead.take().is_none() {
                return Ok(());
            }
            self.reader.skip_frame_payload()?;
        }
    }
}

/// One input of a block-pair merge over one work unit: the unit's frames of
/// that input, in file order, clipped to the unit's window.
///
/// The merge takes a fresh blob from each side before it decides whether to
/// decode, compare or emit it, and hands one back ([`Self::hold`]) when only
/// the other is emitted, so the next round can still try the byte
/// comparison on it.
pub(crate) struct BlockPairSide<'a> {
    frames: &'a [Frame],
    /// Index of the next frame [`Self::next_blob`] yields.
    next: usize,
    /// Index of the frame [`Self::next_blob`] yielded last.
    current: usize,
    /// The current frame was handed back: yield it again.
    held: bool,
    path: &'a std::sync::Arc<std::path::Path>,
    window: Window,
    ids: crate::read::project_ids::IdScratch,
}

impl<'a> BlockPairSide<'a> {
    /// A side over `frames`, one kind's blobs of one input intersecting
    /// `window`, in file order. Errors are located in `path`.
    pub(crate) fn new(
        frames: &'a [Frame],
        path: &'a std::sync::Arc<std::path::Path>,
        window: Window,
    ) -> Self {
        Self {
            frames,
            next: 0,
            current: 0,
            held: false,
            path,
            window,
            ids: crate::read::project_ids::IdScratch::default(),
        }
    }

    #[cold]
    fn locate(&self, desc: &BlobDesc, err: crate::Error) -> crate::Error {
        locate_in(self.path, desc, err)
    }

    /// The next blob of the unit, its body not yet parsed.
    fn next_blob(&mut self) -> Option<BlobDesc> {
        if self.held {
            self.held = false;
            return self.frames.get(self.current).map(|f| f.desc);
        }
        let frame = self.frames.get(self.next)?;
        self.current = self.next;
        self.next += 1;
        Some(frame.desc)
    }

    /// Hand the blob [`Self::next_blob`] returned last back, undecoded.
    fn hold(&mut self) {
        self.held = true;
    }

    /// The body of the blob [`Self::next_blob`] returned last.
    fn body(&self) -> &[u8] {
        self.frames
            .get(self.current)
            .map_or(&[], |f| f.body.as_slice())
    }

    /// The payload of the current blob, for the byte comparison.
    fn payload(&self, desc: &BlobDesc) -> Result<Option<crate::blob::BlobDecodeKey<'_>>> {
        crate::blob::raw_blob_payload(self.body()).map_err(|e| self.locate(desc, e))
    }

    /// Decompress and parse the current blob, refuse it unless its claim is
    /// exact and its elements in order ([`hold_decoded_claim`], before any
    /// clip can hide an element), and clip it to the window when its claimed
    /// range crosses it. Every [`BlockState`] is built here, so every block
    /// the merge reads, emits or residualises has passed that check.
    ///
    /// Keep the already-validated payload and its offset tables for the
    /// comparison cursor. No inline reader tables are needed until a
    /// writer requests a full element.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn decode(&mut self, desc: BlobDesc) -> Result<BlockState> {
        let mut decompressed: Vec<u8> = Vec::new();
        if let Err(e) = crate::blob::decompress_blob_raw(self.body(), &mut decompressed) {
            return Err(self.locate(&desc, e));
        }
        // The planner placed this blob by its claimed kind, so an element of
        // another kind would be merged against the wrong file's blobs of
        // that kind (spurious created/deleted) or dropped: the claimed kind
        // is refused by validation itself.
        let (mut strings, mut groups) = (Vec::new(), Vec::new());
        let path = self.path;
        let ids = &mut self.ids;
        let (_, scan) = crate::read::block::validate_in_place(
            &decompressed,
            &mut strings,
            &mut groups,
            Some(desc.index.kind),
            |_, _| crate::read::project_ids::KindIds::of(desc.index.kind, ids),
        )
        .map_err(|e| locate_in(path, &desc, e))?;
        hold_decoded_claim(&self.ids, &decompressed, &desc).map_err(|e| self.locate(&desc, e))?;
        let count = usize::try_from(scan.element_count)
            .map_err(|_| crate::Error::internal("diff block count does not fit in usize"))?;
        let (skip, end, clip_last) = if self.window.crosses(&desc) {
            clip(&self.ids, &decompressed, self.window, count).map_err(|e| self.locate(&desc, e))?
        } else {
            (0, count, None)
        };
        Ok(BlockState {
            bytes: decompressed,
            strings,
            groups,
            scan,
            full: std::cell::OnceCell::new(),
            skip,
            end,
            clip_last,
            index: desc.index,
        })
    }

    /// The next blob of the unit, decoded, passing over any that holds
    /// nothing inside the window.
    fn next_decoded(&mut self) -> Result<Option<BlockState>> {
        while let Some(desc) = self.next_blob() {
            let state = self.decode(desc)?;
            if !state.spent() {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }
}

/// The window part of a block whose claim crosses the window: the number of
/// leading elements before the window's start (an earlier unit owns them),
/// the index of the first element at or past its end (the block's length
/// when none is), and the ID of the last element inside. Counted from the
/// elements, so a clipped run's count is exact. The validated ID
/// projection is in the same element order as the comparison cursor.
fn clip(
    ids: &crate::read::project_ids::IdScratch,
    bytes: &[u8],
    window: Window,
    count: usize,
) -> Result<(usize, usize, Option<i64>)> {
    let mut skip = 0usize;
    let mut end = count;
    let mut slot = 0usize;
    let mut last_inside = None;
    ids.each(bytes, |_, id| {
        if slot < end {
            let key = crate::osm_id::osm_id_key(id);
            if window.end.is_some_and(|bound| key >= bound) {
                end = slot;
            } else if window.start.is_some_and(|start| key < start) {
                skip = slot + 1;
            } else {
                last_inside = Some(id);
            }
        }
        slot += 1;
        Ok(())
    })?;
    Ok((skip, end, last_inside))
}

/// `err` located at blob `desc` of the input at `path`.
#[cold]
fn locate_in(
    path: &std::sync::Arc<std::path::Path>,
    desc: &BlobDesc,
    err: crate::Error,
) -> crate::Error {
    err.locate(crate::error::Location::new(
        Some(path),
        Some(desc.blob_index),
        Some(crate::ByteOffset(desc.frame_offset)),
    ))
}

/// The order a decoded block must hold, named in its refusal.
const DECODED_ORDER: crate::blob_meta::Order =
    crate::blob_meta::Order::Required("diff pairs each input's elements in canonical order");

/// Refuse a decoded block whose claim is not exact for its elements, whose
/// elements are out of canonical order, or which starts before the blob
/// preceding it in its file ends. ADR-0013: a run that decodes a blob
/// checks the claim it relied on, and every placement this engine makes
/// rests on the claim being exact. The planner carries a blob only while
/// its claimed last key reaches the next unit, so an element past an
/// understated claim would belong to no unit; a claim-disjoint blob is
/// emitted whole as one-sided, so an element past its claim would appear
/// created and deleted at once; and the merge boundary is a claimed last,
/// which decides elements below it single-sided on the strength of the
/// peer's next blob starting at or past it. Exact claims and in-order
/// blocks make each of those true. `ids` is the block's IDs of the claimed
/// kind, recorded by validation in `bytes`, so dense IDs are reconstructed
/// from their column alone.
///
/// A mixed-sign claim proves neither its first element nor, so, that the
/// blob starts after its predecessor's last: the header guard compares the
/// claim's present first key, its `min_id`, and the blob's elements may
/// start at any smaller key. Its actual first element is checked here
/// instead. A blob of one sign has an exact claimed first, which the guard
/// already weighed.
///
/// A byte-identical pair skipped undecoded is not checked; its claim is
/// believed (reference/indexdata-trust.md).
fn hold_decoded_claim(
    ids: &crate::read::project_ids::IdScratch,
    bytes: &[u8],
    desc: &BlobDesc,
) -> Result<()> {
    let claim = &desc.index;
    // Whether the claim names its first element: false only for a
    // mixed-sign range, whose conservative first key is not one it proves.
    let names_first = crate::osm_id::blob_osm_first_key(claim.min_id, claim.max_id)
        == crate::osm_id::blob_osm_present_first_key(claim.min_id, claim.max_id);
    if names_first {
        // `hold_claim` decides a non-negative claim from the tally and walks
        // any other through the canonical replay.
        let mut tally = crate::blob_meta::AscendingTally::default();
        ids.each(bytes, |_, id| {
            tally.add(id);
            Ok(())
        })?;
        return crate::blob_meta::hold_claim(&tally, claim, DECODED_ORDER, || {
            let mut all = Vec::new();
            // Already walked once without error; a second walk of the same
            // accepted bytes yields the same IDs, so this cannot fail. Were
            // it to, the empty replay still refuses.
            if ids
                .each(bytes, |_, id| {
                    all.push(id);
                    Ok(())
                })
                .is_err()
            {
                all.clear();
            }
            all
        });
    }
    let mut walk = crate::blob_meta::ClaimWalk::new(Some(claim), DECODED_ORDER);
    ids.each(bytes, |_, id| walk.observe(id))?;
    let first = walk.finish()?;
    if let (Some(first), Some(prev_last)) = (first, desc.prev_last)
        && crate::osm_id::osm_id_key(first) < prev_last
    {
        return Err(crate::error::new_unsorted(format!(
            "block-pair merge: a {} blob starts at id {first}, before the id {} the blob \
             preceding it in this file reaches. The input's header claims Sort.Type_then_ID \
             but its blobs are not in canonical order.",
            claim.kind.as_str(),
            crate::osm_id::osm_id_from_key(prev_last),
        )));
    }
    Ok(())
}

/// Drain the rest of one side's shard as single-sided runs.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn drain_remaining<S: MergeSink>(
    side: &mut BlockPairSide<'_>,
    is_old: bool,
    kind: ElementKind,
    sink: &mut S,
    stats: &mut BlockPairMergeStats,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    while let Some(bs) = side.next_decoded()? {
        cancel.check()?;
        emit_block(&bs, is_old, kind, sink, stats)?;
    }
    Ok(())
}

/// Emit a decoded block's remaining run as BlobOldOnly or BlobNewOnly.
/// The range claim decided routing, and decoding proved it exact
/// ([`hold_decoded_claim`]), so no element of the run can lie in the
/// peer's range.
fn emit_block<S: MergeSink>(
    bs: &BlockState,
    is_old: bool,
    kind: ElementKind,
    sink: &mut S,
    stats: &mut BlockPairMergeStats,
) -> Result<()> {
    if bs.spent() {
        return Ok(());
    }
    emit_run(bs.cursor(), is_old, kind, sink, stats)
}

/// Emit what is left at `cursor` as BlobOldOnly or BlobNewOnly: a fresh
/// block's window part, or a residual's rest from where the merge left it.
fn emit_run<S: MergeSink>(
    cursor: ElementCursor<'_>,
    is_old: bool,
    kind: ElementKind,
    sink: &mut S,
    stats: &mut BlockPairMergeStats,
) -> Result<()> {
    let run = BlockRun {
        count: cursor.len() as u64,
        cursor,
    };
    if is_old {
        stats.blobs_old_only += 1;
        stats.elements_old_only += run.count;
        sink.record(kind, BlockMergeAction::BlobOldOnly(run))
    } else {
        stats.blobs_new_only += 1;
        stats.elements_new_only += run.count;
        sink.record(kind, BlockMergeAction::BlobNewOnly(run))
    }
}

/// The OSM-order last ID a decoded block can still offer: its claimed last,
/// exact once decoded, or, for a block clipped to the window, the earlier
/// of that and its last element inside the window.
fn state_last(bs: &BlockState) -> i64 {
    let last = blob_last(&bs.index);
    match bs.clip_last {
        Some(inside) if crate::osm_id::osm_id_cmp(inside, last).is_lt() => inside,
        _ => last,
    }
}

/// Merge a pair of decoded overlapping blocks from where their cursors
/// stand, up to the earlier of their lasts; the outcome says which side
/// keeps a residual, its cursor left at the residual's head.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn merge_decoded_pair<S: MergeSink>(
    os: &BlockState,
    ns: &BlockState,
    old_cur: &mut ElementCursor<'_>,
    new_cur: &mut ElementCursor<'_>,
    kind: ElementKind,
    sink: &mut S,
) -> Result<MergePairOutcome> {
    // The merge boundary is the earlier of the two blocks' OSM-order last
    // IDs. `osm_id_cmp` (not raw `i64` order) throughout, because the merge
    // itself orders by `osm_id_cmp`: for negative IDs, which `osm_id.rs`
    // explicitly supports as 0, -1, -2, ..., 1, 2, ..., a raw `min` picks a
    // boundary that cuts the streams in a different place than the order
    // they are being merged in.
    let old_last = state_last(os);
    let new_last = state_last(ns);
    let merge_up_to = if crate::osm_id::osm_id_cmp(old_last, new_last).is_le() {
        old_last
    } else {
        new_last
    };

    let outcome = element_merge_pair(old_cur, new_cur, merge_up_to, kind, sink)?;

    // Decoding proved both claims exact and both blocks in order
    // ([`hold_decoded_claim`]), so the side whose last is the boundary ends
    // there, and only the other keeps elements past it; a side held at the
    // boundary key is one whose peer ran out. Both keeping one is a broken
    // engine invariant, and retrying the same pair would never progress.
    if outcome.old_has_more && outcome.new_has_more {
        return Err(crate::Error::internal(format!(
            "block-pair merge: both blocks still hold elements after merging up to ID \
             {merge_up_to}, though both claims were checked exact"
        )));
    }
    Ok(outcome)
}

/// The OSM-order last ID a blob's index claims to cover.
fn blob_last(index: &crate::blob_meta::BlobIndex) -> i64 {
    crate::osm_id::blob_osm_last_id(index.min_id, index.max_id)
}

/// True when blob `a` ends, in OSM order, strictly before blob `b` begins -
/// i.e. the two ID spans do not overlap and `a` comes first.
///
/// Uses the `osm_id` blob-range keys rather than raw `max_id < min_id`, so
/// the non-overlap test agrees with the order the elements are merged in.
/// Claimed ranges, also for a clipped block: conservative, since a decoded
/// block's claim is exact ([`hold_decoded_claim`]) and its remaining
/// elements lie inside it. A fresh blob compared before decoding is decoded
/// before it is emitted, so the claim it was routed by is checked then.
fn blob_entirely_before(a: &crate::blob_meta::BlobIndex, b: &crate::blob_meta::BlobIndex) -> bool {
    crate::osm_id::blob_osm_last_key(a.min_id, a.max_id)
        < crate::osm_id::blob_osm_first_key(b.min_id, b.max_id)
}

/// Run the block-pair merge over one work unit of one kind.
///
/// `old` and `new` hold the unit's blobs of `kind`, whose headers the input
/// cursors checked for indexdata and canonical order. Falls through to
/// element-level comparison for overlapping blocks.
///
/// When `skip_equal_blobs` is true, a fresh overlapping blob pair (neither
/// side partly consumed, neither crossing the window) with identical claims
/// and identical compressed payload emits `BlobEqual` without
/// decompression. Callers that need per-element output for unchanged
/// elements (diff with `!suppress_common`, and its count-only twin) pass
/// `false`.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines)]
pub(crate) fn block_pair_merge_phase<S: MergeSink>(
    old: &mut BlockPairSide<'_>,
    new: &mut BlockPairSide<'_>,
    kind: ElementKind,
    skip_equal_blobs: bool,
    sink: &mut S,
    stats: &mut BlockPairMergeStats,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    // Each side's decoded block and the cursor borrowing it are separate
    // locals, as in `stream_merge_phase`: a block that outlives several of
    // the peer's is read on from where the last round left it. A cursor is
    // `None` exactly when its side has no residual, and every write of a
    // block slot is followed by a write of its cursor before the cursor is
    // read again, which is what lets the borrow checker see that no
    // borrow of the replaced block survives.
    let mut old_state: Option<BlockState> = None;
    let mut new_state: Option<BlockState> = None;
    let mut old_cur: Option<ElementCursor<'_>> = None;
    let mut new_cur: Option<ElementCursor<'_>> = None;

    loop {
        // Once per merge step, each of which decodes at most one blob per
        // side.
        cancel.check()?;
        // Fast path: both sides need fresh blobs (no residuals). Taken
        // without decompressing so the compressed bytes can be compared.
        // Only here: a residual is partly consumed, and comparing it whole
        // against a fresh blob would count its consumed elements twice.
        if old_cur.is_none() && new_cur.is_none() {
            let op = old.next_blob();
            let np = new.next_blob();

            match (op, np) {
                (None, None) => break,
                (Some(op), None) => {
                    let os = old.decode(op)?;
                    emit_block(&os, true, kind, sink, stats)?;
                    drain_remaining(old, true, kind, sink, stats, cancel)?;
                    break;
                }
                (None, Some(np)) => {
                    let ns = new.decode(np)?;
                    emit_block(&ns, false, kind, sink, stats)?;
                    drain_remaining(new, false, kind, sink, stats, cancel)?;
                    break;
                }
                (Some(op), Some(np)) => {
                    if blob_entirely_before(&op.index, &np.index) {
                        let os = old.decode(op)?;
                        emit_block(&os, true, kind, sink, stats)?;
                        // Hold the new blob undecoded - the next round can
                        // try the byte comparison on it.
                        new.hold();
                        continue;
                    }
                    if blob_entirely_before(&np.index, &op.index) {
                        let ns = new.decode(np)?;
                        emit_block(&ns, false, kind, sink, stats)?;
                        old.hold();
                        continue;
                    }
                    if skip_equal_blobs && blobs_byte_equal(old, &op, new, &np)? {
                        stats.pairs_byte_equal += 1;
                        stats.elements_byte_equal += op.index.count;
                        sink.record(
                            kind,
                            BlockMergeAction::BlobEqual {
                                count: op.index.count,
                                first: crate::osm_id::blob_osm_first_id(
                                    op.index.min_id,
                                    op.index.max_id,
                                ),
                                last: blob_last(&op.index),
                            },
                        )?;
                        continue;
                    }
                    stats.pairs_overlapping_decoded += 1;
                    let os = old.decode(op)?;
                    let ns = new.decode(np)?;
                    stats.elements_overlapping_decoded +=
                        os.scan.element_count + ns.scan.element_count;
                    old_state = Some(os).filter(|bs| !bs.spent());
                    old_cur = old_state.as_ref().map(BlockState::cursor);
                    new_state = Some(ns).filter(|bs| !bs.spent());
                    new_cur = new_state.as_ref().map(BlockState::cursor);
                }
            }
        }

        // Slow path: at least one side has a decoded residual block. A side
        // without one takes its next block, and a cursor over it, here; the
        // cursor is always rewritten right after its block is replaced.
        if old_cur.is_none() {
            old_state = old.next_decoded()?;
            old_cur = old_state.as_ref().map(BlockState::cursor);
        }
        if new_cur.is_none() {
            new_state = new.next_decoded()?;
            new_cur = new_state.as_ref().map(BlockState::cursor);
        }

        match (old_cur.take(), new_cur.take()) {
            (None, None) => break,
            (Some(oc), None) => emit_run(oc, true, kind, sink, stats)?,
            (None, Some(nc)) => emit_run(nc, false, kind, sink, stats)?,
            (Some(mut oc), Some(mut nc)) => {
                let (Some(os), Some(ns)) = (old_state.as_ref(), new_state.as_ref()) else {
                    return Err(crate::Error::internal(
                        "block-pair merge: a cursor outlived its block",
                    ));
                };
                if blob_entirely_before(&os.index, &ns.index) {
                    emit_run(oc, true, kind, sink, stats)?;
                    new_cur = Some(nc);
                    continue;
                }
                if blob_entirely_before(&ns.index, &os.index) {
                    emit_run(nc, false, kind, sink, stats)?;
                    old_cur = Some(oc);
                    continue;
                }
                let outcome = merge_decoded_pair(os, ns, &mut oc, &mut nc, kind, sink)?;
                if outcome.old_has_more {
                    old_cur = Some(oc);
                }
                if outcome.new_has_more {
                    new_cur = Some(nc);
                }
            }
        }
    }

    Ok(())
}

/// Whether a fresh pending pair has identical index claims, compression kind,
/// declared `raw_size` and compressed bytes - every envelope input the
/// decoder reads, so the two decode alike or fail alike, and the pair is
/// believed without decoding either (a corrupt payload or a false claim in a
/// skipped pair goes unseen; nothing here validates). Never for a
/// pair crossing the window: a clipped block's count is not its claim.
/// The envelopes are parsed (and a malformed one refused) only for a pair
/// whose claims match.
///
/// Never for a claim whose range cannot name its first ID either, a range
/// spanning negative and positive IDs, whose elements start at 0 or at any
/// negative: there the lower bound on its first key and the first key it
/// proves present differ. Derive needs the skipped pair's exact first ID to
/// see an ID that repeats across its edge. That pair is decoded; a sorted input holds at most one such
/// blob per kind.
fn blobs_byte_equal(
    old: &BlockPairSide<'_>,
    op: &BlobDesc,
    new: &BlockPairSide<'_>,
    np: &BlobDesc,
) -> Result<bool> {
    if op.index.min_id != np.index.min_id
        || op.index.max_id != np.index.max_id
        || op.index.count != np.index.count
        || crate::osm_id::blob_osm_first_key(op.index.min_id, op.index.max_id)
            != crate::osm_id::blob_osm_present_first_key(op.index.min_id, op.index.max_id)
    {
        return Ok(false);
    }
    if old.window.crosses(op) {
        return Ok(false);
    }
    let a = old.payload(op)?;
    let b = new.payload(np)?;
    Ok(match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    })
}

/// Two-pointer element merge over a pair of decoded blocks, each bounded to
/// its window part.
///
/// Processes elements up to `merge_up_to` ID (inclusive). Elements beyond that
/// boundary in either block are left unconsumed for the caller to handle.
///
/// Both cursors advance past what is consumed and stay at the first element
/// left, so the next round resumes there. Returns a [`MergePairOutcome`]:
/// whether either side still holds elements past the boundary.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn element_merge_pair<S: MergeSink>(
    old_iter: &mut ElementCursor<'_>,
    new_iter: &mut ElementCursor<'_>,
    merge_up_to: i64,
    kind: ElementKind,
    sink: &mut S,
) -> Result<MergePairOutcome> {
    // `osm_id_cmp`, not raw `<=`: every ordering decision below already
    // uses it, and for negative IDs the two disagree - a raw boundary test
    // would cut the streams somewhere the merge order does not.
    let in_range = |&id: &i64| crate::osm_id::osm_id_cmp(id, merge_up_to).is_le();
    let pair = Pair::new(&old_iter.view, &new_iter.view);
    let equality = sink.equality();
    let details = sink.details();

    loop {
        let old_head = old_iter.peek_id()?;
        let new_head = new_iter.peek_id()?;
        // Equal IDs pair in the order they occur on each side, as on the
        // element stream, wherever the blobs happen to end. An ID may repeat
        // in canonical order, and a block that has run out may be followed
        // by a blob starting with the same ID: its next blob starts at or
        // after its claimed last, which is at or after the boundary. So a
        // head at the boundary ID whose peer's block is exhausted is not
        // single-sided yet; it waits, as a residual, for the peer's next
        // blob. A head below the boundary cannot meet that next blob, and a
        // peer still holding an element (necessarily past the boundary)
        // has no more of the ID, so both are decided here.
        let order = match (old_head.filter(in_range), new_head.filter(in_range)) {
            (None, None) => break,
            (Some(o_id), None) => {
                if o_id == merge_up_to && new_head.is_none() {
                    break;
                }
                std::cmp::Ordering::Less
            }
            (None, Some(n_id)) => {
                if n_id == merge_up_to && old_head.is_none() {
                    break;
                }
                std::cmp::Ordering::Greater
            }
            (Some(o_id), Some(n_id)) => crate::osm_id::osm_id_cmp(o_id, n_id),
        };
        match order {
            std::cmp::Ordering::Less => {
                let row = old_iter.take_head()?;
                let old = old_iter.change(&row, details.only[0])?;
                sink.record(kind, BlockMergeAction::ElementOldOnly(old))?;
            }
            std::cmp::Ordering::Greater => {
                let row = new_iter.take_head()?;
                let new = new_iter.change(&row, details.only[1])?;
                sink.record(kind, BlockMergeAction::ElementNewOnly(new))?;
            }
            std::cmp::Ordering::Equal => {
                let old = old_iter.take_head()?;
                let new = new_iter.take_head()?;
                let action = if pair.equal(
                    (&old_iter.view, &new_iter.view),
                    &old,
                    &new,
                    equality,
                    (&mut old_iter.metadata, &mut new_iter.metadata),
                )? {
                    BlockMergeAction::ElementEqual {
                        id: old.id,
                        version: details
                            .common_version
                            .then(|| old.version(&mut old_iter.metadata))
                            .flatten(),
                    }
                } else {
                    BlockMergeAction::ElementModified {
                        old: old_iter.change(&old, details.modified[0])?,
                        new: new_iter.change(&new, details.modified[1])?,
                    }
                };
                sink.record(kind, action)?;
            }
        }
    }

    Ok(MergePairOutcome {
        old_has_more: old_iter.peek_id()?.is_some(),
        new_has_more: new_iter.peek_id()?.is_some(),
    })
}

/// What one [`element_merge_pair`] call did, and what it left behind.
///
/// `*_has_more` is what the bounded cursor actually observed, not what
/// the blob's indexdata claims; the residual decision is made from these.
struct MergePairOutcome {
    /// The side keeps a residual: elements past the boundary, or repeats of
    /// the boundary ID held for the peer's next blob.
    old_has_more: bool,
    new_has_more: bool,
}
