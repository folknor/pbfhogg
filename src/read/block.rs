//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use super::dense::DenseNodeIter;
use super::elements::{Element, Node, Relation, Way};
use crate::error::{new_error, ErrorKind, Result};
use crate::proto::osmformat;
use std;

/// A `HeaderBlock`. It contains metadata about following [`PrimitiveBlock`]s.
#[derive(Clone, Debug)]
pub struct HeaderBlock {
    header: osmformat::HeaderBlock,
}

impl HeaderBlock {
    pub fn new(header: osmformat::HeaderBlock) -> HeaderBlock {
        HeaderBlock { header }
    }

    /// Returns the (optional) bounding box of the included features.
    #[allow(clippy::cast_precision_loss)]
    pub fn bbox(&self) -> Option<HeaderBBox> {
        self.header.bbox.as_ref().map(|bbox| HeaderBBox {
            left: (bbox.left() as f64) * 1.0e-9,
            right: (bbox.right() as f64) * 1.0e-9,
            top: (bbox.top() as f64) * 1.0e-9,
            bottom: (bbox.bottom() as f64) * 1.0e-9,
        })
    }

    /// Returns a list of required features that a parser needs to implement to parse the following
    /// [`PrimitiveBlock`]s.
    pub fn required_features(&self) -> &[protobuf::Chars] {
        self.header.required_features.as_slice()
    }

    /// Returns a list of optional features that a parser can choose to ignore.
    pub fn optional_features(&self) -> &[protobuf::Chars] {
        self.header.optional_features.as_slice()
    }

    /// Returns the name of the program that generated the file or `None` if unset.
    pub fn writing_program(&self) -> Option<&str> {
        if self.header.has_writingprogram() {
            Some(self.header.writingprogram())
        } else {
            None
        }
    }

    /// Returns the source of the `bbox` field or `None` if unset.
    pub fn source(&self) -> Option<&str> {
        if self.header.has_source() {
            Some(self.header.source())
        } else {
            None
        }
    }

    /// Returns the replication timestamp of the file, or `None` if unset.
    /// The timestamp is expressed in seconds since the UNIX epoch.
    pub fn osmosis_replication_timestamp(&self) -> Option<i64> {
        if self.header.has_osmosis_replication_timestamp() {
            Some(self.header.osmosis_replication_timestamp())
        } else {
            None
        }
    }

    /// Returns the replication sequence number of the file, or `None` if unset.
    pub fn osmosis_replication_sequence_number(&self) -> Option<i64> {
        if self.header.has_osmosis_replication_sequence_number() {
            Some(self.header.osmosis_replication_sequence_number())
        } else {
            None
        }
    }

    /// Returns the replication base URL of the file, or `None` if unset.
    pub fn osmosis_replication_base_url(&self) -> Option<&str> {
        if self.header.has_osmosis_replication_base_url() {
            Some(self.header.osmosis_replication_base_url())
        } else {
            None
        }
    }
}

/// A bounding box that is usually included in a [`HeaderBlock`].
/// The maximum precision of the coordinates is one nanodegree (10⁻⁹).
#[derive(Clone, Debug)]
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
/// # Why there is no "scan" or "ID-only" parse mode
///
/// It may seem tempting to add a lightweight parse path that extracts only element
/// IDs (skipping stringtable, tags, coordinates, refs, metadata) for consumers like
/// `IndexedReader::update_element_id_ranges()`. Investigation found this is probably
/// not worth the complexity:
///
/// - **Decompression dominates:** zlib/zstd decompression of the blob is ~60% of total
///   read time and is unavoidable (compression covers the entire serialized block).
///   Even skipping ALL protobuf parsing only saves ~35-40% of the remaining ~40%.
/// - **Few consumers:** Only `IndexedReader` index-building is truly ID-only (one-time
///   pass, not a hot path). `check_refs` needs way refs and relation members too.
/// - **Maintenance cost:** A custom wire-format parser for PrimitiveBlock, PrimitiveGroup,
///   DenseNodes, Way, and Relation (~200-400 lines) must stay in sync with the proto
///   schema — two parallel parse paths is a classic source of subtle bugs.
///
/// A potentially useful variant is a **selective parse for check_refs** that skips
/// stringtable + tags + coordinates + metadata but keeps IDs + way refs + relation
/// members. This has not been benchmarked yet. See TODO.md for details.
///
/// # Stringtable UTF-8 invariant
///
/// At construction time (`new()`), every entry in the block's stringtable is validated
/// with `std::str::from_utf8()`. This means all subsequent stringtable lookups
/// (`str_from_stringtable()`) can use `from_utf8_unchecked` — eliminating 16-48K
/// redundant UTF-8 validations per block (8000 elements × 2-6 tag lookups each).
///
/// The alternative of storing `Vec<&str>` alongside the block was considered but
/// rejected because it would require a self-referential struct or `unsafe` lifetime
/// tricks: the `&str` slices would borrow from `block.stringtable.s` while `block`
/// itself is owned by the same struct. The validation-at-construction approach achieves
/// the same performance benefit (validate once, use many) without any lifetime complexity.
#[derive(Clone, Debug)]
pub struct PrimitiveBlock {
    block: osmformat::PrimitiveBlock,
}

