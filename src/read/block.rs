//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use super::dense::DenseNodeIter;
use super::elements::{Element, Node, Relation, Way};
use super::wire::{
    WireBlock, WireDenseNodes, WireGroup, WireMessageIter, WireNode, WireRelation, WireWay,
};
use crate::error::{new_error, ErrorKind, Result};
use crate::proto;
use bytes::Bytes;
use std;

/// A `HeaderBlock`. It contains metadata about following [`PrimitiveBlock`]s.
#[derive(Clone, Debug)]
pub struct HeaderBlock {
    header: proto::HeaderBlock,
}

impl HeaderBlock {
    pub fn new(header: proto::HeaderBlock) -> HeaderBlock {
        HeaderBlock { header }
    }

    /// Returns the (optional) bounding box of the included features.
    #[allow(clippy::cast_precision_loss)]
    pub fn bbox(&self) -> Option<HeaderBBox> {
        self.header.bbox.as_ref().map(|bbox| HeaderBBox {
            left: (bbox.left as f64) * 1.0e-9,
            right: (bbox.right as f64) * 1.0e-9,
            top: (bbox.top as f64) * 1.0e-9,
            bottom: (bbox.bottom as f64) * 1.0e-9,
        })
    }

    /// Returns a list of required features that a parser needs to implement to parse the following
    /// [`PrimitiveBlock`]s.
    pub fn required_features(&self) -> &[String] {
        self.header.required_features.as_slice()
    }

    /// Returns a list of optional features that a parser can choose to ignore.
    pub fn optional_features(&self) -> &[String] {
        self.header.optional_features.as_slice()
    }

    /// Returns the name of the program that generated the file or `None` if unset.
    pub fn writing_program(&self) -> Option<&str> {
        self.header.writingprogram.as_deref()
    }

    /// Returns the source of the `bbox` field or `None` if unset.
    pub fn source(&self) -> Option<&str> {
        self.header.source.as_deref()
    }

    /// Returns the replication timestamp of the file, or `None` if unset.
    /// The timestamp is expressed in seconds since the UNIX epoch.
    pub fn osmosis_replication_timestamp(&self) -> Option<i64> {
        self.header.osmosis_replication_timestamp
    }

    /// Returns the replication sequence number of the file, or `None` if unset.
    pub fn osmosis_replication_sequence_number(&self) -> Option<i64> {
        self.header.osmosis_replication_sequence_number
    }

    /// Returns the replication base URL of the file, or `None` if unset.
    pub fn osmosis_replication_base_url(&self) -> Option<&str> {
        self.header.osmosis_replication_base_url.as_deref()
    }
}

/// A bounding box that is usually included in a [`HeaderBlock`].
/// The maximum precision of the coordinates is one nanodegree (10⁻⁹).
#[derive(Clone, Copy, Debug)]
pub struct HeaderBBox {
    /// left coordinate in degrees (minimum longitude)
    pub left: f64,
    /// right coordinate in degrees (maximum longitude)
    pub right: f64,
    /// top coordinate in degrees (maximum latitude)
    pub top: f64,
    /// bottom coordinate in degrees (minimum latitude)
    pub bottom: f64,
}

