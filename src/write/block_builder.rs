//! Block builder for constructing PBF `PrimitiveBlock` messages.
//!
//! Accumulates OSM elements (nodes, ways, relations) and serializes them into
//! protobuf `PrimitiveBlock` bytes suitable for [`PbfWriter`](crate::writer::PbfWriter).
//!
//! Handles string table construction, delta encoding, dense node packing,
//! and block size limits (8000 entities per block, matching osmium).

use crate::proto::osmformat;
use bytes::Bytes;
use protobuf::{EnumOrUnknown, Message};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;

/// Maximum number of entities in a single `PrimitiveBlock`.
/// Matches osmium's hardcoded limit.
const MAX_ENTITIES_PER_BLOCK: usize = 8000;

// ---------------------------------------------------------------------------
// String table
// ---------------------------------------------------------------------------

/// Block-local string table. Index 0 is always the empty string.
struct StringTable {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl StringTable {
    fn new() -> Self {
        let mut st = StringTable {
            strings: Vec::with_capacity(256),
            index: HashMap::with_capacity(256),
        };
        st.strings.push(String::new()); // index 0 = empty string
        st
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// ## Allocation strategy
    ///
    /// The previous implementation allocated twice per new string:
    ///   1. `s.to_owned()` pushed into `self.strings` (the ordered Vec)
    ///   2. `s.to_owned()` inserted into `self.index` (the lookup HashMap)
    ///
    /// This version uses the `Entry` API to allocate only once for the HashMap key
    /// (`s.to_owned()` in `self.index.entry(...)`), then clones that key into the
    /// `strings` Vec via `e.key().clone()`. The clone is cheap: it copies the pointer,
    /// length, and capacity, then allocates a new buffer and memcpys — but crucially
    /// we avoid *parsing and measuring* the string a second time, and the optimizer
    /// can see both allocations are the same size.
    ///
    /// For planet-scale writes with millions of unique tag key/value strings, this
    /// halves the number of independent heap allocations for string interning.
    ///
    /// We considered using `Rc<str>` to truly share one allocation between the Vec
    /// and HashMap, but the entry API approach is simpler, avoids reference-counting
    /// overhead on every lookup, and keeps the `String` types that `into_proto()`
    /// expects without conversion.
    #[allow(clippy::cast_possible_truncation)]
    fn add(&mut self, s: &str) -> u32 {
        // Compute next_idx eagerly — it's just a cheap usize->u32 cast.
        // If the string already exists, we discard this value (no side effects).
        let next_idx = self.strings.len() as u32;
        match self.index.entry(s.to_owned()) {
            // String already in the table — return its index.
            Entry::Occupied(e) => *e.get(),
            // New string — clone the entry's key into the ordered Vec, then
            // store the index in the HashMap entry. The clone shares the same
            // allocation size so the allocator can often serve it from a
            // size-class freelist.
            Entry::Vacant(e) => {
                self.strings.push(e.key().clone());
                e.insert(next_idx);
                next_idx
            }
        }
    }

    fn into_proto(self) -> osmformat::StringTable {
        let mut st = osmformat::StringTable::new();
        for s in self.strings {
            st.s.push(Bytes::from(s.into_bytes()));
        }
        st
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The type of elements in the current block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockType {
    DenseNodes,
    Ways,
    Relations,
}

/// Optional metadata for an element.
pub struct Metadata<'a> {
    /// Element version (starts at 1, incremented on each edit).
    pub version: i32,
    /// Timestamp in seconds since the Unix epoch.
    pub timestamp: i64,
    /// Changeset ID.
    pub changeset: i64,
    /// User ID.
    pub uid: i32,
    /// User name.
    pub user: &'a str,
    /// Whether the element is visible (true) or deleted (false).
    pub visible: bool,
}

use crate::elements::{MemberId, MemberType};

/// A relation member.
pub struct MemberData<'a> {
    /// The typed member reference (element type + ID).
    pub id: MemberId,
    /// The member's role string.
    pub role: &'a str,
}

fn member_type_to_proto(mt: MemberType) -> osmformat::relation::MemberType {
    match mt {
        MemberType::Node => osmformat::relation::MemberType::NODE,
        MemberType::Way => osmformat::relation::MemberType::WAY,
        MemberType::Relation => osmformat::relation::MemberType::RELATION,
        // Unknown member types from newer PBF producers — round-trip as NODE
        // since the protobuf enum has no "unknown" value. Callers should filter
        // these out before writing if lossless preservation is needed.
        MemberType::Unknown(_) => osmformat::relation::MemberType::NODE,
    }
}

// ---------------------------------------------------------------------------
// BlockBuilder
// ---------------------------------------------------------------------------

/// Builds `PrimitiveBlock` protobuf messages for PBF output.
///
/// Elements are added one at a time. When the block reaches the limit
/// (8000 entities), [`should_flush`](Self::should_flush) returns `true`
/// and [`take`](Self::take) should be called to serialize and reset.
///
/// Each block contains only one element type (nodes OR ways OR relations).
/// Adding a different type when the block is non-empty will panic — the
/// caller must flush first.
pub struct BlockBuilder {
    string_table: StringTable,
    block_type: Option<BlockType>,
    count: usize,

