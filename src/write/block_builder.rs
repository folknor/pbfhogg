//! Block builder for constructing PBF `PrimitiveBlock` messages.
//!
//! Accumulates OSM elements (nodes, ways, relations) and serializes them into
//! protobuf `PrimitiveBlock` bytes suitable for [`PbfWriter`](crate::writer::PbfWriter).
//!
//! Handles string table construction, delta encoding, dense node packing,
//! and block size limits (8000 entities per block, matching osmium).

use crate::blob_meta::{BlobIndex, ElemKind};
use crate::PrimitiveBlock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::io;
use std::rc::Rc;

/// Encoded block bytes, blob index, and optional pre-serialized tagdata.
pub(crate) type OwnedBlock = (Vec<u8>, BlobIndex, Option<Vec<u8>>);

use protohoggr::{
    encode_bytes_field, encode_bytes_field_always, encode_int64_field,
    encode_packed_bool, encode_packed_int32, encode_packed_sint32, encode_packed_sint64,
    encode_sint64_field_always, encode_varint, zigzag_encode_64,
};

// Re-exported so `crate::block_builder::HeaderBuilder` keeps resolving after
// the type was lifted into its own sibling module.
pub use crate::write::header_builder::HeaderBuilder;

/// Maximum number of entities in a single `PrimitiveBlock`.
/// Matches osmium's hardcoded limit.
const MAX_ENTITIES_PER_BLOCK: usize = 8000;

// ---------------------------------------------------------------------------
// String table
// ---------------------------------------------------------------------------

/// Block-local string table. Index 0 is always the empty string.
///
/// ## Why FxHashMap instead of std HashMap
///
/// The `index` map uses `FxHashMap` (from `rustc-hash`) instead of the standard
/// `HashMap` with `SipHash`. This is safe and beneficial here because:
///
/// **Safety:** This is a write-side-only data structure. All strings inserted
/// come from the caller's in-process data (tag keys, tag values, role strings,
/// user names) - never from untrusted PBF input. There is no risk of
/// HashDoS attacks, which is the sole reason the standard library defaults to
/// the slower SipHash-1-3 hasher.
///
/// **Performance:** FxHash is a simple, non-cryptographic hash (multiply +
/// rotate) that is substantially faster than SipHash for short strings - which
/// is exactly what OSM tag keys/values are (typically 3-30 bytes: "name",
/// "highway", "building", "residential", etc.). The string table is on the hot
/// path of PBF writing: every tag on every element does a hash lookup + possible
/// insert. In profiling, the hasher shows up as a measurable fraction of write
/// time, so switching to FxHash gives a meaningful speedup.
///
/// **Where NOT to use FxHash:** On the *read* side (e.g. if you were building a
/// lookup table from PBF data), strings come from untrusted input files that
/// could be adversarially crafted. In that context, SipHash (or ahash, which
/// also has DoS resistance) should be used to prevent O(n^2) hash collisions.
///
/// **Alternatives considered:**
/// - `ahash`: Also fast and DoS-resistant, but the DoS resistance is
///   unnecessary overhead here since we control the input. FxHash is simpler
///   and marginally faster for the short-string workload.
/// - `IndexMap`: Preserves insertion order (which we need via `self.strings`),
///   but wrapping an IndexMap would still need a fast hasher, and we already
///   maintain the ordered Vec separately. Switching would add a dependency for
///   no net benefit.
/// - Custom perfect hashing: Not viable because the string set is dynamic -
///   we do not know all strings upfront.
struct StringTable {
    strings: Vec<Rc<str>>,
    index: FxHashMap<Rc<str>, u32>,
    empty: Rc<str>,
}

