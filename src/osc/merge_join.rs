//! Merge-joins for sorted PBF operations, used by `diff` and
//! `derive_changes`.
//!
//! [`block_pair_merge_phase`] is the blob-level merge over two indexed
//! inputs, run once per shard of the ID space (one shard at `-j 1`) over
//! [`BlockPairSide`]s the header walk planned. [`stream_merge_phase`] is the
//! element-level merge over two [`StreamingBlocks`] cursors, for inputs
//! without indexdata. Both hand the caller borrowed elements through the
//! same [`BlockMergeAction`] variants to one [`MergeSink`] per command, so
//! an element prints the same way whichever path found it.

use crate::ElementKind;
use crate::{BlockType, Element, PrimitiveBlock};

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
        host: &crate::host::Host,
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
    /// The elements of a dropped block are iterated to classify it but never
    /// compared or printed, so element-level faults (a tag or role whose
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
            match element_kinds(&block) {
                BlockKinds::Empty => {}
                BlockKinds::One(k) if k == kind => {}
                BlockKinds::One(k) if k > kind => {
                    self.stashed = Some(block);
                    return Ok(());
                }
                BlockKinds::One(k) => return Err(self.locate(unsorted_block(k, kind))),
                BlockKinds::Several => return Err(self.locate(mixed_block())),
            }
        }
    }

    /// The next block of the phase's `kind`, or `None` when this side's
    /// phase is over (end of file, or the first block of a later kind,
    /// stashed for the phase or skip that takes it next).
    ///
    /// A block whose [`BlockType`] names `kind` is taken directly (the
    /// normal sorted case); a wrong-kind element inside it is refused when
    /// [`StreamingBlocks::admit`] meets it. Any other block is classified by
    /// the element kinds it actually yields (see
    /// [`StreamingBlocks::skip_kind`] for why the block type cannot decide):
    /// no elements is skipped, `kind` alone is taken, a later kind is
    /// stashed and ends the phase, an earlier kind or several kinds is
    /// refused as unsorted. The classification also accepts a block whose
    /// block type says `Mixed` while every element is one kind (dense and
    /// plain node groups side by side, an empty dense group beside ways).
    fn next_phase_block(&mut self, kind: ElementKind) -> Result<Option<PrimitiveBlock>> {
        loop {
            let Some(block) = self.next_block()? else {
                return Ok(None);
            };
            if block_type_kind(block.block_type()) == Some(kind) {
                return Ok(Some(block));
            }
            match element_kinds(&block) {
                BlockKinds::Empty => {}
                BlockKinds::One(k) if k == kind => return Ok(Some(block)),
                BlockKinds::One(k) if k > kind => {
                    self.stashed = Some(block);
                    return Ok(None);
                }
                BlockKinds::One(k) => return Err(self.locate(unsorted_block(k, kind))),
                BlockKinds::Several => return Err(self.locate(mixed_block())),
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

/// The element kinds a block's elements actually are.
enum BlockKinds {
    Empty,
    One(ElementKind),
    Several,
}

/// Classify `block` by the elements [`PrimitiveBlock::elements`] yields - what
/// a conversion would see - rather than by the first field of each group.
fn element_kinds(block: &PrimitiveBlock) -> BlockKinds {
    let mut found: Option<ElementKind> = None;
    for element in block.elements_skip_metadata() {
        let kind = element_kind(&element);
        match found {
            None => found = Some(kind),
            Some(k) if k == kind => {}
            Some(_) => return BlockKinds::Several,
        }
    }
    found.map_or(BlockKinds::Empty, BlockKinds::One)
}

fn element_kind(element: &Element<'_>) -> ElementKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
        Element::Way(_) => ElementKind::Way,
        Element::Relation(_) => ElementKind::Relation,
    }
}

/// The single kind a [`BlockType`] names, when it names one.
fn block_type_kind(bt: BlockType) -> Option<ElementKind> {
    if bt.is_nodes() {
        Some(ElementKind::Node)
    } else if bt.is_ways() {
        Some(ElementKind::Way)
    } else if bt.is_relations() {
        Some(ElementKind::Relation)
    } else {
        None
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
    let mut on_action = |action: BlockMergeAction<'_>| sink.record(kind, &action);
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
            (Some(o), None) => on_action(BlockMergeAction::ElementOldOnly(&o))?,
            (None, Some(n)) => on_action(BlockMergeAction::ElementNewOnly(&n))?,
            (Some(o), Some(n)) => match crate::osm_id::osm_id_cmp(element_id(&o), element_id(&n)) {
                std::cmp::Ordering::Less => {
                    on_action(BlockMergeAction::ElementOldOnly(&o))?;
                    new_head = Some(n);
                }
                std::cmp::Ordering::Greater => {
                    on_action(BlockMergeAction::ElementNewOnly(&n))?;
                    old_head = Some(o);
                }
                std::cmp::Ordering::Equal => {
                    if crate::content::content_equal(&o, &n) {
                        on_action(BlockMergeAction::ElementEqual(&o))?;
                    } else {
                        on_action(BlockMergeAction::ElementModified { old: &o, new: &n })?;
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
    BlobEqual(u64),
    /// Every element of the run exists only in old.
    BlobOldOnly(BlockRun<'a>),
    /// Every element of the run exists only in new.
    BlobNewOnly(BlockRun<'a>),
    /// Single element unchanged (the old side's copy). A sink that prints
    /// it reads the version from it; one that ignores equal elements pays
    /// nothing for them.
    ElementEqual(&'a Element<'a>),
    /// Single element modified (different content, same ID).
    ElementModified {
        old: &'a Element<'a>,
        new: &'a Element<'a>,
    },
    /// Single element only in old.
    ElementOldOnly(&'a Element<'a>),
    /// Single element only in new.
    ElementNewOnly(&'a Element<'a>),
}

/// A single-sided run of a decoded block: its elements from `skip` up to
/// `end` (the end of the block when `None`).
pub(crate) struct BlockRun<'a> {
    pub(crate) block: &'a PrimitiveBlock,
    /// Elements before the run: consumed by an earlier merge step, or below
    /// the shard window.
    pub(crate) skip: usize,
    /// Exclusive element bound: the first element past the shard window,
    /// for a block that crosses its window's upper end. `None` runs to the
    /// end of the block.
    pub(crate) end: Option<usize>,
    /// Elements in the run: `end - skip` when bounded (counted), the blob's
    /// claimed count less `skip` otherwise (trusted, as everywhere in the
    /// block-pair engine).
    pub(crate) count: u64,
}

impl<'a> BlockRun<'a> {
    /// The run's elements. A sink iterates exactly these, so an element
    /// past the shard window is never printed by the shard below it.
    pub(crate) fn elements(&self) -> impl Iterator<Item = Element<'a>> + use<'a> {
        let take = self
            .end
            .map_or(usize::MAX, |end| end.saturating_sub(self.skip));
        self.block.elements().skip(self.skip).take(take)
    }
}

/// Where a merge's actions go: diff's text and counts, or derive's OSC
/// fragments. One implementation per command serves every path, so an
/// element prints the same whichever merge found it. Generic rather than
/// `dyn` so the per-element call inlines.
pub(crate) trait MergeSink {
    fn record(&mut self, kind: ElementKind, action: &BlockMergeAction<'_>) -> Result<()>;
}

/// One OsmData blob's location and index metadata, from the header walk:
/// enough to pread, compare and decode it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobDesc {
    /// File-wide ordinal and frame start from the header walker; provenance
    /// for errors, never used for scheduling.
    pub(crate) blob_index: u64,
    pub(crate) frame_offset: u64,
    pub(crate) data_offset: u64,
    pub(crate) data_size: usize,
    pub(crate) index: crate::blob_meta::BlobIndex,
}

/// The ID window `(t_low, t_high]` a shard owns. Only multi-shard plans
/// have one, and those hold no negative ID, so the bounds compare as raw
/// `i64`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub(crate) t_low: i64,
    pub(crate) t_high: i64,
}

impl Window {
    /// Whether a blob's claimed range reaches outside the window, so its
    /// decoded block must be clipped. A claim inside stays unclipped, which
    /// keeps [`check_claimed_max`] able to see a block holding more than
    /// its claim says.
    fn crosses(self, index: &crate::blob_meta::BlobIndex) -> bool {
        index.min_id <= self.t_low || index.max_id > self.t_high
    }
}

/// A decoded block of one side and the part of it still to merge.
struct BlockState {
    block: PrimitiveBlock,
    skip_count: usize,
    /// Exclusive bound of the shard-window part; `None` when unclipped.
    end: Option<usize>,
    index: crate::blob_meta::BlobIndex,
}

impl BlockState {
    /// Nothing of the block is left in the window.
    fn spent(&self) -> bool {
        self.end.is_some_and(|end| self.skip_count >= end)
    }

    fn run(&self) -> BlockRun<'_> {
        let count = match self.end {
            Some(end) => (end - self.skip_count.min(end)) as u64,
            None => self.index.count.saturating_sub(self.skip_count as u64),
        };
        BlockRun {
            block: &self.block,
            skip: self.skip_count,
            end: self.end,
            count,
        }
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

/// A fresh, empty [`MergeBuffers`] for one merge.
pub(crate) fn merge_buffers() -> MergeBuffers {
    crate::blob::DecompressPool::new()
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
/// The block-pair header walk runs it over every data blob of each input,
/// before bucketing by kind throws file order away. Without it the answer
/// depended on `-j` back when `-j 1` ran a separate streaming engine: that
/// engine saw only kind transitions, so blobs out of order within one kind
/// gave it a wrong answer (50 created and 50 deleted on identical content)
/// where the parallel walk refused the file, and before the walk had the
/// guard, `-j 1` refused a misplaced kind while the default `-j` reported
/// "Files are identical".
///
/// Sorted blob order is also a precondition of the shard planner, which
/// scans each per-kind list assuming it ascends, and of the merge, which
/// decides from blob ranges which blobs overlap.
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
                "block-pair merge: a {} blob holding id {} follows {} \
                 output already reaching id {}. The input's header claims \
                 Sort.Type_then_ID but its blobs are not in canonical order.",
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

/// One input of a block-pair merge over one shard of one kind: the shard's
/// slice of that kind's blobs from the header walk, read by `pread` from
/// the walker's shared descriptor.
///
/// Holds at most one blob read but not yet decoded (`read_buf`): the merge
/// reads a fresh blob from each side before it decides whether to decode,
/// compare or emit it, and hands one back (`held`) when only the other is
/// emitted, so the next round can still try the byte comparison on it.
pub(crate) struct BlockPairSide<'a> {
    file: &'a std::fs::File,
    source: &'a std::sync::Arc<std::path::Path>,
    descs: &'a [BlobDesc],
    next: usize,
    window: Option<Window>,
    /// The body (`Blob` envelope) of the blob [`BlockPairSide::next_blob`]
    /// returned last. Reused across blobs.
    read_buf: Vec<u8>,
    st: Vec<(u32, u32)>,
    gr: Vec<(u32, u32)>,
    /// A blob handed back undecoded by the non-overlap fast path; its body
    /// is still in `read_buf`. Taken first by `next_blob`.
    held: Option<BlobDesc>,
}

impl<'a> BlockPairSide<'a> {
    /// A side over `descs`, one kind's blobs of this shard in file order,
    /// clipped to `window` when the shard has one.
    pub(crate) fn new(
        file: &'a std::fs::File,
        source: &'a std::sync::Arc<std::path::Path>,
        descs: &'a [BlobDesc],
        window: Option<Window>,
    ) -> Self {
        Self {
            file,
            source,
            descs,
            next: 0,
            window,
            read_buf: Vec::new(),
            st: Vec::new(),
            gr: Vec::new(),
            held: None,
        }
    }

    #[cold]
    fn locate(&self, desc: &BlobDesc, err: crate::Error) -> crate::Error {
        err.locate(crate::error::Location::new(
            Some(self.source),
            Some(desc.blob_index),
            Some(crate::ByteOffset(desc.frame_offset)),
        ))
    }

    /// The next blob of the shard with its body read into `read_buf`,
    /// unparsed.
    fn next_blob(&mut self) -> Result<Option<BlobDesc>> {
        use std::os::unix::fs::FileExt as _;
        if let Some(held) = self.held.take() {
            return Ok(Some(held));
        }
        let Some(&desc) = self.descs.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        self.read_buf.resize(desc.data_size, 0);
        if let Err(e) = self
            .file
            .read_exact_at(&mut self.read_buf, desc.data_offset)
        {
            return Err(self.locate(&desc, e.into()));
        }
        Ok(Some(desc))
    }

    /// The payload of the blob in `read_buf`, for the byte comparison.
    fn payload(&self, desc: &BlobDesc) -> Result<Option<(u8, &[u8])>> {
        crate::blob::raw_blob_payload(&self.read_buf).map_err(|e| self.locate(desc, e))
    }

    /// Decompress and parse the blob in `read_buf`, clipped to the window
    /// when its claimed range crosses it.
    ///
    /// Each block gets a fresh buffer, as the parallel shard workers always
    /// did, and that is kept for now on the default path of a measured
    /// planet command. A fresh buffer arrives at the parse exactly full, so
    /// appending the inline metadata typically doubles its capacity (see
    /// `WireBlock::parse_and_inline_with_scratch`). A per-shard pool shared
    /// by both sides could reuse that grown capacity, avoiding repeated
    /// growth when later blocks fit; both sides hold a live block at once,
    /// a larger block still grows, and a buffer past the pool's retain cap
    /// is dropped. Allocation and free stay on the shard's thread either
    /// way. The time and memory effects of a pool here are unmeasured, so
    /// this allocation policy stays pending a measured comparison.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn decode(&mut self, desc: BlobDesc) -> Result<BlockState> {
        let mut decompressed: Vec<u8> = Vec::new();
        if let Err(e) = crate::blob::decompress_blob_raw(&self.read_buf, &mut decompressed) {
            return Err(self.locate(&desc, e));
        }
        let block =
            match PrimitiveBlock::from_vec_with_scratch(decompressed, &mut self.st, &mut self.gr) {
                Ok(block) => block,
                Err(e) => return Err(self.locate(&desc, e)),
            };
        let (skip_count, end) = match self.window {
            Some(window) if window.crosses(&desc.index) => clip(&block, window),
            _ => (0, None),
        };
        Ok(BlockState {
            block,
            skip_count,
            end,
            index: desc.index,
        })
    }

    /// The next blob of the shard, decoded, passing over any that holds
    /// nothing inside the window.
    fn next_decoded(&mut self) -> Result<Option<BlockState>> {
        while let Some(desc) = self.next_blob()? {
            let state = self.decode(desc)?;
            if !state.spent() {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }
}

/// The window part of a block whose claim crosses the window: the number of
/// leading elements at or below `t_low` (the shard below owns them) and the
/// index of the first element past `t_high` (the block's length when none
/// is). Counted from the elements, so a clipped run's count is exact. Order
/// within the block is trusted, as everywhere in the engine.
fn clip(block: &PrimitiveBlock, window: Window) -> (usize, Option<usize>) {
    let mut skip = 0usize;
    let mut len = 0usize;
    for (i, element) in block.elements_skip_metadata().enumerate() {
        let id = element_id(&element);
        if id > window.t_high {
            return (skip.min(i), Some(i));
        }
        if id <= window.t_low {
            skip = i + 1;
        }
        len = i + 1;
    }
    (skip.min(len), Some(len))
}

/// Drain the rest of one side's shard as single-sided runs.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn drain_remaining<S: MergeSink>(
    side: &mut BlockPairSide<'_>,
    is_old: bool,
    kind: ElementKind,
    sink: &mut S,
    stats: &mut BlockPairMergeStats,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    while let Some(bs) = side.next_decoded()? {
        cancel.check()?;
        emit_block(&bs, is_old, kind, sink, stats)?;
    }
    Ok(())
}

/// Emit a decoded block's remaining run as BlobOldOnly or BlobNewOnly.
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
    let run = bs.run();
    if is_old {
        stats.blobs_old_only += 1;
        stats.elements_old_only += run.count;
        sink.record(kind, &BlockMergeAction::BlobOldOnly(run))
    } else {
        stats.blobs_new_only += 1;
        stats.elements_new_only += run.count;
        sink.record(kind, &BlockMergeAction::BlobNewOnly(run))
    }
}

/// The OSM-order last ID a decoded block can still offer: its claimed last,
/// or the window's upper end for a block clipped there. A clipped block
/// exists only under a window, whose plan holds no negative ID, so the
/// plain `min` is the OSM-order one.
fn state_last(bs: &BlockState, window: Option<Window>) -> i64 {
    let last = blob_last(&bs.index);
    match (bs.end, window) {
        (Some(_), Some(window)) => last.min(window.t_high),
        _ => last,
    }
}

/// Merge a pair of decoded overlapping blocks, tracking residuals.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn merge_decoded_pair<S: MergeSink>(
    old_decoded: &mut Option<BlockState>,
    new_decoded: &mut Option<BlockState>,
    window: Option<Window>,
    kind: ElementKind,
    sink: &mut S,
) -> Result<()> {
    let Some(mut os) = old_decoded.take() else {
        return Err(crate::Error::internal(
            "block-pair merge: no old block to merge",
        ));
    };
    let Some(mut ns) = new_decoded.take() else {
        return Err(crate::Error::internal(
            "block-pair merge: no new block to merge",
        ));
    };

    // The merge boundary is the earlier of the two blocks' OSM-order last
    // IDs. `osm_id_cmp` (not raw `i64` order) throughout, because the merge
    // itself orders by `osm_id_cmp`: for negative IDs, which `osm_id.rs`
    // explicitly supports as 0, -1, -2, ..., 1, 2, ..., a raw `min` picks a
    // boundary that cuts the streams in a different place than the order
    // they are being merged in.
    let old_last = state_last(&os, window);
    let new_last = state_last(&ns, window);
    let merge_up_to = if crate::osm_id::osm_id_cmp(old_last, new_last).is_le() {
        old_last
    } else {
        new_last
    };

    let outcome = element_merge_pair(&os, &ns, merge_up_to, kind, sink)?;

    // ADR-0013: metadata is trusted by default, but nothing load-bearing may
    // break when it turns out to be wrong. The residual decision below is
    // "does this block still hold elements past the boundary", and asking
    // `index.max_id` is only a proxy for it. When a blob's stored `max_id`
    // understates its real contents, the proxy says "finished" while the
    // block still holds elements - and the block is then dropped, taking
    // them with it, with no error and no infinite loop to notice. Compare
    // the proxy against what the iterator actually saw, and reject the
    // mismatch instead of losing elements to it. A block clipped at the
    // window's upper end is bounded there, so what it holds past the window
    // is the next shard's and never counts as "more"; a block whose claim
    // lies inside the window is not clipped, so its contents stay visible.
    check_claimed_max(&os.index, outcome.old_has_more, merge_up_to, "old")?;
    check_claimed_max(&ns.index, outcome.new_has_more, merge_up_to, "new")?;

    if outcome.old_has_more {
        os.skip_count += outcome.old_consumed;
        *old_decoded = Some(os);
    }
    if outcome.new_has_more {
        ns.skip_count += outcome.new_consumed;
        *new_decoded = Some(ns);
    }
    Ok(())
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
/// Claimed ranges, also for a clipped block: conservative, since a clipped
/// block's remaining elements lie inside its claim.
fn blob_entirely_before(a: &crate::blob_meta::BlobIndex, b: &crate::blob_meta::BlobIndex) -> bool {
    crate::osm_id::blob_osm_last_key(a.min_id, a.max_id)
        < crate::osm_id::blob_osm_first_key(b.min_id, b.max_id)
}

/// Verify a blob's stored `max_id` against what the element walk found.
///
/// `has_more` true means the block still holds elements past `merge_up_to`
/// (within its window bound). If the blob's own claimed last ID is at or
/// before the boundary, the stored metadata contradicts the block's
/// contents. Erroring here is also what keeps [`merge_decoded_pair`] from
/// looping: a side is only retained as a residual when it genuinely has
/// more, and the boundary is the earlier of the two effective lasts, so
/// both sides retaining would require both claims to be understated - which
/// this rejects - or both blocks to reach past the window, which their
/// bound stops.
fn check_claimed_max(
    index: &crate::blob_meta::BlobIndex,
    has_more: bool,
    merge_up_to: i64,
    side: &str,
) -> Result<()> {
    if !has_more {
        return Ok(());
    }
    let claimed_last = blob_last(index);
    if crate::osm_id::osm_id_cmp(claimed_last, merge_up_to).is_le() {
        return Err(crate::error::new_indexdata_mismatch(format!(
            "block-pair merge: the {side} blob's indexdata claims its last ID is \
             {claimed_last}, but the block still holds elements past the merge boundary \
             {merge_up_to}. The stored blob metadata does not match the blob's contents."
        )));
    }
    Ok(())
}

/// Run one type phase of the block-pair merge over one shard.
///
/// `old` and `new` hold the shard's blobs of `kind`, which the header walk
/// checked for indexdata and canonical order. Falls through to
/// element-level comparison for overlapping blocks.
///
/// When `skip_equal_blobs` is true, a fresh overlapping blob pair (neither
/// side partly consumed, neither crossing the window) with identical claims
/// and identical compressed payload emits `BlobEqual(count)` without
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
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let window = old.window;
    let mut old_decoded: Option<BlockState> = None;
    let mut new_decoded: Option<BlockState> = None;

    loop {
        // Once per merge step, each of which decodes at most one blob per
        // side.
        cancel.check()?;
        // Fast path: both sides need fresh blobs (no residuals). Read
        // without decompressing so the compressed bytes can be compared.
        // Only here: a residual is partly consumed, and comparing it whole
        // against a fresh blob would count its consumed elements twice.
        if old_decoded.is_none() && new_decoded.is_none() {
            let op = old.next_blob()?;
            let np = new.next_blob()?;

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
                        new.held = Some(np);
                        continue;
                    }
                    if blob_entirely_before(&np.index, &op.index) {
                        let ns = new.decode(np)?;
                        emit_block(&ns, false, kind, sink, stats)?;
                        old.held = Some(op);
                        continue;
                    }
                    if skip_equal_blobs && blobs_byte_equal(old, &op, new, &np)? {
                        stats.pairs_byte_equal += 1;
                        stats.elements_byte_equal += op.index.count;
                        sink.record(kind, &BlockMergeAction::BlobEqual(op.index.count))?;
                        continue;
                    }
                    stats.pairs_overlapping_decoded += 1;
                    stats.elements_overlapping_decoded += op.index.count + np.index.count;
                    old_decoded = Some(old.decode(op)?).filter(|bs| !bs.spent());
                    new_decoded = Some(new.decode(np)?).filter(|bs| !bs.spent());
                }
            }
        }

        // Slow path: at least one side has a decoded residual block.
        if old_decoded.is_none() {
            old_decoded = old.next_decoded()?;
        }
        if new_decoded.is_none() {
            new_decoded = new.next_decoded()?;
        }

        match (old_decoded.take(), new_decoded.take()) {
            (None, None) => break,
            (Some(os), None) => emit_block(&os, true, kind, sink, stats)?,
            (None, Some(ns)) => emit_block(&ns, false, kind, sink, stats)?,
            (Some(os), Some(ns)) => {
                if blob_entirely_before(&os.index, &ns.index) {
                    emit_block(&os, true, kind, sink, stats)?;
                    new_decoded = Some(ns);
                    continue;
                }
                if blob_entirely_before(&ns.index, &os.index) {
                    emit_block(&ns, false, kind, sink, stats)?;
                    old_decoded = Some(os);
                    continue;
                }
                old_decoded = Some(os);
                new_decoded = Some(ns);
                merge_decoded_pair(&mut old_decoded, &mut new_decoded, window, kind, sink)?;
            }
        }
    }

    Ok(())
}

/// Whether a fresh pending pair has identical index claims, compression kind
/// and compressed bytes. Two such blobs hold identical elements. Never for a
/// pair crossing the window: a clipped block's count is not its claim.
/// The envelopes are parsed (and a malformed one refused) only for a pair
/// whose claims match.
fn blobs_byte_equal(
    old: &BlockPairSide<'_>,
    op: &BlobDesc,
    new: &BlockPairSide<'_>,
    np: &BlobDesc,
) -> Result<bool> {
    if op.index.min_id != np.index.min_id
        || op.index.max_id != np.index.max_id
        || op.index.count != np.index.count
    {
        return Ok(false);
    }
    if old.window.is_some_and(|w| w.crosses(&op.index)) {
        return Ok(false);
    }
    let a = old.payload(op)?;
    let b = new.payload(np)?;
    Ok(match (a, b) {
        (Some((ak, ab)), Some((bk, bb))) => ak == bk && ab == bb,
        _ => false,
    })
}

/// Two-pointer element merge over a pair of decoded blocks, each bounded to
/// its window part.
///
/// Processes elements up to `merge_up_to` ID (inclusive). Elements beyond that
/// boundary in either block are left unconsumed for the caller to handle.
///
/// Returns a [`MergePairOutcome`] - how many elements were consumed from each
/// side, so the caller can update skip counts without re-scanning, and whether
/// either side still holds elements past the boundary.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn element_merge_pair<S: MergeSink>(
    os: &BlockState,
    ns: &BlockState,
    merge_up_to: i64,
    kind: ElementKind,
    sink: &mut S,
) -> Result<MergePairOutcome> {
    let mut old_iter = os.run().elements().peekable();
    let mut new_iter = ns.run().elements().peekable();
    let mut old_consumed: usize = 0;
    let mut new_consumed: usize = 0;
    // `osm_id_cmp`, not raw `<=`: every ordering decision below already
    // uses it, and for negative IDs the two disagree - a raw boundary test
    // would cut the streams somewhere the merge order does not.
    let in_range = |e: &Element<'_>| crate::osm_id::osm_id_cmp(element_id(e), merge_up_to).is_le();

    loop {
        // Which side's head goes next, from the in-range heads alone; the
        // element is then taken from the iterator it was peeked on, so the
        // `if let`s below always match.
        let old_head = old_iter.peek().filter(|e| in_range(e)).map(element_id);
        let new_head = new_iter.peek().filter(|e| in_range(e)).map(element_id);
        let order = match (old_head, new_head) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(o_id), Some(n_id)) => crate::osm_id::osm_id_cmp(o_id, n_id),
        };
        match order {
            std::cmp::Ordering::Less => {
                if let Some(o) = old_iter.next() {
                    sink.record(kind, &BlockMergeAction::ElementOldOnly(&o))?;
                    old_consumed += 1;
                }
            }
            std::cmp::Ordering::Greater => {
                if let Some(n) = new_iter.next() {
                    sink.record(kind, &BlockMergeAction::ElementNewOnly(&n))?;
                    new_consumed += 1;
                }
            }
            std::cmp::Ordering::Equal => {
                if let (Some(o), Some(n)) = (old_iter.next(), new_iter.next()) {
                    old_consumed += 1;
                    new_consumed += 1;
                    if crate::content::content_equal(&o, &n) {
                        sink.record(kind, &BlockMergeAction::ElementEqual(&o))?;
                    } else {
                        sink.record(
                            kind,
                            &BlockMergeAction::ElementModified { old: &o, new: &n },
                        )?;
                    }
                }
            }
        }
    }

    Ok(MergePairOutcome {
        old_consumed,
        new_consumed,
        old_has_more: old_iter.peek().is_some(),
        new_has_more: new_iter.peek().is_some(),
    })
}

/// What one [`element_merge_pair`] call did, and what it left behind.
///
/// `*_has_more` is what the bounded iterator actually observed, not what
/// the blob's indexdata claims. The residual decision is made from these, so
/// an understated `max_id` cannot silently drop a block's tail.
struct MergePairOutcome {
    old_consumed: usize,
    new_consumed: usize,
    old_has_more: bool,
    new_has_more: bool,
}