/// A `PrimitiveBlock`. It contains a sequence of groups.
///
/// # Zero-copy wire-format parsing
///
/// The block owns the decompressed bytes (`Bytes`) and contains a `WireBlock`
/// that borrows from them. The `WireBlock` stores only scalar values and byte
/// offset/length pairs — no `Vec<i64>` or `Vec<Bytes>` for packed fields.
/// Element iteration decodes packed varints on-the-fly from the buffer.
///
/// # Stringtable UTF-8 invariant
///
/// At construction time (`new()`), every entry in the block's stringtable is validated
/// with `std::str::from_utf8()`. This means all subsequent stringtable lookups
/// (`str_from_stringtable()`) can use `from_utf8_unchecked` — eliminating 16-48K
/// redundant UTF-8 validations per block (8000 elements × 2-6 tag lookups each).
///
/// # Why `PrimitiveBlock` does not implement `Clone`
///
/// No code in the crate needs to clone a `PrimitiveBlock`. For shared access, use
/// `Arc<PrimitiveBlock>` — a single atomic increment regardless of block size.
pub struct PrimitiveBlock {
    /// Owns the decompressed protobuf bytes.
    #[allow(dead_code)]
    buffer: Bytes,
    /// Zero-copy parsed view. Borrows from `buffer` via lifetime erasure.
    ///
    /// # Safety
    ///
    /// The `'static` lifetime is a lie — `block` actually borrows from `buffer`.
    /// This is safe because:
    /// 1. `buffer` is `Bytes` (immutable, reference-counted), never mutated.
    /// 2. `buffer` and `block` live in the same struct — `block` cannot outlive `buffer`.
    /// 3. `PrimitiveBlock` is not `Clone`, preventing accidental separation.
    /// 4. All public access goes through `&self`, tying the real lifetime to the borrow.
    block: WireBlock<'static>,
}

impl std::fmt::Debug for PrimitiveBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrimitiveBlock")
            .field("groups", &self.block.group_ranges.len())
            .field("stringtable_entries", &self.block.stringtable.len())
            .field("granularity", &self.block.granularity)
            .finish()
    }
}

