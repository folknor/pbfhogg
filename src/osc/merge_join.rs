//! Sequential merge-joins for sorted PBF operations, used by `diff` and
//! `derive_changes`.
//!
//! [`block_pair_merge_phase`] is the blob-level merge over two indexed
//! inputs. [`stream_merge_phase`] is the element-level merge over two
//! [`StreamingBlocks`] cursors, for inputs without indexdata. Both hand the
//! caller borrowed elements through the same [`BlockMergeAction`] variants,
//! so each command has one action handler for both paths and prints an
//! element the same way whichever path found it.

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
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<()> {
    let type_char = kind.type_char();
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
            (Some(o), Some(n)) => {
                let o_id = element_id(&o);
                match crate::osm_id::osm_id_cmp(o_id, element_id(&n)) {
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
                            on_action(BlockMergeAction::ElementEqual {
                                id: o_id,
                                version: element_version(&o),
                                type_char,
                            })?;
                        } else {
                            on_action(BlockMergeAction::ElementModified { old: &o, new: &n })?;
                        }
                    }
                }
            }
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

/// Actions emitted by the block-pair merge engine.
pub(crate) enum BlockMergeAction<'a> {
    /// All elements in this blob are unchanged (count from indexdata).
    /// Emitted by v1 compressed-byte comparison when `skip_equal_blobs` is true
    /// and the old/new blobs have identical compressed bytes.
    BlobEqual(u64),
    /// All elements in this blob exist only in old.
    BlobOldOnly {
        block: &'a PrimitiveBlock,
        count: u64,
        /// Number of elements to skip from the start of the block (for residuals).
        skip: usize,
    },
    /// All elements in this blob exist only in new.
    BlobNewOnly {
        block: &'a PrimitiveBlock,
        count: u64,
        /// Number of elements to skip from the start of the block (for residuals).
        skip: usize,
    },
    /// Single element unchanged. Carries extracted id/version/type_char
    /// so the caller doesn't need to hold the Element borrow.
    ElementEqual {
        id: i64,
        version: Option<i32>,
        type_char: char,
    },
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

/// State for one side of the block-pair merge.
struct BlockState {
    block: PrimitiveBlock,
    skip_count: usize,
    index: crate::blob_meta::BlobIndex,
}

/// Undecoded blob with its index - held between blob read and decompress.
/// Used by v1 compressed-byte comparison: we read the blob, check its index,
/// and optionally compare compressed bytes before deciding whether to decode.
struct PendingBlob {
    blob: crate::blob::Blob,
    index: crate::blob_meta::BlobIndex,
}

/// One input of the block-pair merge: its reader, decode scratch, and the
/// state that carries across phases.
struct MergeSide {
    reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    /// Decompression buffers, shared with the other side; see
    /// [`MergeBuffers`].
    pool: MergeBuffers,
    st: Vec<(u32, u32)>,
    gr: Vec<(u32, u32)>,
    /// The first data frame of a later kind, read during an earlier phase
    /// (or skip). Already classified and observed by `order`; its body has
    /// not been interpreted.
    stash: Option<(crate::blob::RawFrame, crate::blob_meta::BlobIndex)>,
    /// A blob of the running phase handed back undecoded by
    /// [`block_pair_merge_phase`]'s non-overlap fast path, so the next round
    /// can still try the byte comparison on it. Taken first by
    /// [`MergeSide::next_blob`]; never outlives the phase.
    held: Option<PendingBlob>,
    /// Canonical blob order over every data blob this side reads, whatever
    /// the phase, so an excluded kind's blobs are checked as the parallel
    /// path's header walk checks them.
    order: BlobOrderGuard,
}

impl MergeSide {
    fn new(
        reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
        pool: MergeBuffers,
    ) -> Self {
        Self {
            reader,
            pool,
            st: Vec::new(),
            gr: Vec::new(),
            stash: None,
            held: None,
            order: BlobOrderGuard::new(),
        }
    }

