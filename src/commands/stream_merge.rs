//! Streaming element cursor for sorted PBF merge-join operations.
//!
//! Provides [`StreamingBlocks`] — a block-level cursor over a pipelined PBF
//! reader that yields owned elements one at a time, handling block boundaries
//! transparently. Used by `diff` (and later `derive_changes`) to perform
//! streaming two-pointer merge-joins in constant memory.

use crate::{BlockType, Element, PipelinedBlocks, PrimitiveBlock};

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
    blocks: PipelinedBlocks,
    stashed: Option<PrimitiveBlock>,
}

impl StreamingBlocks {
    pub(crate) fn new(blocks: PipelinedBlocks) -> Self {
        Self { blocks, stashed: None }
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
