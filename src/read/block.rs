//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use super::dense::DenseNodeIter;
use super::elements::{Element, Node, Relation, Way};
use super::wire::{
    WireBlock, WireBlockMeta, WireDenseNodes, WireGroup, WireMessageIter, WireNode, WireRelation,
    WireWay,
};
use crate::error::{ErrorKind, Result, new_error, new_wire_error};
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
            match (field, wire_type) {
                (1, 0) => left = cursor.read_sint64()?,
                (2, 0) => right = cursor.read_sint64()?,
                (3, 0) => top = cursor.read_sint64()?,
                (4, 0) => bottom = cursor.read_sint64()?,
                _ => cursor.skip_field(wire_type)?,
            }
        }

        Ok(WireHeaderBBox {
            left,
            right,
            top,
            bottom,
        })
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
                    writingprogram = Some(
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| new_wire_error("invalid UTF-8 in writingprogram"))?,
                    );
                }
                17 => {
                    // source: string
                    let bytes = cursor.read_len_delimited()?;
                    source = Some(
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| new_wire_error("invalid UTF-8 in source"))?,
                    );
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
                    osmosis_replication_base_url =
                        Some(String::from_utf8(bytes.to_vec()).map_err(|_| {
                            new_wire_error("invalid UTF-8 in replication_base_url")
                        })?);
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

    /// Optional feature declaring BlobHeader field-5 way-member bitmaps.
    pub const WAY_MEMBERS_V1: &str = "pbfhogg.WayMembers-v1";

    /// Optional feature declaring Way field-20 shared-node pin bitmaps.
    pub const SHARED_NODE_PINS_V1: &str = "pbfhogg.SharedNodePins-v1";

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

    /// Returns `true` if the header declares `pbfhogg.WayMembers-v1`.
    pub fn has_way_members_v1(&self) -> bool {
        self.header
            .optional_features
            .iter()
            .any(|f| f == Self::WAY_MEMBERS_V1)
    }

    /// Returns `true` if the header declares `pbfhogg.SharedNodePins-v1`.
    pub fn has_shared_node_pins_v1(&self) -> bool {
        self.header
            .optional_features
            .iter()
            .any(|f| f == Self::SHARED_NODE_PINS_V1)
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
/// offset/length pairs - no `Vec<i64>` or `Vec<Bytes>` for packed fields.
/// Element iteration decodes packed varints on-the-fly from the buffer.
///
/// # Stringtable UTF-8 invariant
///
/// At construction time (`new()`), every entry in the block's stringtable is validated
/// with `std::str::from_utf8()`. This means all subsequent stringtable lookups
/// (`str_from_stringtable()`) can use `from_utf8_unchecked` - eliminating 16-48K
/// redundant UTF-8 validations per block (8000 elements × 2-6 tag lookups each).
///
/// # Why `PrimitiveBlock` does not implement `Clone`
///
/// No code in the crate needs to clone a `PrimitiveBlock`. For shared access, use
/// `Arc<PrimitiveBlock>` - a single atomic increment regardless of block size.
pub struct PrimitiveBlock {
    /// Owns the decompressed protobuf bytes.
    #[allow(dead_code)]
    buffer: Bytes,
    /// Zero-copy parsed view. Borrows from `buffer` via lifetime erasure.
    ///
    /// # Safety
    ///
    /// The `'static` lifetime is a lie - `block` actually borrows from `buffer`.
    /// This is safe because:
    /// 1. `buffer` is `Bytes` (immutable, reference-counted), never mutated.
    /// 2. `buffer` and `block` live in the same struct - `block` cannot outlive `buffer`.
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
    // Takes `Bytes` by value to compose with `and_then(PrimitiveBlock::new)`
    // at decompress-boundary callers. The body copies via `to_vec()` so
    // ownership is not strictly needed internally, but the by-value signature
    // is the idiomatic shape at those call sites.
    #[hotpath::measure]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(buffer: Bytes) -> Result<PrimitiveBlock> {
        Self::from_vec(buffer.to_vec())
    }

    /// Validate stringtable entries and construct a `PrimitiveBlock` from already-inlined data.
    ///
    /// Shared finishing step for all constructors: builds the `WireBlock` from
    /// inline metadata, validates UTF-8, and transmutes the lifetime to `'static`
    /// (safe because `bytes` owns the backing data).
    fn finish(bytes: Bytes, meta: &WireBlockMeta) -> Result<PrimitiveBlock> {
        let data: &[u8] = &bytes;
        let block = WireBlock::from_inline(data, meta);

        validate_groups_strict(&block)?;

        // Validate every stringtable entry once at construction time.
        for index in 0..block.stringtable.len() {
            if let Some(raw) = block.stringtable.get(index) {
                std::str::from_utf8(raw)
                    .map_err(|err| new_error(ErrorKind::StringtableUtf8 { err, index }))?;
            }
        }

        #[allow(clippy::transmute_undefined_repr)]
        let block = unsafe { std::mem::transmute::<WireBlock<'_>, WireBlock<'static>>(block) };

        Ok(PrimitiveBlock {
            buffer: bytes,
            block,
        })
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
        Self::finish(Bytes::from(buffer), &meta)
    }

    /// Like [`from_vec`] but reuses caller-provided scratch buffers.
    /// Sequential loops call this to avoid per-block scratch allocation.
    #[hotpath::measure]
    pub(crate) fn from_vec_with_scratch(
        mut buffer: Vec<u8>,
        st_scratch: &mut Vec<(u32, u32)>,
        gr_scratch: &mut Vec<(u32, u32)>,
    ) -> Result<PrimitiveBlock> {
        let meta = WireBlock::parse_and_inline_with_scratch(&mut buffer, st_scratch, gr_scratch)?;
        Self::finish(Bytes::from(buffer), &meta)
    }

    /// Like [`from_vec`] but wraps the buffer with pool recycling.
    /// Workers in `parallel_classify_phase` call this
    /// with loop-local scratch to avoid per-block allocation.
    #[hotpath::measure]
    pub(crate) fn from_vec_pooled_with_scratch(
        mut buffer: Vec<u8>,
        pool: &std::sync::Arc<crate::blob::DecompressPool>,
        st_scratch: &mut Vec<(u32, u32)>,
        gr_scratch: &mut Vec<(u32, u32)>,
    ) -> Result<PrimitiveBlock> {
        let meta = WireBlock::parse_and_inline_with_scratch(&mut buffer, st_scratch, gr_scratch)?;
        Self::finish(crate::blob::pool_wrap(buffer, Some(pool)), &meta)
    }

    /// Returns the size of the decompressed protobuf payload in bytes.
    ///
    /// This is the raw decompressed data backing this block - useful for
    /// byte-budget accounting in batched processing pipelines. When inline
    /// entries are used, returns the original protobuf size (not the extended
    /// buffer).
    pub fn decompressed_size(&self) -> usize {
        self.block.proto_len as usize
    }

    /// Returns the element type contained in this block.
    ///
    /// Inspects only the first protobuf field tag of each group - typically a
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
    /// Scaffolding for future per-group raw passthrough - see
    /// `frame_raw_block` in `src/write/raw_passthrough.rs` for the design
    /// tradeoffs and the measurement prerequisite.
    #[allow(dead_code)]
    pub(crate) fn raw_group_bytes(&self, index: usize) -> &[u8] {
        self.block.group(index)
    }

    /// Returns the number of PrimitiveGroups in this block.
    /// Scaffolding for future per-group raw passthrough.
    #[allow(dead_code)]
    pub(crate) fn group_count(&self) -> usize {
        self.block.group_count()
    }

    /// Returns the raw StringTable protobuf bytes (field 1 of PrimitiveBlock).
    /// Scaffolding for future per-group raw passthrough.
    #[allow(dead_code)]
    pub(crate) fn raw_stringtable_bytes(&self) -> &[u8] {
        self.block.raw_stringtable()
    }

    /// Returns the scalar fields needed to reconstruct a PrimitiveBlock frame.
    /// Scaffolding for future per-group raw passthrough.
    #[allow(dead_code)]
    pub(crate) fn block_scalars(&self) -> (i32, i64, i64, i32) {
        (
            self.block.granularity,
            self.block.lat_offset,
            self.block.lon_offset,
            self.block.date_granularity,
        )
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

    /// Batch-decode all dense nodes into columnar arrays (IDs, lats, lons).
    ///
    /// Reuses the provided `DenseNodeColumns` scratch buffer. Coordinates
    /// are converted to decimicrodegrees. Returns the number of nodes decoded.
    ///
    /// Handles blocks with multiple dense groups (appends across groups).
    /// Non-dense Node messages are NOT included - use element-by-element
    /// iteration for blocks that may contain plain Node groups.
    ///
    /// This is the columnar alternative to iterating `elements()` / `elements_skip_metadata()`
    /// for classification passes that only need ID + coordinates.
    /// A malformed dense group cannot reach here: [`PrimitiveBlock::finish`]
    /// rejects a block whose dense `id`/`lat`/`lon` arrays disagree in length
    /// or fail to parse, so the `Ok` guards below are structure, not recovery.
    pub(crate) fn decode_dense_columns(
        &self,
        columns: &mut super::columnar::DenseNodeColumns,
    ) -> usize {
        columns.clear();
        for group in self.groups() {
            if let Ok(Some(dense_data)) = group.group.dense()
                && let Ok(dense) = super::wire::WireDenseNodes::parse(dense_data)
            {
                columns.decode_append(
                    &dense,
                    self.block.granularity,
                    self.block.lat_offset,
                    self.block.lon_offset,
                );
            }
        }
        columns.len()
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

/// Count the values in a packed varint array, rejecting one that ends
/// mid-varint.
///
/// A varint's last byte has the continuation bit clear, so the value count is
/// the number of such bytes and a trailing continuation byte means the array
/// was truncated. This is a byte scan, not a decode - no value is
/// reconstructed.
#[inline]
fn packed_varint_count(data: &[u8]) -> Result<usize> {
    let mut count = 0usize;
    let mut open = false;
    for &b in data {
        if b & 0x80 == 0 {
            count += 1;
            open = false;
        } else {
            open = true;
        }
    }
    if open {
        return Err(new_wire_error("packed varint array ends mid-varint"));
    }
    Ok(count)
}

/// Structural validation of every group in a block, run once at construction.
///
/// # Why this exists
///
/// `Iterator::next` cannot return an error, so every element-level decode site
/// below the blob boundary used to swallow one: `WireMessageIter::next` ended
/// the stream on a wire error, the `GroupNodeIter` / `GroupWayIter` /
/// `GroupRelationIter` loops skipped any element whose `parse` failed, a
/// malformed `DenseNodes` message became an empty iterator, and mismatched
/// dense array lengths truncated the node run. A corrupt blob therefore
/// produced *fewer elements* and exit code 0 - the silent data loss that
/// `reference/truncation-handling.md` pins as a hard error at the frame layer
/// but which was never carried into the block layer.
///
/// Validating once at construction fixes all of them at a single fallible site
/// and leaves the iterators' signatures alone: after this passes, none of those
/// `Err` arms is reachable, so the skips are dead code rather than silent loss.
///
/// # What is checked
///
/// Per group, in order: the group's own top-level field framing; each `Node`,
/// `Way` and `Relation` sub-message parses; each element's `Info` sub-message
/// parses; every packed array on those elements holds only complete, bounded
/// varints; the parallel-array invariants the zipping iterators depend on hold
/// (`keys`/`vals` on all three kinds, `lat`/`lon`/`refs` on ways,
/// `roles_sid`/`memids`/`types` on relations); the `DenseNodes` sub-message
/// parses; and its packed `id`, `lat` and `lon` arrays are well-formed and of
/// equal length.
///
/// The framing walk comes first deliberately - it is the check that makes the
/// element iterators used below unable to terminate early, so running the
/// element walk first would silently examine a prefix.
///
/// This is a structural pass, not a decode: sub-message parsing walks field
/// headers and records slice bounds without decoding any packed array, and the
/// dense length check is a byte scan. No stringtable index is resolved (that
/// is `str_from_stringtable`'s job, which is already fallible).
///
/// # What is deliberately not checked
///
/// Two dense-node arrays are out of scope for this round and remain sources of
/// silent truncation - both are tracked as open findings in
/// `notes/bugs-read-path.md`:
///
/// * `DenseNodes.keys_vals`, whose zero-terminated per-node structure needs a
///   full stateful walk rather than a byte scan to length-check.
/// * `DenseNodes.denseinfo`, whose six parallel arrays must each match the node
///   count.
///
/// Neither can be validated by the array-length equality this function applies,
/// so asserting them here would mean claiming a check that is not performed.
///
/// No error raised here carries a byte offset. These sites sit below the
/// framing layer and cannot know their position in the file; the enclosing
/// `BlobReader` stamps the blob's offset on the way out, and the innermost-wins
/// rule means an absent inner offset never displaces a more precise one.
fn validate_groups_strict(block: &WireBlock<'_>) -> Result<()> {
    use super::wire::Cursor;

    for index in 0..block.group_count() {
        let data = block.group(index);

        // 1. The group's own field framing. Everything below walks this same
        //    byte range through iterators that cannot report an error.
        let mut cursor = Cursor::new(data);
        while let Some((_field, wire_type)) = cursor.read_tag()? {
            cursor.skip_field(wire_type)?;
        }

        let group = WireGroup::new(data);

        // 2. Every element sub-message, its optional Info sub-message, and
        //    every packed array it carries - both well-formedness and the
        //    parallel-array invariants the zipping iterators depend on.
        for element in group.nodes() {
            let node = WireNode::parse(element)?;
            validate_info(node.info_data)?;
            validate_tag_arrays(
                node.keys_data,
                node.vals_data,
                "node has mismatched keys/vals tag array lengths",
            )?;
        }
        for element in group.ways() {
            let way = WireWay::parse(element)?;
            validate_info(way.info_data)?;
            validate_tag_arrays(
                way.keys_data,
                way.vals_data,
                "way has mismatched keys/vals tag array lengths",
            )?;
            validate_way_geometry_arrays(&way)?;
        }
        for element in group.relations() {
            let relation = WireRelation::parse(element)?;
            validate_info(relation.info_data)?;
            validate_tag_arrays(
                relation.keys_data,
                relation.vals_data,
                "relation has mismatched keys/vals tag array lengths",
            )?;
            validate_relation_member_arrays(&relation)?;
        }

        // 3. The dense group, and the parallel-array invariant every dense
        //    consumer indexes on.
        if let Some(dense_data) = group.dense()? {
            let dense = WireDenseNodes::parse(dense_data)?;
            let ids = packed_varint_count(dense.id_data)?;
            let lats = packed_varint_count(dense.lat_data)?;
            let lons = packed_varint_count(dense.lon_data)?;
            if ids != lats || ids != lons {
                return Err(new_wire_error(
                    "dense group has mismatched id/lat/lon array lengths",
                ));
            }
        }
    }

    Ok(())
}

/// Validate an element's packed `keys` / `vals` tag arrays.
///
/// `TagIter` and `RawTagIter` zip the two arrays and stop at the shorter one,
/// so a `vals` array one entry short drops the final tag and reports success.
/// The two arrays are parallel by definition - a tag is a (key, val) pair -
/// so any disagreement is corruption, never a shorter-but-valid encoding.
///
/// `mismatch` names the element kind in the error text; `ErrorKind::WireFormat`
/// carries a `&'static str`, so the caller supplies the message rather than
/// this function formatting one.
#[inline]
fn validate_tag_arrays(keys_data: &[u8], vals_data: &[u8], mismatch: &'static str) -> Result<()> {
    let keys = packed_varint_count(keys_data)?;
    let vals = packed_varint_count(vals_data)?;
    if keys != vals {
        return Err(new_wire_error(mismatch));
    }
    Ok(())
}

/// Validate a way's packed `refs` array and its optional `LocationsOnWays`
/// coordinate arrays.
///
/// `WayRefIter` walks `refs` alone, so a `refs` array ending mid-varint simply
/// yields one node fewer. `WayNodeLocationsIter` zips `lat` against `lon` and
/// stops at the shorter, so a truncated `lon` silently shortens the geometry -
/// which for a closed way turns a ring into an open line and changes what
/// point-in-polygon answers.
///
/// The coordinate arrays are optional as a pair: a way written without the
/// `LocationsOnWays` optional feature carries neither. When either is present
/// both must be, and both must run parallel to `refs` - libosmium and this
/// crate's own writer emit one coordinate per node reference in a single loop,
/// so a partial coordinate run is not a shape any writer produces.
#[inline]
fn validate_way_geometry_arrays(way: &WireWay<'_>) -> Result<()> {
    let refs = packed_varint_count(way.refs_data)?;
    let lats = packed_varint_count(way.lat_data)?;
    let lons = packed_varint_count(way.lon_data)?;

    if lats == 0 && lons == 0 {
        // No LocationsOnWays payload: refs alone, already length-checked.
        return Ok(());
    }
    if lats != lons {
        return Err(new_wire_error(
            "way has mismatched lat/lon node-location array lengths",
        ));
    }
    if lats != refs {
        return Err(new_wire_error(
            "way has a node-location count that disagrees with its node-reference count",
        ));
    }
    Ok(())
}

/// Validate a relation's three parallel member arrays.
///
/// `RelMemberIter` zips `roles_sid`, `memids` and `types` and ends as soon as
/// any one runs out, so a short `types` array drops trailing members from a
/// multipolygon without any error - the ring assembler then sees an incomplete
/// relation and produces a wrong geometry rather than a failure.
#[inline]
fn validate_relation_member_arrays(relation: &WireRelation<'_>) -> Result<()> {
    let roles = packed_varint_count(relation.roles_sid_data)?;
    let memids = packed_varint_count(relation.memids_data)?;
    let types = packed_varint_count(relation.types_data)?;
    if roles != memids || roles != types {
        return Err(new_wire_error(
            "relation has mismatched roles_sid/memids/types member array lengths",
        ));
    }
    Ok(())
}

/// Validate an element's optional `Info` sub-message.
///
/// `elements.rs` reads info as `WireInfo::parse(d).ok().unwrap_or_default()`,
/// which turns corrupt metadata into default metadata (version 0, timestamp 0)
/// that is indistinguishable from genuinely absent metadata.
#[inline]
fn validate_info(info_data: Option<&[u8]>) -> Result<()> {
    match info_data {
        Some(data) => super::wire::WireInfo::parse(data).map(|_| ()),
        None => Ok(()),
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
                        Ok(dense) => {
                            if self.skip_metadata {
                                DenseNodeIter::new_skip_metadata(self.block, dense)
                            } else {
                                DenseNodeIter::new(self.block, dense)
                            }
                        }
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
                        return Some(Some(Element::Relation(Relation::new(self.block, wire_rel))));
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
/// Uses `from_utf8_unchecked` - the safety relies on the invariant established
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

// Tests use `unwrap()`/`expect()` because panicking is the correct failure mode
// for unit tests; see the note in `blob_wire.rs`'s test module for the
// crate-wide `unwrap_used = "deny"` exemption.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod strict_validation_tests {
    use super::PrimitiveBlock;

    /// Wrap `body` as protobuf field `field`, wire type 2 (length-delimited).
    /// Every fixture here is small enough for a single-byte length varint,
    /// which keeps the hand-encoding readable.
    fn len_field(field: u8, body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 128, "fixture bodies stay single-byte-length");
        #[allow(clippy::cast_possible_truncation)]
        let mut out = vec![(field << 3) | 2, body.len() as u8];
        out.extend_from_slice(body);
        out
    }

    /// A `PrimitiveBlock` carrying a one-entry (empty string) StringTable and
    /// the given PrimitiveGroup bodies.
    fn block_with_groups(groups: &[Vec<u8>]) -> Vec<u8> {
        let mut out = len_field(1, &len_field(1, &[]));
        for group in groups {
            out.extend_from_slice(&len_field(2, group));
        }
        out
    }

    /// A DenseNodes group whose packed id/lat/lon arrays hold the given counts
    /// of one-byte varints.
    fn dense_group(ids: usize, lats: usize, lons: usize) -> Vec<u8> {
        let mut dense = len_field(1, &vec![0x02u8; ids]);
        dense.extend_from_slice(&len_field(8, &vec![0x02u8; lats]));
        dense.extend_from_slice(&len_field(9, &vec![0x02u8; lons]));
        len_field(2, &dense)
    }

    /// A dense group whose `lat` array is shorter than its `id` array is a
    /// hard error at construction, not a silently truncated node run.
    ///
    /// The equal-length case is asserted first and must succeed - without it
    /// this test would pass just as well against a `from_vec` that rejects
    /// every block.
    #[test]
    fn dense_group_with_mismatched_array_lengths_is_rejected() {
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group(4, 4, 4)]))
            .expect("a dense group with equal-length arrays is valid");

        let err = PrimitiveBlock::from_vec(block_with_groups(&[dense_group(4, 2, 4)]))
            .expect_err("a short lat array must be rejected");
        assert!(
            format!("{err}").contains("mismatched"),
            "expected a mismatched-array error, got: {err}"
        );

        PrimitiveBlock::from_vec(block_with_groups(&[dense_group(4, 4, 3)]))
            .expect_err("a short lon array must be rejected");
    }

    /// A packed array whose last varint is cut mid-value is rejected rather
    /// than silently yielding one value fewer.
    ///
    /// The fixture is built so that only the mid-varint check can reject it.
    /// The `id` array's trailing continuation byte contributes no value to the
    /// count, so `id` counts 2 - and `lat`/`lon` carry 2 complete varints each
    /// to match. With equal counts the array-length equality check is satisfied
    /// and cannot fire, which is the whole point: an earlier version of this
    /// fixture gave `lat`/`lon` three values each, so disabling the mid-varint
    /// check still failed the block on the length mismatch and the test passed
    /// while testing nothing.
    #[test]
    fn dense_packed_array_ending_mid_varint_is_rejected() {
        // Two complete ids plus a dangling continuation byte; lat/lon carry
        // exactly two complete varints, so all three counts are 2.
        let mut dense = len_field(1, &[0x02, 0x02, 0x82]);
        dense.extend_from_slice(&len_field(8, &[0x02, 0x02]));
        dense.extend_from_slice(&len_field(9, &[0x02, 0x02]));
        let err = PrimitiveBlock::from_vec(block_with_groups(&[len_field(2, &dense)]))
            .expect_err("a packed array ending mid-varint must be rejected");
        assert!(
            format!("{err}").contains("mid-varint"),
            "the mid-varint check must be what rejects this, not the length \
             equality check; got: {err}"
        );

        // The same fixture with the dangling byte completed is valid, so the
        // rejection above is attributable to the truncation and not to the
        // fixture being malformed in some other way.
        let mut good = len_field(1, &[0x02, 0x02]);
        good.extend_from_slice(&len_field(8, &[0x02, 0x02]));
        good.extend_from_slice(&len_field(9, &[0x02, 0x02]));
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(2, &good)]))
            .expect("the untruncated fixture must parse");
    }

    /// A malformed element sub-message is a hard error, not a skipped element.
    ///
    /// Each fixture pairs one valid element with one whose `keys` field
    /// declares more bytes than the sub-message contains. Before strict
    /// validation the block constructed cleanly and iteration simply yielded
    /// the valid element, so `pbfhogg cat corrupt.pbf` exited 0 having written
    /// fewer elements than it read.
    #[test]
    fn malformed_element_submessage_is_rejected() {
        // field 2 (keys), length-delimited, declares 5 bytes and supplies none.
        let truncated = [0x12u8, 0x05];
        // field 1 (id) = 1: a minimal valid element of any of the three kinds.
        let valid = [0x08u8, 0x01];

        for field in [1u8, 3, 4] {
            let mut group = len_field(field, &valid);
            PrimitiveBlock::from_vec(block_with_groups(&[group.clone()]))
                .unwrap_or_else(|e| panic!("field {field}: valid element must parse: {e}"));

            group.extend_from_slice(&len_field(field, &truncated));
            assert!(
                PrimitiveBlock::from_vec(block_with_groups(&[group])).is_err(),
                "field {field}: a malformed element sub-message must be rejected"
            );
        }
    }

    /// A packed varint array holding `count` one-byte values.
    fn packed(field: u8, count: usize) -> Vec<u8> {
        len_field(field, &vec![0x02u8; count])
    }

    /// Mismatched `keys`/`vals` tag arrays are a hard error on every element
    /// kind, not a silently dropped final tag.
    ///
    /// `TagIter` and `RawTagIter` zip the two arrays and stop at the shorter,
    /// so before this check a way whose `vals` array lost its last entry kept
    /// every other tag and reported success - `pbfhogg tags-filter` would then
    /// silently fail to match on the dropped tag.
    ///
    /// The matched-length case is asserted first for each kind and must
    /// succeed, so the test cannot pass against a validator that rejects
    /// everything.
    #[test]
    fn mismatched_keys_vals_arrays_are_rejected_on_every_element_kind() {
        // Group field 1 = Node, 3 = Way, 4 = Relation.
        for field in [1u8, 3, 4] {
            let mut ok = vec![0x08u8, 0x01];
            ok.extend_from_slice(&packed(2, 3));
            ok.extend_from_slice(&packed(3, 3));
            PrimitiveBlock::from_vec(block_with_groups(&[len_field(field, &ok)]))
                .unwrap_or_else(|e| panic!("field {field}: equal keys/vals must parse: {e}"));

            let mut short = vec![0x08u8, 0x01];
            short.extend_from_slice(&packed(2, 3));
            short.extend_from_slice(&packed(3, 2));
            let err = PrimitiveBlock::from_vec(block_with_groups(&[len_field(field, &short)]))
                .expect_err("a short vals array must be rejected");
            assert!(
                format!("{err}").contains("keys/vals"),
                "field {field}: expected a keys/vals error, got: {err}"
            );
        }
    }

    /// A packed array on a regular element that ends mid-varint is rejected
    /// rather than silently yielding one value fewer.
    ///
    /// The dense equivalent is covered above; this is the regular-element path,
    /// which reaches a different validator.
    #[test]
    fn regular_element_packed_array_ending_mid_varint_is_rejected() {
        // A way whose refs array's final varint sets the continuation bit and
        // has no successor byte.
        let mut way = vec![0x08u8, 0x01];
        way.extend_from_slice(&len_field(8, &[0x02, 0x02, 0x82]));
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(3, &way)]))
            .expect_err("a refs array ending mid-varint must be rejected");

        // The same way with a complete final varint is valid.
        let mut good = vec![0x08u8, 0x01];
        good.extend_from_slice(&len_field(8, &[0x02, 0x02, 0x02]));
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(3, &good)]))
            .expect("a complete refs array must parse");
    }

    /// A way's `LocationsOnWays` coordinate arrays must run parallel to each
    /// other and to `refs`.
    ///
    /// `WayNodeLocationsIter` zips `lat` against `lon` and stops at the
    /// shorter, so a truncated `lon` silently shortens the geometry - for a
    /// closed way that turns a ring into an open line and changes what
    /// point-in-polygon answers, with no error anywhere.
    ///
    /// A way with no coordinate arrays at all is the common case (no
    /// `LocationsOnWays` feature) and must stay valid; it is asserted first.
    #[test]
    fn way_node_location_arrays_must_match_refs() {
        let with_refs_only = {
            let mut way = vec![0x08u8, 0x01];
            way.extend_from_slice(&packed(8, 3));
            way
        };
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(3, &with_refs_only)]))
            .expect("a way without node locations must stay valid");

        let build = |refs: usize, lats: usize, lons: usize| {
            let mut way = vec![0x08u8, 0x01];
            way.extend_from_slice(&packed(8, refs));
            way.extend_from_slice(&packed(9, lats));
            way.extend_from_slice(&packed(10, lons));
            block_with_groups(&[len_field(3, &way)])
        };

        PrimitiveBlock::from_vec(build(3, 3, 3))
            .expect("parallel refs/lat/lon arrays must be valid");

        let err = PrimitiveBlock::from_vec(build(3, 3, 2))
            .expect_err("a short lon array must be rejected");
        assert!(
            format!("{err}").contains("lat/lon"),
            "expected a lat/lon mismatch error, got: {err}"
        );

        PrimitiveBlock::from_vec(build(3, 2, 2))
            .expect_err("coordinates that do not cover every node ref must be rejected");
    }

    /// A relation's three parallel member arrays must agree in length.
    ///
    /// `RelMemberIter` zips `roles_sid`, `memids` and `types` and ends as soon
    /// as any one runs out, so a short `types` array drops trailing members
    /// from a multipolygon - the ring assembler then builds a wrong geometry
    /// from an incomplete member list rather than failing.
    #[test]
    fn mismatched_relation_member_arrays_are_rejected() {
        let build = |roles: usize, memids: usize, types: usize| {
            let mut rel = vec![0x08u8, 0x01];
            rel.extend_from_slice(&packed(8, roles));
            rel.extend_from_slice(&packed(9, memids));
            rel.extend_from_slice(&packed(10, types));
            block_with_groups(&[len_field(4, &rel)])
        };

        PrimitiveBlock::from_vec(build(3, 3, 3)).expect("parallel member arrays must be valid");

        let err = PrimitiveBlock::from_vec(build(3, 3, 2))
            .expect_err("a short types array must be rejected");
        assert!(
            format!("{err}").contains("member array"),
            "expected a member-array mismatch error, got: {err}"
        );

        PrimitiveBlock::from_vec(build(3, 2, 3))
            .expect_err("a short memids array must be rejected");
    }

    /// A corrupt `Info` sub-message is a hard error rather than silently
    /// becoming default metadata (version 0, timestamp 0), which is
    /// indistinguishable from genuinely absent metadata.
    #[test]
    fn malformed_element_info_is_rejected() {
        // Node with field 4 (info) whose body is a varint field with no value.
        let bad_info = len_field(4, &[0x08]);
        let mut node = vec![0x08u8, 0x01];
        node.extend_from_slice(&bad_info);
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(1, &node)]))
            .expect_err("a truncated Info sub-message must be rejected");

        // Same node with a well-formed info (version = 1) still parses.
        let mut good = vec![0x08u8, 0x01];
        good.extend_from_slice(&len_field(4, &[0x08, 0x01]));
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(1, &good)]))
            .expect("a well-formed Info sub-message must parse");
    }
}