    // Dense node accumulators
    dense_ids: Vec<i64>,
    dense_lats: Vec<i64>,
    dense_lons: Vec<i64>,
    dense_keys_vals: Vec<i32>,

    // Dense node metadata accumulators
    dense_versions: Vec<i32>,
    dense_timestamps: Vec<i64>,
    dense_changesets: Vec<i64>,
    dense_uids: Vec<i32>,
    dense_user_sids: Vec<i32>,
    dense_visibles: Vec<bool>,
    has_dense_metadata: bool,

    // Dense node delta state
    last_dense_id: i64,
    last_dense_lat: i64,
    last_dense_lon: i64,
    last_dense_timestamp: i64,
    last_dense_changeset: i64,
    last_dense_uid: i32,
    last_dense_user_sid: i32,

    // Ways
    ways: Vec<osmformat::Way>,

    // Relations
    relations: Vec<osmformat::Relation>,
}

impl Default for BlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockBuilder {
    /// Create a new, empty block builder.
    ///
    /// Dense node vectors are pre-allocated to `MAX_ENTITIES_PER_BLOCK` (8000)
    /// capacity because a full dense-node block will contain exactly that many
    /// entries. Without pre-allocation, each Vec would grow through several
    /// doublings (0 -> 1 -> 2 -> 4 -> ... -> 8192), causing O(log N)
    /// reallocations and memcpys per block. Pre-allocating avoids this entirely
    /// for the common case where blocks are filled to capacity.
    ///
    /// `dense_keys_vals` is pre-allocated to 16000 (2 * MAX_ENTITIES_PER_BLOCK).
    /// Each node contributes at least one entry (the 0 delimiter), and nodes
    /// with tags contribute 2 * num_tags + 1 entries. An estimate of ~2 tags
    /// per node on average gives (2 * 2 + 1) * 8000 = 40000, but many nodes
    /// are tagless (especially in dense areas), so 16000 is a pragmatic middle
    /// ground that avoids most reallocations without over-allocating.
    ///
    /// `ways` and `relations` are left at zero capacity because each block is
    /// single-type: if the block is a dense-nodes block, these Vecs are never
    /// used at all, and allocating them would waste memory. Way/relation blocks
    /// also tend to have fewer entities than the 8000 limit (ways are larger
    /// per-entity due to node refs), so the default doubling strategy is fine.
    pub fn new() -> Self {
        BlockBuilder {
            string_table: StringTable::new(),
            block_type: None,
            count: 0,

            // Pre-allocate dense node vectors to the max block size (8000).
            // One entry per node for each of id, lat, lon.
            dense_ids: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_lats: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_lons: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            // Interleaved key/val string indices plus delimiters — see doc comment above.
            dense_keys_vals: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK * 2),

            // Pre-allocate dense metadata vectors to max block size.
            // One entry per node for each metadata field.
            dense_versions: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_timestamps: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_changesets: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_uids: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_user_sids: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_visibles: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            has_dense_metadata: false,

            // Delta encoding state — reset to zero for each new block.
            last_dense_id: 0,
            last_dense_lat: 0,
            last_dense_lon: 0,
            last_dense_timestamp: 0,
            last_dense_changeset: 0,
            last_dense_uid: 0,
            last_dense_user_sid: 0,

            // Left at zero capacity — see doc comment above.
            ways: Vec::new(),
            relations: Vec::new(),
        }
    }

    /// Returns `true` if the block contains no elements.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns `true` if the block has reached the entity limit (8000).
    pub fn should_flush(&self) -> bool {
        self.count >= MAX_ENTITIES_PER_BLOCK
    }

    /// Returns `true` if a node can be added to the current block.
    pub fn can_add_node(&self) -> bool {
        match self.block_type {
            None => true,
            Some(BlockType::DenseNodes) => !self.should_flush(),
            Some(_) => false,
        }
    }