    /// The next data frame of `kind`, body uninterpreted, or `None` at the
    /// end of the file or at the first frame of a later kind (stashed for
    /// the phase that wants it).
    ///
    /// Every data frame read fresh from the file is checked here, once:
    /// indexdata present, not of a kind earlier than `kind`, and in
    /// canonical order after everything this side read before (the order
    /// guard, which is what catches blobs out of order within one kind). A
    /// stashed frame passed those checks when it was read and is not
    /// observed again.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn next_frame(
        &mut self,
        kind: ElementKind,
    ) -> Result<Option<(crate::blob::RawFrame, crate::blob_meta::BlobIndex)>> {
        let (frame, index) = match self.stash.take() {
            Some(stashed) => stashed,
            None => loop {
                let frame = match self.reader.next_frame() {
                    Some(result) => result?,
                    None => return Ok(None),
                };
                if !frame.is_osm_data() {
                    continue;
                }
                let source = self.reader.source();
                let Some(index) = frame.index() else {
                    return Err(frame.locate_error(
                        source,
                        crate::error::new_missing_indexdata(
                            "block-pair merge requires indexdata on every data blob",
                        ),
                    ));
                };
                if index.kind < kind {
                    // A phase (or skip) for the earlier kind ran first and
                    // read every blob of it up to the first later-kind blob,
                    // so this blob proves the header's Sort.Type_then_ID
                    // claim false - what `degrade --unsort` produces. This
                    // used to `continue`, discarding the blob and every
                    // element in it with no error and no count anomaly.
                    return Err(frame.locate_error(
                        source,
                        crate::error::new_unsorted(format!(
                            "block-pair merge: found a {} blob during the {} phase. The \
                             input's header claims Sort.Type_then_ID but its blobs are not \
                             in canonical order (nodes, then ways, then relations).",
                            index.kind.as_str(),
                            kind.as_str(),
                        )),
                    ));
                }
                self.order
                    .observe(&index)
                    .map_err(|e| frame.locate_error(source, e))?;
                break (frame, index);
            },
        };
        if index.kind > kind {
            self.stash = Some((frame, index));
            return Ok(None);
        }
        Ok(Some((frame, index)))
    }

    /// The next blob of `kind` with its envelope parsed, undecompressed; see
    /// [`MergeSide::next_frame`].
    fn next_blob(&mut self, kind: ElementKind) -> Result<Option<PendingBlob>> {
        if let Some(held) = self.held.take() {
            return Ok(Some(held));
        }
        match self.next_frame(kind)? {
            Some((frame, index)) => Ok(Some(PendingBlob {
                blob: frame.into_blob(self.reader.source())?,
                index,
            })),
            None => Ok(None),
        }
    }

    /// Decompress and parse a pending blob. The compressed bytes are
    /// dropped once it is decoded.
    fn decode(&mut self, pending: PendingBlob) -> Result<BlockState> {
        let blob = pending.blob;
        let block = decode_blob(&blob, &self.pool, &mut self.st, &mut self.gr)
            .map_err(|e| blob.locate_error(self.reader.source(), e))?;
        Ok(BlockState {
            block,
            skip_count: 0,
            index: pending.index,
        })
    }

    /// The next blob of `kind`, decoded.
    fn next_decoded(&mut self, kind: ElementKind) -> Result<Option<BlockState>> {
        match self.next_blob(kind)? {
            Some(pending) => Ok(Some(self.decode(pending)?)),
            None => Ok(None),
        }
    }

    /// Read past every blob of `kind` without interpreting a body.
    fn skip(&mut self, kind: ElementKind) -> Result<()> {
        while self.next_frame(kind)?.is_some() {}
        Ok(())
    }

    /// Check the structure of the rest of the file - framing, indexdata on
    /// every data blob, canonical blob order - with a header-only walk from
    /// where this side's reader stands. A stashed frame was checked when it
    /// was read; the walk starts after it.
    ///
    /// `source` is the input this side's reader was opened from (its pin,
    /// for a command that pinned it): the walk is a new open of it, never of
    /// the path the reader names, which a pinned run must not resolve again.
    fn verify_rest(
        &mut self,
        source: crate::read::input::InputSource<'_>,
        host: &crate::host::Host,
    ) -> Result<()> {
        let internal = || {
            crate::Error::invalid_argument(
                "block-pair merge: the reader's position is unknown, so the rest of the \
                 file cannot be walked",
            )
        };
        let (offset, blob_index) = self.reader.position().ok_or_else(internal)?;
        let blob_index = blob_index.ok_or_else(internal)?;
        let mut walker = crate::read::header_walker::HeaderWalker::open(source, host)?
            .resume_at(offset.0, blob_index);
        while let Some(meta) = walker.next_header()? {
            if meta.blob_type != crate::blob::BlobKind::OsmData {
                continue;
            }
            let Some(index) = meta.index else {
                return Err(walker.locate_meta(
                    &meta,
                    crate::error::new_missing_indexdata(
                        "block-pair merge requires indexdata on every data blob",
                    ),
                ));
            };
            self.order
                .observe(&index)
                .map_err(|e| walker.locate_meta(&meta, e))?;
        }
        Ok(())
    }
}

