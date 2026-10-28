//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use super::dense::DenseNodeIter;
use super::elements::{Element, Node, Relation, Way};
use super::wire::{
    WireBlock, WireBlockMeta, WireDenseInfo, WireDenseNodes, WireGroup, WireMessageIter, WireNode,
    WireRelation, WireWay,
};
use super::wire_schema::{header_bbox, header_block};
use crate::error::{
    Result, new_stringtable_index_out_of_bounds, new_stringtable_utf8, new_wire_error,
};
use bytes::Bytes;
use std;

/// A feature name declared in an `OSMHeader`.
///
/// The associated constants own the spellings used by this crate. Converting
/// from `&str` remains available for callers that preserve extension features
/// unknown to pbfhogg. Any literal check must be scoped to feature declarations:
/// wire fixtures and element diagnostics also contain some of these words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature<'a>(&'a str);

impl<'a> Feature<'a> {
    /// Required feature identifying the OSM PBF schema version.
    pub const OSM_SCHEMA_V06: Self = Self("OsmSchema-V0.6");

    /// Required feature declaring dense node encoding.
    pub const DENSE_NODES: Self = Self("DenseNodes");

    /// Optional feature declaring `Sort.Type_then_ID` ordering.
    pub const SORT_TYPE_THEN_ID: Self = Self("Sort.Type_then_ID");

    /// Optional feature declaring inline way-node coordinates.
    pub const LOCATIONS_ON_WAYS: Self = Self("LocationsOnWays");

    /// Optional feature declaring BlobHeader field-5 way-member bitmaps.
    pub const WAY_MEMBERS_V1: Self = Self("pbfhogg.WayMembers-v1");

    /// Optional feature declaring Way field-20 shared-node pin bitmaps.
    pub const SHARED_NODE_PINS_V1: Self = Self("pbfhogg.SharedNodePins-v1");

    /// Required feature declaring element history metadata.
    pub const HISTORICAL_INFORMATION: Self = Self("HistoricalInformation");

    /// Return the feature name as it appears in an `OSMHeader`.
    pub const fn as_str(self) -> &'a str {
        self.0
    }
}

