//! Streaming element cursor and merge-join for sorted PBF operations.
//!
//! Provides [`StreamingBlocks`] - a block-level cursor over a pipelined PBF
//! reader that yields owned elements one at a time, handling block boundaries
//! transparently. [`merge_join_phase`] runs a generic two-pointer merge-join
//! over two cursors, used by `diff` and `derive_changes`.

use crate::ElementKind;
use crate::{BlockType, Element, PrimitiveBlock};

use super::write::{OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay};
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// StreamingBlocks - block-level cursor with stashing
// ---------------------------------------------------------------------------

/// Block source shared across all type phases of a streaming merge-join.
///
/// Wraps a [`PipelinedBlocks`] iterator and manages stashing: when a block's
/// type doesn't match the current phase, it is stashed for the next phase
/// rather than being dropped.
pub(crate) struct StreamingBlocks {
    blocks: Box<dyn Iterator<Item = crate::error::Result<PrimitiveBlock>>>,
    stashed: Option<PrimitiveBlock>,
}

impl StreamingBlocks {
    /// Create from a sequential BlobReader. Avoids PrimitiveBlock cross-thread
    /// alloc/free retention from the pipelined reader.
    pub(crate) fn new_sequential(
        path: &std::path::Path,
        direct_io: bool,
        host: &crate::host::Host,
    ) -> crate::error::Result<Self> {
        let mut blob_reader = crate::blob::BlobReader::open_in(path, direct_io, host)?;
        blob_reader.set_parse_indexdata(true);
        blob_reader.leading_header()?;
        let mut decompress_buf: Vec<u8> = Vec::new();
        let mut st_scratch: Vec<(u32, u32)> = Vec::new();
        let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
        // Captured once; a decode failure is located with the blob's true
        // index and offset.
        let source = blob_reader.source().cloned();
        let iter = std::iter::from_fn(move || {
            loop {
                let blob = match blob_reader.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                    continue;
                }
                if let Err(e) = blob.decompress_into(&mut decompress_buf) {
                    return Some(Err(blob.locate_error(source.as_ref(), e)));
                }
                return Some(
                    crate::block::PrimitiveBlock::from_vec_with_scratch(
                        std::mem::take(&mut decompress_buf),
                        &mut st_scratch,
                        &mut gr_scratch,
                    )
                    .map_err(|e| blob.locate_error(source.as_ref(), e)),
                );
            }
        });
        Ok(Self {
            blocks: Box::new(iter),
            stashed: None,
        })
    }

    fn next_block(&mut self) -> Result<Option<PrimitiveBlock>> {
        if let Some(b) = self.stashed.take() {
            return Ok(Some(b));
        }
        match self.blocks.next() {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Typed element extraction
// ---------------------------------------------------------------------------

/// Fill `buffer` with owned elements of type `T` from the next matching block.
///
/// Uses [`BlockType`] as a fast path (1-byte classification, no element parsing)
/// to skip blocks of the wrong type. `Mixed` and `Empty` blocks fall through to
/// element-level conversion, which is where a file claiming `Sort.Type_then_ID`
/// while carrying mixed blocks is caught: a mixed block is an error, because
/// consuming it here would discard its other-kind elements for good.
///
/// When a non-matching block is encountered, it is stashed for the next phase.
/// Returns `true` if elements were added to the buffer, `false` if the phase
/// is exhausted (EOF or only non-matching blocks remain).
///
/// A block with no elements is skipped before it is classified, whatever its
/// groups' first field says. An empty `DenseNodes` message classifies as a
/// node block, so after the node phase it used to be stashed as "a later
/// phase's block" - and then end the relation phase when it turned up there,
/// silently dropping every relation after it.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn fill_buffer<T>(
    source: &mut StreamingBlocks,
    buffer: &mut Vec<T>,
    is_phase_type: fn(BlockType) -> bool,
    convert: fn(&Element<'_>) -> ConvertResult<T>,
) -> Result<bool> {
    loop {
        let block = match source.next_block()? {
            Some(b) => b,
            None => return Ok(false),
        };

        if block.elements().next().is_none() {
            continue;
        }

        let bt = block.block_type();

        // Fast path: block type doesn't match this phase and isn't ambiguous.
        if bt != BlockType::Empty && bt != BlockType::Mixed && !is_phase_type(bt) {
            source.stashed = Some(block);
            return Ok(false);
        }

        // Convert elements. For single-type blocks (the normal sorted case),
        // every element converts. For Mixed/Empty, some may not.
        buffer.clear();
        let mut wrong_type_seen = false;
        for element in block.elements() {
            match convert(&element)? {
                Some(owned) => buffer.push(owned),
                None => {
                    wrong_type_seen = true;
                }
            }
        }

        // A wrong-type element in a block this phase consumed is an element
        // that leaves the stream entirely: the block is gone by the time the
        // later phase for its kind runs, and a partially-consumed block
        // cannot be stashed. This used to be waved through with a comment
        // calling it "acceptable lossy behavior", which made a mixed block
        // silently shorten the diff. Sorted input is a documented
        // precondition of this merge, so a mixed block means the input does
        // not meet it - say so rather than quietly dropping elements.
        if wrong_type_seen {
            return Err(Error::invalid_data(
                "streaming merge-join: input contains a mixed-type block, so elements \
                 of a later type would be consumed and discarded by an earlier phase. \
                 This merge requires Sort.Type_then_ID input with one element type per \
                 block; re-sort the file first with `pbfhogg sort`.",
            ));
        }

        if buffer.is_empty() {
            // Truly empty block - keep going.
            continue;
        }

        // Reverse so pop() yields ascending ID order.
        buffer.reverse();
        return Ok(true);
    }
}

/// Yield the next owned element of type `T` from the stream.
///
/// Returns `None` when the current type phase is exhausted.
pub(crate) fn next_element<T>(
    source: &mut StreamingBlocks,
    buffer: &mut Vec<T>,
    is_phase_type: fn(BlockType) -> bool,
    convert: fn(&Element<'_>) -> ConvertResult<T>,
) -> Result<Option<T>> {
    loop {
        if let Some(elem) = buffer.pop() {
            return Ok(Some(elem));
        }
        if !fill_buffer(source, buffer, is_phase_type, convert)? {
            return Ok(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Block type predicates
// ---------------------------------------------------------------------------

pub(crate) fn is_node_block(bt: BlockType) -> bool {
    bt.is_nodes()
}

pub(crate) fn is_way_block(bt: BlockType) -> bool {
    bt.is_ways()
}

pub(crate) fn is_relation_block(bt: BlockType) -> bool {
    bt.is_relations()
}

// ---------------------------------------------------------------------------
// Conversion functions: Element -> Option<Owned*>
// ---------------------------------------------------------------------------

/// `Ok(None)` for an element of another kind; `Err` for a string-table
/// lookup that fails. Conversion is strict so that a corrupt tag or role is
/// an error here rather than a silently shortened owned element whose
/// comparison then cannot see the corruption.
pub(crate) type ConvertResult<T> = crate::error::Result<Option<T>>;

fn owned_tags<'a>(
    tags: impl Iterator<Item = crate::error::Result<(&'a str, &'a str)>>,
) -> crate::error::Result<Vec<(String, String)>> {
    tags.map(|tag| tag.map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect()
}

pub(crate) fn convert_node(element: &Element<'_>) -> ConvertResult<OwnedNode> {
    Ok(match element {
        Element::DenseNode(dn) => Some(OwnedNode {
            id: dn.id(),
            decimicro_lat: dn.decimicro_lat(),
            decimicro_lon: dn.decimicro_lon(),
            tags: owned_tags(dn.tags_checked())?,
            metadata: dn
                .info()
                .map(crate::dense::DenseNodeInfo::version)
                .filter(|&v| v != -1)
                .map(OwnedMetadata::version_only),
        }),
        Element::Node(n) => Some(OwnedNode {
            id: n.id(),
            decimicro_lat: n.decimicro_lat(),
            decimicro_lon: n.decimicro_lon(),
            tags: owned_tags(n.tags_checked())?,
            metadata: n.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    })
}

pub(crate) fn convert_way(element: &Element<'_>) -> ConvertResult<OwnedWay> {
    Ok(match element {
        Element::Way(w) => Some(OwnedWay {
            id: w.id(),
            tags: owned_tags(w.tags_checked())?,
            refs: w.refs().collect(),
            metadata: w.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    })
}

pub(crate) fn convert_relation(element: &Element<'_>) -> ConvertResult<OwnedRelation> {
    Ok(match element {
        Element::Relation(r) => Some(OwnedRelation {
            id: r.id(),
            tags: owned_tags(r.tags_checked())?,
            members: r
                .members()
                .map(|m| {
                    Ok(OwnedMember {
                        id: m.id,
                        role: m.role()?.to_owned(),
                    })
                })
                .collect::<crate::error::Result<_>>()?,
            metadata: r.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// MergeJoinElement trait - shared accessors for the generic merge-join
// ---------------------------------------------------------------------------

/// Equality is not a member of this trait: every element type compares
/// through [`crate::content::content_equal`], the one definition of element
/// content.
pub(crate) trait MergeJoinElement: Sized + crate::content::Content {
    fn id(&self) -> i64;
    fn is_block_type(bt: BlockType) -> bool;
    fn convert(element: &Element<'_>) -> ConvertResult<Self>;
}

impl MergeJoinElement for OwnedNode {
    fn id(&self) -> i64 {
        self.id
    }
    fn is_block_type(bt: BlockType) -> bool {
        is_node_block(bt)
    }
    fn convert(element: &Element<'_>) -> ConvertResult<Self> {
        convert_node(element)
    }
}

impl MergeJoinElement for OwnedWay {
    fn id(&self) -> i64 {
        self.id
    }
    fn is_block_type(bt: BlockType) -> bool {
        is_way_block(bt)
    }
    fn convert(element: &Element<'_>) -> ConvertResult<Self> {
        convert_way(element)
    }
}

impl MergeJoinElement for OwnedRelation {
    fn id(&self) -> i64 {
        self.id
    }
    fn is_block_type(bt: BlockType) -> bool {
        is_relation_block(bt)
    }
    fn convert(element: &Element<'_>) -> ConvertResult<Self> {
        convert_relation(element)
    }
}

// ---------------------------------------------------------------------------
// Generic streaming merge-join
// ---------------------------------------------------------------------------

/// Result of comparing one pair in the merge-join.
pub(crate) enum MergeJoinAction<'a, T> {
    /// Element exists only in old (deleted).
    OldOnly(&'a T),
    /// Element exists only in new (created).
    NewOnly(&'a T),
    /// Element exists in both but differs (old, new).
    Modified(&'a T, &'a T),
    /// Element exists in both and is identical.
    Equal(&'a T),
}

/// Streaming two-pointer merge-join for one element type phase.
///
/// Pulls elements one at a time from both cursors and classifies each pair
/// by ID comparison + content equality. The caller provides a single callback
/// that receives a [`MergeJoinAction`] for each pair.
pub(crate) fn merge_join_phase<T: MergeJoinElement>(
    old_src: &mut StreamingBlocks,
    old_buf: &mut Vec<T>,
    new_src: &mut StreamingBlocks,
    new_buf: &mut Vec<T>,
    mut on_action: impl FnMut(MergeJoinAction<'_, T>) -> Result<()>,
) -> Result<()> {
    let mut old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
    let mut new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;

    loop {
        match (&old_elem, &new_elem) {
            (None, None) => break,
            (Some(o), None) => {
                on_action(MergeJoinAction::OldOnly(o))?;

                old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
            }
            (None, Some(n)) => {
                on_action(MergeJoinAction::NewOnly(n))?;

                new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
            }
            (Some(o), Some(n)) => match crate::osm_id::osm_id_cmp(o.id(), n.id()) {
                std::cmp::Ordering::Less => {
                    on_action(MergeJoinAction::OldOnly(o))?;

                    old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                }
                std::cmp::Ordering::Greater => {
                    on_action(MergeJoinAction::NewOnly(n))?;

                    new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                }
                std::cmp::Ordering::Equal => {
                    if crate::content::content_equal(o, n) {
                        on_action(MergeJoinAction::Equal(o))?;
                    } else {
                        on_action(MergeJoinAction::Modified(o, n))?;
                    }

                    old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                    new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                }
            },
        }
    }
    Ok(())
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

/// Extract version from any Element variant.
pub(crate) fn element_version(e: &Element<'_>) -> Option<i32> {
    match e {
        Element::DenseNode(dn) => dn.info().map(crate::DenseNodeInfo::version),
        Element::Node(n) => n.info().version(),
        Element::Way(w) => w.info().version(),
        Element::Relation(r) => r.info().version(),
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

/// Read the next OsmData blob matching `kind` without decompressing it.
/// Returns None at EOF or when the next blob's type doesn't match `kind`
/// (which means we've moved past this type phase in a sorted PBF).
///
/// **Important:** In a sorted PBF, element types appear in order:
/// nodes → ways → relations. Once we encounter a blob of a different
/// (later) kind, this phase is done. We must NOT consume that blob
/// because the next phase needs it. We store it in `stash` for the
/// next phase to pick up.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn next_blob_for_kind(
    reader: &mut crate::blob::BlobReader<crate::file_reader::FileReader>,
    kind: ElementKind,
    stash: &mut Option<crate::blob::Blob>,
) -> Result<Option<PendingBlob>> {
    loop {
        let blob = if let Some(stashed) = stash.take() {
            stashed
        } else {
            match reader.next() {
                Some(result) => result?,
                None => return Ok(None),
            }
        };

        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        let index = match blob.index() {
            Some(idx) => idx,
            None => {
                return Err(Error::invalid_data(
                    "block-pair merge requires indexdata but blob has none",
                ));
            }
        };
        if index.kind != kind {
            if index.kind > kind {
                *stash = Some(blob);
                return Ok(None);
            }
            // An EARLIER kind at this point cannot be reached on sorted
            // input: the phase for that kind ran first and drained every
            // blob of it before stopping at the first later-kind blob. So
            // reaching here means the file's header claims
            // Sort.Type_then_ID while its blobs are not in canonical order -
            // exactly what `degrade --unsort` produces. This used to
            // `continue`, which discarded the blob and every element in it
            // for the rest of the run, with no error and no count anomaly.
            return Err(Error::invalid_data(format!(
                "block-pair merge: found a {} blob during the {} phase. The input's header \
                 claims Sort.Type_then_ID but its blobs are not in canonical order \
                 (nodes, then ways, then relations); re-sort it first with `pbfhogg sort`.",
                index.kind.as_str(),
                kind.as_str(),
            )));
        }
        return Ok(Some(PendingBlob { blob, index }));
    }
}

/// Decompress a pending blob into a decoded BlockState.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::needless_pass_by_value)] // consumes blob to drop compressed bytes after decode
fn decode_pending(
    pending: PendingBlob,
    buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> crate::error::Result<BlockState> {
    pending.blob.decompress_into(buf)?;
    let block = PrimitiveBlock::from_vec_with_scratch(std::mem::take(buf), st_scratch, gr_scratch)?;
    Ok(BlockState {
        block,
        skip_count: 0,
        index: pending.index,
    })
}

/// Running canonical-order check over one file's blob sequence, in FILE
/// order.
///
/// The sequential block-pair merge discovers a mis-ordered file structurally:
/// it walks blobs in file order, so an earlier-kind blob turning up after
/// that kind's phase closed is unmissable ([`next_blob_for_kind`]). The
/// parallel planners cannot discover it that way, because `walk_file`
/// buckets every blob by kind before any ordering is looked at - file order
/// never reaches them. That asymmetry made `diff` answer differently
/// depending on `-j`: `-j 1` refused the file while the default `-j`
/// reported "Files are identical", on the same pair of inputs.
///
/// Sorted blob order is also a precondition of the shard planners
/// themselves, which binary-search each per-kind list assuming it ascends.
/// So this is not only about agreeing with the sequential path; an
/// unsorted bucket is an input the planner has no defined answer for.
///
/// Both checks use `osm_id` blob-range keys, not raw `i64`, so negative IDs
/// order the way `osm_id_cmp` orders them everywhere else.
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
        if let Err((previous_kind, previous_id)) =
            self.watermark
                .observe(index.kind.ordinal(), index.min_id, index.max_id)
        {
            return Err(Error::invalid_data(format!(
                "block-pair merge: a {} blob starting at id {} follows {} \
                 output already reaching id {previous_id}. The input's header claims \
                 Sort.Type_then_ID but its blobs are not in canonical order; re-sort it \
                 first with `pbfhogg sort`.",
                index.kind.as_str(),
                crate::osm_id::blob_osm_first_id(index.min_id, index.max_id),
                ElementKind::from_ordinal(previous_kind).map_or("unknown", ElementKind::as_str),
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
    pub old_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    pub new_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    pub old_buf: Vec<u8>,
    pub new_buf: Vec<u8>,
    pub old_st: Vec<(u32, u32)>,
    pub old_gr: Vec<(u32, u32)>,
    pub new_st: Vec<(u32, u32)>,
    pub new_gr: Vec<(u32, u32)>,
    /// Stashed blob from a prior phase (consumed but belongs to a later kind).
    old_stash: Option<crate::blob::Blob>,
    new_stash: Option<crate::blob::Blob>,
    /// Shadow counters accumulated across all phases of this merge run.
    pub stats: BlockPairMergeStats,
}

impl BlockPairMergeState {
    pub(crate) fn new(
        old_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
        new_reader: crate::blob::BlobReader<crate::file_reader::FileReader>,
    ) -> Self {
        Self {
            old_reader,
            new_reader,
            old_buf: Vec::new(),
            new_buf: Vec::new(),
            old_st: Vec::new(),
            old_gr: Vec::new(),
            new_st: Vec::new(),
            new_gr: Vec::new(),
            old_stash: None,
            new_stash: None,
            stats: BlockPairMergeStats::default(),
        }
    }
}

/// Decode the next blob from a reader into a BlockState, or return None at EOF.
fn next_decoded_block(
    reader: &mut crate::blob::BlobReader<crate::file_reader::FileReader>,
    buf: &mut Vec<u8>,
    st: &mut Vec<(u32, u32)>,
    gr: &mut Vec<(u32, u32)>,
    kind: ElementKind,
    stash: &mut Option<crate::blob::Blob>,
) -> Result<Option<BlockState>> {
    match next_blob_for_kind(reader, kind, stash)? {
        Some(p) => Ok(Some(decode_pending(p, buf, st, gr)?)),
        None => Ok(None),
    }
}

/// Drain all remaining blobs of `kind` from one side of a merge.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn drain_remaining(
    state: &mut BlockPairMergeState,
    kind: ElementKind,
    is_old: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> Result<()>,
) -> Result<()> {
    let stats = &mut state.stats;
    let (reader, buf, st, gr, stash) = if is_old {
        (
            &mut state.old_reader,
            &mut state.old_buf,
            &mut state.old_st,
            &mut state.old_gr,
            &mut state.old_stash,
        )
    } else {
        (
            &mut state.new_reader,
            &mut state.new_buf,
            &mut state.new_st,
            &mut state.new_gr,
            &mut state.new_stash,
        )
    };
    while let Some(p) = next_blob_for_kind(reader, kind, stash)? {
        let bs = decode_pending(p, buf, st, gr)?;
        emit_block(&bs, is_old, on_action, stats)?;
    }
    Ok(())
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
        return Err(Error::invalid_data(format!(
            "block-pair merge: the {side} blob's indexdata claims its last ID is \
             {claimed_last}, but the block still holds elements past the merge boundary \
             {merge_up_to}. The stored blob metadata does not match the blob's contents; \
             re-generate the file's indexdata (for example with `pbfhogg cat`)."
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
            let op = next_blob_for_kind(&mut state.old_reader, kind, &mut state.old_stash)?;
            let np = next_blob_for_kind(&mut state.new_reader, kind, &mut state.new_stash)?;

            match (op, np) {
                (None, None) => break,
                (Some(op), None) => {
                    let os = decode_pending(
                        op,
                        &mut state.old_buf,
                        &mut state.old_st,
                        &mut state.old_gr,
                    )?;
                    emit_block(&os, true, on_action, &mut state.stats)?;
                    drain_remaining(state, kind, true, on_action)?;
                    break;
                }
                (None, Some(np)) => {
                    let ns = decode_pending(
                        np,
                        &mut state.new_buf,
                        &mut state.new_st,
                        &mut state.new_gr,
                    )?;
                    emit_block(&ns, false, on_action, &mut state.stats)?;
                    drain_remaining(state, kind, false, on_action)?;
                    break;
                }
                (Some(op), Some(np)) => {
                    if blob_entirely_before(&op.index, &np.index) {
                        let os = decode_pending(
                            op,
                            &mut state.old_buf,
                            &mut state.old_st,
                            &mut state.old_gr,
                        )?;
                        emit_block(&os, true, on_action, &mut state.stats)?;
                        // Stash new blob undecoded - next iteration can try v1 byte comparison.
                        state.new_stash = Some(np.blob);
                        continue;
                    }
                    if blob_entirely_before(&np.index, &op.index) {
                        let ns = decode_pending(
                            np,
                            &mut state.new_buf,
                            &mut state.new_st,
                            &mut state.new_gr,
                        )?;
                        emit_block(&ns, false, on_action, &mut state.stats)?;
                        // Stash old blob undecoded - next iteration can try v1 byte comparison.
                        state.old_stash = Some(op.blob);
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
                    old_decoded = Some(decode_pending(
                        op,
                        &mut state.old_buf,
                        &mut state.old_st,
                        &mut state.old_gr,
                    )?);
                    new_decoded = Some(decode_pending(
                        np,
                        &mut state.new_buf,
                        &mut state.new_st,
                        &mut state.new_gr,
                    )?);
                }
            }
        }

        // Slow path: at least one side has a decoded residual block.
        if old_decoded.is_none() {
            old_decoded = next_decoded_block(
                &mut state.old_reader,
                &mut state.old_buf,
                &mut state.old_st,
                &mut state.old_gr,
                kind,
                &mut state.old_stash,
            )?;
        }
        if new_decoded.is_none() {
            new_decoded = next_decoded_block(
                &mut state.new_reader,
                &mut state.new_buf,
                &mut state.new_st,
                &mut state.new_gr,
                kind,
                &mut state.new_stash,
            )?;
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