impl StringTable {
    fn new() -> Self {
        let empty: Rc<str> = Rc::from("");
        let mut st = StringTable {
            strings: Vec::with_capacity(256),
            index: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            empty: Rc::clone(&empty),
        };
        st.strings.push(empty); // index 0 = empty string
        st
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// ## Fast path (cache hit, ~99% of calls)
    ///
    /// `self.index.get(s)` looks up the `&str` directly via the `Borrow` trait -
    /// no allocation, just FxHash + probe. This is the hot path: a typical 8000-
    /// element block has ~1200 unique strings but ~16,000+ add() calls, so the
    /// vast majority are cache hits.
    ///
    /// ## Slow path (cache miss, ~1% of calls)
    ///
    /// On the first occurrence of a string, allocates a single `Rc<str>` shared
    /// between the HashMap key and the Vec entry. `Rc::clone` is just a refcount
    /// bump - one heap allocation per unique string total.
    #[allow(clippy::cast_possible_truncation)]
    fn add(&mut self, s: &str) -> u32 {
        // Fast path: string already interned - hash-only lookup, no allocation.
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        // Slow path: first occurrence - single Rc<str> allocation, shared
        // between the Vec and HashMap (Rc::clone is just a refcount bump).
        let next_idx = self.strings.len() as u32;
        let rc: Rc<str> = Rc::from(s);
        self.strings.push(Rc::clone(&rc));
        self.index.insert(rc, next_idx);
        next_idx
    }

    /// Pre-seed from an input block's string table, populating the index map.
    ///
    /// After pre-seeding, input index N maps to output index N (identity).
    /// Index 0 (empty string) is already present from `new()` and is skipped.
    fn pre_seed(&mut self, block: &PrimitiveBlock) {
        let len = block.string_table_len();
        for i in 1..len {
            if let Some(s) = block.string_table_entry(i) {
                self.add(s);
            }
        }
    }

    fn clear(&mut self) {
        self.strings.clear();
        self.index.clear();
        self.strings.push(Rc::clone(&self.empty));
    }

    /// Encode the string table directly to wire format bytes.
    ///
    /// StringTable has one field: `repeated bytes s = 1;`
    fn encode_to(&self, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) {
        scratch.clear();
        for s in &self.strings {
            encode_bytes_field_always(scratch, 1, s.as_bytes());
        }
        encode_bytes_field(buf, 1, scratch);
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberData<'a> {
    /// The typed member reference (element type + ID).
    pub id: MemberId,
    /// The member's role string.
    pub role: &'a str,
}

/// Metadata with a raw string table index for the user name.
///
/// Used by the raw-index `add_*_raw` methods where user strings are passed
/// through as pre-seeded indices rather than re-interned via [`StringTable::add`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawMetadata {
    pub version: i32,
    pub timestamp: i64,
    pub changeset: i64,
    pub uid: i32,
    pub user_sid: i32,
    pub visible: bool,
}

/// Map a MemberType to its protobuf enum integer value.
///
/// MemberType enum: NODE=0, WAY=1, RELATION=2.
fn member_type_value(mt: MemberType) -> i32 {
    match mt {
        MemberType::Node => 0,
        MemberType::Way => 1,
        MemberType::Relation => 2,
        // Unknown member types from newer PBF producers - round-trip as Node
        // since the protobuf enum has no "unknown" value. Callers should filter
        // these out before writing if lossless preservation is needed.
        MemberType::Unknown(_) => 0,
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
/// Adding a different type when the block is non-empty will panic - the
/// caller must flush first.
pub struct BlockBuilder {
    string_table: StringTable,
    block_type: Option<BlockType>,
    count: usize,

    // ID range tracking for BlobIndex (avoids scan_block_ids on the write path).
    min_id: i64,
    max_id: i64,

    // Coordinate range tracking for BlobIndex v2 spatial bbox (node blobs only).
    min_lat: i32,
    max_lat: i32,
    min_lon: i32,
    max_lon: i32,

    // Tag key string table indices (for pre-computed tagdata, avoids scan_block_tags rescan).
    tag_key_indices: FxHashSet<u32>,
    // Scratch buffer for sorting tag key indices during tagdata serialization.
    tag_key_scratch: Vec<u32>,
    // Pre-serialized tagdata from the last encode_block() call.
    last_tagdata: Option<Vec<u8>>,

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

    // Wire-format accumulators for ways and relations
    group_buf: Vec<u8>,       // per-block: all serialized way/relation messages
    elem_scratch: Vec<u8>,    // per-element body (cleared each add_way/add_relation call)
    packed_scratch: Vec<u8>,      // per-field packed content (refs in location path)
    packed_vals_scratch: Vec<u8>, // tag values packed encoding (dual-buffer single-pass)
    packed_lat_scratch: Vec<u8>,  // way location lat encoding (single-pass)
    packed_lon_scratch: Vec<u8>,  // way location lon encoding (single-pass)
    info_scratch: Vec<u8>,        // Info sub-message body

    // Reusable encode buffer for take() - avoids allocating a fresh Vec<u8> per block.
    encode_buf: Vec<u8>,

    // True when the string table has been pre-seeded from an input block (merge only).
    // Reset to false by take()/reset(). Checked by merge to re-seed after mid-block flushes.
    pre_seeded: bool,
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
    /// Wire-format scratch buffers (`group_buf`, `elem_scratch`, etc.) are left
    /// at zero capacity because each block is single-type: if the block is a
    /// dense-nodes block, these buffers are never used at all. Way/relation
    /// blocks grow as needed via the standard doubling strategy.
    pub fn new() -> Self {
        BlockBuilder {
            string_table: StringTable::new(),
            block_type: None,
            count: 0,
            min_id: i64::MAX,
            max_id: i64::MIN,
            min_lat: i32::MAX,
            max_lat: i32::MIN,
            min_lon: i32::MAX,
            max_lon: i32::MIN,

            tag_key_indices: FxHashSet::default(),
            tag_key_scratch: Vec::new(),
            last_tagdata: None,

            // Pre-allocate dense node vectors to the max block size (8000).
            // One entry per node for each of id, lat, lon.
            dense_ids: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_lats: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            dense_lons: Vec::with_capacity(MAX_ENTITIES_PER_BLOCK),
            // Interleaved key/val string indices plus delimiters - see doc comment above.
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

            // Delta encoding state - reset to zero for each new block.
            last_dense_id: 0,
            last_dense_lat: 0,
            last_dense_lon: 0,
            last_dense_timestamp: 0,
            last_dense_changeset: 0,
            last_dense_uid: 0,
            last_dense_user_sid: 0,

            // Wire-format scratch buffers - left at zero capacity since
            // way/relation blocks will grow as needed, and dense-node blocks
            // never use them.
            group_buf: Vec::new(),
            elem_scratch: Vec::new(),
            packed_scratch: Vec::new(),
            packed_vals_scratch: Vec::new(),
            packed_lat_scratch: Vec::new(),
            packed_lon_scratch: Vec::new(),
            info_scratch: Vec::new(),

            encode_buf: Vec::new(),

            pre_seeded: false,
        }
    }

    /// Returns `true` if the block contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Track an element ID for BlobIndex min/max range.
    #[inline]
    fn track_id(&mut self, id: i64) {
        if id < self.min_id {
            self.min_id = id;
        }
        if id > self.max_id {
            self.max_id = id;
        }
    }

    /// Track node coordinates for BlobIndex v2 spatial bbox.
    #[inline]
    fn track_coords(&mut self, decimicro_lat: i32, decimicro_lon: i32) {
        if decimicro_lat < self.min_lat { self.min_lat = decimicro_lat; }
        if decimicro_lat > self.max_lat { self.max_lat = decimicro_lat; }
        if decimicro_lon < self.min_lon { self.min_lon = decimicro_lon; }
        if decimicro_lon > self.max_lon { self.max_lon = decimicro_lon; }
    }

    /// Returns `true` if the string table has been pre-seeded from an input block.
    ///
    /// Reset to `false` by `take()`. Used by merge to detect when raw string
    /// table indices are valid (pre-seeded) vs when re-seeding is needed after
    /// a mid-block flush.
    #[inline]
    pub(crate) fn is_pre_seeded(&self) -> bool {
        self.pre_seeded
    }

    /// Returns `true` if the block has reached the entity limit (8000).
    #[inline]
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
    #[hotpath::measure]
    #[allow(clippy::cast_possible_wrap)]
    pub fn add_node<'t>(
        &mut self,
        id: i64,
        decimicro_lat: i32,
        decimicro_lon: i32,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_node(),
            "cannot add node: block full or wrong type"
        );
        self.block_type = Some(BlockType::DenseNodes);

        let lat = i64::from(decimicro_lat);
        let lon = i64::from(decimicro_lon);

        self.dense_ids.push(id - self.last_dense_id);
        self.dense_lats.push(lat - self.last_dense_lat);
        self.dense_lons.push(lon - self.last_dense_lon);

        self.last_dense_id = id;
        self.last_dense_lat = lat;
        self.last_dense_lon = lon;
        self.track_coords(decimicro_lat, decimicro_lon);

        // Tags: interleaved [key_sid, val_sid, ...] terminated by 0
        for (key, val) in tags {
            let key_idx = self.string_table.add(key);
            self.tag_key_indices.insert(key_idx);
            self.dense_keys_vals.push(key_idx as i32);
            self.dense_keys_vals
                .push(self.string_table.add(val) as i32);
        }
        self.dense_keys_vals.push(0);

        // Metadata - maintain parallel arrays with dense_ids.
        // When mixing nodes with and without metadata in the same block
        // (e.g. merge: base nodes have metadata, OSC replacements don't),
        // we must keep all DenseInfo arrays the same length as dense_ids.
        if let Some(meta) = metadata {
            if !self.has_dense_metadata && self.count > 0 {
                // First metadata in this block, but previous nodes had none.
                // Backfill zeroed entries so arrays stay aligned.
                self.backfill_default_dense_metadata();
            }
            self.add_dense_metadata(meta);
        } else if self.has_dense_metadata {
            // Previous nodes had metadata but this one doesn't.
            // Push default entry to keep arrays aligned.
            self.push_default_dense_metadata();
        }

        self.track_id(id);
        self.count += 1;
    }

    #[allow(clippy::cast_possible_wrap)]
    fn add_dense_metadata(&mut self, meta: &Metadata<'_>) {
        self.has_dense_metadata = true;

        // Version is NOT delta-encoded
        self.dense_versions.push(meta.version);

        // Timestamp - delta-encoded
        self.dense_timestamps
            .push(meta.timestamp - self.last_dense_timestamp);
        self.last_dense_timestamp = meta.timestamp;

        // Changeset - delta-encoded
        self.dense_changesets
            .push(meta.changeset - self.last_dense_changeset);
        self.last_dense_changeset = meta.changeset;

        // UID - delta-encoded
        self.dense_uids.push(meta.uid - self.last_dense_uid);
        self.last_dense_uid = meta.uid;

        // User SID - delta-encoded
        let user_sid = self.string_table.add(meta.user) as i32;
        self.dense_user_sids
            .push(user_sid - self.last_dense_user_sid);
        self.last_dense_user_sid = user_sid;

        // Visible (only meaningful for history files, but we preserve it)
        self.dense_visibles.push(meta.visible);
    }

    /// Backfill zeroed metadata for `self.count` nodes already added without it.
    ///
    /// Called when the first metadata-bearing node arrives but the block already
    /// contains nodes that were added with `metadata: None`. All delta accumulators
    /// are still at their initial value (0), so every backfilled delta is 0.
    fn backfill_default_dense_metadata(&mut self) {
        self.has_dense_metadata = true;
        for _ in 0..self.count {
            self.dense_versions.push(0);
            self.dense_timestamps.push(0);
            self.dense_changesets.push(0);
            self.dense_uids.push(0);
            self.dense_user_sids.push(0);
            self.dense_visibles.push(true);
        }
        // Delta state remains at 0 (initial value), matching the backfilled zeros.
    }

    /// Push a single default (zeroed) metadata entry for a node without metadata
    /// in a block that already has metadata from other nodes.
    ///
    /// Delta-encodes the transition back to zero for all fields so that a
    /// subsequent node with real metadata can delta from zero correctly.
    fn push_default_dense_metadata(&mut self) {
        self.dense_versions.push(0);

        self.dense_timestamps.push(-self.last_dense_timestamp);
        self.last_dense_timestamp = 0;

        self.dense_changesets.push(-self.last_dense_changeset);
        self.last_dense_changeset = 0;

        self.dense_uids.push(-self.last_dense_uid);
        self.last_dense_uid = 0;

        self.dense_user_sids.push(-self.last_dense_user_sid);
        self.last_dense_user_sid = 0;

        self.dense_visibles.push(true);
    }

    /// Add a way.
    ///
    /// `refs` are absolute node IDs (the builder handles delta encoding internally).
    #[hotpath::measure]
    pub fn add_way<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        refs: &[i64],
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_way(),
            "cannot add way: block full or wrong type"
        );
        self.block_type = Some(BlockType::Ways);
        encode_way(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.packed_vals_scratch,
            &mut self.info_scratch,
            &mut self.tag_key_indices,
            id,
            tags,
            refs,
            metadata,
        );
        self.track_id(id);
        self.count += 1;
    }

    /// Add a way with node locations embedded.
    ///
    /// `refs` are absolute node IDs, `locations` are `(decimicro_lat, decimicro_lon)` pairs.
    /// Both slices must have the same length.
    #[hotpath::measure]
    #[allow(clippy::too_many_arguments)]
    pub fn add_way_with_locations<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
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
        encode_way_with_locations(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.packed_vals_scratch,
            &mut self.packed_lat_scratch,
            &mut self.packed_lon_scratch,
            &mut self.info_scratch,
            &mut self.tag_key_indices,
            id,
            tags,
            refs,
            locations,
            metadata,
        );
        self.track_id(id);
        self.count += 1;
    }

    /// Add a relation.
    ///
    /// `members` are absolute member IDs (the builder handles delta encoding internally).
    #[hotpath::measure]
    pub fn add_relation<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        members: &[MemberData<'_>],
        metadata: Option<&Metadata<'_>>,
    ) {
        assert!(
            self.can_add_relation(),
            "cannot add relation: block full or wrong type"
        );
        self.block_type = Some(BlockType::Relations);
        encode_relation(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.packed_vals_scratch,
            &mut self.info_scratch,
            &mut self.tag_key_indices,
            id,
            tags,
            members,
            metadata,
        );
        self.track_id(id);
        self.count += 1;
    }

    // -----------------------------------------------------------------------
    // Raw-index methods for pre-seeded string table passthrough (merge only)
    // -----------------------------------------------------------------------

    /// Pre-seed the string table from an input block for index passthrough.
    ///
    /// After calling this, raw string table indices from the input block can be
    /// written directly via the `add_*_raw` methods. Indices from the input block
    /// map to the same indices in the output block (identity mapping).
    ///
    /// Must be called on an empty `BlockBuilder` (after `new()` or `take()`).
    pub(crate) fn pre_seed_string_table(&mut self, block: &PrimitiveBlock) {
        debug_assert!(self.is_empty(), "pre_seed must be called on empty builder");
        self.string_table.pre_seed(block);
        self.pre_seeded = true;
    }

    /// Add a dense node using pre-seeded string table indices.
    ///
    /// `raw_tags` are `(key_sid, val_sid)` pairs from [`DenseNode::raw_tags()`].
    /// The string table must have been pre-seeded from the same input block.
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn add_node_raw(
        &mut self,
        id: i64,
        decimicro_lat: i32,
        decimicro_lon: i32,
        raw_tags: impl Iterator<Item = (i32, i32)>,
        metadata: Option<&RawMetadata>,
    ) {
        assert!(
            self.can_add_node(),
            "cannot add node: block full or wrong type"
        );
        self.block_type = Some(BlockType::DenseNodes);

        let lat = i64::from(decimicro_lat);
        let lon = i64::from(decimicro_lon);
        self.dense_ids.push(id - self.last_dense_id);
        self.dense_lats.push(lat - self.last_dense_lat);
        self.dense_lons.push(lon - self.last_dense_lon);
        self.last_dense_id = id;
        self.last_dense_lat = lat;
        self.last_dense_lon = lon;
        self.track_coords(decimicro_lat, decimicro_lon);

        // Tags: write raw indices directly - no StringTable::add()
        for (key_sid, val_sid) in raw_tags {
            #[allow(clippy::cast_sign_loss)]
            self.tag_key_indices.insert(key_sid as u32);
            self.dense_keys_vals.push(key_sid);
            self.dense_keys_vals.push(val_sid);
        }
        self.dense_keys_vals.push(0); // delimiter

        if let Some(meta) = metadata {
            if !self.has_dense_metadata && self.count > 0 {
                self.backfill_default_dense_metadata();
            }
            self.add_dense_metadata_raw(meta);
        } else if self.has_dense_metadata {
            self.push_default_dense_metadata();
        }

        self.track_id(id);
        self.count += 1;
    }

    /// Add a way using raw wire-format bytes from the input PBF.
    ///
    /// All byte slices are raw protobuf packed field content from the source
    /// `WireWay`, passed through without decode or re-encode. Requires a
    /// pre-seeded string table (identity mapping of indices).
    pub(crate) fn add_way_raw_bytes(
        &mut self,
        id: i64,
        keys_data: &[u8],
        vals_data: &[u8],
        refs_data: &[u8],
        info_data: Option<&[u8]>,
    ) {
        assert!(
            self.can_add_way(),
            "cannot add way: block full or wrong type"
        );
        self.block_type = Some(BlockType::Ways);
        // Track tag key indices from raw packed varint keys
        collect_packed_varint_keys(keys_data, &mut self.tag_key_indices);
        encode_way_raw_bytes(
            &mut self.group_buf,
            &mut self.elem_scratch,
            id,
            keys_data,
            vals_data,
            refs_data,
            info_data,
        );
        self.track_id(id);
        self.count += 1;
    }

    /// Add a way using raw wire-format bytes, including LocationsOnWays data.
    ///
    /// Like `add_way_raw_bytes` but also passes through raw lat/lon packed field
    /// bytes (protobuf fields 9 and 10) for LocationsOnWays preservation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_way_raw_bytes_with_locations(
        &mut self,
        id: i64,
        keys_data: &[u8],
        vals_data: &[u8],
        refs_data: &[u8],
        lat_data: &[u8],
        lon_data: &[u8],
        info_data: Option<&[u8]>,
    ) {
        assert!(
            self.can_add_way(),
            "cannot add way: block full or wrong type"
        );
        self.block_type = Some(BlockType::Ways);
        collect_packed_varint_keys(keys_data, &mut self.tag_key_indices);
        encode_way_raw_bytes_with_locations(
            &mut self.group_buf,
            &mut self.elem_scratch,
            id,
            keys_data,
            vals_data,
            refs_data,
            lat_data,
            lon_data,
            info_data,
        );
        self.track_id(id);
        self.count += 1;
    }

    /// Add a relation using raw wire-format bytes from the input PBF.
    ///
    /// All byte slices are raw protobuf packed field content from the source
    /// `WireRelation`, passed through without decode or re-encode. Requires a
    /// pre-seeded string table (identity mapping of indices).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_relation_raw_bytes(
        &mut self,
        id: i64,
        keys_data: &[u8],
        vals_data: &[u8],
        roles_sid_data: &[u8],
        memids_data: &[u8],
        types_data: &[u8],
        info_data: Option<&[u8]>,
    ) {
        assert!(
            self.can_add_relation(),
            "cannot add relation: block full or wrong type"
        );
        self.block_type = Some(BlockType::Relations);
        // Track tag key indices from raw packed varint keys
        collect_packed_varint_keys(keys_data, &mut self.tag_key_indices);
        encode_relation_raw_bytes(
            &mut self.group_buf,
            &mut self.elem_scratch,
            id,
            keys_data,
            vals_data,
            roles_sid_data,
            memids_data,
            types_data,
            info_data,
        );
        self.track_id(id);
        self.count += 1;
    }

    #[allow(clippy::cast_possible_wrap)]
    fn add_dense_metadata_raw(&mut self, meta: &RawMetadata) {
        self.has_dense_metadata = true;
        self.dense_versions.push(meta.version);
        self.dense_timestamps
            .push(meta.timestamp - self.last_dense_timestamp);
        self.last_dense_timestamp = meta.timestamp;
        self.dense_changesets
            .push(meta.changeset - self.last_dense_changeset);
        self.last_dense_changeset = meta.changeset;
        self.dense_uids.push(meta.uid - self.last_dense_uid);
        self.last_dense_uid = meta.uid;
        self.dense_user_sids
            .push(meta.user_sid - self.last_dense_user_sid);
        self.last_dense_user_sid = meta.user_sid;
        self.dense_visibles.push(meta.visible);
    }

    /// Encode the current block into `encode_buf` and reset block state.
    ///
    /// Returns `None` if the block is empty (nothing to encode).
    /// After this returns `Some(index)`, `encode_buf` contains the serialized
    /// `PrimitiveBlock` bytes and the returned `BlobIndex` describes the block
    /// contents (element type, ID range, count).
    fn encode_block(&mut self) -> io::Result<Option<BlobIndex>> {
        let block_type = match self.block_type {
            Some(t) => t,
            None => return Ok(None),
        };

        let kind = match block_type {
            BlockType::DenseNodes => ElemKind::Node,
            BlockType::Ways => ElemKind::Way,
            BlockType::Relations => ElemKind::Relation,
        };
        let bbox = if kind == ElemKind::Node && self.min_lat <= self.max_lat {
            Some(crate::blob_meta::BlobBbox::new(
                self.min_lat,
                self.max_lat,
                self.min_lon,
                self.max_lon,
            ))
        } else {
            None
        };
        let index = BlobIndex {
            kind,
            min_id: self.min_id,
            max_id: self.max_id,
            count: self.count as u64,
            bbox,
        };

        // All block types: direct wire-format encoding
        self.encode_buf.clear();

        // PrimitiveBlock field 1: StringTable submessage
        self.string_table.encode_to(&mut self.encode_buf, &mut self.elem_scratch);

        match block_type {
            BlockType::DenseNodes => {
                // PrimitiveBlock field 2: PrimitiveGroup submessage
                // containing DenseNodes (field 2 of PrimitiveGroup).
                //
                // Note: we do NOT set granularity, lat_offset, lon_offset, or
                // date_granularity. Omitting them uses the protobuf defaults
                // (granularity=100, offsets=0, date_gran=1000).
                self.encode_dense_nodes_group();
            }
            BlockType::Ways | BlockType::Relations => {
                // PrimitiveBlock field 2: PrimitiveGroup submessage
                // group_buf already contains the Way/Relation field entries
                // that form the body of the PrimitiveGroup.
                encode_bytes_field(&mut self.encode_buf, 2, &self.group_buf);
            }
        }

        // Build tagdata from tracked tag key indices.
        // Sort indices by their string table byte content and serialize directly,
        // avoiding per-key Box<[u8]> allocations and the TagIndex intermediary.
        // Wire format: version (u8) + key_count (u16 LE) + repeated [key_len (u16 LE) + key bytes].
        self.last_tagdata = if self.tag_key_indices.is_empty() {
            None
        } else {
            self.tag_key_scratch.clear();
            self.tag_key_scratch.extend(
                self.tag_key_indices.iter()
                    .copied()
                    .filter(|&idx| !self.string_table.strings[idx as usize].is_empty()),
            );
            self.tag_key_scratch.sort_by(|&a, &b| {
                self.string_table.strings[a as usize]
                    .as_bytes()
                    .cmp(self.string_table.strings[b as usize].as_bytes())
            });
            let total: usize = 3 + self.tag_key_scratch.iter()
                .map(|&idx| 2 + self.string_table.strings[idx as usize].len())
                .sum::<usize>();
            let mut buf = Vec::with_capacity(total);
            buf.push(crate::blob_meta::TAG_INDEX_VERSION);
            #[allow(clippy::cast_possible_truncation)]
            let count = self.tag_key_scratch.len() as u16;
            buf.extend_from_slice(&count.to_le_bytes());
            for &idx in &self.tag_key_scratch {
                let key = self.string_table.strings[idx as usize].as_bytes();
                #[allow(clippy::cast_possible_truncation)]
                let key_len = key.len() as u16;
                buf.extend_from_slice(&key_len.to_le_bytes());
                buf.extend_from_slice(key);
            }
            Some(buf)
        };

        self.reset();
        Ok(Some(index))
    }

    /// Serialize the current block to `PrimitiveBlock` bytes and reset.
    ///
    /// Returns `None` if the block is empty. The returned slice borrows from
    /// an internal encode buffer that is reused across calls, eliminating
    /// per-block allocation after the first `take()`.
    #[hotpath::measure]
    pub fn take(&mut self) -> io::Result<Option<&[u8]>> {
        if self.encode_block()?.is_some() {
            Ok(Some(&self.encode_buf))
        } else {
            Ok(None)
        }
    }

    /// Serialize the current block and return owned bytes with a [`BlobIndex`]
    /// and optional pre-serialized tagdata.
    ///
    /// Like [`take`](Self::take) but returns an owned `Vec<u8>` instead of a
    /// borrow, plus a pre-computed [`BlobIndex`] describing the block contents
    /// (element type, ID range, count) and optional tagdata bytes for the
    /// BlobHeader tag key index. This eliminates the need for the writer to
    /// rescan the serialized bytes via `scan_block_ids` and `scan_block_tags`.
    ///
    /// Unlike `take()`, this does not reuse the encode buffer across calls -
    /// each call yields a fresh `Vec` and the internal buffer restarts empty.
    /// The total allocation is the same as `take()` + `to_vec()` but the
    /// `memcpy` is eliminated.
    #[hotpath::measure]
    pub(crate) fn take_owned(&mut self) -> io::Result<Option<OwnedBlock>> {
        if let Some(index) = self.encode_block()? {
            let tagdata = self.last_tagdata.take();
            Ok(Some((std::mem::take(&mut self.encode_buf), index, tagdata)))
        } else {
            Ok(None)
        }
    }

    /// Like [`take_owned`](Self::take_owned) but swaps `swap` in as the
    /// new `encode_buf` instead of leaving `Vec::new()` behind. Pass a
    /// cleared `Vec<u8>` with retained capacity (e.g. from a
    /// `BlockBufPool`); the next encode reuses the capacity instead of
    /// reallocating. When this returns `Some`, the returned tuple owns
    /// the previously-built block bytes. When it returns `None`, the
    /// swap still happens (the caller's `swap` becomes the new
    /// `encode_buf`), so subsequent encodes continue to benefit.
    #[hotpath::measure]
    pub(crate) fn take_owned_swap(&mut self, swap: Vec<u8>) -> io::Result<Option<OwnedBlock>> {
        if let Some(index) = self.encode_block()? {
            let tagdata = self.last_tagdata.take();
            let filled = std::mem::replace(&mut self.encode_buf, swap);
            Ok(Some((filled, index, tagdata)))
        } else {
            // No block was built this round, but still install the swap so
            // the next build uses the pool-sourced capacity. Drop whatever
            // was in encode_buf (empty Vec::new() in the steady state).
            self.encode_buf = swap;
            Ok(None)
        }
    }

    /// Encode DenseNodes directly to wire format into `encode_buf`.
    ///
    /// Encodes PrimitiveGroup (field 2 of PrimitiveBlock) containing
    /// DenseNodes (field 2 of PrimitiveGroup) with all packed fields.
    ///
    /// DenseNodes fields:
    ///   id (sint64 packed, field 1), denseinfo (submessage, field 5),
    ///   lat (sint64 packed, field 8), lon (sint64 packed, field 9),
    ///   keys_vals (int32 packed, field 10).
    ///
    /// DenseInfo fields:
    ///   version (int32 packed, field 1), timestamp (sint64 packed, field 2),
    ///   changeset (sint64 packed, field 3), uid (sint32 packed, field 4),
    ///   user_sid (sint32 packed, field 5), visible (bool packed, field 6).
    fn encode_dense_nodes_group(&mut self) {
        // Build the DenseNodes body into group_buf (reused scratch)
        self.group_buf.clear();

        // DenseNodes field 1: id (packed sint64)
        encode_packed_sint64(&mut self.group_buf, &mut self.elem_scratch, 1, &self.dense_ids);

        // DenseNodes field 5: denseinfo (submessage)
        if self.has_dense_metadata {
            // Build DenseInfo into elem_scratch
            self.elem_scratch.clear();
            self.packed_scratch.clear();

            // DenseInfo field 1: version (packed int32)
            encode_packed_int32(&mut self.elem_scratch, &mut self.packed_scratch, 1, &self.dense_versions);
            // DenseInfo field 2: timestamp (packed sint64)
            encode_packed_sint64(&mut self.elem_scratch, &mut self.packed_scratch, 2, &self.dense_timestamps);
            // DenseInfo field 3: changeset (packed sint64)
            encode_packed_sint64(&mut self.elem_scratch, &mut self.packed_scratch, 3, &self.dense_changesets);
            // DenseInfo field 4: uid (packed sint32)
            encode_packed_sint32(&mut self.elem_scratch, &mut self.packed_scratch, 4, &self.dense_uids);
            // DenseInfo field 5: user_sid (packed sint32)
            encode_packed_sint32(&mut self.elem_scratch, &mut self.packed_scratch, 5, &self.dense_user_sids);
            // DenseInfo field 6: visible (packed bool)
            encode_packed_bool(&mut self.elem_scratch, 6, &self.dense_visibles);

            encode_bytes_field(&mut self.group_buf, 5, &self.elem_scratch);
        }

        // DenseNodes field 8: lat (packed sint64)
        encode_packed_sint64(&mut self.group_buf, &mut self.elem_scratch, 8, &self.dense_lats);
        // DenseNodes field 9: lon (packed sint64)
        encode_packed_sint64(&mut self.group_buf, &mut self.elem_scratch, 9, &self.dense_lons);
        // DenseNodes field 10: keys_vals (packed int32)
        encode_packed_int32(&mut self.group_buf, &mut self.elem_scratch, 10, &self.dense_keys_vals);

        // Wrap DenseNodes as PrimitiveGroup field 2 (submessage)
        self.elem_scratch.clear();
        encode_bytes_field(&mut self.elem_scratch, 2, &self.group_buf);

        // Write PrimitiveGroup as PrimitiveBlock field 2
        encode_bytes_field(&mut self.encode_buf, 2, &self.elem_scratch);
    }

    fn reset(&mut self) {
        self.block_type = None;
        self.count = 0;
        self.min_id = i64::MAX;
        self.max_id = i64::MIN;
        self.min_lat = i32::MAX;
        self.max_lat = i32::MIN;
        self.min_lon = i32::MAX;
        self.max_lon = i32::MIN;
        self.tag_key_indices.clear();
        self.has_dense_metadata = false;
        self.pre_seeded = false;

        self.last_dense_id = 0;
        self.last_dense_lat = 0;
        self.last_dense_lon = 0;
        self.last_dense_timestamp = 0;
        self.last_dense_changeset = 0;
        self.last_dense_uid = 0;
        self.last_dense_user_sid = 0;

        // Clear wire-format accumulators for ways/relations.
        self.group_buf.clear();

        // Clear dense Vecs (encode_dense_nodes_group reads without consuming).
        self.dense_ids.clear();
        self.dense_lats.clear();
        self.dense_lons.clear();
        self.dense_keys_vals.clear();
        self.dense_versions.clear();
        self.dense_timestamps.clear();
        self.dense_changesets.clear();
        self.dense_uids.clear();
        self.dense_user_sids.clear();
        self.dense_visibles.clear();

        // Reset string table (reuse allocation, clear content).
        self.string_table.clear();
    }
}