/// The decompression buffers one sequential merge recycles, shared by both
/// of its sides and every phase, for the indexed block-pair merge and the
/// unindexed streaming merge alike.
///
/// Each decoded block holds its buffer and hands it back here when it drops
/// ([`PrimitiveBlock::from_vec_pooled_with_scratch`]), so the next decode on
/// either side reuses it instead of allocating a fresh, often megabyte-sized
/// one per block. One pool rather than one per side, so a buffer freed on
/// one side or in one phase serves the other. The idle buffers it retains
/// never outnumber the decoded blocks the merge has held at once (a current
/// block per side, a stashed one, and a block still alive while
/// [`drain_remaining`] decodes the next), within the pool's own count and
/// per-buffer capacity limits. A buffer whose decompression fails, or whose
/// parse fails before the block wraps it, is freed rather than returned; one
/// that fails validation after wrapping returns as usual. Everything runs on the merge's one thread, so
/// the pool's lock is never contended.
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
/// Both block-pair paths run it over every data blob of each input: the
/// parallel walk before bucketing by kind, which throws file order away, and
/// the sequential merge as it reads each blob ([`MergeSide::next_frame`],
/// and [`MergeSide::verify_rest`] for a tail no phase reads). Without it the
/// paths answered differently depending on `-j`: the sequential merge saw
/// only kind transitions, so blobs out of order within one kind gave it a
/// wrong answer (50 created and 50 deleted on identical content) where the
/// parallel walk refused the file, and before the walk had the guard,
/// `-j 1` refused a misplaced kind while the default `-j` reported "Files
/// are identical".
///
/// Sorted blob order is also a precondition of the shard planners
/// themselves, which binary-search each per-kind list assuming it ascends,
/// and of the sequential merge, which decides from blob ranges which blobs
/// overlap.
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

/// Shadow-mode counters for block-pair merge classification.
///
/// Accumulated across all phases of a single diff/derive_changes run.
/// What fraction of blobs hit the v1 byte-equal fast path, how many
/// get element-merged after decoding both sides, and how many are
/// emitted as entirely single-sided.
///
/// Element counts come from blob indexdata (no per-element increments),
/// so the overhead is a handful of u64 adds per blob.
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct BlockPairMergeStats {
    /// Blob pairs that matched via v1 compressed-byte equality (no decode).
    pub pairs_byte_equal: u64,
    pub elements_byte_equal: u64,
    /// Overlapping blob pairs that required decoding both sides for element merge.
    pub pairs_overlapping_decoded: u64,
    /// Sum of both sides' element counts for overlapping pairs.
    pub elements_overlapping_decoded: u64,
    /// Blobs emitted as BlobOldOnly (entirely single-sided, all code paths).
    /// Covers fast-path non-overlap, EOF drain, and slow-path residuals.
    pub blobs_old_only: u64,
    pub elements_old_only: u64,
    /// Blobs emitted as BlobNewOnly (entirely single-sided, all code paths).
    pub blobs_new_only: u64,
    pub elements_new_only: u64,
}

/// State for the block-pair merge engine (one per diff/derive_changes call).
pub(crate) struct BlockPairMergeState {
    old: MergeSide,
    new: MergeSide,
    /// Shadow counters accumulated across all phases of this merge run.
    pub stats: BlockPairMergeStats,
}

impl BlockPairMergeState {
    pub(crate) fn new(
        old_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
        new_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    ) -> Self {
        let pool = merge_buffers();
        Self {
            old: MergeSide::new(old_reader, std::sync::Arc::clone(&pool)),
            new: MergeSide::new(new_reader, pool),
            stats: BlockPairMergeStats::default(),
        }
    }
}

/// Drain all remaining blobs of `kind` from one side of a merge.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn drain_remaining(
    side: &mut MergeSide,
    stats: &mut BlockPairMergeStats,
    kind: ElementKind,
    is_old: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<()> {
    while let Some(bs) = side.next_decoded(kind)? {
        emit_block(&bs, is_old, on_action, stats)?;
    }
    Ok(())
}