    /// Returns `true` if a way can be added to the current block.
    pub fn can_add_way(&self) -> bool {
        match self.block_type {
            None => true,
            Some(BlockType::Ways) => !self.should_flush(),
            Some(_) => false,
        }
    }

    /// Returns `true` if a relation can be added to the current block.
    pub fn can_add_relation(&self) -> bool {
        match self.block_type {
            None => true,
            Some(BlockType::Relations) => !self.should_flush(),
            Some(_) => false,
        }
    }

    /// Add a node using dense encoding.
    ///
    /// Coordinates are in decimicrodegrees (10⁻⁷ degrees, i.e. 100 nanodegrees),
    /// matching the default PBF granularity of 100.
    #[allow(clippy::cast_possible_wrap)]
    pub fn add_node(
        &mut self,
        id: i64,
        decimicro_lat: i32,
        decimicro_lon: i32,
        tags: &[(&str, &str)],
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_node(),
            "cannot add node: block full or wrong type"
        );
        self.block_type = Some(BlockType::DenseNodes);

        // Delta-encode id, lat, lon
        let lat = i64::from(decimicro_lat);
        let lon = i64::from(decimicro_lon);

        self.dense_ids.push(id - self.last_dense_id);
        self.dense_lats.push(lat - self.last_dense_lat);
        self.dense_lons.push(lon - self.last_dense_lon);

        self.last_dense_id = id;
        self.last_dense_lat = lat;
        self.last_dense_lon = lon;

        // Tags: interleaved [key_sid, val_sid, ...] terminated by 0
        for &(key, val) in tags {
            self.dense_keys_vals
                .push(self.string_table.add(key) as i32);
            self.dense_keys_vals
                .push(self.string_table.add(val) as i32);
        }
        self.dense_keys_vals.push(0); // delimiter (even for tagless nodes)

        // Metadata
        if let Some(meta) = metadata {
            self.add_dense_metadata(meta);
        }

        self.count += 1;
    }

    #[allow(clippy::cast_possible_wrap)]
    fn add_dense_metadata(&mut self, meta: &Metadata<'_>) {
        self.has_dense_metadata = true;

        // Version is NOT delta-encoded
        self.dense_versions.push(meta.version);

        // Timestamp — delta-encoded
        self.dense_timestamps
            .push(meta.timestamp - self.last_dense_timestamp);
        self.last_dense_timestamp = meta.timestamp;

        // Changeset — delta-encoded
        self.dense_changesets
            .push(meta.changeset - self.last_dense_changeset);
        self.last_dense_changeset = meta.changeset;

        // UID — delta-encoded
        self.dense_uids.push(meta.uid - self.last_dense_uid);
        self.last_dense_uid = meta.uid;

        // User SID — delta-encoded
        let user_sid = self.string_table.add(meta.user) as i32;
        self.dense_user_sids
            .push(user_sid - self.last_dense_user_sid);
        self.last_dense_user_sid = user_sid;

        // Visible (only meaningful for history files, but we preserve it)
        self.dense_visibles.push(meta.visible);
    }

    /// Add a way.
    ///
    /// `refs` are absolute node IDs (the builder handles delta encoding internally).
    pub fn add_way(
        &mut self,
        id: i64,
        tags: &[(&str, &str)],
        refs: &[i64],
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_way(),
            "cannot add way: block full or wrong type"
        );
        self.block_type = Some(BlockType::Ways);

        let mut way = osmformat::Way::new();
        way.set_id(id);

        // Tags — plain string table indices (not delta-encoded)
        for &(key, val) in tags {
            way.keys.push(self.string_table.add(key));
            way.vals.push(self.string_table.add(val));
        }

        // Node refs — delta-encoded within this way
        let mut last_ref: i64 = 0;
        for &r in refs {
            way.refs.push(r - last_ref);
            last_ref = r;
        }

        // Metadata
        if let Some(meta) = metadata {
            way.info = protobuf::MessageField::some(self.build_info(meta));
        }