// ---------------------------------------------------------------------------
// Wire-format encoding free functions
//
// These are free functions (not methods on BlockBuilder) to avoid borrow-checker
// issues: they need `&mut string_table` + `&mut` multiple scratch buffers
// simultaneously, which is impossible with `&mut self`.
// ---------------------------------------------------------------------------

/// Encode an `int32` field unconditionally (even when value is 0).
///
/// Writes the field tag + varint(0) even for the zero value. This differs from
/// `encode_int32_field` which skips zero values (matching non-optional fields).
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int32(buf: &mut Vec<u8>, field: u32, value: i32) {
    debug_assert!(field <= 15, "single-byte tag requires field <= 15, got {field}");
    buf.push((field << 3) as u8); // wire type 0 (varint)
    encode_varint(buf, value as i64 as u64);
}

/// Encode an `int64` field unconditionally (even when value is 0).
///
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int64(buf: &mut Vec<u8>, field: u32, value: i64) {
    debug_assert!(field <= 15, "single-byte tag requires field <= 15, got {field}");
    buf.push((field << 3) as u8);
    encode_varint(buf, value as u64);
}

/// Encode a `uint32` field unconditionally (even when value is 0).
///
/// Only valid for field numbers <= 15 (single-byte tag encoding).
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_optional_uint32(buf: &mut Vec<u8>, field: u32, value: u32) {
    debug_assert!(field <= 15, "single-byte tag requires field <= 15, got {field}");
    buf.push((field << 3) as u8);
    encode_varint(buf, u64::from(value));
}