impl PrimitiveBlock {
    /// Parse a `PrimitiveBlock` from decompressed protobuf bytes.
    ///
    /// Validates every entry in the stringtable as UTF-8. This up-front validation
    /// allows all later stringtable lookups to skip per-access UTF-8 checks.
    ///
    /// # Errors
    ///
    /// Returns `ErrorKind::StringtableUtf8` if any stringtable entry contains invalid
    /// UTF-8 bytes. Returns `ErrorKind::WireFormat` if the protobuf wire format is invalid.
    #[hotpath::measure]
    pub fn new(buffer: Bytes) -> Result<PrimitiveBlock> {
        let data: &[u8] = &buffer;
        let block = WireBlock::parse(data)?;

        // Validate every stringtable entry once at construction time.
        for index in 0..block.stringtable.len() {
            if let Some(bytes) = block.stringtable.get(index) {
                std::str::from_utf8(bytes)
                    .map_err(|err| new_error(ErrorKind::StringtableUtf8 { err, index }))?;
            }
        }

        // SAFETY: `block` borrows exclusively from `buffer` which is:
        // - immutable (Bytes is a read-only reference-counted buffer)
        // - stored in the same struct (cannot outlive the buffer)
        // - never exposed mutably (PrimitiveBlock has no &mut self methods on buffer)
        // WireBlock<'a> is covariant in 'a (contains only &'a [u8] references),
        // so transmuting the lifetime is sound.
        #[allow(clippy::transmute_undefined_repr)]
        let block =
            unsafe { std::mem::transmute::<WireBlock<'_>, WireBlock<'static>>(block) };

        Ok(PrimitiveBlock { buffer, block })
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`.
    // wontfix(name-iter-convention): elements() is more descriptive than iter() here;
    // BlockElementsIter name matches established osmpbf public API.
    pub fn elements(&self) -> BlockElementsIter<'_> {
        BlockElementsIter::new(&self.block)
    }

    /// Returns an iterator over the groups in this `PrimitiveBlock`.
    pub fn groups(&self) -> GroupIter<'_> {
        GroupIter::new(&self.block)
    }

    /// Calls the given closure on each element.
    pub fn for_each_element<F>(&self, mut f: F)
    where
        F: for<'a> FnMut(Element<'a>),
    {
        for group in self.groups() {
            for node in group.nodes() {
                f(Element::Node(node));
            }
            for dnode in group.dense_nodes() {
                f(Element::DenseNode(dnode));
            }
            for way in group.ways() {
                f(Element::Way(way));
            }
            for relation in group.relations() {
                f(Element::Relation(relation));
            }
        }
    }
}

/// A `PrimitiveGroup` contains a sequence of elements of one type.
pub struct PrimitiveGroup<'a> {
    block: &'a WireBlock<'static>,
    group: WireGroup<'a>,
}

impl<'a> PrimitiveGroup<'a> {
    fn new(block: &'a WireBlock<'static>, data: &'a [u8]) -> PrimitiveGroup<'a> {
        PrimitiveGroup {
            block,
            group: WireGroup::new(data),
        }
    }

    /// Returns an iterator over the nodes in this group.
    pub fn nodes(&self) -> GroupNodeIter<'a> {
        GroupNodeIter {
            block: self.block,
            iter: self.group.nodes(),
        }
    }

    /// Returns an iterator over the dense nodes in this group.
    pub fn dense_nodes(&self) -> DenseNodeIter<'a> {
        match self.group.dense() {
            Ok(Some(data)) => match WireDenseNodes::parse(data) {
                Ok(dense) => DenseNodeIter::new(self.block, dense),
                Err(_) => DenseNodeIter::empty(self.block),
            },
            _ => DenseNodeIter::empty(self.block),
        }
    }

    /// Returns an iterator over the ways in this group.
    pub fn ways(&self) -> GroupWayIter<'a> {
        GroupWayIter {
            block: self.block,
            iter: self.group.ways(),
        }
    }

    /// Returns an iterator over the relations in this group.
    pub fn relations(&self) -> GroupRelationIter<'a> {
        GroupRelationIter {
            block: self.block,
            iter: self.group.relations(),
        }
    }
}

/// An iterator over the elements in a [`PrimitiveGroup`].
pub struct BlockElementsIter<'a> {
    block: &'a WireBlock<'static>,
    state: ElementsIterState,
    group_index: usize,
    group_count: usize,
    dense_nodes: DenseNodeIter<'a>,
    nodes: WireMessageIter<'a>,
    ways: WireMessageIter<'a>,
    relations: WireMessageIter<'a>,
}

#[derive(Copy, Clone, Debug)]
enum ElementsIterState {
    Group,
    DenseNode,
    Node,
    Way,
    Relation,
}

impl<'a> BlockElementsIter<'a> {
    fn new(block: &'a WireBlock<'static>) -> BlockElementsIter<'a> {
        BlockElementsIter {
            block,
            state: ElementsIterState::Group,
            group_index: 0,
            group_count: block.group_ranges.len(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: WireMessageIter::empty(),
            ways: WireMessageIter::empty(),
            relations: WireMessageIter::empty(),
        }
    }

    /// Performs an internal iteration step. Returns [`None`] until there is a value for the iterator to
    /// return. Returns [`Some(None)`] to end the iteration.
    #[inline]
    #[allow(clippy::option_option)]
    fn step(&mut self) -> Option<Option<Element<'a>>> {
        match self.state {
            ElementsIterState::Group => {
                if self.group_index >= self.group_count {
                    return Some(None);
                }
                let group_data = self.block.group(self.group_index);
                self.group_index += 1;
                self.state = ElementsIterState::DenseNode;
                let group = WireGroup::new(group_data);
                self.dense_nodes = match group.dense() {
                    Ok(Some(data)) => match WireDenseNodes::parse(data) {
                        Ok(dense) => DenseNodeIter::new(self.block, dense),
                        Err(_) => DenseNodeIter::empty(self.block),
                    },
                    _ => DenseNodeIter::empty(self.block),
                };
                self.nodes = group.nodes();
                self.ways = group.ways();
                self.relations = group.relations();
                None
            }
            ElementsIterState::DenseNode => match self.dense_nodes.next() {
                Some(dense_node) => Some(Some(Element::DenseNode(dense_node))),
                None => {
                    self.state = ElementsIterState::Node;
                    None
                }
            },
            ElementsIterState::Node => {
                while let Some(data) = self.nodes.next() {
                    if let Ok(wire_node) = WireNode::parse(data) {
                        return Some(Some(Element::Node(Node::new(self.block, wire_node))));
                    }
                }
                self.state = ElementsIterState::Way;
                None
            }
            ElementsIterState::Way => {
                while let Some(data) = self.ways.next() {
                    if let Ok(wire_way) = WireWay::parse(data) {
                        return Some(Some(Element::Way(Way::new(self.block, wire_way))));
                    }
                }
                self.state = ElementsIterState::Relation;
                None
            }
            ElementsIterState::Relation => {
                while let Some(data) = self.relations.next() {
                    if let Ok(wire_rel) = WireRelation::parse(data) {
                        return Some(Some(Element::Relation(Relation::new(
                            self.block, wire_rel,
                        ))));
                    }
                }
                self.state = ElementsIterState::Group;
                None
            }
        }
    }
}

impl<'a> Iterator for BlockElementsIter<'a> {
    type Item = Element<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(element) = self.step() {
                return element;
            }
        }
    }
}

/// An iterator over the groups in a [`PrimitiveBlock`].
pub struct GroupIter<'a> {
    block: &'a WireBlock<'static>,
    index: usize,
    count: usize,
}

impl<'a> GroupIter<'a> {
    fn new(block: &'a WireBlock<'static>) -> GroupIter<'a> {
        GroupIter {
            block,
            index: 0,
            count: block.group_ranges.len(),
        }
    }
}

impl<'a> Iterator for GroupIter<'a> {
    type Item = PrimitiveGroup<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }
        let data = self.block.group(self.index);
        self.index += 1;
        Some(PrimitiveGroup::new(self.block, data))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for GroupIter<'_> {}

/// An iterator over the nodes in a [`PrimitiveGroup`].
pub struct GroupNodeIter<'a> {
    block: &'a WireBlock<'static>,
    iter: WireMessageIter<'a>,
}

impl<'a> Iterator for GroupNodeIter<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let data = self.iter.next()?;
            if let Ok(wire_node) = WireNode::parse(data) {
                return Some(Node::new(self.block, wire_node));
            }
            // Skip malformed nodes
        }
    }
}

/// An iterator over the ways in a [`PrimitiveGroup`].
pub struct GroupWayIter<'a> {
    block: &'a WireBlock<'static>,
    iter: WireMessageIter<'a>,
}

impl<'a> Iterator for GroupWayIter<'a> {
    type Item = Way<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let data = self.iter.next()?;
            if let Ok(wire_way) = WireWay::parse(data) {
                return Some(Way::new(self.block, wire_way));
            }
        }
    }
}

/// An iterator over the relations in a [`PrimitiveGroup`].
pub struct GroupRelationIter<'a> {
    block: &'a WireBlock<'static>,
    iter: WireMessageIter<'a>,
}

impl<'a> Iterator for GroupRelationIter<'a> {
    type Item = Relation<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let data = self.iter.next()?;
            if let Ok(wire_rel) = WireRelation::parse(data) {
                return Some(Relation::new(self.block, wire_rel));
            }
        }
    }
}

/// Look up a stringtable entry by index, returning it as a `&str`.
///
/// Uses `from_utf8_unchecked` — the safety relies on the invariant established
/// by `PrimitiveBlock::new()`, which validates every stringtable entry at
/// construction time.
pub(crate) fn str_from_stringtable<'a>(block: &'a WireBlock<'_>, index: usize) -> Result<&'a str> {
    if let Some(bytes) = block.stringtable.get(index) {
        // SAFETY: All stringtable entries were validated as UTF-8 in
        // PrimitiveBlock::new(). The PrimitiveBlock struct does not expose any
        // mutable access to the underlying buffer, so entries cannot have been
        // modified since construction.
        Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
    } else {
        Err(new_error(ErrorKind::StringtableIndexOutOfBounds { index }))
    }
}

/// Construct a key-value tuple from key/value indexes, using the stringtable from a block.
pub(crate) fn get_stringtable_key_value<'a>(
    block: &'a WireBlock<'_>,
    key_index: Option<usize>,
    value_index: Option<usize>,
) -> Option<(&'a str, &'a str)> {
    match (key_index, value_index) {
        (Some(key_index), Some(val_index)) => {
            let k_res = str_from_stringtable(block, key_index);
            let v_res = str_from_stringtable(block, val_index);
            if let (Ok(k), Ok(v)) = (k_res, v_res) {
                Some((k, v))
            } else {
                None
            }
        }
        _ => None,
    }
}
