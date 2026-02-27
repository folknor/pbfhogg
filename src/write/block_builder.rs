//! Block builder for constructing PBF `PrimitiveBlock` messages.
//!
//! Accumulates OSM elements (nodes, ways, relations) and serializes them into
//! protobuf `PrimitiveBlock` bytes suitable for [`PbfWriter`](crate::writer::PbfWriter).
//!
//! Handles string table construction, delta encoding, dense node packing,
//! and block size limits (8000 entities per block, matching osmium).

use crate::PrimitiveBlock;
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;
use std::io;

use super::wire::{
    encode_bytes_field, encode_bytes_field_always, encode_int64_field,
    encode_packed_bool, encode_packed_int32, encode_packed_sint32, encode_packed_sint64,
    encode_sint64_field_always, encode_varint, zigzag_encode_64,
};

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
/// user names) — never from untrusted PBF input. There is no risk of
/// HashDoS attacks, which is the sole reason the standard library defaults to
/// the slower SipHash-1-3 hasher.
///
/// **Performance:** FxHash is a simple, non-cryptographic hash (multiply +
/// rotate) that is substantially faster than SipHash for short strings — which
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
/// - Custom perfect hashing: Not viable because the string set is dynamic —
///   we do not know all strings upfront.
struct StringTable {
    strings: Vec<String>,
    index: FxHashMap<String, u32>,
}

impl StringTable {
    fn new() -> Self {
        let mut st = StringTable {
            strings: Vec::with_capacity(256),
            index: FxHashMap::with_capacity_and_hasher(256, Default::default()),
        };
        st.strings.push(String::new()); // index 0 = empty string
        st
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// ## Fast path (cache hit, ~99% of calls)
    ///
    /// `self.index.get(s)` looks up the `&str` directly via the `Borrow` trait —
    /// no allocation, just FxHash + probe. This is the hot path: a typical 8000-
    /// element block has ~1200 unique strings but ~16,000+ add() calls, so the
    /// vast majority are cache hits.
    ///
    /// ## Slow path (cache miss, ~1% of calls)
    ///
    /// On the first occurrence of a string, falls through to the `Entry` API
    /// which allocates once for the HashMap key, then clones it into the Vec.
    /// The double-hash (get then entry) costs ~3ns extra on 1% of calls — 0.03ns
    /// amortized, negligible.
    #[allow(clippy::cast_possible_truncation)]
    fn add(&mut self, s: &str) -> u32 {
        // Fast path: string already interned — hash-only lookup, no allocation.
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        // Slow path: first occurrence, allocate and insert.
        let next_idx = self.strings.len() as u32;
        match self.index.entry(s.to_owned()) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                self.strings.push(e.key().clone());
                e.insert(next_idx);
                next_idx
            }
        }
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
        self.strings.push(String::new());
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
        // Unknown member types from newer PBF producers — round-trip as Node
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

    // Wire-format accumulators for ways and relations
    group_buf: Vec<u8>,       // per-block: all serialized way/relation messages
    elem_scratch: Vec<u8>,    // per-element body (cleared each add_way/add_relation call)
    packed_scratch: Vec<u8>,  // per-field packed content
    info_scratch: Vec<u8>,    // Info sub-message body

    // Reusable encode buffer for take() — avoids allocating a fresh Vec<u8> per block.
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

            // Wire-format scratch buffers — left at zero capacity since
            // way/relation blocks will grow as needed, and dense-node blocks
            // never use them.
            group_buf: Vec::new(),
            elem_scratch: Vec::new(),
            packed_scratch: Vec::new(),
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