impl<'a, 'b> From<&'b str> for Feature<'a>
where
    'b: 'a,
{
    fn from(value: &'b str) -> Self {
        Self(value)
    }
}

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
                (header_bbox::LEFT, 0) => left = cursor.read_sint64()?,
                (header_bbox::RIGHT, 0) => right = cursor.read_sint64()?,
                (header_bbox::TOP, 0) => top = cursor.read_sint64()?,
                (header_bbox::BOTTOM, 0) => bottom = cursor.read_sint64()?,
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
                header_block::BBOX => {
                    // bbox: HeaderBBox submessage
                    let sub_data = cursor.read_len_delimited()?;
                    bbox = Some(WireHeaderBBox::parse(sub_data)?);
                }
                header_block::REQUIRED_FEATURES => {
                    // required_features: repeated string
                    let bytes = cursor.read_len_delimited()?;
                    let s = String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in required_features"))?;
                    required_features.push(s);
                }
                header_block::OPTIONAL_FEATURES => {
                    // optional_features: repeated string
                    let bytes = cursor.read_len_delimited()?;
                    let s = String::from_utf8(bytes.to_vec())
                        .map_err(|_| new_wire_error("invalid UTF-8 in optional_features"))?;
                    optional_features.push(s);
                }
                header_block::WRITING_PROGRAM => {
                    // writingprogram: string
                    let bytes = cursor.read_len_delimited()?;
                    writingprogram = Some(
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| new_wire_error("invalid UTF-8 in writingprogram"))?,
                    );
                }
                header_block::SOURCE => {
                    // source: string
                    let bytes = cursor.read_len_delimited()?;
                    source = Some(
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| new_wire_error("invalid UTF-8 in source"))?,
                    );
                }
                header_block::REPLICATION_TIMESTAMP => {
                    // osmosis_replication_timestamp: int64
                    osmosis_replication_timestamp = Some(cursor.read_varint_i64()?);
                }
                header_block::REPLICATION_SEQUENCE_NUMBER => {
                    // osmosis_replication_sequence_number: int64
                    osmosis_replication_sequence_number = Some(cursor.read_varint_i64()?);
                }
                header_block::REPLICATION_BASE_URL => {
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
    pub const SORT_TYPE_THEN_ID: &str = Feature::SORT_TYPE_THEN_ID.as_str();

    /// Returns `true` if the header declares `Sort.Type_then_ID`.
    pub fn is_sorted(&self) -> bool {
        self.header
            .optional_features
            .iter()
            .any(|f| f == Self::SORT_TYPE_THEN_ID)
    }

    /// PBF optional feature string indicating ways contain inline node coordinates.
    pub const LOCATIONS_ON_WAYS: &str = Feature::LOCATIONS_ON_WAYS.as_str();

    /// Optional feature declaring BlobHeader field-5 way-member bitmaps.
    pub const WAY_MEMBERS_V1: &str = Feature::WAY_MEMBERS_V1.as_str();

    /// Optional feature declaring Way field-20 shared-node pin bitmaps.
    pub const SHARED_NODE_PINS_V1: &str = Feature::SHARED_NODE_PINS_V1.as_str();

    /// PBF required feature identifying the OSM schema version.
    pub const OSM_SCHEMA_V06: &str = Feature::OSM_SCHEMA_V06.as_str();

    /// PBF required feature declaring dense node encoding.
    pub const DENSE_NODES: &str = Feature::DENSE_NODES.as_str();

    /// PBF required feature string indicating history metadata (`visible`) may
    /// be present on elements.
    pub const HISTORICAL_INFORMATION: &str = Feature::HISTORICAL_INFORMATION.as_str();

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
    /// Keeps alive the decompressed bytes borrowed by the zero-copy block view.
    _buffer: Bytes,
    /// Zero-copy parsed view. Borrows from `_buffer` via lifetime erasure.
    ///
    /// # Safety
    ///
    /// The `'static` lifetime is a lie - `block` actually borrows from `_buffer`.
    /// This is safe because:
    /// 1. `_buffer` is `Bytes` (immutable, reference-counted), never mutated.
    /// 2. `_buffer` and `block` live in the same struct - `block` cannot outlive `_buffer`.
    /// 3. `PrimitiveBlock` is not `Clone`, preventing accidental separation.
    /// 4. All public access goes through `&self`, tying the real lifetime to the borrow.
    block: WireBlock<'static>,
    /// The kinds of the elements the block holds, recorded by the
    /// construction-time validation pass.
    kinds: crate::element_kind::KindSet,
    /// How many elements [`elements`](Self::elements) yields, counted by the
    /// same validation pass.
    element_count: u64,
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
    /// Validate stringtable entries and construct a `PrimitiveBlock` from already-inlined data.
    ///
    /// Shared finishing step for all constructors: builds the `WireBlock` from
    /// inline metadata, validates UTF-8, and transmutes the lifetime to `'static`
    /// (safe because `bytes` owns the backing data).
    fn finish(bytes: Bytes, meta: &WireBlockMeta) -> Result<PrimitiveBlock> {
        let data: &[u8] = &bytes;
        let block = WireBlock::from_inline(data, meta);

        let (kinds, element_count) = validate_groups_strict(&block)?;

        // Validate every stringtable entry once at construction time.
        for index in 0..block.stringtable.len() {
            if let Some(raw) = block.stringtable.get(index) {
                std::str::from_utf8(raw).map_err(|err| new_stringtable_utf8(err, index))?;
            }
        }

        #[allow(clippy::transmute_undefined_repr)]
        let block = unsafe { std::mem::transmute::<WireBlock<'_>, WireBlock<'static>>(block) };

        Ok(PrimitiveBlock {
            _buffer: bytes,
            block,
            kinds,
            element_count,
        })
    }

    /// Refuse this block when `claim` names a kind and the block holds an
    /// element of another: the run believed the claim to route the blob, so
    /// an off-kind element is one that run would silently drop or never see.
    /// One compare when the block agrees. The caller locates the error at
    /// the blob.
    #[inline]
    pub(crate) fn refuse_foreign_kind(&self, claim: Option<crate::ElementKind>) -> Result<()> {
        self.kinds.refuse_under(claim)
    }

    /// How many elements [`elements`](Self::elements) yields: one per plain
    /// node, way and relation and one per dense ID, counted when the block
    /// was validated, so it is the decoded count, never a blob's claim.
    #[inline]
    pub(crate) fn element_count(&self) -> u64 {
        self.element_count
    }

    /// Parse a `PrimitiveBlock` from decompressed protobuf bytes, inlining
    /// string table entries and group ranges into the buffer itself. Zero
    /// separate heap allocations beyond the buffer.
    ///
    /// This takes an owned `Vec<u8>` rather than `Bytes` because the buffer is
    /// extended in place with the inline entry data; a `Bytes` would have to be
    /// copied first. The former public constructor `new(Bytes)` did exactly
    /// that copy - a full decompressed-block memcpy, ~1.4 MB per block - and
    /// every internal caller had already been migrated off it. Callers holding
    /// `Bytes` should decompress into a `Vec` instead of converting.
    ///
    /// # Errors
    ///
    /// Returns `ErrorKind::StringtableUtf8` if any stringtable entry contains
    /// invalid UTF-8 bytes. Returns `ErrorKind::WireFormat` if the protobuf
    /// wire format is invalid or a group fails structural validation.
    ///
    /// This eliminates the cross-thread `Box<[(u32, u32)]>` retention that caused
    /// 25+ GB OOM at Europe scale (520K blocks) with the pipelined reader.
    /// The temp Vecs during parsing are allocated and freed on the calling thread.
    ///
    /// The buffer is extended with inline entry data. After conversion to `Bytes`,
    /// the block references both the protobuf data and the appended entries.
    pub fn from_vec(mut buffer: Vec<u8>) -> Result<PrimitiveBlock> {
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

    /// The block's coordinate framing as `(granularity, lat_offset,
    /// lon_offset)`: a coordinate is `offset + granularity * raw`
    /// nanodegrees. `(100, 0, 0)` unless the block declares otherwise.
    /// For writers that copy raw coordinate bytes and must know whether the
    /// output block's (default) framing reads them the same way.
    pub(crate) fn coord_framing(&self) -> (i32, i64, i64) {
        (
            self.block.granularity,
            self.block.lat_offset,
            self.block.lon_offset,
        )
    }

    /// The block's `date_granularity` (milliseconds per raw timestamp unit,
    /// 1000 unless the block declares otherwise). For writers that copy raw
    /// `Info` bytes.
    pub(crate) fn date_granularity(&self) -> i32 {
        self.block.date_granularity
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
    ///
    /// `Mixed` also stands for a group this classifier cannot name: a
    /// changeset group (field 5), an unknown field or an unexpected wire type.
    /// Dense plus plain node groups are `Mixed` too though they hold one kind.
    /// So `Mixed` is "look at the elements", never evidence of several kinds,
    /// and this is not a kind test: callers either walk a `Mixed` block anyway
    /// (extract simple), reject it (digest), or fall back to the kinds of the
    /// elements present (the diff merge).
    ///
    /// Empty groups carry no elements and are neutral: an empty group beside
    /// a way group is a way block, not a mixed one. A block whose groups are
    /// all empty (or that has none) is [`Empty`](BlockType::Empty). Counting
    /// an empty group as a type of its own used to classify spec-valid
    /// single-kind blocks as mixed, which the streaming merge-join rejects.
    pub fn block_type(&self) -> BlockType {
        let mut result: Option<BlockType> = None;
        for i in 0..self.block.group_count() {
            let group_data = self.block.group(i);
            let group_type = classify_group(group_data);
            if group_type == BlockType::Empty {
                continue;
            }
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
            // PrimitiveBlock::finish, which every constructor funnels through.
            // The PrimitiveBlock struct does not expose any mutable access to
            // the underlying buffer.
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
    /// or fail to parse, so a re-parse failure is a broken invariant and
    /// panics (see `validated`) rather than dropping the group's nodes.
    pub(crate) fn decode_dense_columns(
        &self,
        columns: &mut super::columnar::DenseNodeColumns,
    ) -> usize {
        columns.clear();
        for group in self.groups() {
            if let Some(dense) = validated_dense(&group.group) {
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

/// Count the values in a packed varint array, rejecting exactly the arrays
/// that the packed iterators would stop decoding early.
///
/// Every packed iterator reads through `protohoggr::Cursor::read_varint` and
/// turns its error into an end of iteration, so the parallel-array checks
/// built on this count are only worth anything if this accepts precisely what
/// repeated `read_varint` consumes in full. That decoder refuses three shapes:
/// an array that ends mid-varint, a tenth byte with any bit set above the
/// lowest (payload past 64 bits, or a continuation bit), and so anything
/// longer than ten bytes. A plain count of terminal bytes, which this used to
/// be, catches only the first: a ten-byte varint ending in `0x02` counted as
/// one value here and decoded as none, so a "validated" dense group yielded
/// one node fewer than its id count, and the independently decoded columnar
/// arrays could pair an id with another node's coordinates.
///
/// Byte `i` of a varint (zero-based) is accepted as-is for `i < 9` and must be
/// at most `1` at `i == 9`. Overlong encodings that stay within ten bytes are
/// accepted, as the decoder accepts them, and so are sign-extended ten-byte
/// negative `int32` values. This is a byte scan, not a decode - no value is
/// reconstructed. `packed_varint_count_matches_read_varint` pins the
/// equivalence against the decoder itself.
#[inline]
pub(crate) fn packed_varint_count(data: &[u8]) -> Result<usize> {
    let mut count = 0usize;
    // Index of the current byte within its varint.
    let mut pos = 0u8;
    for &b in data {
        if pos == 9 {
            if b > 1 {
                return Err(overlong_varint_err());
            }
            count += 1;
            pos = 0;
        } else if b & 0x80 == 0 {
            count += 1;
            pos = 0;
        } else {
            pos += 1;
        }
    }
    if pos != 0 {
        return Err(new_wire_error("packed varint array ends mid-varint"));
    }
    Ok(count)
}

/// A varint that runs past ten bytes or carries payload past bit 63 - one the
/// decoder refuses, which the packed iterators would turn into a silent end.
#[cold]
#[inline(never)]
fn overlong_varint_err() -> crate::error::Error {
    new_wire_error("packed varint array holds a varint wider than 64 bits")
}

/// [`packed_varint_count`] for a `uint32` string-index array (`keys` / `vals`
/// on Node, Way and Relation), refusing any value above `u32::MAX` in the
/// same single byte scan.
///
/// `PackedUint32Iter` cannot report an error and narrows each varint to
/// `u32`, so an index above `u32::MAX` would silently alias a different,
/// possibly valid, string. The bound is checked here, once per block at
/// construction, rather than in `WireNode` / `WireWay` / `WireRelation`
/// parsing: those run again per element on every iteration and in the raw
/// scanners (altw's way-ref scan never reads tags at all), so a check there
/// is an extra traversal of both arrays on the decode hot path. Folding it
/// into the count walk this validation already performs costs no extra pass.
///
/// Byte `i` of a varint carries value bits `7i..7i+7`, so a value fits `u32`
/// iff byte 4 carries nothing above bit 31 (payload at most `0x0f`) and every
/// later byte carries nothing at all. An overlong encoding of a small value
/// (zero payload bytes past the fifth) therefore fits, as it does for a
/// decoding reader - up to the decoder's own ten-byte limit, past which it
/// refuses the varint (see [`packed_varint_count`]), so a continuation bit on
/// the tenth byte is refused here too.
#[inline]
fn packed_uint32_count(data: &[u8]) -> Result<usize> {
    let mut count = 0usize;
    // Index of the current byte within its varint; never past 9, because the
    // tenth byte either terminates the varint or is refused.
    let mut pos = 0u8;
    for &b in data {
        let payload = b & 0x7f;
        let too_wide = match pos {
            0..=3 => false,
            4 => payload > 0x0f,
            _ => payload != 0,
        };
        if too_wide {
            return Err(new_wire_error("tag string index exceeds uint32"));
        }
        if b & 0x80 == 0 {
            count += 1;
            pos = 0;
        } else if pos == 9 {
            return Err(overlong_varint_err());
        } else {
            pos += 1;
        }
    }
    if pos != 0 {
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
/// re-parses can fail. The iterators no longer skip or end on one; they treat
/// a failure as a broken construction invariant and panic through
/// [`validated`], because any fallback would be the silent loss this pass
/// exists to prevent. `WireMessageIter` is the exception: it is shared framing
/// plumbing (this pass drives it too, as does the raw node scanner), its every
/// caller walks the framing first, and its doc says why its error arms are
/// unreachable.
///
/// # What is checked
///
/// Per group, in order: the group's own top-level field framing; each `Node`,
/// `Way` and `Relation` sub-message parses; each element's `Info` sub-message
/// parses; every packed array on those elements holds only complete, bounded
/// varints, and every `keys` / `vals` string index fits `u32`; the
/// parallel-array invariants the zipping iterators depend on hold
/// (`keys`/`vals` on all three kinds, `lat`/`lon`/`refs` on ways,
/// `roles_sid`/`memids`/`types` on relations); the `DenseNodes` sub-message
/// appears at most once (`WireGroup::dense`) and parses; its packed `id`, `lat` and `lon` arrays are well-formed and of
/// equal length; its `keys_vals` array describes exactly as many per-node tag
/// runs as there are ids, and every string index in it fits a nonnegative
/// `int32` (the dense tag iterators narrow to `i32`); and every array in its
/// optional `denseinfo` sub-message is either absent or exactly as long as the
/// id array.
///
/// # What is deliberately not checked
///
/// Coordinate and timestamp *values*. The block header scan in `wire.rs`
/// checks that both scales fit their declared signed int32 type, without
/// imposing positivity or checking the scaled values. Delta reconstruction
/// is modular, and so is the step after it: `offset + granularity * raw` for
/// coordinates and `raw * date_granularity` for timestamps use wrapping `i64`
/// arithmetic in every block-API accessor, so debug and release agree and
/// nothing panics.
/// A value whose scaling runs past `i64` is garbage in, garbage out, exactly
/// like the far larger class of in-range but meaningless values (a latitude
/// of 10^6 degrees) that no reader refuses, and like the `as i32` narrowing
/// of the decimicrodegree accessors. Refusing overflow here would mean
/// decoding every `lat` / `lon` array at construction instead of the byte
/// count scan below - a second decode of the bulk of every file, on every
/// read - to reject only the narrowest slice of nonsense. Nor can a bound
/// come from the header alone: modular deltas let an accumulated value be any
/// `i64`, so every header except granularity 0, or granularity 1 with a
/// zero offset, admits some overflowing coordinate.
/// Paths that turn coordinates into output they vouch for (the altw node
/// scanner in `scan/node.rs`, apply-changes' `node_coords.rs` extractor) do
/// their own checked scaling and narrowing during the decode they already
/// perform.
///
/// The framing walk comes first deliberately - it is the check that makes the
/// element iterators used below unable to terminate early, so running the
/// element walk first would silently examine a prefix.
///
/// This is a structural pass, not a decode: sub-message parsing walks field
/// headers and records slice bounds without decoding any packed array. Every
/// check but one is a byte scan; the exception is `keys_vals`, whose
/// zero-terminated per-node runs have to be walked varint by varint because
/// nothing in the encoding states how many nodes an array describes.
/// No stringtable index is resolved (that is `str_from_stringtable`'s job,
/// which is already fallible).
///
/// Consequently a block accepted here is not a certificate that raw rewrite
/// indices are in table bounds. Enforcing that globally would change when
/// out-of-range strings fail and require decoding regular-element keys and
/// values, relation roles and Info/DenseInfo user indices even for readers
/// that never access those strings (dense `keys_vals` is the one array this
/// pass already decodes, for its run count and int32 width checks).
/// Keep structural validation distinct from string resolution; do not infer
/// a raw rewrite sanitization guarantee from successful construction.
///
/// No error raised here carries a byte offset. These sites sit below the
/// framing layer and cannot know their position in the file; the enclosing
/// `BlobReader` stamps the blob's offset on the way out, and the innermost-wins
/// rule means an absent inner offset never displaces a more precise one.
///
/// Returns the kinds of the elements the block holds
/// ([`KindSet`](crate::element_kind::KindSet)), recorded in the loops this
/// pass runs anyway: one bit-or per element, and one per dense group with at
/// least one ID. And how many elements the block holds, the count
/// `PrimitiveBlock::elements` yields: one per plain node, way and relation
/// message and the dense ID count each dense group's validation computes.
fn validate_groups_strict(block: &WireBlock<'_>) -> Result<(crate::element_kind::KindSet, u64)> {
    use super::wire::Cursor;
    use crate::ElementKind;

    let mut kinds = crate::element_kind::KindSet::default();
    let mut count: u64 = 0;
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
            kinds.add(ElementKind::Node);
            count += 1;
            let node = WireNode::parse(element)?;
            validate_info(node.info_data)?;
            validate_tag_arrays(
                node.keys_data,
                node.vals_data,
                "node has mismatched keys/vals tag array lengths",
            )?;
        }
        for element in group.ways() {
            kinds.add(ElementKind::Way);
            count += 1;
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
            kinds.add(ElementKind::Relation);
            count += 1;
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
            validate_dense_keys_vals(dense.keys_vals_data, ids)?;
            if let Some(info_data) = dense.info_data {
                validate_dense_info_arrays(info_data, ids)?;
            }
            if ids > 0 {
                kinds.add(ElementKind::Node);
            }
            count += ids as u64;
        }
    }

    Ok((kinds, count))
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
    let keys = packed_uint32_count(keys_data)?;
    let vals = packed_uint32_count(vals_data)?;
    if keys != vals {
        return Err(new_wire_error(mismatch));
    }
    Ok(())
}

/// Validate that `keys_vals` describes exactly `node_count` per-node tag runs.
///
/// `keys_vals` is one flat array holding the tags of every node in the dense
/// group as `(key, val)` varint pairs, each node's run terminated by a single
/// `0` varint. Nothing in the encoding records how many nodes it covers, so a
/// short array cannot be caught by comparing lengths the way the `id` / `lat` /
/// `lon` triple is: it has to be walked. `DenseNodeIter` walks it lazily and
/// cannot report an error, so a short array silently gives the trailing nodes
/// no tags and a long one silently discards the tail - both of which this
/// rejects.
///
/// Two encodings of the same content are accepted, because both occur in the
/// wild:
///
/// * an empty array, which means "no node in this group has tags"; and
/// * a final run that ends at the end of the array without its `0` terminator,
///   which is what a writer that omits the trailing delimiter produces. It is
///   counted as a complete run, matching what `DenseNodeIter` now yields.
///
/// A run whose final key varint has no matching value varint is corruption and
/// is rejected: `DenseTagIter` would drop that tag with no signal.
fn validate_dense_keys_vals(keys_vals_data: &[u8], node_count: usize) -> Result<()> {
    use super::wire::Cursor;

    if keys_vals_data.is_empty() {
        return Ok(());
    }

    let mut cursor = Cursor::new(keys_vals_data);
    let mut runs = 0usize;
    let mut in_run = false;
    // Dense tag iterators narrow to i32. Require nonnegative int32 string
    // indices here, including values, so high bits cannot alias a valid sid.
    // This is a width check, not string-table resolution: tags_checked still
    // reports a fitting index that points beyond the table.
    while !cursor.is_empty() {
        let key = cursor.read_varint()?;
        if key > 0x7fff_ffff {
            return Err(new_wire_error("dense tag string index exceeds int32"));
        }
        if key == 0 {
            runs += 1;
            in_run = false;
            continue;
        }
        if cursor.is_empty() {
            return Err(new_wire_error(
                "dense keys_vals ends with a key that has no value",
            ));
        }
        if cursor.read_varint()? > 0x7fff_ffff {
            return Err(new_wire_error("dense tag string index exceeds int32"));
        }
        in_run = true;
    }
    // A trailing run with no terminating 0 still describes one node.
    if in_run {
        runs += 1;
    }

    if runs != node_count {
        return Err(new_wire_error(
            "dense keys_vals describes a different number of nodes than the id array",
        ));
    }
    Ok(())
}

/// Validate the six parallel arrays of a dense group's `denseinfo`.
///
/// `DenseNodeInfoIter` zips them and stops at the shortest, so one short array
/// makes metadata vanish for every node from that point on while iteration
/// reports success. Each array must therefore be either absent (length 0) or
/// exactly `node_count` long.
///
/// libosmium tolerates an individually absent array - it tests each for
/// emptiness per node - so absence alone is not corruption. What it cannot
/// express, and what this additionally rejects, is a `denseinfo` that carries
/// *some* of the five core arrays and not the others: our iterator yields
/// `None` for every node in that case, which is the same silent metadata loss
/// as a short array. Either all five core arrays are present at full length or
/// none of them is.
///
/// Values are not width-checked, deliberately: `version` (int32) and the
/// `uid` / `user_sid` deltas (sint32) narrow as protobuf specifies, a cast and
/// a truncation to 32 bits before the zigzag step, so only a malformed writer
/// can make one alias another; a per-byte width scan on these per-node arrays
/// was tried and rejected, since it adds per-byte work to every well-formed
/// dense block to refuse only malformed input.
/// Truncation is also what
/// reads back the ten-byte sign-extended sint32 deltas that `BlockBuilder`
/// wrote through protohoggr before 0.5.0, which files already on disk still
/// carry. See
/// `reference/truncation-handling.md`, "Which varints count as complete".
fn validate_dense_info_arrays(info_data: &[u8], node_count: usize) -> Result<()> {
    let info = WireDenseInfo::parse(info_data)?;
    let core = [
        info.version_data,
        info.timestamp_data,
        info.changeset_data,
        info.uid_data,
        info.user_sid_data,
    ];

    let mut present = 0usize;
    for data in core {
        let count = packed_varint_count(data)?;
        if count == 0 {
            continue;
        }
        if count != node_count {
            return Err(new_wire_error(
                "dense denseinfo array length does not match the id array",
            ));
        }
        present += 1;
    }

    // `visible` is genuinely optional - it only appears in history files - so
    // it is length-checked but never required.
    let visible = packed_varint_count(info.visible_data)?;
    if visible != 0 && visible != node_count {
        return Err(new_wire_error(
            "dense denseinfo visible array length does not match the id array",
        ));
    }

    if present != 0 && present != core.len() {
        return Err(new_wire_error(
            "dense denseinfo has some but not all of its parallel arrays",
        ));
    }
    if present == 0 && visible != 0 {
        return Err(new_wire_error(
            "dense denseinfo has a visible array but no metadata arrays",
        ));
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
///
/// `roles_sid` and `types` (int32) are counted, not width-checked: a value
/// outside `i32` narrows with a cast, as protobuf specifies. The role then
/// resolves through the checked string lookup and an unknown type is
/// represented as `MemberType::Unknown`; the one exception is the debug-build
/// raw-passthrough assertion in `BlockBuilder`, which reads roles as raw
/// `u64`. Renumber's raw rewriter dispatches on the same truncated type. See
/// `reference/truncation-handling.md`.
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
/// `Node::info`, `Way::info` and `Relation::info` (`validated_info` in
/// `elements.rs`) re-parse this same slice per call and treat a parse failure
/// as a broken invariant, panicking: this check is what makes that failure
/// unreachable. It covers the element's only metadata, because the element
/// parsers refuse a second data-bearing `Info` (`take_message` in `wire.rs`).
/// It is a parse check only; the user string index is not resolved.
#[inline]
fn validate_info(info_data: Option<&[u8]>) -> Result<()> {
    match info_data {
        Some(data) => super::wire::WireInfo::parse(data).map(|_| ()),
        None => Ok(()),
    }
}

/// Unwrap a re-parse of bytes that block construction already validated.
///
/// The block iterators (`PrimitiveGroup`, `BlockElementsIter`, the group
/// element iterators, `DenseNodeIter`) and the element `info()` accessors
/// re-parse slices of a block's bytes - a group's `DenseNodes`, `DenseInfo`
/// and `keys_vals` varints, each `Node` / `Way` / `Relation`, each element
/// `Info` - because the block stores no parsed layout. Every one of those re-parses
/// runs a parser that [`validate_groups_strict`] already ran, successfully,
/// on the same immutable bytes: `PrimitiveBlock::finish` is the only
/// constructor and validates before handing out a block, and the iterators
/// derive their slices from that block themselves. A failure here is
/// therefore a pbfhogg bug (a slice that never went through validation, or a
/// validator that drifted from the parser the iterator runs), not a property
/// of the input, and it panics naming `what` failed.
///
/// It must not fall back instead. Every available fallback is a silent wrong
/// answer: a dense group that reads as no nodes, a dense group whose metadata
/// reads as absent, a dense node whose tags end early, an element skipped, an
/// `Info` that reads as version 0 / timestamp 0. Corrupt input is refused at construction, where an error can
/// be returned; this is the consumption side of that contract.
///
/// The success path is a match; the failure path is a cold, never-inlined
/// call.
///
/// Provenance - that a slice handed to an iterator constructor came from the
/// validated block it is paired with - is enforced by encapsulation rather
/// than by a type: the constructors that pair a slice with its block are
/// private to `src/read`, and each derives the slice from that block. A
/// validated-block wrapper type alone would not prove more, because these
/// constructors take sub-slices alongside the block. Provenance-carrying
/// views (a private pairing type assembled only where the slice is derived)
/// are possible without runtime cost, but would restate the boundary this
/// module already keeps, not replace the invariant or this check.
#[inline]
#[track_caller]
pub(super) fn validated<T, E: std::fmt::Display>(
    result: std::result::Result<T, E>,
    what: &'static str,
) -> T {
    match result {
        Ok(value) => value,
        Err(err) => construction_invariant_broken(what, &err),
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn construction_invariant_broken(what: &'static str, err: &dyn std::fmt::Display) -> ! {
    panic!(
        "{what} failed to parse after PrimitiveBlock construction validated it \
         (validate_groups_strict in block.rs): construction invariant broken: {err}"
    )
}

/// The dense group of a group in a validated block, if it has one.
///
/// Both [`WireGroup::dense`] and [`WireDenseNodes::parse`] ran successfully on
/// these bytes in [`validate_groups_strict`]; see [`validated`].
#[inline]
#[track_caller]
fn validated_dense<'a>(group: &WireGroup<'a>) -> Option<WireDenseNodes<'a>> {
    validated(group.dense(), "PrimitiveGroup.dense")
        .map(|data| validated(WireDenseNodes::parse(data), "DenseNodes"))
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
    ///
    /// # Panics
    ///
    /// Only on a broken internal invariant: a dense group or its `DenseInfo`
    /// that fails to parse although block construction validated it. That is
    /// a pbfhogg bug, not a property of the input; corrupt input is refused
    /// when the block is constructed.
    pub fn dense_nodes(&self) -> DenseNodeIter<'a> {
        match validated_dense(&self.group) {
            Some(dense) => DenseNodeIter::new(self.block, dense),
            None => DenseNodeIter::empty(self.block),
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
                self.dense_nodes = match validated_dense(&group) {
                    Some(dense) if self.skip_metadata => {
                        DenseNodeIter::new_skip_metadata(self.block, dense)
                    }
                    Some(dense) => DenseNodeIter::new(self.block, dense),
                    None => DenseNodeIter::empty(self.block),
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
            ElementsIterState::Node => match self.nodes.next() {
                Some(data) => {
                    let wire_node = validated(WireNode::parse(data), "Node");
                    Some(Some(Element::Node(Node::new(self.block, wire_node))))
                }
                None => {
                    self.state = ElementsIterState::Way;
                    None
                }
            },
            ElementsIterState::Way => match self.ways.next() {
                Some(data) => {
                    let wire_way = validated(WireWay::parse(data), "Way");
                    Some(Some(Element::Way(Way::new(self.block, wire_way))))
                }
                None => {
                    self.state = ElementsIterState::Relation;
                    None
                }
            },
            ElementsIterState::Relation => match self.relations.next() {
                Some(data) => {
                    let wire_rel = validated(WireRelation::parse(data), "Relation");
                    Some(Some(Element::Relation(Relation::new(self.block, wire_rel))))
                }
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

    /// # Panics
    ///
    /// Only on a broken internal invariant; see [`PrimitiveGroup::dense_nodes`].
    fn next(&mut self) -> Option<Self::Item> {
        let data = self.iter.next()?;
        Some(Node::new(
            self.block,
            validated(WireNode::parse(data), "Node"),
        ))
    }
}

/// An iterator over the ways in a [`PrimitiveGroup`].
pub struct GroupWayIter<'a> {
    block: &'a WireBlock<'static>,
    iter: WireMessageIter<'a>,
}

impl<'a> Iterator for GroupWayIter<'a> {
    type Item = Way<'a>;

    /// # Panics
    ///
    /// Only on a broken internal invariant; see [`PrimitiveGroup::dense_nodes`].
    fn next(&mut self) -> Option<Self::Item> {
        let data = self.iter.next()?;
        Some(Way::new(self.block, validated(WireWay::parse(data), "Way")))
    }
}

/// An iterator over the relations in a [`PrimitiveGroup`].
pub struct GroupRelationIter<'a> {
    block: &'a WireBlock<'static>,
    iter: WireMessageIter<'a>,
}

impl<'a> Iterator for GroupRelationIter<'a> {
    type Item = Relation<'a>;

    /// # Panics
    ///
    /// Only on a broken internal invariant; see [`PrimitiveGroup::dense_nodes`].
    fn next(&mut self) -> Option<Self::Item> {
        let data = self.iter.next()?;
        Some(Relation::new(
            self.block,
            validated(WireRelation::parse(data), "Relation"),
        ))
    }
}

/// Look up a stringtable entry by index, returning it as a `&str`.
///
/// Uses `from_utf8_unchecked` - the safety relies on the invariant established
/// by `PrimitiveBlock::finish`, which every constructor funnels through and
/// which validates every stringtable entry at construction time.
pub(crate) fn str_from_stringtable<'a>(block: &'a WireBlock<'_>, index: usize) -> Result<&'a str> {
    if let Some(bytes) = block.stringtable.get(index) {
        // SAFETY: All stringtable entries were validated as UTF-8 in
        // PrimitiveBlock::finish, which every constructor funnels through. The
        // PrimitiveBlock struct does not expose any mutable access to the
        // underlying buffer, so entries cannot have been modified since
        // construction.
        Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
    } else {
        Err(new_stringtable_index_out_of_bounds(index))
    }
}

/// Construct a key-value tuple from key/value indexes, using the stringtable from a block.
///
/// Returns `None` both when either index is absent (end of the tag arrays) and
/// when either lookup fails: the `StringtableIndexOutOfBounds` error from
/// [`str_from_stringtable`] is discarded here, not propagated. The tag
/// iterators that call this therefore end early on a bad index instead of
/// erroring. See CORRECTNESS.md "Tag iterators end early on an out-of-range
/// string index"; a caller that must reject such input uses the checked
/// iterators (`tags_checked()`), which resolve through `str_from_stringtable`
/// and keep the error.
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
    use crate::read::elements::Element;

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

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            #[allow(clippy::cast_possible_truncation)]
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        #[allow(clippy::cast_possible_truncation)]
        out.push(v as u8);
    }

    /// A block holding one dense node with the given raw lat (lon 0) under
    /// the given granularity and lat_offset.
    fn scaled_dense_block(granularity: i32, lat_offset: i64, raw_lat: i64) -> PrimitiveBlock {
        let zz = |v: i64| ((v << 1) ^ (v >> 63)).cast_unsigned();
        let mut lat = Vec::new();
        varint(zz(raw_lat), &mut lat);
        let mut dense = len_field(1, &[0x02]);
        dense.extend_from_slice(&len_field(8, &lat));
        dense.extend_from_slice(&len_field(9, &[0x00]));
        let mut buffer = block_with_groups(&[len_field(2, &dense)]);
        varint(17 << 3, &mut buffer);
        varint(i64::from(granularity).cast_unsigned(), &mut buffer);
        varint(19 << 3, &mut buffer);
        varint(lat_offset.cast_unsigned(), &mut buffer);
        PrimitiveBlock::from_vec(buffer).unwrap()
    }

    /// Every dense-node reader puts a node at the same coordinate: the
    /// columnar decode (extract smart, multi-extract) and the element
    /// accessors (every other reader) always, and the vouching node scanner
    /// (altw, apply-changes -l) wherever it accepts the value. That holds on
    /// the cancelling encoding, whose product overflows `i64` while the
    /// offset brings it back to 100 nanodegrees, and on a block whose true
    /// nanodegree value is past `i64`, where columnar and accessors give the
    /// same wrapped value (the deleted granularity-100 shortcut did not).
    #[test]
    fn columnar_accessors_and_scanner_agree_on_coordinates() {
        let cases: [(i32, i64, i64); 5] = [
            (100, 0, 550_000_000),
            (100, -9_223_372_036_854_775_800, 92_233_720_368_547_759),
            (100, 9_223_372_036_854_775_799, -92_233_720_368_547_759),
            (1000, 500, -35),
            // True value 100 * (i64::MAX / 50) is past i64.
            (100, 0, i64::MAX / 50),
        ];
        for (granularity, lat_offset, raw_lat) in cases {
            let block = scaled_dense_block(granularity, lat_offset, raw_lat);
            let mut columns = crate::read::columnar::DenseNodeColumns::new();
            block.decode_dense_columns(&mut columns);
            let accessor: Vec<i32> = block
                .elements()
                .filter_map(|e| match e {
                    Element::DenseNode(n) => Some(n.decimicro_lat()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                columns.lats, accessor,
                "case {granularity} {lat_offset} {raw_lat}"
            );
            if let Some(scanned) =
                crate::scan::node::scale_to_decimicro(lat_offset, i64::from(granularity), raw_lat)
            {
                assert_eq!(accessor, [scanned], "scanner, case {lat_offset} {raw_lat}");
            }
        }
        // The cancelling encodings scan, at 1 and -1 decimicrodegrees.
        assert_eq!(
            crate::scan::node::scale_to_decimicro(
                -9_223_372_036_854_775_800,
                100,
                92_233_720_368_547_759
            ),
            Some(1)
        );
        assert_eq!(
            crate::scan::node::scale_to_decimicro(
                9_223_372_036_854_775_799,
                100,
                -92_233_720_368_547_759
            ),
            Some(-1)
        );
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
            // "mismatched" alone also appears in the way and relation array
            // messages, so name the dense guard specifically.
            format!("{err}").contains("dense group has mismatched id/lat/lon"),
            "expected the dense id/lat/lon mismatch error, got: {err}"
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

    /// A ten-byte varint whose last byte carries payload past bit 63 counts as
    /// one terminal byte but decodes as nothing, so a count-only check accepted
    /// it and the dense iterator then yielded one node fewer than the id count.
    /// Every count here is 1, so only the width check can reject it.
    #[test]
    fn dense_packed_array_with_overlong_varint_is_rejected() {
        let mut overflow = vec![0x80u8; 9];
        overflow.push(0x02);
        let mut dense = len_field(1, &overflow);
        dense.extend_from_slice(&len_field(8, &[0x00]));
        dense.extend_from_slice(&len_field(9, &[0x00]));
        let err = PrimitiveBlock::from_vec(block_with_groups(&[len_field(2, &dense)]))
            .expect_err("a varint wider than 64 bits must be rejected");
        assert!(
            format!("{err}").contains("wider than 64 bits"),
            "expected the varint width check, got: {err}"
        );

        // The widest legal ten-byte varint is the passing counterpart.
        let mut widest = vec![0xffu8; 9];
        widest.push(0x01);
        let mut good = len_field(1, &widest);
        good.extend_from_slice(&len_field(8, &[0x00]));
        good.extend_from_slice(&len_field(9, &[0x00]));
        let block = PrimitiveBlock::from_vec(block_with_groups(&[len_field(2, &good)]))
            .expect("a legal ten-byte varint must parse");
        assert_eq!(block.elements().count(), 1);
    }

    /// The packed-array validators accept exactly the arrays the decoder
    /// consumes in full. Every packed iterator turns a `read_varint` error into
    /// an end of iteration, so any input the validator accepts and the decoder
    /// does not is a silently short array behind a passed parallel-length
    /// check. The oracle is the decoder itself, stopping at its first error;
    /// for the `uint32` validator it also requires every value to fit `u32`.
    #[test]
    fn packed_varint_count_matches_read_varint() {
        use crate::read::wire::Cursor;

        fn oracle(data: &[u8], max: u64) -> Option<usize> {
            let mut cursor = Cursor::new(data);
            let mut count = 0;
            while !cursor.is_empty() {
                let value = cursor.read_varint().ok()?;
                if value > max {
                    return None;
                }
                count += 1;
            }
            Some(count)
        }

        fn check(data: &[u8]) {
            assert_eq!(
                super::packed_varint_count(data).ok(),
                oracle(data, u64::MAX),
                "packed_varint_count disagrees with read_varint on {data:02x?}"
            );
            assert_eq!(
                super::packed_uint32_count(data).ok(),
                oracle(data, u64::from(u32::MAX)),
                "packed_uint32_count disagrees with read_varint on {data:02x?}"
            );
        }

        // Exhaustive short strings over bytes that sit on every boundary the
        // validators care about: terminal versus continuation, payload above
        // the u32 limit in byte 4, and the tenth-byte limit of 1.
        let alphabet = [0x00u8, 0x01, 0x02, 0x0f, 0x10, 0x7f, 0x80, 0x81, 0x8f, 0xff];
        let mut buf = Vec::new();
        for len in 0..=5u32 {
            for mut index in 0..alphabet.len().pow(len) {
                buf.clear();
                for _ in 0..len {
                    buf.push(alphabet[index % alphabet.len()]);
                    index /= alphabet.len();
                }
                check(&buf);
            }
        }

        // Random strings almost never reach a tenth byte, so build that case
        // directly: every continuation run up to eleven bytes, then every
        // possible next byte, with and without a valid value before it and
        // with a terminal, a continuation, or nothing after it.
        for fill in [0x80u8, 0x8f, 0xff] {
            for run in 0..=11 {
                for last in 0..=255u8 {
                    for prefix in [&[][..], &[0x02]] {
                        for suffix in [&[][..], &[0x02], &[0x80]] {
                            buf.clear();
                            buf.extend_from_slice(prefix);
                            buf.extend(std::iter::repeat_n(fill, run));
                            buf.push(last);
                            buf.extend_from_slice(suffix);
                            check(&buf);
                        }
                    }
                }
            }
        }

        // Longer pseudo-random strings, biased toward continuation bytes so
        // multi-byte and overlong varints are common.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            buf.clear();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            #[allow(clippy::cast_possible_truncation)]
            let len = (state % 40) as usize;
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                #[allow(clippy::cast_possible_truncation)]
                let byte = state as u8;
                buf.push(if state & 0x300 == 0 {
                    byte & 0x7f
                } else {
                    byte | 0x80
                });
            }
            check(&buf);
        }
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

    /// A block view over `buffer` that skipped `validate_groups_strict` - what
    /// `PrimitiveBlock::finish` builds, minus the validation. No production
    /// path can produce one; it exists so the tests below can reach the
    /// consumption side of the construction contract.
    fn unvalidated_block(mut buffer: Vec<u8>) -> PrimitiveBlock {
        let meta = super::WireBlock::parse_and_inline(&mut buffer).unwrap();
        let bytes = bytes::Bytes::from(buffer);
        let block = super::WireBlock::from_inline(&bytes, &meta);
        // SAFETY: as in `PrimitiveBlock::finish`: `bytes` owns the heap data
        // `block` borrows and moves into the same `PrimitiveBlock`, which
        // never hands out mutable access to it.
        #[allow(clippy::transmute_undefined_repr)]
        let block = unsafe {
            std::mem::transmute::<super::WireBlock<'_>, super::WireBlock<'static>>(block)
        };
        PrimitiveBlock {
            _buffer: bytes,
            block,
            kinds: crate::element_kind::KindSet::default(),
            element_count: 0,
        }
    }

    /// Run `consume` and return its panic message, failing if it does not
    /// panic.
    fn panic_message(consume: impl FnOnce()) -> String {
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(consume))
            .expect_err("consumption must panic");
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default()
    }

    /// Bytes that block construction refuses are refused there, and a
    /// consumer handed them anyway (a block view that bypassed
    /// `validate_groups_strict`) panics naming the broken invariant instead
    /// of reading a corrupt dense group as no nodes, dropping dense metadata,
    /// or skipping an element.
    ///
    /// Each case is checked on both sides: `from_vec` returns `Err` (the
    /// public contract), and every consumer that re-parses those bytes
    /// panics with a message naming what failed (the internal invariant).
    /// The three dense failures are distinct parsers: `WireGroup::dense`
    /// (a repeated `dense` field), `WireDenseNodes::parse` (a truncated
    /// field inside `DenseNodes`) and `WireDenseInfo::parse` (a truncated
    /// field inside `denseinfo`).
    #[test]
    fn consumers_panic_on_bytes_construction_would_refuse() {
        // A length-delimited field 1 declaring 5 bytes and supplying 1.
        let truncated_field = [0x0au8, 0x05, 0x02];

        let mut repeated_dense = len_field(2, &[]);
        repeated_dense.extend_from_slice(&len_field(2, &[]));
        let bad_dense_nodes = len_field(2, &truncated_field);
        let bad_dense_info = dense_group_with(1, &[], Some(&truncated_field));

        let dense_cases = [
            (repeated_dense, "PrimitiveGroup.dense"),
            (bad_dense_nodes, "DenseNodes"),
            (bad_dense_info.clone(), "DenseInfo"),
        ];
        for (group, what) in dense_cases {
            let buffer = block_with_groups(&[group]);
            PrimitiveBlock::from_vec(buffer.clone())
                .expect_err("construction must refuse the fixture");
            let expected = format!("{what} failed to parse after PrimitiveBlock construction");

            let block = unvalidated_block(buffer.clone());
            let msg = panic_message(|| {
                block.groups().for_each(|g| g.dense_nodes().for_each(drop));
            });
            assert!(msg.contains(&expected), "dense_nodes, {what}: {msg}");

            let block = unvalidated_block(buffer.clone());
            let msg = panic_message(|| block.elements().for_each(drop));
            assert!(msg.contains(&expected), "elements, {what}: {msg}");

            let block = unvalidated_block(buffer.clone());
            let msg = panic_message(|| block.for_each_element(|_| {}));
            assert!(msg.contains(&expected), "for_each_element, {what}: {msg}");

            if what != "DenseInfo" {
                // The metadata-skipping iterator and the columnar decode never
                // parse `denseinfo`, so only the group-level failures reach
                // them.
                let block = unvalidated_block(buffer.clone());
                let msg = panic_message(|| block.elements_skip_metadata().for_each(drop));
                assert!(
                    msg.contains(&expected),
                    "elements_skip_metadata, {what}: {msg}"
                );

                let block = unvalidated_block(buffer);
                let msg = panic_message(|| {
                    let mut columns = crate::read::columnar::DenseNodeColumns::new();
                    block.decode_dense_columns(&mut columns);
                });
                assert!(
                    msg.contains(&expected),
                    "decode_dense_columns, {what}: {msg}"
                );
            }
        }

        // The metadata-skipping iterator still reads the nodes of a group
        // whose only defect is its denseinfo: it never looks at it.
        let block = unvalidated_block(block_with_groups(&[bad_dense_info]));
        assert_eq!(block.elements_skip_metadata().count(), 1);

        // `keys_vals` that `validate_dense_keys_vals` refuses: a key varint
        // cut short, a value varint cut short, and a final key with no value.
        // `DenseNodeIter` walks the same varints per node; it used to end the
        // node's tags early.
        let keys_vals_cases: [(&[u8], usize, &str); 3] = [
            (&[0x81], 1, "DenseNodes keys_vals key"),
            (&[0x01, 0x81], 1, "DenseNodes keys_vals value"),
            (&[0x01, 0x01, 0x00, 0x02], 2, "DenseNodes keys_vals value"),
        ];
        for (keys_vals, nodes, what) in keys_vals_cases {
            let buffer = block_with_groups(&[dense_group_with(nodes, keys_vals, None)]);
            PrimitiveBlock::from_vec(buffer.clone())
                .expect_err("construction must refuse the fixture");
            let expected = format!("{what} failed to parse after PrimitiveBlock construction");
            for skip_metadata in [false, true] {
                let block = unvalidated_block(buffer.clone());
                let msg = panic_message(|| {
                    if skip_metadata {
                        block.elements_skip_metadata().for_each(drop);
                    } else {
                        block.elements().for_each(drop);
                    }
                });
                assert!(
                    msg.contains(&expected),
                    "{what}, skip {skip_metadata}: {msg}"
                );
            }
        }

        // A valid element followed by one whose `keys` field declares more
        // bytes than it holds, on each element kind: used to be skipped.
        let truncated_element = [0x12u8, 0x05];
        for (field, what) in [(1u8, "Node"), (3, "Way"), (4, "Relation")] {
            let mut group = len_field(field, &[0x08, 0x01]);
            group.extend_from_slice(&len_field(field, &truncated_element));
            let buffer = block_with_groups(&[group]);
            PrimitiveBlock::from_vec(buffer.clone())
                .expect_err("construction must refuse the fixture");
            let expected = format!("{what} failed to parse after PrimitiveBlock construction");

            let block = unvalidated_block(buffer.clone());
            let msg = panic_message(|| block.elements().for_each(drop));
            assert!(msg.contains(&expected), "elements, {what}: {msg}");

            let block = unvalidated_block(buffer.clone());
            let msg = panic_message(|| block.elements_skip_metadata().for_each(drop));
            assert!(
                msg.contains(&expected),
                "elements_skip_metadata, {what}: {msg}"
            );

            let block = unvalidated_block(buffer);
            let msg = panic_message(|| block.for_each_element(|_| {}));
            assert!(msg.contains(&expected), "group iterator, {what}: {msg}");
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

    /// `TagIter` on a tag index past the string table: its `size_hint` lower
    /// bound must not promise the remaining tags (iteration ends there), and
    /// the end is final - the tags after the bad one never resume. The
    /// zipped `RawTagIter` hint stays exact on the same arrays.
    #[test]
    fn tag_iter_hint_and_fusing_on_out_of_range_string_index() {
        use std::iter::FusedIterator;
        fn assert_fused<I: FusedIterator>(_: &I) {}

        // The fixture string table has one entry (index 0); key index 1 on
        // the second tag points past it. Three tags, all one-byte varints.
        let mut way = vec![0x08u8, 0x01];
        way.extend_from_slice(&len_field(2, &[0x00, 0x01, 0x00]));
        way.extend_from_slice(&len_field(3, &[0x00, 0x00, 0x00]));
        let block = PrimitiveBlock::from_vec(block_with_groups(&[len_field(3, &way)]))
            .expect("bad string indices are not a construction error");
        let Some(Element::Way(w)) = block.elements().next() else {
            panic!("expected one way");
        };

        let raw = w.raw_tags();
        assert_eq!(raw.size_hint(), (0, Some(3)));
        assert_eq!(raw.count(), 3);

        let mut tags = w.tags();
        assert_fused(&tags);
        let (lower, upper) = tags.size_hint();
        assert_eq!(lower, 0);
        assert_eq!(upper, Some(3));
        assert_eq!(tags.next(), Some(("", "")));
        assert_eq!(tags.next(), None);
        // The third tag is valid; a non-fused iterator would resume with it.
        assert_eq!(tags.next(), None);
        assert_eq!(tags.size_hint(), (0, Some(0)));
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

        // A malformed Info followed by a valid one: the element parser used to
        // keep only the last, so validation saw the valid one and the
        // malformed one went unchecked. Two data-bearing Infos are refused.
        let mut shadowed = vec![0x08u8, 0x01];
        shadowed.extend_from_slice(&bad_info);
        shadowed.extend_from_slice(&len_field(4, &[0x08, 0x01]));
        PrimitiveBlock::from_vec(block_with_groups(&[len_field(1, &shadowed)]))
            .expect_err("a malformed Info shadowed by a later valid one must be rejected");
    }

    /// `info()` reads present metadata through the validated slice and
    /// absent metadata as `None` (checked on `version`), on all three element
    /// kinds.
    #[test]
    fn element_info_reads_present_and_absent_metadata() {
        let mut with_info = vec![0x08u8, 0x01];
        with_info.extend_from_slice(&len_field(4, &[0x08, 0x07])); // version = 7
        let without_info = vec![0x08u8, 0x02];
        for group_field in [1, 3, 4] {
            let block = PrimitiveBlock::from_vec(block_with_groups(&[
                len_field(group_field, &with_info),
                len_field(group_field, &without_info),
            ]))
            .expect("valid elements");
            let versions: Vec<Option<i32>> = block
                .elements()
                .map(|element| match element {
                    Element::Node(n) => n.info().version(),
                    Element::Way(w) => w.info().version(),
                    Element::Relation(r) => r.info().version(),
                    other => panic!("unexpected element {other:?}"),
                })
                .collect();
            assert_eq!(versions, vec![Some(7), None], "group field {group_field}");
        }
    }

    /// A dense group with `n` one-byte ids/lats/lons plus a raw `keys_vals`
    /// body and an optional raw `denseinfo` body.
    fn dense_group_with(n: usize, keys_vals: &[u8], denseinfo: Option<&[u8]>) -> Vec<u8> {
        let mut dense = len_field(1, &vec![0x02u8; n]);
        if let Some(info) = denseinfo {
            dense.extend_from_slice(&len_field(5, info));
        }
        dense.extend_from_slice(&len_field(8, &vec![0x02u8; n]));
        dense.extend_from_slice(&len_field(9, &vec![0x02u8; n]));
        dense.extend_from_slice(&len_field(10, keys_vals));
        len_field(2, &dense)
    }

    /// `keys_vals` must describe exactly as many per-node tag runs as there
    /// are ids.
    ///
    /// Nothing in the encoding states the node count, so a short array used to
    /// leave the trailing nodes silently untagged and a long one silently
    /// discarded its tail - `DenseNodeIter` walks it lazily and cannot report
    /// either. Every assertion below is on the run-count message specifically,
    /// which only `validate_dense_keys_vals` produces: the id/lat/lon length
    /// equality and the mid-varint scan both hold for these fixtures, so
    /// neither can be what fires.
    #[test]
    fn dense_keys_vals_run_count_must_match_the_id_array() {
        // Two nodes, two terminated runs.
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[0x01, 0x01, 0x00, 0x02, 0x02, 0x00],
            None,
        )]))
        .expect("two runs for two nodes must parse");

        // An absent keys_vals means "no node here has tags" and stays legal.
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(2, &[], None)]))
            .expect("an empty keys_vals must parse");

        let short = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[0x01, 0x01, 0x00],
            None,
        )]))
        .expect_err("one run for two nodes must be rejected");
        assert!(
            format!("{short}").contains("keys_vals describes a different number of nodes"),
            "the run-count check must be what rejects this, got: {short}"
        );

        let long = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[0x01, 0x01, 0x00, 0x02, 0x02, 0x00, 0x03, 0x03, 0x00],
            None,
        )]))
        .expect_err("three runs for two nodes must be rejected");
        assert!(
            format!("{long}").contains("keys_vals describes a different number of nodes"),
            "the run-count check must be what rejects this, got: {long}"
        );

        // A final run that ends without its 0 terminator still describes one
        // node - some writers omit the trailing delimiter, and rejecting that
        // would reject conforming-enough real files.
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[0x01, 0x01, 0x00, 0x02, 0x02],
            None,
        )]))
        .expect("a final run without a trailing delimiter must parse");

        // A key with no value is corruption: DenseTagIter would drop the tag.
        let dangling = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[0x01, 0x01, 0x00, 0x02],
            None,
        )]))
        .expect_err("a key with no value must be rejected");
        assert!(
            format!("{dangling}").contains("keys_vals ends with a key that has no value"),
            "the dangling-key check must be what rejects this, got: {dangling}"
        );
    }

    /// Every `denseinfo` array must be either absent or exactly as long as the
    /// id array.
    ///
    /// `DenseNodeInfoIter` zips the six arrays and stops at the shortest, so a
    /// short one made metadata vanish for every node from that point on while
    /// iteration reported success. The fixtures keep id/lat/lon equal and every
    /// packed array complete, so only the denseinfo check can fire.
    #[test]
    fn dense_denseinfo_arrays_must_match_the_id_array() {
        // version, timestamp, changeset, uid, user_sid - fields 1..=5.
        let full: Vec<u8> = (1u8..=5).flat_map(|f| packed(f, 2)).collect();
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(2, &[], Some(&full))]))
            .expect("five full-length info arrays must parse");

        // Absent denseinfo entirely is legal - metadata is optional.
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(2, &[], None)]))
            .expect("an absent denseinfo must parse");

        let mut short: Vec<u8> = packed(1, 1);
        short.extend((2u8..=5).flat_map(|f| packed(f, 2)));
        let err =
            PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(2, &[], Some(&short))]))
                .expect_err("a short version array must be rejected");
        assert!(
            format!("{err}").contains("denseinfo array length does not match"),
            "the denseinfo length check must be what rejects this, got: {err}"
        );

        // Some but not all of the five core arrays: our iterator yields None
        // for every node, which is the same silent loss as a short array.
        let partial: Vec<u8> = (1u8..=2).flat_map(|f| packed(f, 2)).collect();
        let err = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[],
            Some(&partial),
        )]))
        .expect_err("a partially populated denseinfo must be rejected");
        assert!(
            format!("{err}").contains("some but not all"),
            "the all-or-nothing check must be what rejects this, got: {err}"
        );

        // `visible` (field 6) is optional but must be full length when present.
        let mut with_visible = full.clone();
        with_visible.extend(packed(6, 2));
        PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[],
            Some(&with_visible),
        )]))
        .expect("a full-length visible array must parse");

        let mut short_visible = full.clone();
        short_visible.extend(packed(6, 1));
        let err = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
            2,
            &[],
            Some(&short_visible),
        )]))
        .expect_err("a short visible array must be rejected");
        assert!(
            format!("{err}").contains("visible array length"),
            "the visible-length check must be what rejects this, got: {err}"
        );
    }

    /// The last tag of a node whose `keys_vals` run ends without a trailing
    /// `0` delimiter must survive.
    ///
    /// `DenseNodeIter` used to derive the end of the tag run by subtracting one
    /// byte from the cursor position, on the assumption that the run always
    /// ended at a delimiter. When it ended at the end of the array instead, that
    /// chopped the last byte off the final value varint - so the fixture below
    /// uses a two-byte value varint (index 130), where the truncation is the
    /// difference between one decoded tag and none. A single-byte value index
    /// would leave a complete varint behind and the test would pass either way.
    #[test]
    fn final_tag_run_without_a_trailing_delimiter_keeps_its_last_tag() {
        // A stringtable big enough that index 130 exists, so the block is a
        // legal one rather than one that happens to survive validation.
        let mut entries = Vec::new();
        for _ in 0..200 {
            entries.extend_from_slice(&len_field(1, &[]));
        }
        #[allow(clippy::cast_possible_truncation)]
        let st_len = [(entries.len() as u8) | 0x80, (entries.len() >> 7) as u8];
        let mut block = vec![0x0a];
        block.extend_from_slice(&st_len);
        block.extend_from_slice(&entries);

        // One node, one tag: key index 1, value index 130 (0x82 0x01), and no
        // trailing 0 delimiter.
        block.extend_from_slice(&len_field(
            2,
            &dense_group_with(1, &[0x01, 0x82, 0x01], None),
        ));

        let parsed = PrimitiveBlock::from_vec(block).expect("fixture block must parse");
        let node = match parsed.elements().next().expect("one element") {
            super::Element::DenseNode(node) => node,
            other => panic!("expected a dense node, got {other:?}"),
        };
        let tags: Vec<(i32, i32)> = node.raw_tags().collect();
        assert_eq!(
            tags,
            vec![(1, 130)],
            "the final tag must survive a run that ends without a delimiter"
        );
    }

    #[test]
    fn dense_tag_indices_must_fit_signed_int32() {
        use super::validate_dense_keys_vals;

        // Largest nonnegative int32, first overflowing value, and a value
        // whose low 32 bits alias string index 1.
        let max = [0xff, 0xff, 0xff, 0xff, 0x07];
        for sid in [
            &[0x80, 0x80, 0x80, 0x80, 0x08][..],
            &[0x81, 0x80, 0x80, 0x80, 0x10],
        ] {
            for as_key in [false, true] {
                let mut tags = Vec::new();
                tags.extend_from_slice(if as_key { sid } else { &[1] });
                tags.extend_from_slice(if as_key { &[1] } else { sid });
                tags.push(0);
                let err = PrimitiveBlock::from_vec(block_with_groups(&[dense_group_with(
                    1, &tags, None,
                )]))
                .expect_err("oversized dense string index must fail construction");
                assert!(err.to_string().contains("exceeds int32"));
            }
        }
        let mut tags = max.to_vec();
        tags.extend_from_slice(&max);
        tags.push(0);
        validate_dense_keys_vals(&tags, 1).expect("int32 maximum fits");
        validate_dense_keys_vals(&[0x81, 0x80, 0, 1, 0], 1)
            .expect("overlong encoding of a small index fits");
    }

    #[test]
    fn coordinate_scaling_overflow_wraps_instead_of_panicking() {
        // Each encoding carries raw latitude 1. With default granularity 100
        // and lat_offset i64::MAX the scaled value runs past i64; the block
        // still constructs (values are not validated, by policy) and every
        // accessor wraps, so debug and release agree.
        let mut dense = len_field(1, &[2]);
        dense.extend(len_field(8, &[2]));
        dense.extend(len_field(9, &[0]));
        let mut way = vec![0x08, 1];
        way.extend(len_field(8, &[2]));
        way.extend(len_field(9, &[2]));
        way.extend(len_field(10, &[0]));
        for group in [
            len_field(1, &[0x08, 2, 0x40, 2, 0x48, 0]),
            len_field(2, &dense),
            len_field(3, &way),
        ] {
            PrimitiveBlock::from_vec(block_with_groups(std::slice::from_ref(&group)))
                .expect("ordinary coordinate must parse");
            let mut block = block_with_groups(&[group]);
            // PrimitiveBlock field 19 (lat_offset), signed int64 MAX.
            block.extend_from_slice(&[0x98, 0x01]);
            block.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
            let block = PrimitiveBlock::from_vec(block)
                .expect("coordinate values are not validated at construction");
            let wrapped = i64::MAX.wrapping_add(100);
            let mut seen = 0;
            for element in block.elements() {
                let nano_lat = match element {
                    super::Element::Node(node) => node.nano_lat(),
                    super::Element::DenseNode(node) => node.nano_lat(),
                    super::Element::Way(way) => way
                        .node_locations()
                        .next()
                        .expect("one way location")
                        .nano_lat(),
                    super::Element::Relation(_) => continue,
                };
                assert_eq!(nano_lat, wrapped);
                seen += 1;
            }
            assert_eq!(seen, 1);
        }
    }
}