/// Encode an `Info` submessage from high-level [`Metadata`].
///
/// Uses unconditional field writers to always emit all metadata fields,
/// even when their value is zero (matching the OSMPBF convention).
fn encode_info_to(
    info: &mut Vec<u8>,
    string_table: &mut StringTable,
    meta: &Metadata<'_>,
) {
    info.clear();
    // Field 1: version (optional int32) - always present
    encode_optional_int32(info, 1, meta.version);
    // Field 2: timestamp (optional int64) - always present
    encode_optional_int64(info, 2, meta.timestamp);
    // Field 3: changeset (optional int64) - always present
    encode_optional_int64(info, 3, meta.changeset);
    // Field 4: uid (optional int32) - always present
    encode_optional_int32(info, 4, meta.uid);
    // Field 5: user_sid (optional uint32) - always present
    encode_optional_uint32(info, 5, string_table.add(meta.user));
    // Field 6: visible (optional bool) - only emit when false
    // When visible=true, the current code leaves info.visible as None (prost skips it).
    // When visible=false, it sets Some(false), and prost writes tag + varint(0).
    if !meta.visible {
        info.push(6 << 3); // tag for field 6, wire type 0
        info.push(0x00); // false = varint(0)
    }
}

/// Encode tag key/value pairs into packed fields 2 (keys) and 3 (vals) on `elem`.
///
/// Uses two scratch buffers (`keys_buf` and `vals_buf`) for single-pass dual-buffer
/// encoding. Each tag key index is also inserted into `tag_key_indices`.
fn encode_tags<'t>(
    string_table: &mut StringTable,
    elem: &mut Vec<u8>,
    keys_buf: &mut Vec<u8>,
    vals_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
) {
    keys_buf.clear();
    vals_buf.clear();
    for (key, val) in tags {
        let key_idx = string_table.add(key);
        tag_key_indices.insert(key_idx);
        encode_varint(keys_buf, u64::from(key_idx));
        encode_varint(vals_buf, u64::from(string_table.add(val)));
    }
    if !keys_buf.is_empty() {
        encode_bytes_field(elem, 2, keys_buf);
        encode_bytes_field(elem, 3, vals_buf);
    }
}

