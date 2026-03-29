//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use super::dense::DenseNodeIter;
use super::elements::{Element, Node, Relation, Way};
use super::wire::{
    WireBlock, WireDenseNodes, WireGroup, WireMessageIter, WireNode, WireRelation, WireWay,
};
use crate::error::{new_error, new_wire_error, ErrorKind, Result};
use bytes::Bytes;
use std;

// ---------------------------------------------------------------------------
// Wire-format protobuf message types for header parsing
// ---------------------------------------------------------------------------

/// Parsed HeaderBBox from a PBF header.
#[derive(Clone, Debug)]
pub(crate) struct WireHeaderBBox {
    pub left: i64,
    pub right: i64,
    pub top: i64,
    pub bottom: i64,
}

impl WireHeaderBBox {
    fn parse(data: &[u8]) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(data);
        let mut left: i64 = 0;
        let mut right: i64 = 0;
        let mut top: i64 = 0;
        let mut bottom: i64 = 0;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match field {
                1 => left = cursor.read_sint64()?,
                2 => right = cursor.read_sint64()?,
                3 => top = cursor.read_sint64()?,
                4 => bottom = cursor.read_sint64()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireHeaderBBox { left, right, top, bottom })
    }
}

/// Parsed HeaderBlock from protobuf wire format.
#[derive(Clone, Debug)]
pub(crate) struct WireHeaderBlock {
    pub bbox: Option<WireHeaderBBox>,
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub writingprogram: Option<String>,
    pub source: Option<String>,
    pub osmosis_replication_timestamp: Option<i64>,
    pub osmosis_replication_sequence_number: Option<i64>,
    pub osmosis_replication_base_url: Option<String>,
}