impl PrimitiveBlock {
    /// Parse a `PrimitiveBlock` from its protobuf representation.
    ///
    /// Validates every entry in the stringtable as UTF-8. This up-front validation
    /// allows all later stringtable lookups to skip per-access UTF-8 checks, which
    /// eliminates 16-48K redundant `std::str::from_utf8()` calls per block (a typical
    /// block has up to 8000 elements, each with 2-6 tag key/value lookups into the
    /// stringtable).
    ///
    /// # Errors
    ///
    /// Returns `ErrorKind::StringtableUtf8` if any stringtable entry contains invalid
    /// UTF-8 bytes.
    pub fn new(block: osmformat::PrimitiveBlock) -> Result<PrimitiveBlock> {
        // Validate every stringtable entry once at construction time.
        // This establishes the invariant that all entries are valid UTF-8,
        // which str_from_stringtable() relies on for its unsafe from_utf8_unchecked.
        for (index, entry) in block.stringtable.s.iter().enumerate() {
            std::str::from_utf8(entry)
                .map_err(|err| new_error(ErrorKind::StringtableUtf8 { err, index }))?;
        }
        Ok(PrimitiveBlock { block })
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`.
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

    /// Returns the raw stringtable. Elements in a `PrimitiveBlock` do not store strings
    /// themselves; instead, they just store indices to the stringtable.
    ///
    /// All entries are guaranteed to be valid UTF-8: this is checked at construction time
    /// in `PrimitiveBlock::new()`, which rejects blocks with invalid stringtable entries.
    pub fn raw_stringtable(&self) -> &[bytes::Bytes] {
        self.block.stringtable.s.as_slice()
    }
}

/// A `PrimitiveGroup` contains a sequence of elements of one type.
#[derive(Clone, Debug)]
pub struct PrimitiveGroup<'a> {
    block: &'a osmformat::PrimitiveBlock,
    group: &'a osmformat::PrimitiveGroup,
}

impl<'a> PrimitiveGroup<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        group: &'a osmformat::PrimitiveGroup,
    ) -> PrimitiveGroup<'a> {
        PrimitiveGroup { block, group }
    }

    /// Returns an iterator over the nodes in this group.
    pub fn nodes(&self) -> GroupNodeIter<'a> {
        GroupNodeIter::new(self.block, self.group)
    }

    /// Returns an iterator over the dense nodes in this group.
    pub fn dense_nodes(&self) -> DenseNodeIter<'a> {
        DenseNodeIter::new(self.block, self.group.dense.get_or_default())
    }

    /// Returns an iterator over the ways in this group.
    pub fn ways(&self) -> GroupWayIter<'a> {
        GroupWayIter::new(self.block, self.group)
    }

    /// Returns an iterator over the relations in this group.
    pub fn relations(&self) -> GroupRelationIter<'a> {
        GroupRelationIter::new(self.block, self.group)
    }
}

/// An iterator over the elements in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct BlockElementsIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    state: ElementsIterState,
    groups: std::slice::Iter<'a, osmformat::PrimitiveGroup>,
    dense_nodes: DenseNodeIter<'a>,
    nodes: std::slice::Iter<'a, osmformat::Node>,
    ways: std::slice::Iter<'a, osmformat::Way>,
    relations: std::slice::Iter<'a, osmformat::Relation>,
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
    fn new(block: &'a osmformat::PrimitiveBlock) -> BlockElementsIter<'a> {
        BlockElementsIter {
            block,
            state: ElementsIterState::Group,
            groups: block.primitivegroup.iter(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: [].iter(),
            ways: [].iter(),
            relations: [].iter(),
        }
    }

    /// Performs an internal iteration step. Returns [`None`] until there is a value for the iterator to
    /// return. Returns [`Some(None)`] to end the iteration.
    #[inline]
    #[allow(clippy::option_option)]
    fn step(&mut self) -> Option<Option<Element<'a>>> {
        match self.state {
            ElementsIterState::Group => match self.groups.next() {
                Some(group) => {
                    self.state = ElementsIterState::DenseNode;
                    self.dense_nodes = DenseNodeIter::new(self.block, group.dense.get_or_default());
                    self.nodes = group.nodes.iter();
                    self.ways = group.ways.iter();
                    self.relations = group.relations.iter();
                    None
                }
                None => Some(None),
            },
            ElementsIterState::DenseNode => match self.dense_nodes.next() {
                Some(dense_node) => Some(Some(Element::DenseNode(dense_node))),
                None => {
                    self.state = ElementsIterState::Node;
                    None
                }
            },
            ElementsIterState::Node => match self.nodes.next() {
                Some(node) => Some(Some(Element::Node(Node::new(self.block, node)))),
                None => {
                    self.state = ElementsIterState::Way;
                    None
                }
            },
            ElementsIterState::Way => match self.ways.next() {
                Some(way) => Some(Some(Element::Way(Way::new(self.block, way)))),
                None => {
                    self.state = ElementsIterState::Relation;
                    None
                }
            },
            ElementsIterState::Relation => match self.relations.next() {
                Some(rel) => Some(Some(Element::Relation(Relation::new(self.block, rel)))),
                None => {
                    self.state = ElementsIterState::Group;
                    None
                }
            },
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
#[derive(Clone, Debug)]
pub struct GroupIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    groups: std::slice::Iter<'a, osmformat::PrimitiveGroup>,
}

impl<'a> GroupIter<'a> {
    fn new(block: &'a osmformat::PrimitiveBlock) -> GroupIter<'a> {
        GroupIter {
            block,
            groups: block.primitivegroup.iter(),
        }
    }
}

impl<'a> Iterator for GroupIter<'a> {
    type Item = PrimitiveGroup<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.groups.next().map(|g| PrimitiveGroup::new(self.block, g))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.groups.size_hint()
    }
}

impl ExactSizeIterator for GroupIter<'_> {}

/// An iterator over the nodes in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupNodeIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    nodes: std::slice::Iter<'a, osmformat::Node>,
}

impl<'a> GroupNodeIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        group: &'a osmformat::PrimitiveGroup,
    ) -> GroupNodeIter<'a> {
        GroupNodeIter {
            block,
            nodes: group.nodes.iter(),
        }
    }
}

impl<'a> Iterator for GroupNodeIter<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.nodes.next().map(|n| Node::new(self.block, n))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.nodes.size_hint()
    }
}

impl ExactSizeIterator for GroupNodeIter<'_> {}

/// An iterator over the ways in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupWayIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    ways: std::slice::Iter<'a, osmformat::Way>,
}

impl<'a> GroupWayIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        group: &'a osmformat::PrimitiveGroup,
    ) -> GroupWayIter<'a> {
        GroupWayIter {
            block,
            ways: group.ways.iter(),
        }
    }
}

impl<'a> Iterator for GroupWayIter<'a> {
    type Item = Way<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.ways.next().map(|way| Way::new(self.block, way))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ways.size_hint()
    }
}

impl ExactSizeIterator for GroupWayIter<'_> {}

/// An iterator over the relations in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupRelationIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    rels: std::slice::Iter<'a, osmformat::Relation>,
}

impl<'a> GroupRelationIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        group: &'a osmformat::PrimitiveGroup,
    ) -> GroupRelationIter<'a> {
        GroupRelationIter {
            block,
            rels: group.relations.iter(),
        }
    }
}

impl<'a> Iterator for GroupRelationIter<'a> {
    type Item = Relation<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rels.next().map(|rel| Relation::new(self.block, rel))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rels.size_hint()
    }
}

impl ExactSizeIterator for GroupRelationIter<'_> {}

/// Look up a stringtable entry by index, returning it as a `&str`.
///
/// # Performance
///
/// This function uses `from_utf8_unchecked` instead of `from_utf8`, avoiding a
/// per-call O(n) UTF-8 validation scan. For a typical PBF block with 8000 elements
/// and 2-3 tags each, this eliminates 32-48K redundant validations per block.
///
/// The safety of this relies on the invariant established by `PrimitiveBlock::new()`,
/// which validates every stringtable entry at construction time. Since `PrimitiveBlock`
/// has no API that allows mutating the stringtable after construction, the invariant
/// holds for the lifetime of the block.
pub(crate) fn str_from_stringtable(
    block: &osmformat::PrimitiveBlock,
    index: usize,
) -> Result<&str> {
    if let Some(bytes) = block.stringtable.s.get(index) {
        // SAFETY: All stringtable entries were validated as UTF-8 in
        // PrimitiveBlock::new(). The PrimitiveBlock struct does not expose any
        // mutable access to the underlying osmformat::PrimitiveBlock, so the
        // stringtable entries cannot have been modified since construction.
        Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
    } else {
        Err(new_error(ErrorKind::StringtableIndexOutOfBounds { index }))
    }
}

/// Construct a key-value tuple from key/value indexes, using the stringtable from a block.
pub(crate) fn get_stringtable_key_value(
    block: &osmformat::PrimitiveBlock,
    key_index: Option<usize>,
    value_index: Option<usize>,
) -> Option<(&str, &str)> {
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
