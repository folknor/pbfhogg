//! Streaming element cursor and merge-join for sorted PBF operations.
//!
//! Provides [`StreamingBlocks`] - a block-level cursor over a pipelined PBF
//! reader that yields owned elements one at a time, handling block boundaries
//! transparently. [`merge_join_phase`] runs a generic two-pointer merge-join
//! over two cursors, used by `diff` and `derive_changes`.

use crate::blob_meta::ElemKind;
use crate::{BlockType, Element, PrimitiveBlock};

use super::write::{OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay};
use crate::BoxResult;

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
    ) -> crate::error::Result<Self> {
        let mut blob_reader = crate::blob::BlobReader::open(path, direct_io)?;
        blob_reader.set_parse_indexdata(true);
        blob_reader.next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let mut decompress_buf: Vec<u8> = Vec::new();
        let mut st_scratch: Vec<(u32, u32)> = Vec::new();
        let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
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
                    return Some(Err(e));
                }
                return Some(crate::block::PrimitiveBlock::from_vec_with_scratch(
                    std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
                ));
            }
        });
        Ok(Self { blocks: Box::new(iter), stashed: None })
    }

    fn next_block(&mut self) -> BoxResult<Option<PrimitiveBlock>> {
        if let Some(b) = self.stashed.take() {
            return Ok(Some(b));
        }
        match self.blocks.next() {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(e.into()),
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
/// to skip blocks of the wrong type. `Mixed` and `Empty` blocks always fall
/// through to element-level conversion - mandatory for safety with malformed
/// files that claim `Sort.Type_then_ID` but have mixed blocks.
///
/// When a non-matching block is encountered, it is stashed for the next phase.
/// Returns `true` if elements were added to the buffer, `false` if the phase
/// is exhausted (EOF or only non-matching blocks remain).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn fill_buffer<T>(
    source: &mut StreamingBlocks,
    buffer: &mut Vec<T>,
    is_phase_type: fn(BlockType) -> bool,
    convert: fn(&Element<'_>) -> Option<T>,
) -> BoxResult<bool> {
    loop {
        let block = match source.next_block()? {
            Some(b) => b,
            None => return Ok(false),
        };

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
            match convert(&element) {
                Some(owned) => buffer.push(owned),
                None => {
                    wrong_type_seen = true;
                }
            }
        }

        // If we saw wrong-type elements in a Mixed block but also got some
        // matching elements, we consumed the matching ones. The wrong-type
        // elements are lost - but in a properly sorted PBF this never happens
        // (blocks are single-type). For truly mixed blocks, this is acceptable
        // lossy behavior consistent with requiring sorted input.

        if buffer.is_empty() {
            // Empty block or all elements were wrong type - skip and try next.
            if wrong_type_seen {
                // We consumed the block but got nothing. The wrong-type elements
                // are from a later phase, but we can't stash a partially consumed
                // block. This only happens with malformed Mixed blocks in
                // nominally sorted PBFs. Continue to the next block.
                continue;
            }
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
    convert: fn(&Element<'_>) -> Option<T>,
) -> BoxResult<Option<T>> {
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

pub(crate) fn convert_node(element: &Element<'_>) -> Option<OwnedNode> {
    match element {
        Element::DenseNode(dn) => Some(OwnedNode {
            id: dn.id(),
            decimicro_lat: dn.decimicro_lat(),
            decimicro_lon: dn.decimicro_lon(),
            tags: dn.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
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
            tags: n.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            metadata: n.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    }
}

pub(crate) fn convert_way(element: &Element<'_>) -> Option<OwnedWay> {
    match element {
        Element::Way(w) => Some(OwnedWay {
            id: w.id(),
            tags: w.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            refs: w.refs().collect(),
            metadata: w.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    }
}

pub(crate) fn convert_relation(element: &Element<'_>) -> Option<OwnedRelation> {
    match element {
        Element::Relation(r) => Some(OwnedRelation {
            id: r.id(),
            tags: r.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            members: r
                .members()
                .map(|m| OwnedMember {
                    id: m.id,
                    role: m.role().unwrap_or("").to_owned(),
                })
                .collect(),
            metadata: r.info().version().map(OwnedMetadata::version_only),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// MergeJoinElement trait - shared accessors for the generic merge-join
// ---------------------------------------------------------------------------

pub(crate) trait MergeJoinElement: Sized {
    fn id(&self) -> i64;
    fn is_block_type(bt: BlockType) -> bool;
    fn equal(a: &Self, b: &Self) -> bool;
    fn convert(element: &Element<'_>) -> Option<Self>;
}

impl MergeJoinElement for OwnedNode {
    fn id(&self) -> i64 { self.id }
    fn is_block_type(bt: BlockType) -> bool { is_node_block(bt) }
    fn equal(a: &Self, b: &Self) -> bool { super::write::nodes_equal(a, b) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_node(element) }
}

impl MergeJoinElement for OwnedWay {
    fn id(&self) -> i64 { self.id }
    fn is_block_type(bt: BlockType) -> bool { is_way_block(bt) }
    fn equal(a: &Self, b: &Self) -> bool { super::write::ways_equal(a, b) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_way(element) }
}

impl MergeJoinElement for OwnedRelation {
    fn id(&self) -> i64 { self.id }
    fn is_block_type(bt: BlockType) -> bool { is_relation_block(bt) }
    fn equal(a: &Self, b: &Self) -> bool { super::write::relations_equal(a, b) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_relation(element) }
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
    mut on_action: impl FnMut(MergeJoinAction<'_, T>) -> BoxResult<()>,
) -> BoxResult<()> {
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
            (Some(o), Some(n)) => {
                match crate::commands::osm_id_cmp(o.id(), n.id()) {
                    std::cmp::Ordering::Less => {
                        on_action(MergeJoinAction::OldOnly(o))?;

                        old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                    }
                    std::cmp::Ordering::Greater => {
                        on_action(MergeJoinAction::NewOnly(n))?;

                        new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                    }
                    std::cmp::Ordering::Equal => {
                        if T::equal(o, n) {
                            on_action(MergeJoinAction::Equal(o))?;
                        } else {
                            on_action(MergeJoinAction::Modified(o, n))?;
                        }

                        old_elem = next_element(old_src, old_buf, T::is_block_type, T::convert)?;
                        new_elem = next_element(new_src, new_buf, T::is_block_type, T::convert)?;
                    }
                }
            }
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

/// Compare two node elements (DenseNode or Node) by coords + tags.
/// Handles all 4 cross-match combinations. Matches `nodes_equal` semantics:
/// compares decimicro_lat, decimicro_lon, tags - NOT id or metadata.
fn borrowed_nodes_equal(a: &Element<'_>, b: &Element<'_>) -> bool {
    let (a_lat, a_lon) = match a {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return false,
    };
    let (b_lat, b_lon) = match b {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return false,
    };
    if a_lat != b_lat || a_lon != b_lon {
        return false;
    }
    // Tag comparison: DenseTagIter and TagIter are different types but both
    // yield (&str, &str). Handle all 4 cross-match combinations explicitly.
    match (a, b) {
        (Element::DenseNode(da), Element::DenseNode(db)) => da.tags().eq(db.tags()),
        (Element::DenseNode(da), Element::Node(nb)) => {
            iter_tags_equal(da.tags(), nb.tags())
        }
        (Element::Node(na), Element::DenseNode(db)) => {
            iter_tags_equal(na.tags(), db.tags())
        }
        (Element::Node(na), Element::Node(nb)) => na.tags().eq(nb.tags()),
        _ => false,
    }
}

/// Compare two tag iterators of different concrete types.
/// Both must yield `(&str, &str)`.
fn iter_tags_equal<'a>(
    a: impl Iterator<Item = (&'a str, &'a str)>,
    b: impl Iterator<Item = (&'a str, &'a str)>,
) -> bool {
    a.eq(b)
}

/// Compare two Way elements by refs + tags. Matches `ways_equal` semantics.
fn borrowed_ways_equal(a: &crate::Way<'_>, b: &crate::Way<'_>) -> bool {
    a.refs().eq(b.refs()) && a.tags().eq(b.tags())
}

/// Compare two Relation elements by tags + members. Matches `relations_equal` semantics.
fn borrowed_relations_equal(a: &crate::Relation<'_>, b: &crate::Relation<'_>) -> bool {
    if !a.tags().eq(b.tags()) {
        return false;
    }
    borrowed_members_equal(a, b)
}

/// Compare relation members by (MemberId, role). Matches `members_equal` semantics.
/// Role uses `unwrap_or("")` matching the owned conversion path.
fn borrowed_members_equal(a: &crate::Relation<'_>, b: &crate::Relation<'_>) -> bool {
    let mut a_iter = a.members();
    let mut b_iter = b.members();
    loop {
        match (a_iter.next(), b_iter.next()) {
            (None, None) => return true,
            (Some(am), Some(bm)) => {
                if am.id != bm.id {
                    return false;
                }
                let a_role = am.role().unwrap_or("");
                let b_role = bm.role().unwrap_or("");
                if a_role != b_role {
                    return false;
                }
            }
            _ => return false, // different lengths
        }
    }
}

/// Compare two elements of the same type. Dispatches to type-specific comparison.
fn borrowed_elements_equal(a: &Element<'_>, b: &Element<'_>) -> bool {
    match (a, b) {
        // Node phase: any combination of DenseNode/Node
        (Element::DenseNode(_) | Element::Node(_), Element::DenseNode(_) | Element::Node(_)) => {
            borrowed_nodes_equal(a, b)
        }
        // Way phase
        (Element::Way(wa), Element::Way(wb)) => borrowed_ways_equal(wa, wb),
        // Relation phase
        (Element::Relation(ra), Element::Relation(rb)) => borrowed_relations_equal(ra, rb),
        // Different types with same ID = Modified
        _ => false,
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
fn next_blob_for_kind(
    reader: &mut crate::blob::BlobReader<crate::file_reader::FileReader>,
    kind: ElemKind,
    stash: &mut Option<crate::blob::Blob>,
) -> BoxResult<Option<PendingBlob>> {
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
                return Err("block-pair merge requires indexdata but blob has none".into());
            }
        };
        if index.kind != kind {
            if kind_order(index.kind) > kind_order(kind) {
                *stash = Some(blob);
                return Ok(None);
            }
            continue;
        }
        return Ok(Some(PendingBlob { blob, index }));
    }
}

/// Decompress a pending blob into a decoded BlockState.
#[allow(clippy::needless_pass_by_value)] // consumes blob to drop compressed bytes after decode
fn decode_pending(
    pending: PendingBlob,
    buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> BoxResult<BlockState> {
    pending.blob.decompress_into(buf)?;
    let block = PrimitiveBlock::from_vec_with_scratch(
        std::mem::take(buf), st_scratch, gr_scratch,
    )?;
    Ok(BlockState {
        block,
        skip_count: 0,
        index: pending.index,
    })
}

/// Canonical order: nodes (0) → ways (1) → relations (2).
fn kind_order(kind: ElemKind) -> u8 {
    match kind {
        ElemKind::Node => 0,
        ElemKind::Way => 1,
        ElemKind::Relation => 2,
    }
}

/// Type char for an ElemKind.
pub(crate) fn kind_type_char(kind: ElemKind) -> char {
    match kind {
        ElemKind::Node => 'n',
        ElemKind::Way => 'w',
        ElemKind::Relation => 'r',
    }
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
        }
    }
}

/// Decode the next blob from a reader into a BlockState, or return None at EOF.
fn next_decoded_block(
    reader: &mut crate::blob::BlobReader<crate::file_reader::FileReader>,
    buf: &mut Vec<u8>,
    st: &mut Vec<(u32, u32)>,
    gr: &mut Vec<(u32, u32)>,
    kind: ElemKind,
    stash: &mut Option<crate::blob::Blob>,
) -> BoxResult<Option<BlockState>> {
    match next_blob_for_kind(reader, kind, stash)? {
        Some(p) => Ok(Some(decode_pending(p, buf, st, gr)?)),
        None => Ok(None),
    }
}

/// Drain all remaining blobs of `kind` from one side of a merge.
fn drain_remaining(
    state: &mut BlockPairMergeState,
    kind: ElemKind,
    is_old: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>,
) -> BoxResult<()> {
    let (reader, buf, st, gr, stash) = if is_old {
        (&mut state.old_reader, &mut state.old_buf, &mut state.old_st, &mut state.old_gr, &mut state.old_stash)
    } else {
        (&mut state.new_reader, &mut state.new_buf, &mut state.new_st, &mut state.new_gr, &mut state.new_stash)
    };
    while let Some(p) = next_blob_for_kind(reader, kind, stash)? {
        let bs = decode_pending(p, buf, st, gr)?;
        emit_block(&bs, is_old, on_action)?;
    }
    Ok(())
}

/// Emit a decoded block as BlobOldOnly or BlobNewOnly, accounting for skip.
fn emit_block(
    bs: &BlockState,
    is_old: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>,
) -> BoxResult<()> {
    let remaining = bs.index.count.saturating_sub(bs.skip_count as u64);
    if is_old {
        on_action(BlockMergeAction::BlobOldOnly {
            block: &bs.block,
            count: remaining,
            skip: bs.skip_count,
        })
    } else {
        on_action(BlockMergeAction::BlobNewOnly {
            block: &bs.block,
            count: remaining,
            skip: bs.skip_count,
        })
    }
}

/// Merge a pair of decoded overlapping blocks, tracking residuals.
fn merge_decoded_pair(
    old_decoded: &mut Option<BlockState>,
    new_decoded: &mut Option<BlockState>,
    type_char: char,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>,
) -> BoxResult<()> {
    let mut os = old_decoded.take().expect("checked Some");
    let mut ns = new_decoded.take().expect("checked Some");

    let merge_up_to = os.index.max_id.min(ns.index.max_id);
    let (old_consumed, new_consumed) = element_merge_pair(
        &os.block,
        os.skip_count,
        &ns.block,
        ns.skip_count,
        merge_up_to,
        type_char,
        on_action,
    )?;

    if os.index.max_id > merge_up_to {
        os.skip_count += old_consumed;
        *old_decoded = Some(os);
    }
    if ns.index.max_id > merge_up_to {
        ns.skip_count += new_consumed;
        *new_decoded = Some(ns);
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
pub(crate) fn block_pair_merge_phase(
    state: &mut BlockPairMergeState,
    kind: ElemKind,
    skip_equal_blobs: bool,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>,
) -> BoxResult<()> {
    let type_char = kind_type_char(kind);
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
                    let os = decode_pending(op, &mut state.old_buf, &mut state.old_st, &mut state.old_gr)?;
                    emit_block(&os, true, on_action)?;
                    drain_remaining(state, kind, true, on_action)?;
                    break;
                }
                (None, Some(np)) => {
                    let ns = decode_pending(np, &mut state.new_buf, &mut state.new_st, &mut state.new_gr)?;
                    emit_block(&ns, false, on_action)?;
                    drain_remaining(state, kind, false, on_action)?;
                    break;
                }
                (Some(op), Some(np)) => {
                    if op.index.max_id < np.index.min_id {
                        let os = decode_pending(op, &mut state.old_buf, &mut state.old_st, &mut state.old_gr)?;
                        emit_block(&os, true, on_action)?;
                        // Stash new blob undecoded - next iteration can try v1 byte comparison.
                        state.new_stash = Some(np.blob);
                        continue;
                    }
                    if np.index.max_id < op.index.min_id {
                        let ns = decode_pending(np, &mut state.new_buf, &mut state.new_st, &mut state.new_gr)?;
                        emit_block(&ns, false, on_action)?;
                        // Stash old blob undecoded - next iteration can try v1 byte comparison.
                        state.old_stash = Some(op.blob);
                        continue;
                    }
                    // Overlapping - try compressed byte comparison (v1).
                    if skip_equal_blobs && blobs_byte_equal(&op, &np) {
                        on_action(BlockMergeAction::BlobEqual(op.index.count))?;
                        continue;
                    }
                    old_decoded = Some(decode_pending(op, &mut state.old_buf, &mut state.old_st, &mut state.old_gr)?);
                    new_decoded = Some(decode_pending(np, &mut state.new_buf, &mut state.new_st, &mut state.new_gr)?);
                }
            }
        }

        // Slow path: at least one side has a decoded residual block.
        if old_decoded.is_none() {
            old_decoded = next_decoded_block(&mut state.old_reader, &mut state.old_buf, &mut state.old_st, &mut state.old_gr, kind, &mut state.old_stash)?;
        }
        if new_decoded.is_none() {
            new_decoded = next_decoded_block(&mut state.new_reader, &mut state.new_buf, &mut state.new_st, &mut state.new_gr, kind, &mut state.new_stash)?;
        }

        match (&old_decoded, &new_decoded) {
            (None, None) => break,
            (Some(_), None) => {
                let os = old_decoded.take().expect("checked");
                emit_block(&os, true, on_action)?;
            }
            (None, Some(_)) => {
                let ns = new_decoded.take().expect("checked");
                emit_block(&ns, false, on_action)?;
            }
            (Some(os), Some(ns)) => {
                if os.index.max_id < ns.index.min_id {
                    let os = old_decoded.take().expect("checked");
                    emit_block(&os, true, on_action)?;
                    continue;
                }
                if ns.index.max_id < os.index.min_id {
                    let ns = new_decoded.take().expect("checked");
                    emit_block(&ns, false, on_action)?;
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
/// Returns `(old_consumed, new_consumed)` - the number of elements consumed
/// from each side, so the caller can update skip counts without re-scanning.
fn element_merge_pair(
    old_block: &PrimitiveBlock,
    old_skip: usize,
    new_block: &PrimitiveBlock,
    new_skip: usize,
    merge_up_to: i64,
    type_char: char,
    on_action: &mut dyn FnMut(BlockMergeAction<'_>) -> BoxResult<()>,
) -> BoxResult<(usize, usize)> {
    let mut old_iter = old_block.elements().skip(old_skip).peekable();
    let mut new_iter = new_block.elements().skip(new_skip).peekable();
    let mut old_consumed: usize = 0;
    let mut new_consumed: usize = 0;

    loop {
        let old_in_range = old_iter
            .peek()
            .is_some_and(|e| element_id(e) <= merge_up_to);
        let new_in_range = new_iter
            .peek()
            .is_some_and(|e| element_id(e) <= merge_up_to);

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

                match crate::commands::osm_id_cmp(o_id, n_id) {
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
                        if borrowed_elements_equal(&o, &n) {
                            on_action(BlockMergeAction::ElementEqual {
                                id: o_id,
                                version: element_version(&o),
                                type_char,
                            })?;
                        } else {
                            on_action(BlockMergeAction::ElementModified {
                                old: &o,
                                new: &n,
                            })?;
                        }
                    }
                }
            }
        }
    }

    Ok((old_consumed, new_consumed))
}