impl WireHeaderBlock {
    /// Parse a HeaderBlock from decompressed protobuf bytes.
    pub fn parse(data: &[u8]) -> Result<Self> {
        use super::wire::Cursor;
        let mut cursor = Cursor::new(data);
        let mut bbox: Option<WireHeaderBBox> = None;
        let mut required_features: Vec<String> = Vec::new();
        let mut optional_features: Vec<String> = Vec::new();
        let mut writingprogram: Option<String> = None;
        let mut source: Option<String> = None;
        let mut osmosis_replication_timestamp: Option<i64> = None;
        let mut osmosis_replication_sequence_number: Option<i64> = None;
        let mut osmosis_replication_base_url: Option<String> = None;

        while let Some((field, wire_type)) = cursor.read_tag()? {
            match field {
                1 => {
                    // bbox: HeaderBBox submessage
                    let sub_data = cursor.read_len_delimited()?;
                    bbox = Some(WireHeaderBBox::parse(sub_data)?);
                }
                4 => {
                    // required_features: repeated string
                    let bytes = cursor.read_len_delimited()?;
                    let s = String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in required_features"))?;
                    required_features.push(s);
                }
                5 => {
                    // optional_features: repeated string
                    let bytes = cursor.read_len_delimited()?;
                    let s = String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in optional_features"))?;
                    optional_features.push(s);
                }
                16 => {
                    // writingprogram: string
                    let bytes = cursor.read_len_delimited()?;
                    writingprogram = Some(String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in writingprogram"))?);
                }
                17 => {
                    // source: string
                    let bytes = cursor.read_len_delimited()?;
                    source = Some(String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in source"))?);
                }
                32 => {
                    // osmosis_replication_timestamp: int64
                    osmosis_replication_timestamp = Some(cursor.read_varint_i64()?);
                }
                33 => {
                    // osmosis_replication_sequence_number: int64
                    osmosis_replication_sequence_number = Some(cursor.read_varint_i64()?);
                }
                34 => {
                    // osmosis_replication_base_url: string
                    let bytes = cursor.read_len_delimited()?;
                    osmosis_replication_base_url = Some(String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in replication_base_url"))?);
                }
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireHeaderBlock {
            bbox,
            required_features,
            optional_features,
            writingprogram,
            source,
            osmosis_replication_timestamp,
            osmosis_replication_sequence_number,
            osmosis_replication_base_url,
        })
    }
}

/// A `HeaderBlock`. It contains metadata about following [`PrimitiveBlock`]s.
#[derive(Clone, Debug)]
pub struct HeaderBlock {
    header: WireHeaderBlock,
}

impl HeaderBlock {
    pub(crate) fn new(header: WireHeaderBlock) -> HeaderBlock {
        HeaderBlock { header }
    }

    /// Parse a HeaderBlock from decompressed protobuf bytes.
    pub(crate) fn parse_from_bytes(data: &[u8]) -> Result<HeaderBlock> {
        WireHeaderBlock::parse(data).map(HeaderBlock::new)
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

    /// PBF optional feature string indicating entities are sorted by type then ID.
    pub const SORT_TYPE_THEN_ID: &str = "Sort.Type_then_ID";

    /// Returns `true` if the header declares `Sort.Type_then_ID`.
    pub fn is_sorted(&self) -> bool {
        self.header
            .optional_features
            .iter()
            .any(|f| f == Self::SORT_TYPE_THEN_ID)
    }

    /// PBF optional feature string indicating ways contain inline node coordinates.
    pub const LOCATIONS_ON_WAYS: &str = "LocationsOnWays";

    /// PBF required feature string indicating history metadata (`visible`) may
    /// be present on elements.
    pub const HISTORICAL_INFORMATION: &str = "HistoricalInformation";

    /// Returns `true` if the header declares `LocationsOnWays`.
    pub fn has_locations_on_ways(&self) -> bool {
        self.header
            .optional_features
            .iter()
            .any(|f| f == Self::LOCATIONS_ON_WAYS)
    }

    /// Returns `true` if the header declares `HistoricalInformation` as a
    /// required feature.
    pub fn has_historical_information(&self) -> bool {
        self.header
            .required_features
            .iter()
            .any(|f| f == Self::HISTORICAL_INFORMATION)
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

/// The element type(s) contained in a [`PrimitiveBlock`].
///
/// In well-formed sorted PBFs ([`Sort.Type_then_ID`](HeaderBlock::is_sorted)),
/// each block contains exactly one type. Unsorted or hand-crafted PBFs may
/// produce [`Mixed`](BlockType::Mixed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockType {
    /// Block contains dense-encoded nodes (the common node encoding).
    DenseNodes,
    /// Block contains individually-encoded nodes (rare legacy format).
    Nodes,
    /// Block contains ways.
    Ways,
    /// Block contains relations.
    Relations,
    /// Block contains multiple element types across its groups.
    Mixed,
    /// Block contains no groups (empty block).
    Empty,
}

impl BlockType {
    /// Returns `true` if this block contains nodes (dense or non-dense).
    pub fn is_nodes(&self) -> bool {
        matches!(self, Self::DenseNodes | Self::Nodes)
    }

    /// Returns `true` if this block contains ways.
    pub fn is_ways(&self) -> bool {
        matches!(self, Self::Ways)
    }

    /// Returns `true` if this block contains relations.
    pub fn is_relations(&self) -> bool {
        matches!(self, Self::Relations)
    }
}

/// Classify a `PrimitiveGroup` by reading its first wire tag byte.
///
/// PrimitiveGroup field tags (all LEN-delimited, wire type 2):
///   field 1 = Node (0x0A), field 2 = DenseNodes (0x12),
///   field 3 = Way (0x1A), field 4 = Relation (0x22).
///
/// Cost: one byte read per group. No element parsing.
fn classify_group(data: &[u8]) -> BlockType {
    if data.is_empty() {
        return BlockType::Empty;
    }
    let tag_byte = data[0];
    let field = tag_byte >> 3;
    let wire_type = tag_byte & 0x07;
    if wire_type != 2 {
        return BlockType::Mixed; // unexpected wire type
    }
    match field {
        1 => BlockType::Nodes,
        2 => BlockType::DenseNodes,
        3 => BlockType::Ways,
        4 => BlockType::Relations,
        _ => BlockType::Mixed, // changeset (5) or unknown
    }
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
            .field("groups", &self.block.group_count())
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
        // Use the inline path: copy buffer to Vec, append string table entries
        // and group ranges inline. Avoids separate Box allocations. The ~2 MB
        // copy is acceptable for the public API (not at pipeline scale — the
        // pipeline uses from_vec / from_vec_pooled directly).
        Self::from_vec(buffer.to_vec())
    }

    /// Parse from a mutable Vec, inlining string table entries and group ranges
    /// into the buffer itself. Zero separate heap allocations beyond the buffer.
    ///
    /// This eliminates the cross-thread `Box<[(u32, u32)]>` retention that caused
    /// 25+ GB OOM at Europe scale (520K blocks) with the pipelined reader.
    /// The temp Vecs during parsing are allocated and freed on the calling thread.
    ///
    /// The buffer is extended with inline entry data. After conversion to `Bytes`,
    /// the block references both the protobuf data and the appended entries.
    pub(crate) fn from_vec(mut buffer: Vec<u8>) -> Result<PrimitiveBlock> {
        let meta = WireBlock::parse_and_inline(&mut buffer)?;
        let bytes = Bytes::from(buffer);
        let data: &[u8] = &bytes;
        let block = WireBlock::from_inline(data, &meta);

        // Validate every stringtable entry once at construction time.
        for index in 0..block.stringtable.len() {
            if let Some(raw) = block.stringtable.get(index) {
                std::str::from_utf8(raw)
                    .map_err(|err| new_error(ErrorKind::StringtableUtf8 { err, index }))?;
            }
        }

        #[allow(clippy::transmute_undefined_repr)]
        let block =
            unsafe { std::mem::transmute::<WireBlock<'_>, WireBlock<'static>>(block) };

        Ok(PrimitiveBlock { buffer: bytes, block })
    }

    /// Like [`from_vec`] but wraps the buffer with pool recycling.
    /// On drop, the Vec returns to the DecompressPool instead of being freed.
    /// This eliminates cross-thread Vec retention in the pipelined reader.
    pub(crate) fn from_vec_pooled(
        mut buffer: Vec<u8>,
        pool: &std::sync::Arc<crate::blob::DecompressPool>,
    ) -> Result<PrimitiveBlock> {
        let meta = WireBlock::parse_and_inline(&mut buffer)?;
        let bytes = crate::blob::pool_wrap(buffer, Some(pool));
        let data: &[u8] = &bytes;
        let block = WireBlock::from_inline(data, &meta);

        for index in 0..block.stringtable.len() {
            if let Some(raw) = block.stringtable.get(index) {
                std::str::from_utf8(raw)
                    .map_err(|err| new_error(ErrorKind::StringtableUtf8 { err, index }))?;
            }
        }

        #[allow(clippy::transmute_undefined_repr)]
        let block =
            unsafe { std::mem::transmute::<WireBlock<'_>, WireBlock<'static>>(block) };

        Ok(PrimitiveBlock { buffer: bytes, block })
    }

    /// Returns the size of the decompressed protobuf payload in bytes.
    ///
    /// This is the raw decompressed data backing this block — useful for
    /// byte-budget accounting in batched processing pipelines. When inline
    /// entries are used, returns the original protobuf size (not the extended
    /// buffer).
    pub fn decompressed_size(&self) -> usize {
        self.block.proto_len as usize
    }

    /// Returns the element type contained in this block.
    ///
    /// Inspects only the first protobuf field tag of each group — typically a
    /// single byte read per group. No elements are decoded.
    ///
    /// In sorted PBFs, each block is single-type, so this returns one of
    /// [`DenseNodes`](BlockType::DenseNodes), [`Nodes`](BlockType::Nodes),
    /// [`Ways`](BlockType::Ways), or [`Relations`](BlockType::Relations).
    /// For unsorted files where groups contain different types, returns
    /// [`Mixed`](BlockType::Mixed).
    pub fn block_type(&self) -> BlockType {
        let mut result: Option<BlockType> = None;
        for i in 0..self.block.group_count() {
            let group_data = self.block.group(i);
            let group_type = classify_group(group_data);
            match result {
                None => result = Some(group_type),
                Some(prev) if prev == group_type => {} // same type, continue
                Some(_) => return BlockType::Mixed,
            }
        }
        result.unwrap_or(BlockType::Empty)
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`.
    // wontfix(name-iter-convention): elements() is more descriptive than iter() here;
    // BlockElementsIter name matches established osmpbf public API.
    pub fn elements(&self) -> BlockElementsIter<'_> {
        BlockElementsIter::new(&self.block)
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`,
    /// skipping metadata (version, timestamp, changeset, uid, user) for
    /// dense nodes. Use this for scan-only passes that only need IDs,
    /// coordinates, refs, and tags.
    pub fn elements_skip_metadata(&self) -> BlockElementsIter<'_> {
        BlockElementsIter::new_skip_metadata(&self.block)
    }

    /// Returns the raw protobuf bytes of a PrimitiveGroup by index.
    /// Used by raw group passthrough to copy all-match groups without re-encoding.
    pub(crate) fn raw_group_bytes(&self, index: usize) -> &[u8] {
        self.block.group(index)
    }

    /// Returns the number of PrimitiveGroups in this block.
    pub(crate) fn group_count(&self) -> usize {
        self.block.group_count()
    }

    /// Returns the raw StringTable protobuf bytes (field 1 of PrimitiveBlock).
    /// Used by raw group passthrough to copy the string table into output blocks.
    pub(crate) fn raw_stringtable_bytes(&self) -> &[u8] {
        self.block.raw_stringtable()
    }

    /// Returns the scalar fields needed to reconstruct a PrimitiveBlock frame.
    pub(crate) fn block_scalars(&self) -> (i32, i64, i64, i32) {
        (self.block.granularity, self.block.lat_offset, self.block.lon_offset, self.block.date_granularity)
    }

    /// Returns the number of entries in this block's string table.
    pub fn string_table_len(&self) -> usize {
        self.block.stringtable.len()
    }

    /// Returns the string at the given string table index, or `None` if out of bounds.
    ///
    /// Index 0 is always the empty string. Entries were validated as UTF-8 at
    /// construction time.
    pub fn string_table_entry(&self, index: usize) -> Option<&str> {
        self.block.stringtable.get(index).map(|bytes| {
            // SAFETY: All stringtable entries were validated as UTF-8 in
            // PrimitiveBlock::new(). The PrimitiveBlock struct does not expose any
            // mutable access to the underlying buffer.
            unsafe { std::str::from_utf8_unchecked(bytes) }
        })
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
    skip_metadata: bool,
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
            group_count: block.group_count(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: WireMessageIter::empty(),
            ways: WireMessageIter::empty(),
            relations: WireMessageIter::empty(),
            skip_metadata: false,
        }
    }

    fn new_skip_metadata(block: &'a WireBlock<'static>) -> BlockElementsIter<'a> {
        BlockElementsIter {
            block,
            state: ElementsIterState::Group,
            group_index: 0,
            group_count: block.group_count(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: WireMessageIter::empty(),
            ways: WireMessageIter::empty(),
            relations: WireMessageIter::empty(),
            skip_metadata: true,
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
                        Ok(dense) => if self.skip_metadata {
                            DenseNodeIter::new_skip_metadata(self.block, dense)
                        } else {
                            DenseNodeIter::new(self.block, dense)
                        },
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
                for data in self.nodes.by_ref() {
                    if let Ok(wire_node) = WireNode::parse(data) {
                        return Some(Some(Element::Node(Node::new(self.block, wire_node))));
                    }
                }
                self.state = ElementsIterState::Way;
                None
            }
            ElementsIterState::Way => {
                for data in self.ways.by_ref() {
                    if let Ok(wire_way) = WireWay::parse(data) {
                        return Some(Some(Element::Way(Way::new(self.block, wire_way))));
                    }
                }
                self.state = ElementsIterState::Relation;
                None
            }
            ElementsIterState::Relation => {
                for data in self.relations.by_ref() {
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
            count: block.group_count(),
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