/// Advance both sides past every blob of `kind` without interpreting any
/// body, for a caller that excludes `kind` but still runs a later kind's
/// phase.
///
/// A later phase cannot simply start where an earlier phase was never run:
/// it would meet the excluded kind's first blob and report the file as out
/// of canonical order. Draining through [`block_pair_merge_phase`] instead
/// would decode every excluded blob, so the decode and claimed-max checks
/// would refuse inputs for a kind the caller excluded - inputs the parallel
/// path accepts, since its per-kind buckets never read an excluded kind's
/// bodies, and the answer must not depend on the shard count. So the
/// excluded blobs go through the same classifier as any phase (missing
/// indexdata, an earlier kind and blob order are checked; the first
/// later-kind blob is stashed for the next phase) and are dropped with
/// their bodies read past but not interpreted - not even the Blob envelope
/// is parsed, which is what the parallel path's header walk does with them.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn skip_phase(state: &mut BlockPairMergeState, kind: ElementKind) -> Result<()> {
    state.old.skip(kind)?;
    state.new.skip(kind)
}

/// Check the structure of the rest of both files after the last phase a
/// caller runs, when it excludes the kinds after that one.
///
/// The parallel path walks every blob header of both files before it
/// starts, so a truncated tail, a data blob without indexdata or blobs out
/// of order there are refused at any filter. The sequential merge never
/// reads past the first blob of the first kind after its last phase, so
/// without this `diff -t node` refused those files only at `-j` above one.
/// The walk reads headers only (no body is read), so it costs a pread per
/// remaining blob and nothing for a caller that runs the relation phase.
///
/// `old` and `new` are the inputs the two readers were opened from; each
/// side's walk opens its own.
pub(crate) fn verify_rest(
    state: &mut BlockPairMergeState,
    old: crate::read::input::InputSource<'_>,
    new: crate::read::input::InputSource<'_>,
    host: &crate::host::Host,
) -> Result<()> {
    state.old.verify_rest(old, host)?;
    state.new.verify_rest(new, host)
}

/// Emit a decoded block as BlobOldOnly or BlobNewOnly, accounting for skip.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn emit_block(
    bs: &BlockState,
    is_old: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
    stats: &mut BlockPairMergeStats,
) -> Result<()> {
    let remaining = bs.index.count.saturating_sub(bs.skip_count as u64);
    if is_old {
        stats.blobs_old_only += 1;
        stats.elements_old_only += remaining;
        on_action(BlockMergeAction::BlobOldOnly {
            block: &bs.block,
            count: remaining,
            skip: bs.skip_count,
        })
    } else {
        stats.blobs_new_only += 1;
        stats.elements_new_only += remaining;
        on_action(BlockMergeAction::BlobNewOnly {
            block: &bs.block,
            count: remaining,
            skip: bs.skip_count,
        })
    }
}