        // Metadata — maintain parallel arrays with dense_ids.
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
        encode_way(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.info_scratch,
            id,
            tags,
            refs,
            metadata,
        );
        self.count += 1;
    }

    /// Add a way with node locations embedded.
    ///
    /// `refs` are absolute node IDs, `locations` are `(decimicro_lat, decimicro_lon)` pairs.
    /// Both slices must have the same length.
    #[hotpath::measure]
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
        encode_way_with_locations(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.info_scratch,
            id,
            tags,
            refs,
            locations,
            metadata,
        );
        self.count += 1;
    }

    /// Add a relation.
    ///
    /// `members` are absolute member IDs (the builder handles delta encoding internally).
    #[hotpath::measure]
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
        encode_relation(
            &mut self.string_table,
            &mut self.group_buf,
            &mut self.elem_scratch,
            &mut self.packed_scratch,
            &mut self.info_scratch,
            id,
            tags,
            members,
            metadata,
        );
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

        // Tags: write raw indices directly — no StringTable::add()
        for (key_sid, val_sid) in raw_tags {
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
        encode_way_raw_bytes(
            &mut self.group_buf,
            &mut self.elem_scratch,
            id,
            keys_data,
            vals_data,
            refs_data,
            info_data,
        );
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

    /// Serialize the current block to `PrimitiveBlock` bytes and reset.
    ///
    /// Returns `None` if the block is empty. The returned slice borrows from
    /// an internal encode buffer that is reused across calls, eliminating
    /// per-block allocation after the first `take()`.
    #[hotpath::measure]
    pub fn take(&mut self) -> io::Result<Option<&[u8]>> {
        let block_type = match self.block_type {
            Some(t) => t,
            None => return Ok(None),
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

        self.reset();
        Ok(Some(&self.encode_buf))
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
            let mut packed_scratch = Vec::new();

            // DenseInfo field 1: version (packed int32)
            encode_packed_int32(&mut self.elem_scratch, &mut packed_scratch, 1, &self.dense_versions);
            // DenseInfo field 2: timestamp (packed sint64)
            encode_packed_sint64(&mut self.elem_scratch, &mut packed_scratch, 2, &self.dense_timestamps);
            // DenseInfo field 3: changeset (packed sint64)
            encode_packed_sint64(&mut self.elem_scratch, &mut packed_scratch, 3, &self.dense_changesets);
            // DenseInfo field 4: uid (packed sint32)
            encode_packed_sint32(&mut self.elem_scratch, &mut packed_scratch, 4, &self.dense_uids);
            // DenseInfo field 5: user_sid (packed sint32)
            encode_packed_sint32(&mut self.elem_scratch, &mut packed_scratch, 5, &self.dense_user_sids);
            // DenseInfo field 6: visible (packed bool)
            encode_packed_bool(&mut self.elem_scratch, &mut packed_scratch, 6, &self.dense_visibles);

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
/// Matches prost's encoding of `Option<i32>::Some(0)` which writes the
/// field tag + varint(0) even for the zero value. This differs from
/// `encode_int32_field` which skips zero values (matching non-optional fields).
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int32(buf: &mut Vec<u8>, field: u32, value: i32) {
    buf.push((field << 3) as u8); // wire type 0 (varint)
    encode_varint(buf, value as i64 as u64);
}

/// Encode an `int64` field unconditionally (even when value is 0).
///
/// Matches prost's encoding of `Option<i64>::Some(0)`.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_optional_int64(buf: &mut Vec<u8>, field: u32, value: i64) {
    buf.push((field << 3) as u8);
    encode_varint(buf, value as u64);
}

/// Encode a `uint32` field unconditionally (even when value is 0).
///
/// Matches prost's encoding of `Option<u32>::Some(0)`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_optional_uint32(buf: &mut Vec<u8>, field: u32, value: u32) {
    buf.push((field << 3) as u8);
    encode_varint(buf, u64::from(value));
}

/// Encode an `Info` submessage from high-level [`Metadata`].
///
/// Uses unconditional field writers (matching prost's `Option<T>::Some(v)` encoding)
/// to produce bit-identical output with the previous prost-based `build_info`.
fn encode_info_to(
    info: &mut Vec<u8>,
    string_table: &mut StringTable,
    meta: &Metadata<'_>,
) {
    info.clear();
    // Field 1: version (optional int32) — always present
    encode_optional_int32(info, 1, meta.version);
    // Field 2: timestamp (optional int64) — always present
    encode_optional_int64(info, 2, meta.timestamp);
    // Field 3: changeset (optional int64) — always present
    encode_optional_int64(info, 3, meta.changeset);
    // Field 4: uid (optional int32) — always present
    encode_optional_int32(info, 4, meta.uid);
    // Field 5: user_sid (optional uint32) — always present
    encode_optional_uint32(info, 5, string_table.add(meta.user));
    // Field 6: visible (optional bool) — only emit when false
    // When visible=true, the current code leaves info.visible as None (prost skips it).
    // When visible=false, it sets Some(false), and prost writes tag + varint(0).
    if !meta.visible {
        info.push(6 << 3); // tag for field 6, wire type 0
        info.push(0x00); // false = varint(0)
    }
}

/// Encode a Way and append it as `PrimitiveGroup.ways` (field 3) to `group_buf`.
#[allow(clippy::too_many_arguments)]
fn encode_way(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    id: i64,
    tags: &[(&str, &str)],
    refs: &[i64],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();

    // Field 1: id (int64)
    encode_int64_field(elem, 1, id);

    // Fields 2+3: keys/vals (packed uint32)
    if !tags.is_empty() {
        packed.clear();
        for &(key, _) in tags {
            encode_varint(packed, u64::from(string_table.add(key)));
        }
        encode_bytes_field(elem, 2, packed);

        packed.clear();
        for &(_, val) in tags {
            encode_varint(packed, u64::from(string_table.add(val)));
        }
        encode_bytes_field(elem, 3, packed);
    }

    // Field 4: info (submessage)
    if let Some(meta) = metadata {
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Field 8: refs (packed sint64, delta-encoded)
    if !refs.is_empty() {
        packed.clear();
        let mut last_ref: i64 = 0;
        for &r in refs {
            encode_varint(packed, zigzag_encode_64(r - last_ref));
            last_ref = r;
        }
        encode_bytes_field(elem, 8, packed);
    }

    // Wrap as PrimitiveGroup field 3 (Way submessage)
    encode_bytes_field(group_buf, 3, elem);
}

/// Encode a Way with embedded node locations (fields 9/10: lat/lon).
#[allow(clippy::too_many_arguments)]
fn encode_way_with_locations(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    id: i64,
    tags: &[(&str, &str)],
    refs: &[i64],
    locations: &[(i32, i32)],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    if !tags.is_empty() {
        packed.clear();
        for &(key, _) in tags {
            encode_varint(packed, u64::from(string_table.add(key)));
        }
        encode_bytes_field(elem, 2, packed);

        packed.clear();
        for &(_, val) in tags {
            encode_varint(packed, u64::from(string_table.add(val)));
        }
        encode_bytes_field(elem, 3, packed);
    }

    if let Some(meta) = metadata {
        encode_info_to(info_buf, string_table, meta);
        encode_bytes_field(elem, 4, info_buf);
    }

    // Fields 8, 9, 10: refs + lat + lon (all delta-encoded)
    if !refs.is_empty() {
        let mut last_ref: i64 = 0;
        let mut last_lat: i64 = 0;
        let mut last_lon: i64 = 0;

        // Field 8: refs (packed sint64)
        packed.clear();
        for &r in refs {
            encode_varint(packed, zigzag_encode_64(r - last_ref));
            last_ref = r;
        }
        encode_bytes_field(elem, 8, packed);

        // Field 9: lat (packed sint64)
        packed.clear();
        for &(loc_lat, _) in locations {
            let lat = i64::from(loc_lat);
            encode_varint(packed, zigzag_encode_64(lat - last_lat));
            last_lat = lat;
        }
        encode_bytes_field(elem, 9, packed);

        // Field 10: lon (packed sint64)
        packed.clear();
        for &(_, loc_lon) in locations {
            let lon = i64::from(loc_lon);
            encode_varint(packed, zigzag_encode_64(lon - last_lon));
            last_lon = lon;
        }
        encode_bytes_field(elem, 10, packed);
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

/// Encode a Relation and append it as `PrimitiveGroup.relations` (field 4) to `group_buf`.
#[allow(clippy::too_many_arguments)]
fn encode_relation(
    string_table: &mut StringTable,
    group_buf: &mut Vec<u8>,
    elem: &mut Vec<u8>,
    packed: &mut Vec<u8>,
    info_buf: &mut Vec<u8>,
    id: i64,
    tags: &[(&str, &str)],
    members: &[MemberData<'_>],
    metadata: Option<&Metadata<'_>>,
) {
    elem.clear();
    encode_int64_field(elem, 1, id);

    if !tags.is_empty() {
        packed.clear();
        for &(key, _) in tags {
            encode_varint(packed, u64::from(string_table.add(key)));
        }
        encode_bytes_field(elem, 2, packed);

        packed.clear();
        for &(_, val) in tags {
            encode_varint(packed, u64::from(string_table.add(val)));
        }
        encode_bytes_field(elem, 3, packed);
    }

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

// ---------------------------------------------------------------------------
// Header builder
// ---------------------------------------------------------------------------

/// Builder for constructing the `OSMHeader` blob that starts every PBF file.
///
/// Use [`new`](Self::new) for a blank header, or [`from_header`](Self::from_header)
/// to copy bbox and replication metadata from an existing [`HeaderBlock`].
///
/// # Examples
///
/// ```rust
/// use pbfhogg::block_builder::HeaderBuilder;
///
/// // Minimal header (tests, quick scripts)
/// let bytes = HeaderBuilder::new().build()?;
///
/// // Sorted PBF with bounding box
/// let bytes = HeaderBuilder::new()
///     .bbox(9.0, 54.0, 13.0, 58.0)
///     .sorted()
///     .build()?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct HeaderBuilder<'a> {
    bbox: Option<(f64, f64, f64, f64)>,
    replication_timestamp: Option<i64>,
    replication_sequence_number: Option<i64>,
    replication_base_url: Option<&'a str>,
    optional_features: Vec<&'a str>,
    sorted: bool,
    writing_program: &'a str,
}

impl Default for HeaderBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> HeaderBuilder<'a> {
    /// Create a blank header builder.
    ///
    /// The writing program defaults to `"pbfhogg"`. Required features
    /// (`OsmSchema-V0.6`, `DenseNodes`) are always included.
    #[must_use]
    pub fn new() -> Self {
        HeaderBuilder {
            bbox: None,
            replication_timestamp: None,
            replication_sequence_number: None,
            replication_base_url: None,
            optional_features: Vec::new(),
            sorted: false,
            writing_program: "pbfhogg",
        }
    }

    /// Create a header builder pre-populated with bbox and replication metadata
    /// from an existing [`HeaderBlock`].
    ///
    /// Optional features (including `Sort.Type_then_ID`) are **not** copied —
    /// call [`.sorted()`](Self::sorted) explicitly if the output should declare
    /// sorted order.
    #[must_use]
    pub fn from_header(header: &'a crate::HeaderBlock) -> Self {
        let mut hb = Self::new();
        if let Some(b) = header.bbox() {
            hb.bbox = Some((b.left, b.bottom, b.right, b.top));
        }
        hb.replication_timestamp = header.osmosis_replication_timestamp();
        hb.replication_sequence_number = header.osmosis_replication_sequence_number();
        hb.replication_base_url = header.osmosis_replication_base_url();
        hb
    }

    /// Set the bounding box (left/bottom/right/top in degrees).
    #[must_use]
    pub fn bbox(mut self, left: f64, bottom: f64, right: f64, top: f64) -> Self {
        self.bbox = Some((left, bottom, right, top));
        self
    }

    /// Set the replication timestamp (seconds since UNIX epoch).
    #[must_use]
    pub fn replication_timestamp(mut self, ts: i64) -> Self {
        self.replication_timestamp = Some(ts);
        self
    }

    /// Set the replication sequence number.
    #[must_use]
    pub fn replication_sequence_number(mut self, seq: i64) -> Self {
        self.replication_sequence_number = Some(seq);
        self
    }

    /// Set the replication base URL.
    #[must_use]
    pub fn replication_base_url(mut self, url: &'a str) -> Self {
        self.replication_base_url = Some(url);
        self
    }

    /// Declare `Sort.Type_then_ID` — elements are sorted by type then by ID.
    #[must_use]
    pub fn sorted(mut self) -> Self {
        self.sorted = true;
        self
    }

    /// Add an arbitrary optional feature string (e.g. `"LocationsOnWays"`).
    ///
    /// For `Sort.Type_then_ID`, prefer the type-safe [`.sorted()`](Self::sorted)
    /// method instead.
    #[must_use]
    pub fn optional_feature(mut self, feature: &'a str) -> Self {
        self.optional_features.push(feature);
        self
    }

    /// Override the writing program name (default: `"pbfhogg"`).
    #[must_use]
    pub fn writing_program(mut self, program: &'a str) -> Self {
        self.writing_program = program;
        self
    }

    /// Serialize the header into protobuf bytes suitable for
    /// [`PbfWriter::write_header`](crate::writer::PbfWriter::write_header).
    ///
    /// HeaderBlock fields: bbox (submessage, field 1),
    /// required_features (repeated string, field 4),
    /// optional_features (repeated string, field 5),
    /// writingprogram (string, field 16), source (string, field 17),
    /// osmosis_replication_timestamp (int64, field 32),
    /// osmosis_replication_sequence_number (int64, field 33),
    /// osmosis_replication_base_url (string, field 34).
    #[allow(clippy::cast_possible_truncation)]
    pub fn build(self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Field 1: bbox (HeaderBBox submessage, optional)
        // HeaderBBox: left (sint64, field 1), right (sint64, field 2),
        //             top (sint64, field 3), bottom (sint64, field 4).
        if let Some((left, bottom, right, top)) = self.bbox {
            let mut bbox_buf = Vec::new();
            encode_sint64_field_always(&mut bbox_buf, 1, (left * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 2, (right * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 3, (top * 1e9).round() as i64);
            encode_sint64_field_always(&mut bbox_buf, 4, (bottom * 1e9).round() as i64);
            encode_bytes_field(&mut buf, 1, &bbox_buf);
        }

        // Field 4: required_features (repeated string)
        encode_bytes_field(&mut buf, 4, b"OsmSchema-V0.6");
        encode_bytes_field(&mut buf, 4, b"DenseNodes");

        // Field 5: optional_features (repeated string)
        if self.sorted {
            encode_bytes_field(&mut buf, 5, crate::HeaderBlock::SORT_TYPE_THEN_ID.as_bytes());
        }
        for feature in &self.optional_features {
            encode_bytes_field(&mut buf, 5, feature.as_bytes());
        }

        // Field 16: writingprogram (string)
        encode_bytes_field(&mut buf, 16, self.writing_program.as_bytes());

        // Field 32: osmosis_replication_timestamp (int64)
        if let Some(ts) = self.replication_timestamp {
            encode_int64_field(&mut buf, 32, ts);
        }

        // Field 33: osmosis_replication_sequence_number (int64)
        if let Some(seq) = self.replication_sequence_number {
            encode_int64_field(&mut buf, 33, seq);
        }

        // Field 34: osmosis_replication_base_url (string)
        if let Some(url) = self.replication_base_url {
            encode_bytes_field(&mut buf, 34, url.as_bytes());
        }

        Ok(buf)
    }
}