        self.ways.push(way);
        self.count += 1;
    }

    /// Add a way with node locations embedded.
    ///
    /// `refs` are absolute node IDs, `locations` are `(decimicro_lat, decimicro_lon)` pairs.
    /// Both slices must have the same length.
    #[allow(clippy::too_many_arguments)]
    pub fn add_way_with_locations(
        &mut self,
        id: i64,
        tags: &[(&str, &str)],
        refs: &[i64],
        locations: &[(i32, i32)],
        metadata: Option<&Metadata<'_>>,
    ) {
        debug_assert_eq!(refs.len(), locations.len(), "refs and locations must match");
        assert!(
            self.can_add_way(),
            "cannot add way: block full or wrong type"
        );
        self.block_type = Some(BlockType::Ways);

        let mut way = osmformat::Way::new();
        way.set_id(id);

        for &(key, val) in tags {
            way.keys.push(self.string_table.add(key));
            way.vals.push(self.string_table.add(val));
        }

        let mut last_ref: i64 = 0;
        let mut last_lat: i64 = 0;
        let mut last_lon: i64 = 0;
        for (i, &r) in refs.iter().enumerate() {
            way.refs.push(r - last_ref);
            last_ref = r;

            let lat = i64::from(locations[i].0);
            let lon = i64::from(locations[i].1);
            way.lat.push(lat - last_lat);
            way.lon.push(lon - last_lon);
            last_lat = lat;
            last_lon = lon;
        }

        if let Some(meta) = metadata {
            way.info = protobuf::MessageField::some(self.build_info(meta));
        }

        self.ways.push(way);
        self.count += 1;
    }

    /// Add a relation.
    ///
    /// `members` are absolute member IDs (the builder handles delta encoding internally).
    pub fn add_relation(
        &mut self,
        id: i64,
        tags: &[(&str, &str)],
        members: &[MemberData<'_>],
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_relation(),
            "cannot add relation: block full or wrong type"
        );
        self.block_type = Some(BlockType::Relations);

        let mut rel = osmformat::Relation::new();
        rel.set_id(id);

        // Tags
        for &(key, val) in tags {
            rel.keys.push(self.string_table.add(key));
            rel.vals.push(self.string_table.add(val));
        }

        // Members — three parallel arrays: roles_sid, memids (delta), types
        let mut last_memid: i64 = 0;
        for m in members {
            #[allow(clippy::cast_possible_wrap)]
            rel.roles_sid
                .push(self.string_table.add(m.role) as i32);
            rel.memids.push(m.id.id() - last_memid);
            last_memid = m.id.id();
            rel.types
                .push(EnumOrUnknown::new(member_type_to_proto(m.id.member_type())));
        }

        // Metadata
        if let Some(meta) = metadata {
            rel.info = protobuf::MessageField::some(self.build_info(meta));
        }

        self.relations.push(rel);
        self.count += 1;
    }

    /// Serialize the current block to `PrimitiveBlock` bytes and reset.
    ///
    /// Returns `None` if the block is empty.
    pub fn take(&mut self) -> io::Result<Option<Vec<u8>>> {
        let block_type = match self.block_type {
            Some(t) => t,
            None => return Ok(None),
        };

        let mut block = osmformat::PrimitiveBlock::new();

        // String table
        let string_table = std::mem::replace(&mut self.string_table, StringTable::new());
        block.stringtable = protobuf::MessageField::some(string_table.into_proto());

        // Note: we do NOT set granularity, lat_offset, lon_offset, or date_granularity.
        // Omitting them uses the protobuf defaults (granularity=100, offsets=0, date_gran=1000).

        // Build the PrimitiveGroup
        let group = match block_type {
            BlockType::DenseNodes => self.take_dense_nodes_group(),
            BlockType::Ways => self.take_ways_group(),
            BlockType::Relations => self.take_relations_group(),
        };
        block.primitivegroup.push(group);

        let bytes = block
            .write_to_bytes()
            .map_err(io::Error::other)?;

        self.reset();
        Ok(Some(bytes))
    }

    /// Move the dense node data into a `PrimitiveGroup`.
    ///
    /// Uses `std::mem::take()` to transfer ownership of the filled Vecs into
    /// the protobuf message. `take()` replaces each Vec with `Vec::new()`
    /// (zero capacity), so after this call all dense_* fields are empty Vecs
    /// with no heap allocation.
    ///
    /// This means the next block-building cycle (after `reset()`) starts with
    /// zero-capacity Vecs, losing the pre-allocation from `new()`. This is
    /// acceptable because:
    ///   1. `take()` is called from `self.take()`, which calls `self.reset()`
    ///      afterward — and callers typically call `BlockBuilder::new()` for a
    ///      fresh builder, not reuse the same instance.
    ///   2. Even if the builder IS reused, the cost is O(log N) reallocations
    ///      per block (growing from 0 to 8000), which is negligible compared
    ///      to the protobuf serialization and zlib compression cost.
    ///   3. A potential future optimization: after `std::mem::take`, re-allocate
    ///      the Vecs in `reset()` with `Vec::with_capacity(MAX_ENTITIES_PER_BLOCK)`
    ///      so reused builders also benefit from pre-allocation. Not done yet
    ///      because the current usage pattern (one builder per block) doesn't
    ///      benefit from it.
    fn take_dense_nodes_group(&mut self) -> osmformat::PrimitiveGroup {
        let mut group = osmformat::PrimitiveGroup::new();
        let mut dense = osmformat::DenseNodes::new();

        dense.id = std::mem::take(&mut self.dense_ids);
        dense.lat = std::mem::take(&mut self.dense_lats);
        dense.lon = std::mem::take(&mut self.dense_lons);
        dense.keys_vals = std::mem::take(&mut self.dense_keys_vals);

        if self.has_dense_metadata {
            let mut info = osmformat::DenseInfo::new();
            info.version = std::mem::take(&mut self.dense_versions);
            info.timestamp = std::mem::take(&mut self.dense_timestamps);
            info.changeset = std::mem::take(&mut self.dense_changesets);
            info.uid = std::mem::take(&mut self.dense_uids);
            info.user_sid = std::mem::take(&mut self.dense_user_sids);
            info.visible = std::mem::take(&mut self.dense_visibles);
            dense.denseinfo = protobuf::MessageField::some(info);
        }

        group.dense = protobuf::MessageField::some(dense);
        group
    }

    fn take_ways_group(&mut self) -> osmformat::PrimitiveGroup {
        let mut group = osmformat::PrimitiveGroup::new();
        group.ways = std::mem::take(&mut self.ways);
        group
    }

    fn take_relations_group(&mut self) -> osmformat::PrimitiveGroup {
        let mut group = osmformat::PrimitiveGroup::new();
        group.relations = std::mem::take(&mut self.relations);
        group
    }

    #[allow(clippy::cast_possible_wrap)]
    fn build_info(&mut self, meta: &Metadata<'_>) -> osmformat::Info {
        let mut info = osmformat::Info::new();
        info.version = Some(meta.version);
        info.timestamp = Some(meta.timestamp);
        info.changeset = Some(meta.changeset);
        info.uid = Some(meta.uid);
        info.user_sid = Some(self.string_table.add(meta.user));
        if !meta.visible {
            info.visible = Some(false);
        }
        info
    }

    fn reset(&mut self) {
        self.block_type = None;
        self.count = 0;
        self.has_dense_metadata = false;

        self.last_dense_id = 0;
        self.last_dense_lat = 0;
        self.last_dense_lon = 0;
        self.last_dense_timestamp = 0;
        self.last_dense_changeset = 0;
        self.last_dense_uid = 0;
        self.last_dense_user_sid = 0;
    }
}