/// Merge a pair of decoded overlapping blocks, tracking residuals.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn merge_decoded_pair(
    old_decoded: &mut Option<BlockState>,
    new_decoded: &mut Option<BlockState>,
    type_char: char,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<()> {
    let mut os = old_decoded.take().expect("checked Some");
    let mut ns = new_decoded.take().expect("checked Some");

    // The merge boundary is the earlier of the two blobs' OSM-order last
    // IDs. `osm_id_cmp` (not raw `i64` order) throughout, because the merge
    // itself orders by `osm_id_cmp`: for negative IDs, which `osm_id.rs`
    // explicitly supports as 0, -1, -2, ..., 1, 2, ..., a raw `min` picks a
    // boundary that cuts the streams in a different place than the order
    // they are being merged in.
    let old_last = blob_last(&os.index);
    let new_last = blob_last(&ns.index);
    let merge_up_to = if crate::osm_id::osm_id_cmp(old_last, new_last).is_le() {
        old_last
    } else {
        new_last
    };

    let outcome = element_merge_pair(
        &os.block,
        os.skip_count,
        &ns.block,
        ns.skip_count,
        merge_up_to,
        type_char,
        on_action,
    )?;

    // ADR-0013: metadata is trusted by default, but nothing load-bearing may
    // break when it turns out to be wrong. The residual decision below is
    // "does this blob still hold elements past the boundary", and asking
    // `index.max_id` is only a proxy for it. When a blob's stored `max_id`
    // understates its real contents, the proxy says "finished" while the
    // block still holds elements - and the block is then dropped, taking
    // them with it, with no error and no infinite loop to notice. Compare
    // the proxy against what the iterator actually saw, and reject the
    // mismatch instead of losing elements to it.
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
fn blob_entirely_before(a: &crate::blob_meta::BlobIndex, b: &crate::blob_meta::BlobIndex) -> bool {
    crate::osm_id::blob_osm_last_key(a.min_id, a.max_id)
        < crate::osm_id::blob_osm_first_key(b.min_id, b.max_id)
}

/// Verify a blob's stored `max_id` against what the element walk found.
///
/// `has_more` true means the block still holds elements past `merge_up_to`.
/// If the blob's own claimed last ID is at or before the boundary, the
/// stored metadata contradicts the block's contents. Erroring here is also
/// what keeps [`merge_decoded_pair`] from looping: a side is only retained
/// as a residual when it genuinely has more, and the boundary is the earlier
/// of the two claimed lasts, so both sides retaining would require both
/// claims to be understated - which this rejects.
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

/// Run one type phase of the block-pair merge.
///
/// Requires both readers to have `set_parse_indexdata(true)` and both inputs
/// to be sorted. Falls through to element-level comparison for overlapping
/// blocks.
///
/// When `skip_equal_blobs` is true, overlapping blobs with identical compressed
/// bytes emit `BlobEqual(count)` without decompression (v1 optimization).
/// Callers that need per-element output for unchanged elements (e.g., diff with
/// `!suppress_common`) should pass `false`.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines)]
pub(crate) fn block_pair_merge_phase(
    state: &mut BlockPairMergeState,
    kind: ElementKind,
    skip_equal_blobs: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<()> {
    let type_char = kind.type_char();
    let mut old_decoded: Option<BlockState> = None;
    let mut new_decoded: Option<BlockState> = None;

    loop {
        // Fast path: both sides need fresh blobs (no residuals).
        // Read without decompressing so we can compare compressed bytes.
        if old_decoded.is_none() && new_decoded.is_none() {
            let op = state.old.next_blob(kind)?;
            let np = state.new.next_blob(kind)?;

            match (op, np) {
                (None, None) => break,
                (Some(op), None) => {
                    let os = state.old.decode(op)?;
                    emit_block(&os, true, on_action, &mut state.stats)?;
                    drain_remaining(&mut state.old, &mut state.stats, kind, true, on_action)?;
                    break;
                }
                (None, Some(np)) => {
                    let ns = state.new.decode(np)?;
                    emit_block(&ns, false, on_action, &mut state.stats)?;
                    drain_remaining(&mut state.new, &mut state.stats, kind, false, on_action)?;
                    break;
                }
                (Some(op), Some(np)) => {
                    if blob_entirely_before(&op.index, &np.index) {
                        let os = state.old.decode(op)?;
                        emit_block(&os, true, on_action, &mut state.stats)?;
                        // Hold the new blob undecoded - next iteration can try v1 byte comparison.
                        state.new.held = Some(np);
                        continue;
                    }
                    if blob_entirely_before(&np.index, &op.index) {
                        let ns = state.new.decode(np)?;
                        emit_block(&ns, false, on_action, &mut state.stats)?;
                        // Hold the old blob undecoded - next iteration can try v1 byte comparison.
                        state.old.held = Some(op);
                        continue;
                    }
                    // Overlapping - try compressed byte comparison (v1).
                    if skip_equal_blobs && blobs_byte_equal(&op, &np) {
                        state.stats.pairs_byte_equal += 1;
                        state.stats.elements_byte_equal += op.index.count;
                        on_action(BlockMergeAction::BlobEqual(op.index.count))?;
                        continue;
                    }
                    state.stats.pairs_overlapping_decoded += 1;
                    state.stats.elements_overlapping_decoded += op.index.count + np.index.count;
                    old_decoded = Some(state.old.decode(op)?);
                    new_decoded = Some(state.new.decode(np)?);
                }
            }
        }

        // Slow path: at least one side has a decoded residual block.
        if old_decoded.is_none() {
            old_decoded = state.old.next_decoded(kind)?;
        }
        if new_decoded.is_none() {
            new_decoded = state.new.next_decoded(kind)?;
        }

        match (&old_decoded, &new_decoded) {
            (None, None) => break,
            (Some(_), None) => {
                let os = old_decoded.take().expect("checked");
                emit_block(&os, true, on_action, &mut state.stats)?;
            }
            (None, Some(_)) => {
                let ns = new_decoded.take().expect("checked");
                emit_block(&ns, false, on_action, &mut state.stats)?;
            }
            (Some(os), Some(ns)) => {
                if blob_entirely_before(&os.index, &ns.index) {
                    let os = old_decoded.take().expect("checked");
                    emit_block(&os, true, on_action, &mut state.stats)?;
                    continue;
                }
                if blob_entirely_before(&ns.index, &os.index) {
                    let ns = new_decoded.take().expect("checked");
                    emit_block(&ns, false, on_action, &mut state.stats)?;
                    continue;
                }
                merge_decoded_pair(&mut old_decoded, &mut new_decoded, type_char, on_action)?;
            }
        }
    }

    Ok(())
}

/// Check if two pending blobs have identical index metadata, compression kind,
/// and compressed bytes.
fn blobs_byte_equal(a: &PendingBlob, b: &PendingBlob) -> bool {
    a.index.min_id == b.index.min_id
        && a.index.max_id == b.index.max_id
        && a.index.count == b.index.count
        && match (a.blob.compressed_data(), b.blob.compressed_data()) {
            (Some((ak, ab)), Some((bk, bb))) => ak == bk && ab == bb,
            _ => false,
        }
}

/// Two-pointer element merge over a pair of decoded blocks.
///
/// Processes elements up to `merge_up_to` ID (inclusive). Elements beyond that
/// boundary in either block are left unconsumed for the caller to handle.
///
/// Returns a [`MergePairOutcome`] - how many elements were consumed from each
/// side, so the caller can update skip counts without re-scanning, and whether
/// either side still holds elements past the boundary.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn element_merge_pair(
    old_block: &PrimitiveBlock,
    old_skip: usize,
    new_block: &PrimitiveBlock,
    new_skip: usize,
    merge_up_to: i64,
    type_char: char,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<MergePairOutcome> {
    let mut old_iter = old_block.elements().skip(old_skip).peekable();
    let mut new_iter = new_block.elements().skip(new_skip).peekable();
    let mut old_consumed: usize = 0;
    let mut new_consumed: usize = 0;

    loop {
        // `osm_id_cmp`, not raw `<=`: every ordering decision below already
        // uses it, and for negative IDs the two disagree - a raw boundary
        // test would cut the streams somewhere the merge order does not.
        let in_range =
            |e: &Element<'_>| crate::osm_id::osm_id_cmp(element_id(e), merge_up_to).is_le();
        let old_in_range = old_iter.peek().is_some_and(in_range);
        let new_in_range = new_iter.peek().is_some_and(in_range);

        match (old_in_range, new_in_range) {
            (false, false) => break,
            (true, false) => {
                let o = old_iter.next().expect("checked peek");
                on_action(BlockMergeAction::ElementOldOnly(&o))?;
                old_consumed += 1;
            }
            (false, true) => {
                let n = new_iter.next().expect("checked peek");
                on_action(BlockMergeAction::ElementNewOnly(&n))?;
                new_consumed += 1;
            }
            (true, true) => {
                let o_id = element_id(old_iter.peek().expect("checked"));
                let n_id = element_id(new_iter.peek().expect("checked"));

                match crate::osm_id::osm_id_cmp(o_id, n_id) {
                    std::cmp::Ordering::Less => {
                        let o = old_iter.next().expect("checked");
                        on_action(BlockMergeAction::ElementOldOnly(&o))?;
                        old_consumed += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        let n = new_iter.next().expect("checked");
                        on_action(BlockMergeAction::ElementNewOnly(&n))?;
                        new_consumed += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        let o = old_iter.next().expect("checked");
                        let n = new_iter.next().expect("checked");
                        old_consumed += 1;
                        new_consumed += 1;
                        if crate::content::content_equal(&o, &n) {
                            on_action(BlockMergeAction::ElementEqual {
                                id: o_id,
                                version: element_version(&o),
                                type_char,
                            })?;
                        } else {
                            on_action(BlockMergeAction::ElementModified { old: &o, new: &n })?;
                        }
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
/// `*_has_more` is what the iterator actually observed, not what the blob's
/// indexdata claims. The residual decision is made from these, so an
/// understated `max_id` cannot silently drop a block's tail.
struct MergePairOutcome {
    old_consumed: usize,
    new_consumed: usize,
    old_has_more: bool,
    new_has_more: bool,
}