/// Encode a Way and append it as `PrimitiveGroup.ways` (field 3) to `group_buf`.
#[allow(clippy::too_many_arguments)]
fn encode_way<'t>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed_keys: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    refs: &[i64],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();

    // Field 1: id (int64)
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(string_table, elem, packed_keys, packed_vals, tag_key_indices, tags);

    // Field 4: info (submessage)
    if let Some(meta) = metadata {
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Field 8: refs (packed sint64, delta-encoded)
    if !refs.is_empty() {
        packed_keys.clear();
        let mut last_ref: i64 = 0;
        for &r in refs {
            encode_varint(packed_keys, zigzag_encode_64(r - last_ref));
            last_ref = r;
        }
        encode_bytes_field(elem, 8, packed_keys);
    }

    // Wrap as PrimitiveGroup field 3 (Way submessage)
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way with embedded node locations (fields 9/10: lat/lon).
///
/// Uses three packed buffers in a single zip loop for refs/lat/lon to avoid
/// iterating the data three separate times.
#[allow(clippy::too_many_arguments)]
fn encode_way_with_locations<'t>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed_refs: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    packed_lats: &mut Vec<u8>,
    packed_lons: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    refs: &[i64],
    locations: &[(i32, i32)],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(string_table, elem, packed_refs, packed_vals, tag_key_indices, tags);

    if let Some(meta) = metadata {
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Fields 8, 9, 10: refs + lat + lon (all delta-encoded, single pass)
    if !refs.is_empty() {
        let mut last_ref: i64 = 0;
        let mut last_lat: i64 = 0;
        let mut last_lon: i64 = 0;

        packed_refs.clear();
        packed_lats.clear();
        packed_lons.clear();

        for (&r, &(loc_lat, loc_lon)) in refs.iter().zip(locations.iter()) {
            encode_varint(packed_refs, zigzag_encode_64(r - last_ref));
            last_ref = r;
            let lat = i64::from(loc_lat);
            encode_varint(packed_lats, zigzag_encode_64(lat - last_lat));
            last_lat = lat;
            let lon = i64::from(loc_lon);
            encode_varint(packed_lons, zigzag_encode_64(lon - last_lon));
            last_lon = lon;
        }

        encode_bytes_field(elem, 8, packed_refs);
        encode_bytes_field(elem, 9, packed_lats);
        encode_bytes_field(elem, 10, packed_lons);
    }

    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way from raw wire-format bytes (zero decode/reencode passthrough).
///
/// All byte slices are raw protobuf packed field content from `WireWay`,
/// written directly with field tag + length prefix.
fn encode_way_raw_bytes(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    refs_data: &[u8],
    info_data: Option<&[u8]>,
) {
    debug_assert!(
        keys_data.is_empty() == vals_data.is_empty(),
        "keys/vals must be paired: keys={} vals={} bytes",
        keys_data.len(),
        vals_data.len(),
    );
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, refs_data);
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way with LocationsOnWays raw bytes and append to `group_buf`.
///
/// Same as `encode_way_raw_bytes` but also writes fields 9 (lat) and 10 (lon).
#[allow(clippy::too_many_arguments)]
fn encode_way_raw_bytes_with_locations(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    refs_data: &[u8],
    lat_data: &[u8],
    lon_data: &[u8],
    info_data: Option<&[u8]>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, refs_data);
    if !lat_data.is_empty() {
        encode_bytes_field(elem, 9, lat_data);
    }
    if !lon_data.is_empty() {
        encode_bytes_field(elem, 10, lon_data);
    }
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Relation and append it as `PrimitiveGroup.relations` (field 4) to `group_buf`.
#[allow(clippy::too_many_arguments)]
fn encode_relation<'t>(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed: &mut Vec<u8>,
    packed_vals: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    tag_key_indices: &mut FxHashSet<u32>,
    id: i64,
    tags: impl IntoIterator<Item = (&'t str, &'t str)>,
    members: &[MemberData<'_>],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals
    encode_tags(string_table, elem, packed, packed_vals, tag_key_indices, tags);

    if let Some(meta) = metadata {
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Members: three parallel packed arrays
    if !members.is_empty() {
        // Field 8: roles_sid (packed int32)
        packed.clear();
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        for m in members {
            let role_sid = string_table.add(m.role) as i32;
            encode_varint(packed, role_sid as i64 as u64);
        }
        encode_bytes_field(elem, 8, packed);

        // Field 9: memids (packed sint64, delta-encoded)
        packed.clear();
        let mut last_memid: i64 = 0;
        for m in members {
            encode_varint(packed, zigzag_encode_64(m.id.id() - last_memid));
            last_memid = m.id.id();
        }
        encode_bytes_field(elem, 9, packed);

        // Field 10: types (packed int32)
        packed.clear();
        for m in members {
            // Protobuf int32 wire encoding: sign-extend i32 → i64 → u64.
            // MemberType enum values are 0/1/2 so no actual sign extension occurs.
            let mt = member_type_value(m.id.member_type());
            #[allow(clippy::cast_sign_loss)]
            encode_varint(packed, mt as u64);
        }
        encode_bytes_field(elem, 10, packed);
    }

    // PrimitiveGroup field 4 = Relation
    encode_bytes_field(group_buf, 4, elem);
}

/// Encode a Relation from raw wire-format bytes (zero decode/reencode passthrough).
///
/// All byte slices are raw protobuf packed field content from `WireRelation`,
/// written directly with field tag + length prefix.
#[allow(clippy::too_many_arguments)]
fn encode_relation_raw_bytes(
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    id: i64,
    keys_data: &[u8],
    vals_data: &[u8],
    roles_sid_data: &[u8],
    memids_data: &[u8],
    types_data: &[u8],
    info_data: Option<&[u8]>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);
    encode_bytes_field(elem, 2, keys_data);
    encode_bytes_field(elem, 3, vals_data);
    if let Some(info) = info_data {
        encode_bytes_field(elem, 4, info);
    }
    encode_bytes_field(elem, 8, roles_sid_data);
    encode_bytes_field(elem, 9, memids_data);
    encode_bytes_field(elem, 10, types_data);
    encode_bytes_field(group_buf, 4, elem);
}

/// Decode packed varint uint32 values from raw bytes and insert them into a set.
///
/// Used to extract string table key indices from raw way/relation keys_data
/// (packed uint32 protobuf field) for tag key tracking.
#[allow(clippy::cast_possible_truncation)]
fn collect_packed_varint_keys(data: &[u8], indices: &mut FxHashSet<u32>) {
    let mut cur = protohoggr::Cursor::new(data);
    while !cur.is_empty() {
        if let Ok(val) = cur.read_varint() {
            indices.insert(val as u32);
        } else {
            break;
        }
    }
}