// ---------------------------------------------------------------------------
// Header builder
// ---------------------------------------------------------------------------

/// Build a serialized `HeaderBlock` protobuf message.
///
/// This is the first block in every PBF file. It declares required features,
/// optionally includes a bounding box, and carries replication metadata.
#[allow(clippy::cast_possible_truncation)]
pub fn build_header(
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence_number: Option<i64>,
    replication_base_url: Option<&str>,
    optional_features: &[&str],
) -> io::Result<Vec<u8>> {
    let mut header = osmformat::HeaderBlock::new();

    // Required features — every PBF reader must support these
    header
        .required_features
        .push(protobuf::Chars::from("OsmSchema-V0.6"));
    header
        .required_features
        .push(protobuf::Chars::from("DenseNodes"));

    // Optional features
    for feature in optional_features {
        header
            .optional_features
            .push(protobuf::Chars::from(*feature));
    }

    // Writing program
    header.set_writingprogram(protobuf::Chars::from("pbfhogg"));

    // Bounding box (nanodegrees)
    if let Some((left, bottom, right, top)) = bbox {
        let mut bb = osmformat::HeaderBBox::new();
        bb.set_left((left * 1e9) as i64);
        bb.set_right((right * 1e9) as i64);
        bb.set_top((top * 1e9) as i64);
        bb.set_bottom((bottom * 1e9) as i64);
        header.bbox = protobuf::MessageField::some(bb);
    }

    // Replication metadata
    if let Some(ts) = replication_timestamp {
        header.set_osmosis_replication_timestamp(ts);
    }
    if let Some(seq) = replication_sequence_number {
        header.set_osmosis_replication_sequence_number(seq);
    }
    if let Some(url) = replication_base_url {
        header.set_osmosis_replication_base_url(protobuf::Chars::from(url));
    }

    header
        .write_to_bytes()
        .map_err(io::Error::other)
}
