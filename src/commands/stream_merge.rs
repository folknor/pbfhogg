//! Streaming element cursor and merge-join for sorted PBF operations.
//!
//! Provides [`StreamingBlocks`] — a block-level cursor over a pipelined PBF
//! reader that yields owned elements one at a time, handling block boundaries
//! transparently. [`merge_join_phase`] runs a generic two-pointer merge-join
//! over two cursors, used by `diff` and `derive_changes`.

use crate::{BlockType, Element, PrimitiveBlock};

use super::elements_xml::{OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay};
use super::Result;

// ---------------------------------------------------------------------------
// StreamingBlocks — block-level cursor with stashing
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
        let pool = crate::blob::DecompressPool::new();
        let iter = std::iter::from_fn(move || {
            loop {
                let blob = match blob_reader.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                    continue;
                }
                let decompressed = match blob.decompress_pooled(&pool) {
                    Ok(d) => d,
                    Err(e) => return Some(Err(e)),
                };
                return Some(crate::block::PrimitiveBlock::new(decompressed));
            }
        });
        Ok(Self { blocks: Box::new(iter), stashed: None })
    }

    fn next_block(&mut self) -> Result<Option<PrimitiveBlock>> {
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
/// through to element-level conversion — mandatory for safety with malformed
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
) -> Result<bool> {
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
        // elements are lost — but in a properly sorted PBF this never happens
        // (blocks are single-type). For truly mixed blocks, this is acceptable
        // lossy behavior consistent with requiring sorted input.

        if buffer.is_empty() {
            // Empty block or all elements were wrong type — skip and try next.
            if wrong_type_seen {
                // We consumed the block but got nothing. The wrong-type elements
                // are from a later phase, but we can't stash a partially consumed
                // block. This only happens with malformed Mixed blocks in
                // nominally sorted PBFs. Continue to the next block.
                continue;
            }
            // Truly empty block — keep going.
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
// MergeJoinElement trait — shared accessors for the generic merge-join
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
    fn equal(a: &Self, b: &Self) -> bool { super::elements_xml::nodes_equal(a, b) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_node(element) }
}

impl MergeJoinElement for OwnedWay {
    fn id(&self) -> i64 { self.id }
    fn is_block_type(bt: BlockType) -> bool { is_way_block(bt) }
    fn equal(a: &Self, b: &Self) -> bool { super::elements_xml::ways_equal(a, b) }
    fn convert(element: &Element<'_>) -> Option<Self> { convert_way(element) }
}

impl MergeJoinElement for OwnedRelation {
    fn id(&self) -> i64 { self.id }
    fn is_block_type(bt: BlockType) -> bool { is_relation_block(bt) }
    fn equal(a: &Self, b: &Self) -> bool { super::elements_xml::relations_equal(a, b) }
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
            (Some(o), Some(n)) => {
                match super::osm_id_cmp(o.id(), n.id()) {
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
