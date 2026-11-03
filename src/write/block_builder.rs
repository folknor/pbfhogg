//! Block builder for constructing PBF `PrimitiveBlock` messages.
//!
//! Accumulates OSM elements (nodes, ways, relations) and serializes them into
//! protobuf `PrimitiveBlock` bytes suitable for [`PbfWriter`](crate::writer::PbfWriter).
//!
//! Handles string table construction, delta encoding, dense node packing,
//! and per-block element caps. The default cap matches the PBF interop
//! convention used by osmium.
//! Callers that choose a different cap also provide a bound on pre-allocation
//! through [`BlockBuilder::with_element_cap_prealloc`].

use crate::ElementKind;
use crate::PrimitiveBlock;
use crate::blob_meta::BlobIndex;
use crate::error::Result;
use rustc_hash::FxHashSet;

/// Encoded block bytes plus the metadata needed to frame them into a blob.
///
/// The worker-to-consumer vehicle: rayon workers build these, the writer
/// thread frames them. `way_members` carries the optional `BlobHeader`
/// field-5 (`pbfhogg.WayMembers-v1`) payload; every current producer sets it
/// to `None` (a later landing populates it for enriched output).
pub(crate) struct OwnedBlock {
    /// Serialized `PrimitiveBlock` bytes.
    pub bytes: Vec<u8>,
    /// Pre-computed blob index (element type, ID range, count).
    pub index: BlobIndex,
    /// Optional pre-serialized tagdata (`BlobHeader` field 4 tag key index).
    pub tagdata: Option<Vec<u8>>,
    /// Optional full field-5 way-member payload (preamble included).
    pub way_members: Option<Vec<u8>>,
}

/// An encoded block lent from its [`BlockBuilder`]'s buffers: what
/// [`BlockBuilder::lend_encoded`] hands its caller, for the duration of that
/// call.
pub(crate) struct EncodedBlock<'a> {
    /// Serialized `PrimitiveBlock` bytes.
    pub bytes: &'a [u8],
    /// Pre-computed blob index (element type, ID range, count).
    pub index: BlobIndex,
    /// Optional pre-serialized tagdata (`BlobHeader` field 4 tag key index).
    pub tagdata: Option<&'a [u8]>,
}

use crate::tunables::MAX_RETAINED_SCRATCH_CAPACITY;
use protohoggr::{
    encode_bytes_field, encode_packed_bool, encode_packed_int32, encode_packed_sint32,
    encode_packed_sint64,
};

use super::encode::{
    collect_checked_keys, encode_relation, encode_relation_raw_bytes, encode_way,
    encode_way_raw_bytes, encode_way_raw_bytes_with_locations, encode_way_with_locations,
};
use super::string_table::StringTable;

// Preserve the public builder import path for downstream callers.
pub use crate::write::header_builder::HeaderBuilder;

/// Default per-block element cap used by [`BlockBuilder::new`]; owned by
/// `crate::tunables`, re-exported here so the public path is unchanged.
pub use crate::tunables::DEFAULT_ELEMENT_CAP;

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
pub(super) fn member_type_value(mt: MemberType) -> i32 {
    match mt {
        MemberType::Node => 0,
        MemberType::Way => 1,
        MemberType::Relation => 2,
        // Unreachable on the encode path: `BlockBuilder::add_relation` rejects
        // a member whose type has no enum value and poisons the builder, so no
        // `Unknown` ever reaches the encoder. The arm exists only because the
        // enum is non-exhaustive by construction; 0 is never written for one.
        MemberType::Unknown(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// BlockBuilder
// ---------------------------------------------------------------------------

/// Builds `PrimitiveBlock` protobuf messages for PBF output.
///
/// Elements are added one at a time. When the block reaches the cap
/// (default [`DEFAULT_ELEMENT_CAP`]; configurable via
/// [`Self::with_element_cap_prealloc`]),
/// [`should_flush`](Self::should_flush) returns `true` and
/// [`take`](Self::take) should be called to serialize and reset.
///
/// Each block contains only one element type (nodes OR ways OR relations).
/// Adding a different type when the block is non-empty, or adding at the
/// cap, is refused: the element is not encoded and the builder is poisoned,
/// so every later `take` / `take_owned` fails. Check `can_add_*` and flush
/// first.
///
/// Dropping a builder is cleanup only: what it holds, a refusal included,
/// is discarded without an error. A refusal reaches the caller only through
/// finalization, so finalize every builder whose output counts.
pub struct BlockBuilder {
    string_table: StringTable,
    block_type: Option<BlockType>,
    count: usize,
    cap: usize,

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
    group_buf: Vec<u8>,      // per-block: all serialized way/relation messages
    elem_scratch: Vec<u8>,   // per-element body (cleared each add_way/add_relation call)
    packed_scratch: Vec<u8>, // per-field packed content (refs in location path)
    packed_vals_scratch: Vec<u8>, // tag values packed encoding (dual-buffer single-pass)
    packed_lat_scratch: Vec<u8>, // way location lat encoding (single-pass)
    packed_lon_scratch: Vec<u8>, // way location lon encoding (single-pass)
    info_scratch: Vec<u8>,   // Info sub-message body

    // The block encode buffer. `take` and `lend_encoded` lend it and keep it
    // for the next block; `take_owned` moves it out, and the next block then
    // reserves `encode_reserve` up front instead of regrowing from empty.
    encode_buf: Vec<u8>,
    // Up-front reservation for the encode buffer after a `take_owned`: the
    // last owned block's length plus slack, never above the retention limit.
    encode_reserve: usize,

    // The source block's string table length while the table is pre-seeded
    // from it (`pre_seed_string_table`), so a raw string index can be checked
    // against the slots it was copied with rather than the grown table.
    // Cleared by every flush (`reset`); apply-changes' raw writers re-seed
    // after a mid-block flush.
    seeded: Option<usize>,

    // Sticky poison. Set by an `add_*` method that was handed input it cannot
    // encode without corrupting the output (a way whose ref and location
    // counts disagree, a relation member whose type has no protobuf enum
    // value), or called when `can_add_*` says no (the block holds another
    // kind, or is at its cap). The offending element is not encoded, and
    // `encode_block` refuses to produce a block from that point on, so no
    // such element can reach a file. Deliberately NOT cleared by `reset()`:
    // once a caller has fed the builder something unencodable, every later
    // block from the same builder is suspect, and a writer that keeps going
    // has already lost the element. Later `add_*` calls are not checked
    // against it (that would cost every element a load and a branch): they
    // may still mutate the doomed block's buffers, but nothing is emitted.
    //
    // Dropping a builder is cleanup and checks nothing, poisoned or not.
    // Finalization is where the poison is enforced; abandonment is legal,
    // because a caller abandons its builder on every `?` bail-out, and the
    // builder cannot tell that from a caller that dropped it instead of
    // finalizing. A drop-time assert was tried: compiled into every debug
    // build, it turned bad input (a relation member of an unknown type,
    // poisoning the builder) plus a later unrelated error into a panic that
    // replaced the caller's error, the reason `PbfWriter`'s `Drop` keeps
    // its forgot-to-flush check to the crate's unit tests.
    poison: Option<String>,
}

impl Default for BlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockBuilder {
    /// Create a new, empty block builder with the default element cap.
    ///
    /// Equivalent to `BlockBuilder::with_element_cap_prealloc(
    /// DEFAULT_ELEMENT_CAP, DEFAULT_ELEMENT_CAP)`.
    pub fn new() -> Self {
        Self::with_element_cap_prealloc(DEFAULT_ELEMENT_CAP, DEFAULT_ELEMENT_CAP.get())
    }

    /// Create a new, empty block builder with a caller-supplied per-block
    /// element cap and a separate ceiling on how much is *pre-allocated*.
    ///
    /// `cap` is a policy: how many elements go in a block before it is flushed.
    /// The pre-allocation is only an optimization - the vectors grow if they
    /// need to - so the two do not have to be the same number, and on an
    /// attacker-shaped input they must not be. `repack --elements-per-blob N`
    /// takes N straight off the command line and hands it to one `BlockBuilder`
    /// per worker plus a central one; at `N = usize::MAX / 2` that was several
    /// unbounded allocations requested by a single integer, before a byte of
    /// input had been read.
    ///
    /// `prealloc_bound` limits the initial capacity of each element vector.
    /// `repack` derives this budget from the input file size, so a tiny input
    /// with a huge requested cap cannot demand unbounded initial allocations.
    /// Compressed input may contain more elements than bytes; the vectors can
    /// still grow beyond the budget, and `cap` remains the flush threshold.
    ///
    /// A zero cap is unrepresentable (`NonZeroUsize`), so neither this
    /// constructor nor the commands that take a cap from their caller check
    /// for one.
    pub fn with_element_cap_prealloc(cap: std::num::NonZeroUsize, prealloc_bound: usize) -> Self {
        let cap = cap.get();
        let prealloc = cap.min(prealloc_bound.max(1));
        // dense_keys_vals carries 2*N entries on average for tagless nodes
        // (one key/val pair plus a 0 delimiter is more typical, but the
        // historical pre-alloc is 2*cap; preserved here). saturating_mul
        // guards against pathological caps near usize::MAX.
        let kv_cap = prealloc.saturating_mul(2);
        BlockBuilder {
            string_table: StringTable::new(),
            block_type: None,
            count: 0,
            cap,
            min_id: i64::MAX,
            max_id: i64::MIN,
            min_lat: i32::MAX,
            max_lat: i32::MIN,
            min_lon: i32::MAX,
            max_lon: i32::MIN,

            tag_key_indices: FxHashSet::default(),
            tag_key_scratch: Vec::new(),
            last_tagdata: None,

            dense_ids: Vec::with_capacity(prealloc),
            dense_lats: Vec::with_capacity(prealloc),
            dense_lons: Vec::with_capacity(prealloc),
            dense_keys_vals: Vec::with_capacity(kv_cap),

            dense_versions: Vec::with_capacity(prealloc),
            dense_timestamps: Vec::with_capacity(prealloc),
            dense_changesets: Vec::with_capacity(prealloc),
            dense_uids: Vec::with_capacity(prealloc),
            dense_user_sids: Vec::with_capacity(prealloc),
            dense_visibles: Vec::with_capacity(prealloc),
            has_dense_metadata: false,

            last_dense_id: 0,
            last_dense_lat: 0,
            last_dense_lon: 0,
            last_dense_timestamp: 0,
            last_dense_changeset: 0,
            last_dense_uid: 0,
            last_dense_user_sid: 0,

            group_buf: Vec::new(),
            elem_scratch: Vec::new(),
            packed_scratch: Vec::new(),
            packed_vals_scratch: Vec::new(),
            packed_lat_scratch: Vec::new(),
            packed_lon_scratch: Vec::new(),
            info_scratch: Vec::new(),

            encode_buf: Vec::new(),
            encode_reserve: 0,

            seeded: None,
            poison: None,
        }
    }

    /// Returns `true` if the block contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Record an unencodable element and refuse to produce blocks from here on.
    ///
    /// First poison wins: the earliest unencodable element is the one that
    /// explains the failure, and a later one is just a consequence of the
    /// caller not having checked yet.
    fn poison(&mut self, reason: String) {
        if self.poison.is_none() {
            self.poison = Some(reason);
        }
    }

    /// Refuse an element the current block cannot take, because the block
    /// holds another kind or is at its cap.
    ///
    /// The `add_*` methods used to `assert!` here. Every caller is required
    /// to check `can_add_*` first and every caller in this crate does, so
    /// this is a caller bug rather than a data condition - but a library
    /// caller's bug should be an error, not a panic. It poisons rather than
    /// returning a `Result` from `add_*`: the poison is the builder's one
    /// failure model, finalization (`take` / `take_owned`) is its error
    /// boundary, and no block can be emitted after a refusal. Kept out of
    /// line and cold so the per-element guard costs what the assert did.
    #[cold]
    #[inline(never)]
    fn refuse(&mut self, attempted: BlockType, id: i64) {
        let reason = self.refusal(attempted, id);
        self.poison(reason);
    }

    /// [`refuse`](Self::refuse)'s reason, which the raw adds return as an
    /// error instead of poisoning.
    #[cold]
    #[inline(never)]
    fn refusal(&self, attempted: BlockType, id: i64) -> String {
        let what = match attempted {
            BlockType::DenseNodes => "node",
            BlockType::Ways => "way",
            BlockType::Relations => "relation",
        };
        // A block of another kind is reported as that, whether or not it is
        // also full: its fullness is not why this element was refused.
        match self.block_type {
            Some(held) if held != attempted => {
                let held = match held {
                    BlockType::DenseNodes => "nodes",
                    BlockType::Ways => "ways",
                    BlockType::Relations => "relations",
                };
                format!(
                    "{what} {id} added to a block holding {held}; a block holds one \
                     element kind, so take the block before adding another kind"
                )
            }
            _ => format!(
                "{what} {id} added to a full block ({} element cap); take the \
                 block when can_add returns false",
                self.cap
            ),
        }
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
        if decimicro_lat < self.min_lat {
            self.min_lat = decimicro_lat;
        }
        if decimicro_lat > self.max_lat {
            self.max_lat = decimicro_lat;
        }
        if decimicro_lon < self.min_lon {
            self.min_lon = decimicro_lon;
        }
        if decimicro_lon > self.max_lon {
            self.max_lon = decimicro_lon;
        }
    }

    /// Returns `true` if the string table has been pre-seeded from an input block.
    ///
    /// Cleared by every flush, encoding or not. apply-changes' raw writers
    /// use it to tell when the block in progress can take raw string
    /// indices and when it must be flushed and re-seeded first (after a
    /// mid-block flush, or behind diff elements that started a block).
    #[inline]
    pub(crate) fn is_pre_seeded(&self) -> bool {
        self.seeded.is_some()
    }

    /// Returns `true` if the block has reached its per-block element cap
    /// (default [`DEFAULT_ELEMENT_CAP`]; configurable via
    /// [`Self::with_element_cap_prealloc`]).
    #[inline]
    pub fn should_flush(&self) -> bool {
        self.count >= self.cap
    }

    /// Returns `true` if a node can be added to the current block.
    ///
    /// The `can_add_*` methods answer kind and capacity only, not whether the
    /// builder has been poisoned. When one returns `false`, call
    /// [`take`](Self::take) (or flush however the caller flushes) before
    /// adding: an `add_*` the block cannot take is refused rather than
    /// encoded, and the builder is poisoned - the element is lost and every
    /// later `take` / `take_owned` returns an error, so no block is emitted
    /// after the refusal. Subsequent additions may still modify the
    /// builder's internal buffers.
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
    pub fn add_node<'t>(
        &mut self,
        id: i64,
        decimicro_lat: i32,
        decimicro_lon: i32,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        metadata: Option<&Metadata<'_>>,
    ) {
        if !self.can_add_node() {
            self.refuse(BlockType::DenseNodes, id);
            return;
        }
        self.block_type = Some(BlockType::DenseNodes);

        let lat = i64::from(decimicro_lat);
        let lon = i64::from(decimicro_lon);

        // Dense fields store signed deltas. Preserve their two's-complement
        // wire values explicitly so debug and release builds encode alike.
        self.dense_ids.push(id.wrapping_sub(self.last_dense_id));
        self.dense_lats.push(lat.wrapping_sub(self.last_dense_lat));
        self.dense_lons.push(lon.wrapping_sub(self.last_dense_lon));

        self.last_dense_id = id;
        self.last_dense_lat = lat;
        self.last_dense_lon = lon;
        self.track_coords(decimicro_lat, decimicro_lon);

        // Tags: interleaved [key_sid, val_sid, ...] terminated by 0
        for (key, val) in tags {
            let key_idx = self.string_table.add(key);
            self.tag_key_indices.insert(key_idx);
            self.dense_keys_vals.push(key_idx as i32);
            self.dense_keys_vals.push(self.string_table.add(val) as i32);
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
            .push(meta.timestamp.wrapping_sub(self.last_dense_timestamp));
        self.last_dense_timestamp = meta.timestamp;

        // Changeset - delta-encoded
        self.dense_changesets
            .push(meta.changeset.wrapping_sub(self.last_dense_changeset));
        self.last_dense_changeset = meta.changeset;

        // UID - delta-encoded
        self.dense_uids
            .push(meta.uid.wrapping_sub(self.last_dense_uid));
        self.last_dense_uid = meta.uid;

        // User SID - delta-encoded
        let user_sid = self.string_table.add(meta.user) as i32;
        self.dense_user_sids
            .push(user_sid.wrapping_sub(self.last_dense_user_sid));
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
    /// subsequent node with real metadata can delta from zero correctly. The
    /// signed minimum has no positive counterpart, so these wire deltas use
    /// two's-complement wrapping when negating it.
    fn push_default_dense_metadata(&mut self) {
        self.dense_versions.push(0);

        self.dense_timestamps
            .push(self.last_dense_timestamp.wrapping_neg());
        self.last_dense_timestamp = 0;

        self.dense_changesets
            .push(self.last_dense_changeset.wrapping_neg());
        self.last_dense_changeset = 0;

        self.dense_uids.push(self.last_dense_uid.wrapping_neg());
        self.last_dense_uid = 0;

        self.dense_user_sids
            .push(self.last_dense_user_sid.wrapping_neg());
        self.last_dense_user_sid = 0;

        self.dense_visibles.push(true);
    }

    /// Add a way.
    ///
    /// `refs` are absolute node IDs (the builder handles delta encoding internally).
    pub fn add_way<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        refs: &[i64],
        metadata: Option<&Metadata<'_>>,
    ) {
        if !self.can_add_way() {
            self.refuse(BlockType::Ways, id);
            return;
        }
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
    ///
    /// A length mismatch is not encoded: the encoder zips the two slices, so
    /// the shorter one would silently decide the way's node count and the way
    /// would be written with refs missing. The builder is poisoned instead and
    /// the next [`take`](Self::take) / `take_owned` fails.
    #[allow(clippy::too_many_arguments)]
    pub fn add_way_with_locations<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        refs: &[i64],
        locations: &[(i32, i32)],
        metadata: Option<&Metadata<'_>>,
    ) {
        if refs.len() != locations.len() {
            self.poison(format!(
                "way {id}: {} node ref(s) but {} location(s); encoding it would \
                 silently drop refs",
                refs.len(),
                locations.len(),
            ));
            return;
        }
        if !self.can_add_way() {
            self.refuse(BlockType::Ways, id);
            return;
        }
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
    ///
    /// Every member type must be one of node / way / relation. A
    /// [`MemberType::Unknown`] carries a wire value the `Relation.MemberType`
    /// protobuf enum cannot express, and writing it as anything else produces
    /// a file that is valid PBF and semantically wrong - no downstream check
    /// catches that. Such a member poisons the builder instead, and the next
    /// [`take`](Self::take) / `take_owned` fails.
    ///
    /// [`MemberType::Unknown`]: crate::MemberType::Unknown
    pub fn add_relation<'t>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        members: &[MemberData<'_>],
        metadata: Option<&Metadata<'_>>,
    ) {
        if let Some(raw) = members.iter().find_map(|m| match m.id.member_type() {
            MemberType::Unknown(raw) => Some(raw),
            _ => None,
        }) {
            self.poison(format!(
                "relation {id}: member type {raw} has no Relation.MemberType enum \
                 value; encoding it would rewrite the member as a node",
            ));
            return;
        }
        if !self.can_add_relation() {
            self.refuse(BlockType::Relations, id);
            return;
        }
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
    // Raw string indices, copied from a pre-seeded source block (apply-changes)
    // -----------------------------------------------------------------------
    //
    // apply-changes rewrites a base blob the diff touches by copying each
    // surviving element's string indices unresolved: the output table starts
    // as a slot-for-slot copy of the source block's (`pre_seed_string_table`),
    // so source index N names output slot N. The diff's own elements in the
    // same output block intern their strings behind that copy. A source index
    // at or past the source table therefore does not stay out of range in the
    // output: it names one of the diff's strings (a valid file with the wrong
    // tag, role or user), or, past those too, a slot that does not exist (a
    // tag key there would make the tagdata build index past the table).
    // `PrimitiveBlock` construction checks the index widths but resolves no
    // index (`validate_block`), so the raw adds check every index they copy
    // against the seeded length and refuse a bad one with an error naming the
    // element. A refusal also poisons the builder (already surfaced: the
    // caller has the error), since the block in progress may hold part of
    // the refused element: no later take can emit it. The raw adds are the
    // only way a raw index enters a block, so the bound holds for every block
    // they build.
    //
    // The comparison is the reader's own rule for each field: tag keys and
    // values as the `u32` construction fits them to, relation roles and
    // `Info` users as the `i32` they narrow to (negative refused, as
    // `RelMember::role` and `Info::user` refuse it), dense users as the
    // running sum of their deltas. Dense and plain nodes arrive with their
    // indices decoded; a way or relation costs one varint pass over its
    // values and roles, a scan of its `Info` for the user, and the key pass
    // the tag key tracking already made.

    /// Pre-seed the string table from an input block for index passthrough.
    ///
    /// After this, raw string indices from `block` can be written with the
    /// `add_*_raw*` methods: the output table holds every source slot at its
    /// source index (duplicates and empty strings included), and the methods
    /// check each index against the source table's length.
    ///
    /// The builder must hold no element: its state is reset here (sticky
    /// poison aside), so a builder left mid-way by a failed earlier task
    /// starts clean. A source table whose slot zero is
    /// missing or not the empty string is refused: an output block's slot
    /// zero is always the empty string (it is the dense end-of-tags marker,
    /// and the diff's elements and default dense metadata use index zero as
    /// the empty user), so a source reference to a nonempty slot zero would
    /// change meaning in the copy.
    pub(crate) fn pre_seed_string_table(&mut self, block: &PrimitiveBlock) -> Result<()> {
        if !self.is_empty() {
            return Err(crate::error::Error::internal(
                "a block builder holding elements was pre-seeded from another block",
            ));
        }
        if block.string_table_entry(0) != Some("") {
            return Err(crate::error::Error::invalid_data(
                "a rewritten block's string table does not begin with the empty string",
            ));
        }
        self.reset();
        self.string_table.pre_seed(block);
        self.seeded = Some(block.string_table_len());
        Ok(())
    }

    /// The source table length a raw index is checked against.
    fn seeded_len(&self) -> Result<usize> {
        self.seeded.ok_or_else(|| {
            crate::error::Error::internal(
                "a raw string index was added to a block builder that is not pre-seeded",
            )
        })
    }

    /// A raw add the current block cannot take (another kind, or full): an
    /// error rather than [`refuse`](Self::refuse)'s poison, since the raw
    /// adds already return one.
    #[cold]
    #[inline(never)]
    fn raw_refusal(&self, attempted: BlockType, id: i64) -> crate::error::Error {
        crate::error::Error::internal(self.refusal(attempted, id))
    }

    /// Refuse a raw add over a bad string index: the error goes back to the
    /// caller, and the builder is poisoned with it, so the block in progress,
    /// which may hold part of the refused element, is never emitted.
    #[cold]
    #[inline(never)]
    fn refuse_raw_index(&mut self, err: crate::error::Error) -> crate::error::Error {
        self.poison(err.to_string());
        err
    }

    /// Add a dense node using pre-seeded string table indices.
    ///
    /// `raw_tags` are `(key_sid, val_sid)` pairs as the reader yields them
    /// (`DenseNode::raw_tags`, cast to `u32`, or a plain `Node`'s
    /// `raw_tags`), from the block the table was pre-seeded from. An index
    /// at or past that block's table, or a user index outside it, is an
    /// error, and poisons the builder.
    ///
    /// A plain node's tag may have key index zero (the empty string), which
    /// a dense node cannot carry: zero ends a dense node's tags. Such a key
    /// is written as the empty string's nonzero slot (`StringTable::add`
    /// never returns zero), which reads back as the same key.
    pub(crate) fn add_node_raw(
        &mut self,
        id: i64,
        decimicro_lat: i32,
        decimicro_lon: i32,
        raw_tags: impl Iterator<Item = (u32, u32)>,
        metadata: Option<&RawMetadata>,
    ) -> Result<()> {
        if !self.can_add_node() {
            return Err(self.raw_refusal(BlockType::DenseNodes, id));
        }
        let seeded = self.seeded_len()?;
        if let Some(meta) = metadata
            && let Err(e) = check_signed_sid(meta.user_sid, seeded)
        {
            let e = e.context(raw_sid_context("node", id, "user name", meta.user_sid));
            return Err(self.refuse_raw_index(e));
        }
        self.block_type = Some(BlockType::DenseNodes);

        let lat = i64::from(decimicro_lat);
        let lon = i64::from(decimicro_lon);
        self.dense_ids.push(id.wrapping_sub(self.last_dense_id));
        self.dense_lats.push(lat.wrapping_sub(self.last_dense_lat));
        self.dense_lons.push(lon.wrapping_sub(self.last_dense_lon));
        self.last_dense_id = id;
        self.last_dense_lat = lat;
        self.last_dense_lon = lon;
        self.track_coords(decimicro_lat, decimicro_lon);

        for (key_sid, val_sid) in raw_tags {
            if !index_in(key_sid, seeded) {
                let e = bad_unsigned_sid("node", id, "tag key", key_sid);
                return Err(self.refuse_raw_index(e));
            }
            if !index_in(val_sid, seeded) {
                let e = bad_unsigned_sid("node", id, "tag value", val_sid);
                return Err(self.refuse_raw_index(e));
            }
            let key_sid = if key_sid == 0 {
                self.string_table.add("")
            } else {
                key_sid
            };
            self.tag_key_indices.insert(key_sid);
            // Both are below a table length, which a block's byte limit
            // keeps far below `i32::MAX`.
            self.dense_keys_vals.push(key_sid.cast_signed());
            self.dense_keys_vals.push(val_sid.cast_signed());
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
        Ok(())
    }

    /// Check a raw way's or relation's copied values, member roles (empty for
    /// a way) and `Info` user against the source table, and track its keys,
    /// checked the same way; on a bad index the builder is poisoned.
    fn check_raw_strings(
        &mut self,
        kind: &'static str,
        id: i64,
        keys: &[u8],
        vals: &[u8],
        roles: &[u8],
        info: Option<&[u8]>,
    ) -> Result<()> {
        let seeded = self.seeded_len()?;
        let checked = check_raw_values(kind, id, vals, roles, info, seeded).and_then(|()| {
            collect_checked_keys(keys, seeded, &mut self.tag_key_indices)
                .map_err(|sid| bad_unsigned_sid(kind, id, "tag key", sid))
        });
        checked.map_err(|e| self.refuse_raw_index(e))
    }

    /// Add a way using raw wire-format bytes from the input PBF.
    ///
    /// All byte slices are raw protobuf packed field content from the source
    /// `WireWay`, passed through without decode or re-encode. Requires a
    /// pre-seeded string table (identity mapping of indices); a string index
    /// outside the source table is an error, and poisons the builder.
    pub(crate) fn add_way_raw_bytes(
        &mut self,
        id: i64,
        keys_data: &[u8],
        vals_data: &[u8],
        refs_data: &[u8],
        info_data: Option<&[u8]>,
    ) -> Result<()> {
        if !self.can_add_way() {
            return Err(self.raw_refusal(BlockType::Ways, id));
        }
        self.check_raw_strings("way", id, keys_data, vals_data, &[], info_data)?;
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
        self.track_id(id);
        self.count += 1;
        Ok(())
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
    ) -> Result<()> {
        if !self.can_add_way() {
            return Err(self.raw_refusal(BlockType::Ways, id));
        }
        self.check_raw_strings("way", id, keys_data, vals_data, &[], info_data)?;
        self.block_type = Some(BlockType::Ways);
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
        Ok(())
    }

    /// Add a relation using raw wire-format bytes from the input PBF.
    ///
    /// All byte slices are raw protobuf packed field content from the source
    /// `WireRelation`, passed through without decode or re-encode. Requires a
    /// pre-seeded string table (identity mapping of indices); a string index
    /// outside the source table is an error, and poisons the builder.
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
    ) -> Result<()> {
        if !self.can_add_relation() {
            return Err(self.raw_refusal(BlockType::Relations, id));
        }
        self.check_raw_strings(
            "relation",
            id,
            keys_data,
            vals_data,
            roles_sid_data,
            info_data,
        )?;
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
        self.track_id(id);
        self.count += 1;
        Ok(())
    }

    #[allow(clippy::cast_possible_wrap)]
    fn add_dense_metadata_raw(&mut self, meta: &RawMetadata) {
        self.has_dense_metadata = true;
        self.dense_versions.push(meta.version);
        // Raw signed metadata uses the same wrapping wire-delta convention as
        // values decoded and re-encoded through `add_dense_metadata`.
        self.dense_timestamps
            .push(meta.timestamp.wrapping_sub(self.last_dense_timestamp));
        self.last_dense_timestamp = meta.timestamp;
        self.dense_changesets
            .push(meta.changeset.wrapping_sub(self.last_dense_changeset));
        self.last_dense_changeset = meta.changeset;
        self.dense_uids
            .push(meta.uid.wrapping_sub(self.last_dense_uid));
        self.last_dense_uid = meta.uid;
        self.dense_user_sids
            .push(meta.user_sid.wrapping_sub(self.last_dense_user_sid));
        self.last_dense_user_sid = meta.user_sid;
        self.dense_visibles.push(meta.visible);
    }

    /// Encode the current block into `encode_buf` and reset block state.
    ///
    /// Returns `None` if the block is empty (nothing to encode).
    /// After this returns `Some(index)`, `encode_buf` contains the serialized
    /// `PrimitiveBlock` bytes and the returned `BlobIndex` describes the block
    /// contents (element type, ID range, count).
    fn encode_block(&mut self) -> Result<Option<BlobIndex>> {
        if let Some(reason) = self.poison.as_deref() {
            return Err(crate::error::Error::invalid_data(format!(
                "BlockBuilder refused to encode a block: {reason}"
            )));
        }
        let Some(block_type) = self.block_type else {
            // Nothing to encode, but the builder is reset all the same, so
            // every flush ends a block: a pre-seeded table left behind by a
            // block that took no element (a blob the diff deletes whole)
            // would otherwise be the next pre-seed's starting point.
            self.reset();
            return Ok(None);
        };

        // Every tracked key is in the table: `add` returns slots it has, and
        // the raw adds refuse a key at or past the seeded source table, which
        // the table only grows from. The tagdata build below indexes them.

        let kind = match block_type {
            BlockType::DenseNodes => ElementKind::Node,
            BlockType::Ways => ElementKind::Way,
            BlockType::Relations => ElementKind::Relation,
        };
        let bbox = if kind == ElementKind::Node && self.min_lat <= self.max_lat {
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

        // All block types: direct wire-format encoding. The previous block's
        // bytes (lent by `take` / `lend_encoded`) are dead from here, so this
        // is where an outlier encode buffer is released - never in `reset`,
        // which runs before the caller reads the block it just encoded.
        if self.encode_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            self.encode_buf = Vec::new();
        }
        self.encode_buf.clear();
        self.encode_buf.reserve(self.encode_reserve);

        // PrimitiveBlock field 1: StringTable submessage
        self.string_table.encode_to(&mut self.encode_buf)?;

        match block_type {
            BlockType::DenseNodes => {
                // PrimitiveBlock field 2: PrimitiveGroup submessage
                // containing DenseNodes (field 2 of PrimitiveGroup).
                //
                // Note: we do NOT set granularity, lat_offset, lon_offset, or
                // date_granularity. Omitting them uses the protobuf defaults
                // (granularity=100, offsets=0, date_gran=1000).
                self.encode_dense_nodes_group()?;
            }
            BlockType::Ways | BlockType::Relations => {
                // PrimitiveBlock field 2: PrimitiveGroup submessage
                // group_buf already contains the Way/Relation field entries
                // that form the body of the PrimitiveGroup.
                encode_bytes_field(&mut self.encode_buf, 2, &self.group_buf);
            }
        }

        // Build tagdata from tracked tag key indices. Sorting indices by their
        // string table byte content avoids per-key Box<[u8]> allocations and
        // the TagIndex intermediary; the encoding itself routes through
        // `blob_meta::encode_tag_index` so the wire format has exactly one
        // implementation. This used to be a second copy of it, carrying its own
        // pair of silent `as u16` truncations on the key count and each key
        // length - the encoder now refuses to emit an index it cannot
        // represent, and a `None` tagdata simply means "decode this blob".
        self.last_tagdata = if self.tag_key_indices.is_empty() {
            None
        } else {
            self.tag_key_scratch.clear();
            // An empty string is still an explicit tag key; retain it.
            self.tag_key_scratch
                .extend(self.tag_key_indices.iter().copied());
            let strings = &self.string_table;
            self.tag_key_scratch
                .sort_by(|&a, &b| strings.get(a as usize).cmp(strings.get(b as usize)));
            if self.seeded.is_some() {
                // Identity-preserving source tables can hold the same key at
                // multiple indices. Tagdata encodes a set of key strings,
                // so collapse equal strings (by content, not slot) after
                // sorting their source slots.
                self.tag_key_scratch
                    .dedup_by(|a, b| strings.get(*a as usize) == strings.get(*b as usize));
            }
            crate::blob_meta::encode_tag_index(
                self.tag_key_scratch
                    .iter()
                    .map(|&idx| strings.get(idx as usize)),
            )
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
    pub fn take(&mut self) -> Result<Option<&[u8]>> {
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
    /// Unlike `take()`, this moves the encode buffer out: the caller owns
    /// the bytes and may send them to another thread. The next block then
    /// starts from a buffer reserved at this block's length plus an eighth
    /// (at most the retention limit), so a long-lived builder pays one
    /// allocation per owned block rather than regrowing from empty by
    /// doubling. A caller that frames on the encoding thread should use
    /// [`lend_encoded`](Self::lend_encoded), which allocates nothing.
    #[hotpath::measure]
    pub(crate) fn take_owned(&mut self) -> Result<Option<OwnedBlock>> {
        if let Some(index) = self.encode_block()? {
            let tagdata = self.last_tagdata.take();
            let bytes = std::mem::take(&mut self.encode_buf);
            self.encode_reserve = bytes
                .len()
                .saturating_add(bytes.len() / 8)
                .min(MAX_RETAINED_SCRATCH_CAPACITY);
            Ok(Some(OwnedBlock {
                bytes,
                index,
                tagdata,
                way_members: None,
            }))
        } else {
            Ok(None)
        }
    }

    /// Serialize the current block and lend it to `use_block`, with its
    /// [`BlobIndex`] and tagdata, from the builder's own buffers, which stay
    /// with the builder for the next block. For callers that frame the block
    /// before touching the builder again (a worker framing its own output),
    /// so no encode buffer changes hands. Returns `None`, without calling
    /// `use_block`, when the block is empty.
    ///
    /// Once `use_block` has returned, an encode buffer grown past
    /// `MAX_RETAINED_SCRATCH_CAPACITY` by this block is released, so a
    /// builder kept for a run never sits on its last outlier block between
    /// tasks (the lent bytes are dead by then; `take`, whose borrow outlives
    /// the call, leaves that release to the next `encode_block`). It is not
    /// released when `use_block` fails: the caller is then failing its run
    /// and drops the builder.
    pub(crate) fn lend_encoded<R>(
        &mut self,
        use_block: impl FnOnce(EncodedBlock<'_>) -> Result<R>,
    ) -> Result<Option<R>> {
        let Some(index) = self.encode_block()? else {
            return Ok(None);
        };
        let used = use_block(EncodedBlock {
            bytes: &self.encode_buf,
            index,
            tagdata: self.last_tagdata.as_deref(),
        })?;
        if self.encode_buf.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            self.encode_buf = Vec::new();
        }
        Ok(Some(used))
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
    fn encode_dense_nodes_group(&mut self) -> Result<()> {
        // Build the DenseNodes body into group_buf (reused scratch)
        self.group_buf.clear();

        // DenseNodes field 1: id (packed sint64)
        encode_packed_sint64(
            &mut self.group_buf,
            &mut self.elem_scratch,
            1,
            &self.dense_ids,
        );

        // DenseNodes field 5: denseinfo (submessage)
        if self.has_dense_metadata {
            // Build DenseInfo into elem_scratch
            self.elem_scratch.clear();
            self.packed_scratch.clear();

            // DenseInfo field 1: version (packed int32)
            encode_packed_int32(
                &mut self.elem_scratch,
                &mut self.packed_scratch,
                1,
                &self.dense_versions,
            );
            // DenseInfo field 2: timestamp (packed sint64)
            encode_packed_sint64(
                &mut self.elem_scratch,
                &mut self.packed_scratch,
                2,
                &self.dense_timestamps,
            );
            // DenseInfo field 3: changeset (packed sint64)
            encode_packed_sint64(
                &mut self.elem_scratch,
                &mut self.packed_scratch,
                3,
                &self.dense_changesets,
            );
            // DenseInfo field 4: uid (packed sint32)
            encode_packed_sint32(
                &mut self.elem_scratch,
                &mut self.packed_scratch,
                4,
                &self.dense_uids,
            );
            // DenseInfo field 5: user_sid (packed sint32)
            encode_packed_sint32(
                &mut self.elem_scratch,
                &mut self.packed_scratch,
                5,
                &self.dense_user_sids,
            );
            // DenseInfo field 6: visible (packed bool)
            encode_packed_bool(&mut self.elem_scratch, 6, &self.dense_visibles);

            encode_bytes_field(&mut self.group_buf, 5, &self.elem_scratch);
        }

        // DenseNodes field 8: lat (packed sint64)
        encode_packed_sint64(
            &mut self.group_buf,
            &mut self.elem_scratch,
            8,
            &self.dense_lats,
        );
        // DenseNodes field 9: lon (packed sint64)
        encode_packed_sint64(
            &mut self.group_buf,
            &mut self.elem_scratch,
            9,
            &self.dense_lons,
        );
        // DenseNodes field 10: keys_vals (packed int32)
        encode_packed_int32(
            &mut self.group_buf,
            &mut self.elem_scratch,
            10,
            &self.dense_keys_vals,
        );

        // Write PrimitiveBlock field 2 (PrimitiveGroup) holding PrimitiveGroup
        // field 2 (DenseNodes, the body in group_buf). Both lengths follow
        // from the body's, so the body is copied once, straight behind the
        // two prefixes, instead of being wrapped in a scratch buffer first.
        // The body is never empty: a dense block holds at least one node, and
        // its id field is written whatever the id.
        debug_assert!(!self.group_buf.is_empty());
        let overflow = || crate::error::Error::internal("dense node group length overflows usize");
        let dense_len = self.group_buf.len();
        let group_len = super::encode::len_field_size(dense_len).ok_or_else(overflow)?;
        self.encode_buf
            .reserve(super::encode::len_field_size(group_len).ok_or_else(overflow)?);
        for len in [group_len, dense_len] {
            protohoggr::encode_tag(&mut self.encode_buf, 2, protohoggr::WIRE_LEN);
            protohoggr::encode_varint(&mut self.encode_buf, len as u64);
        }
        self.encode_buf.extend_from_slice(&self.group_buf);
        Ok(())
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
        self.seeded = None;

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
        self.release_outliers();
    }

    /// Drop every retained buffer that one block grew past the retention
    /// limit (`MAX_RETAINED_SCRATCH_CAPACITY` per buffer, the frame scratch's
    /// rule), back to empty, so a builder kept across blocks - a worker's,
    /// for a whole run - holds a fixed bound per buffer rather than the
    /// union of every outlier it ever met. A released buffer regrows only if
    /// a later block needs it. Normal blocks never trip it: at the default
    /// cap the dense vectors are 8 to 64 KB each.
    ///
    /// Runs inside [`reset`](Self::reset), so it excludes the two buffers
    /// that still hold the block just encoded: `encode_buf` (released at the
    /// start of the next `encode_block`) and `last_tagdata` (replaced there).
    fn release_outliers(&mut self) {
        const LIMIT: usize = MAX_RETAINED_SCRATCH_CAPACITY;
        fn release<T>(v: &mut Vec<T>) {
            if v.capacity().saturating_mul(std::mem::size_of::<T>()) > LIMIT {
                *v = Vec::new();
            }
        }
        release(&mut self.dense_ids);
        release(&mut self.dense_lats);
        release(&mut self.dense_lons);
        release(&mut self.dense_keys_vals);
        release(&mut self.dense_versions);
        release(&mut self.dense_timestamps);
        release(&mut self.dense_changesets);
        release(&mut self.dense_uids);
        release(&mut self.dense_user_sids);
        release(&mut self.dense_visibles);
        release(&mut self.group_buf);
        release(&mut self.elem_scratch);
        release(&mut self.packed_scratch);
        release(&mut self.packed_vals_scratch);
        release(&mut self.packed_lat_scratch);
        release(&mut self.packed_lon_scratch);
        release(&mut self.info_scratch);
        release(&mut self.tag_key_scratch);
        // Approximate: buckets times entry size, ignoring control bytes.
        if self.tag_key_indices.capacity() * std::mem::size_of::<u32>() > LIMIT {
            self.tag_key_indices = FxHashSet::default();
        }
        self.string_table.release_outliers(LIMIT);
    }
}

/// Whether an unsigned raw string index (a tag key or value) names a slot of
/// the source table.
#[inline]
fn index_in(sid: u32, seeded: usize) -> bool {
    usize::try_from(sid).is_ok_and(|sid| sid < seeded)
}

/// Check a signed raw string index (a role or user) as the reader resolves
/// it: negative is a wire error, as `RelMember::role` and `Info::user`
/// report it, and past the source table is out of bounds.
#[inline]
fn check_signed_sid(sid: i32, seeded: usize) -> Result<()> {
    match usize::try_from(sid) {
        Ok(index) if index < seeded => Ok(()),
        Ok(index) => Err(crate::error::new_stringtable_index_out_of_bounds(index)),
        Err(_) => Err(crate::error::new_wire_error("negative string index")),
    }
}

/// Check a raw way's or relation's tag values, member roles and `Info` user
/// against `seeded` slots of the source table, as the reader decodes each
/// (`PackedUint32Iter`, `PackedInt32Iter`, `WireInfo::user_sid_of`).
#[inline]
fn check_raw_values(
    kind: &'static str,
    id: i64,
    vals: &[u8],
    roles: &[u8],
    info: Option<&[u8]>,
    seeded: usize,
) -> Result<()> {
    for sid in crate::read::wire::PackedUint32Iter::new(vals) {
        if !index_in(sid, seeded) {
            return Err(bad_unsigned_sid(kind, id, "tag value", sid));
        }
    }
    for (position, sid) in crate::read::wire::PackedInt32Iter::new(roles).enumerate() {
        if let Err(e) = check_signed_sid(sid, seeded) {
            return Err(e.context(format!(
                "relation {id} member {position}: role string index {sid}"
            )));
        }
    }
    if let Some(info) = info
        && let Some(sid) = crate::read::wire::WireInfo::user_sid_of(info)?
        && let Err(e) = check_signed_sid(sid, seeded)
    {
        return Err(e.context(raw_sid_context(kind, id, "user name", sid)));
    }
    Ok(())
}

/// The context a refused raw string index carries: the element, the field
/// and the index as the reader decodes it.
#[cold]
#[inline(never)]
fn raw_sid_context(kind: &str, id: i64, field: &str, sid: impl std::fmt::Display) -> String {
    format!("{kind} {id}: {field} string index {sid}")
}

/// A tag key or value index past the source table.
#[cold]
#[inline(never)]
fn bad_unsigned_sid(kind: &str, id: i64, field: &str, sid: u32) -> crate::error::Error {
    crate::error::new_stringtable_index_out_of_bounds(usize::try_from(sid).unwrap_or(usize::MAX))
        .context(raw_sid_context(kind, id, field, sid))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{BlockBuilder, MemberData, Metadata, RawMetadata};
    use crate::{MemberId, MemberType, PrimitiveBlock};
    use protohoggr::encode_bytes_field;

    #[test]
    fn raw_rewrites_preserve_duplicate_and_empty_source_strings() {
        let mut strings = Vec::new();
        for value in ["", "key", "value", "key", "", "tail"] {
            protohoggr::encode_bytes_field_always(&mut strings, 1, value.as_bytes());
        }
        let mut bytes = Vec::new();
        encode_bytes_field(&mut bytes, 1, &strings);
        let source = PrimitiveBlock::from_vec(bytes).expect("valid source table");
        for kind in 0..4 {
            let mut bb = BlockBuilder::new();
            bb.pre_seed_string_table(&source).expect("pre-seed");
            let added = match kind {
                0 => bb.add_node_raw(1, 0, 0, [(3, 5), (1, 4)].into_iter(), None),
                1 => bb.add_way_raw_bytes(1, &[3, 1], &[5, 4], &[], None),
                2 => bb.add_way_raw_bytes_with_locations(1, &[3, 1], &[5, 4], &[], &[], &[], None),
                _ => bb.add_relation_raw_bytes(1, &[3, 1], &[5, 4], &[4], &[2], &[0], None),
            };
            added.expect("in-range raw indices");
            let bytes = bb.take().expect("encode raw rewrite").expect("one block");
            let output = PrimitiveBlock::from_vec(bytes.to_vec()).expect("valid output");
            let tagdata = bb.last_tagdata.as_deref().expect("tag key index");
            let tag_index =
                crate::blob_meta::TagIndex::deserialize(tagdata).expect("valid tag key index");
            assert_eq!(tag_index.keys().len(), 1);
            assert_eq!(tag_index.keys()[0].as_ref(), b"key");
            assert_eq!(output.string_table_len(), source.string_table_len());
            for i in 0..source.string_table_len() {
                assert_eq!(output.string_table_entry(i), source.string_table_entry(i));
            }
            let element = output.elements().next().expect("one element");
            let tags: Vec<_> = match element {
                crate::Element::DenseNode(node) => node.tags().collect(),
                crate::Element::Way(way) => way.tags().collect(),
                crate::Element::Relation(relation) => relation.tags().collect(),
                crate::Element::Node(_) => panic!("expected dense node"),
            };
            assert_eq!(tags, [("key", "tail"), ("key", "")]);
        }
    }

    /// Every `add_*` path refuses an element the block cannot take.
    ///
    /// These used to `assert!`, so a caller that skipped `can_add_*` panicked
    /// the thread. Each path is driven separately - eight copies of the guard
    /// exist, and one left as an assert would leave the other seven green -
    /// on a block of another kind. The public paths poison, and the refusal
    /// must reach both emit paths; the raw paths return it as their error and
    /// leave the block as it was, which then still encodes. The paired success
    /// is the first add on each fresh builder, which the refusal must not have
    /// disturbed: the message names the second element, not the first.
    #[test]
    fn every_add_path_refuses_a_block_of_another_kind() {
        let empty = std::iter::empty::<(&str, &str)>;
        let members = [MemberData {
            id: MemberId::Node(1),
            role: "",
        }];
        for path in 0..8 {
            let mut bb = BlockBuilder::new();
            // Seed a block of a kind the path under test does not add. The
            // raw paths need no pre-seeded table: the guard runs first.
            let held = if [2, 3, 4, 6].contains(&path) {
                bb.add_node(1, 0, 0, empty(), None);
                "nodes"
            } else {
                bb.add_way(1, empty(), &[1, 2], None);
                "ways"
            };
            let raw = match path {
                0 => {
                    bb.add_node(7, 0, 0, empty(), None);
                    None
                }
                1 => Some(bb.add_node_raw(7, 0, 0, std::iter::empty(), None)),
                2 => {
                    bb.add_way(7, empty(), &[1, 2], None);
                    None
                }
                3 => {
                    bb.add_way_with_locations(7, empty(), &[1], &[(0, 0)], None);
                    None
                }
                4 => Some(bb.add_way_raw_bytes(7, &[], &[], &[2], None)),
                5 => {
                    bb.add_relation(7, empty(), &members, None);
                    None
                }
                6 => Some(bb.add_way_raw_bytes_with_locations(7, &[], &[], &[2], &[0], &[0], None)),
                _ => Some(bb.add_relation_raw_bytes(7, &[], &[], &[0], &[2], &[0], None)),
            };
            let needle = format!(" 7 added to a block holding {held}");
            if let Some(raw) = raw {
                let msg = raw
                    .expect_err("a raw add the block cannot take is an error")
                    .to_string();
                assert!(
                    msg.contains(&needle),
                    "path {path}: the error must name the element and the held kind, got: {msg}"
                );
                assert!(
                    bb.take_owned().expect("unpoisoned").is_some(),
                    "path {path}: the held block still encodes"
                );
                continue;
            }
            let msg = match bb.take_owned() {
                Ok(_) => panic!("path {path}: a refused element must stop the block"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains(&needle),
                "path {path}: the error must name the element and the held kind, got: {msg}"
            );
            assert!(bb.take().is_err(), "path {path}: take must refuse it too");
        }
    }

    /// A source table of `strings` as a block with no elements.
    fn source_table(strings: &[&str]) -> PrimitiveBlock {
        let mut table = Vec::new();
        for s in strings {
            protohoggr::encode_bytes_field_always(&mut table, 1, s.as_bytes());
        }
        let mut bytes = Vec::new();
        encode_bytes_field(&mut bytes, 1, &table);
        PrimitiveBlock::from_vec(bytes).expect("valid source table")
    }

    /// A raw index is checked against the source table it was seeded from,
    /// not the table the diff's strings grew, on every field and in both
    /// orders (the diff's string before and after the copied element), and
    /// a refused add poisons the builder, so the block is never emitted.
    #[test]
    fn raw_indices_past_the_source_table_are_refused_after_growth() {
        let source = source_table(&["", "k", "v"]);
        let user = |user_sid| RawMetadata {
            version: 1,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user_sid,
            visible: true,
        };
        let mut info = Vec::new();
        for (field, value) in [(1u32, 1u64), (5, 3)] {
            protohoggr::encode_tag(&mut info, field, protohoggr::WIRE_VARINT);
            protohoggr::encode_varint(&mut info, value);
        }
        // The diff's element 1, of the case's kind; index 3 is the first
        // slot its strings take.
        let add_diff = |bb: &mut BlockBuilder, case: usize| {
            let tags = [("name", "new")];
            let members = [MemberData {
                id: MemberId::Node(1),
                role: "new",
            }];
            match case {
                0..=3 => bb.add_node(1, 0, 0, tags, None),
                4..=6 => bb.add_way(1, tags, &[1, 2], None),
                _ => bb.add_relation(1, tags, &members, None),
            }
        };
        for diff_first in [true, false] {
            for case in 0..8 {
                let mut bb = BlockBuilder::new();
                bb.pre_seed_string_table(&source).expect("pre-seed");
                if diff_first {
                    add_diff(&mut bb, case);
                }
                let (result, needle) = match case {
                    0 => (
                        bb.add_node_raw(2, 0, 0, [(3, 2)].into_iter(), None),
                        "node 2: tag key string index 3",
                    ),
                    1 => (
                        bb.add_node_raw(2, 0, 0, [(1, 3)].into_iter(), None),
                        "node 2: tag value string index 3",
                    ),
                    2 => (
                        bb.add_node_raw(2, 0, 0, [(1, 2)].into_iter(), Some(&user(3))),
                        "node 2: user name string index 3",
                    ),
                    3 => (
                        bb.add_node_raw(2, 0, 0, [(1, 2)].into_iter(), Some(&user(-1))),
                        "node 2: user name string index -1",
                    ),
                    4 => (
                        bb.add_way_raw_bytes(2, &[3], &[2], &[2], None),
                        "way 2: tag key string index 3",
                    ),
                    5 => (
                        bb.add_way_raw_bytes(2, &[1], &[3], &[2], None),
                        "way 2: tag value string index 3",
                    ),
                    6 => (
                        bb.add_way_raw_bytes(2, &[1], &[2], &[2], Some(&info)),
                        "way 2: user name string index 3",
                    ),
                    _ => (
                        bb.add_relation_raw_bytes(2, &[1], &[2], &[3], &[2], &[0], None),
                        "relation 2 member 0: role string index 3",
                    ),
                };
                let msg = result
                    .expect_err("an index past the source table")
                    .to_string();
                assert!(msg.contains(needle), "case {case}: got {msg}");
                if !diff_first {
                    add_diff(&mut bb, case);
                }
                let poisoned = bb.take().expect_err("a refused raw add poisons the block");
                assert!(
                    poisoned.to_string().contains(needle),
                    "case {case}: the poison carries the refusal, got {poisoned}"
                );
            }
        }
    }

    /// A plain node's tag with key index zero (the empty string) is written
    /// under the empty string's nonzero slot, since zero ends a dense node's
    /// tags, also when the source table holds nothing but slot zero.
    #[test]
    fn a_plain_node_empty_key_survives_the_dense_copy() {
        let mut bb = BlockBuilder::new();
        bb.pre_seed_string_table(&source_table(&[""]))
            .expect("pre-seed");
        bb.add_node_raw(1, 0, 0, [(0, 0)].into_iter(), None)
            .expect("in range");
        let bytes = bb.take().expect("encode").expect("one block");
        let block = PrimitiveBlock::from_vec(bytes.to_vec()).expect("valid output");
        let tags: Vec<_> = match block.elements().next().expect("one element") {
            crate::Element::DenseNode(n) => n.tags().collect(),
            _ => panic!("expected a dense node"),
        };
        assert_eq!(tags, [("", "")]);

        let source = source_table(&["", "v"]);
        let mut bb = BlockBuilder::new();
        bb.pre_seed_string_table(&source).expect("pre-seed");
        bb.add_node_raw(2, 0, 0, [(0, 1), (1, 0)].into_iter(), None)
            .expect("in range");
        let bytes = bb.take().expect("encode").expect("one block");
        let block = PrimitiveBlock::from_vec(bytes.to_vec()).expect("valid output");
        let nodes: Vec<(i64, Vec<(String, String)>)> = block
            .elements()
            .map(|e| match e {
                crate::Element::DenseNode(n) => (
                    n.id(),
                    n.tags()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                ),
                _ => panic!("expected a dense node"),
            })
            .collect();
        assert_eq!(
            nodes,
            [(
                2,
                vec![
                    (String::new(), "v".to_owned()),
                    ("v".to_owned(), String::new())
                ]
            )]
        );
    }

    /// A copied `Info` user is judged as the reader resolves it: the last
    /// varint user field wins, so an earlier bad one it overrides passes and
    /// a later bad one is refused; a user field of another wire type is not
    /// a user.
    #[test]
    fn a_raw_info_user_is_judged_by_its_last_varint_field() {
        use protohoggr::{WIRE_LEN, WIRE_VARINT, encode_tag, encode_varint};
        let source = source_table(&["", "u"]);
        let info = |users: &[u64], len_user: bool| {
            let mut info = Vec::new();
            encode_tag(&mut info, 1, WIRE_VARINT);
            encode_varint(&mut info, 1);
            for &user in users {
                encode_tag(&mut info, 5, WIRE_VARINT);
                encode_varint(&mut info, user);
            }
            if len_user {
                encode_tag(&mut info, 5, WIRE_LEN);
                encode_varint(&mut info, 1);
                info.push(9);
            }
            info
        };
        for (users, len_user, ok) in [
            (&[9, 1][..], false, true),
            (&[1, 9][..], false, false),
            (&[1][..], true, true),
            (&[(1 << 32) | 1][..], false, true),
        ] {
            let mut bb = BlockBuilder::new();
            bb.pre_seed_string_table(&source).expect("pre-seed");
            let added = bb.add_way_raw_bytes(1, &[], &[], &[2], Some(&info(users, len_user)));
            assert_eq!(added.is_ok(), ok, "users {users:?}, len_user {len_user}");
        }
    }

    /// Pre-seeding refuses a source table whose slot zero is not the empty
    /// string, and a builder holding elements.
    #[test]
    fn pre_seeding_refuses_a_nonempty_slot_zero_and_a_nonempty_builder() {
        let mut bb = BlockBuilder::new();
        assert!(
            bb.pre_seed_string_table(&source_table(&["x", "k"]))
                .is_err()
        );
        assert!(bb.pre_seed_string_table(&source_table(&[])).is_err());
        bb.add_node(1, 0, 0, [("k", "v")], None);
        assert!(bb.pre_seed_string_table(&source_table(&["", "k"])).is_err());
        assert!(bb.take().expect("encode").is_some());
    }

    /// A flush with nothing to encode still ends the block: a pre-seeded
    /// table that took no element is gone, so the next pre-seed starts at
    /// slot one and a raw index names its own source string.
    #[test]
    fn an_empty_flush_clears_the_pre_seeded_table() {
        let first = source_table(&["", "gone", "one"]);
        let second = source_table(&["", "kept", "yes"]);
        let mut bb = BlockBuilder::new();
        bb.pre_seed_string_table(&first).expect("pre-seed");
        assert!(bb.take().expect("encode").is_none());
        assert!(!bb.is_pre_seeded(), "an empty flush resets the seed");
        bb.pre_seed_string_table(&second).expect("pre-seed");
        bb.add_node_raw(1, 0, 0, [(1, 2)].into_iter(), None)
            .expect("in range");
        let bytes = bb.take().expect("encode").expect("one block");
        let block = PrimitiveBlock::from_vec(bytes.to_vec()).expect("valid output");
        let tags: Vec<_> = match block.elements().next().expect("one element") {
            crate::Element::DenseNode(n) => n.tags().collect(),
            _ => panic!("expected a dense node"),
        };
        assert_eq!(tags, [("kept", "yes")]);
    }

    /// A full block refuses one more element of its own kind, and the
    /// refusal is sticky across a later legal block.
    #[test]
    fn a_full_block_refuses_the_next_element_and_stays_refused() {
        let cap = std::num::NonZeroUsize::new(2).expect("non-zero");
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap, 2);
        bb.add_node(1, 0, 0, [("k", "v")], None);
        bb.add_node(2, 0, 0, [("k", "v")], None);
        assert!(!bb.can_add_node(), "fixture: the block is at its cap");
        bb.add_node(3, 0, 0, [("k", "v")], None);
        let msg = match bb.take() {
            Ok(_) => panic!("an over-cap block must not encode"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("node 3 added to a full block (2 element cap)"),
            "the error must name the element and the cap, got: {msg}"
        );
        // Sticky: the block is gone, and a fresh legal add cannot flush past
        // the refusal into a file with node 3 missing.
        bb.add_way(10, [("k", "v")], &[1, 2], None);
        assert!(bb.take_owned().is_err(), "the refusal must survive");
    }

    #[test]
    fn explicit_empty_dense_tag_key_round_trips() {
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, [("", "value")], None);
        let bytes = bb.take().expect("encode node").expect("one block");
        let output = PrimitiveBlock::from_vec(bytes.to_vec()).expect("valid dense tags");
        let crate::Element::DenseNode(node) = output.elements().next().expect("one node") else {
            panic!("expected dense node");
        };
        assert_eq!(node.tags().collect::<Vec<_>>(), [("", "value")]);
    }

    /// A ref/location length mismatch must not reach the encoder.
    ///
    /// `encode_way_with_locations` zips the two slices, so the shorter one
    /// decides the way's node count: a way silently written with fewer refs
    /// than it has. Only a `debug_assert_eq!` stood between that and a
    /// release build. The assertion here is on the refusal to produce the
    /// block at all, not on any message the builder shares with another
    /// guard - the way count and the location count are strings only this
    /// check can produce.
    #[test]
    fn a_way_whose_refs_and_locations_disagree_never_reaches_a_block() {
        let mut bb = BlockBuilder::new();
        bb.add_way_with_locations(
            42,
            [("highway", "residential")],
            &[1, 2, 3],
            &[(10, 20), (30, 40)],
            None,
        );
        let err = bb
            .take()
            .expect_err("a way with 3 refs and 2 locations must not encode");
        let msg = err.to_string();
        assert!(
            msg.contains("way 42") && msg.contains("3 node ref(s) but 2 location(s)"),
            "the error must name the way and the two counts, got: {msg}"
        );

        // Sticky: the caller cannot flush past it into a later block.
        bb.add_node(1, 0, 0, [("k", "v")], None);
        assert!(
            bb.take().is_err(),
            "poison must survive reset - a builder that was handed an \
             unencodable element stays refused"
        );
    }

    /// `take_owned` must refuse a poisoned builder too.
    ///
    /// The poison doc promises "the next `take` / `take_owned` fails", and
    /// `take_owned` is the emit path production actually uses - the flush
    /// helper in `commands` and every `time_filter` flush go through it,
    /// while `take` is the borrowing convenience. Testing only `take`
    /// establishes nothing about the path that carries the data: if the
    /// guard had been placed in `take` rather than in the shared
    /// `encode_block`, every real writer would still have emitted the
    /// corrupt block and the existing tests would still be green.
    #[test]
    fn take_owned_refuses_a_poisoned_builder_as_well_as_take() {
        let mut bb = BlockBuilder::new();
        bb.add_way_with_locations(
            42,
            [("highway", "residential")],
            &[1, 2, 3],
            &[(10, 20)],
            None,
        );
        // `OwnedBlock` is not `Debug`, so unwrap the Result by hand.
        let msg = match bb.take_owned() {
            Ok(_) => panic!("take_owned must refuse a poisoned builder"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("way 42") && msg.contains("3 node ref(s) but 1 location(s)"),
            "take_owned must surface the same diagnosis as take, got: {msg}"
        );

        // The relation poison must reach `take_owned` as well - both setters
        // funnel through the same check, and that is the claim under test.
        let mut bb = BlockBuilder::new();
        let members = [MemberData {
            id: MemberId::Unknown(7, 99),
            role: "future",
        }];
        bb.add_relation(500, [("type", "multipolygon")], &members, None);
        assert!(
            bb.take_owned().is_err(),
            "an unmodelled member type must not slip out through take_owned"
        );
    }

    /// Dropping a poisoned builder without finalizing it is cleanup, as on
    /// any `?` bail-out after bad input poisoned it: nothing panics, in a
    /// debug build either (a drop-time check used to, replacing the
    /// caller's error).
    #[test]
    fn a_poisoned_builder_dropped_without_a_take_is_cleanup() {
        let members = [MemberData {
            id: MemberId::Unknown(7, 99),
            role: "future",
        }];
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, [("k", "v")], None);
        let mut other = BlockBuilder::new();
        other.add_relation(499, [("type", "route")], &[], None);
        other.add_relation(500, [("type", "multipolygon")], &members, None);
        drop(other);
        // A builder poisoned while empty, abandoned the same way.
        let mut empty = BlockBuilder::new();
        empty.add_relation(501, [("type", "route")], &members, None);
        assert!(empty.is_empty());
        drop(empty);
        // The unrelated builder is untouched by either drop.
        assert!(bb.take().expect("unpoisoned").is_some());
    }

    /// Passing counterpart: `take_owned` on a clean builder still yields a
    /// block. Without it, a regression that poisoned every builder would
    /// leave the refusal tests above green.
    #[test]
    fn take_owned_still_encodes_an_unpoisoned_builder() {
        let mut bb = BlockBuilder::new();
        bb.add_way_with_locations(
            42,
            [("highway", "residential")],
            &[1, 2, 3],
            &[(10, 20), (30, 40), (50, 60)],
            None,
        );
        assert!(
            bb.take_owned().unwrap().is_some(),
            "matched refs and locations must encode exactly as before"
        );
    }

    /// A builder kept across blocks (a worker's) lends the same bytes a
    /// fresh builder hands out owned, and does not pin an outlier block's
    /// buffers: a lent outlier's encode buffer is released when the lend
    /// ends, and after a block above the retention limit the next ordinary
    /// block leaves every buffer it grew within the limit again. An owned
    /// take after an outlier reserves no more than the limit for the next
    /// block either.
    #[test]
    fn a_reused_builder_lends_identical_bytes_and_releases_outliers() {
        use crate::tunables::MAX_RETAINED_SCRATCH_CAPACITY as LIMIT;
        let big = "x".repeat(LIMIT + 1);
        let encode = |bb: &mut BlockBuilder, value: &str| {
            bb.add_way(7, [("note", value)], &[1, 2, 3], None);
        };

        let mut fresh = BlockBuilder::new();
        encode(&mut fresh, "small");
        let owned = fresh.take_owned().unwrap().unwrap();

        let mut kept = BlockBuilder::new();
        encode(&mut kept, &big);
        let lent_len = kept.lend_encoded(|lent| Ok(lent.bytes.len())).unwrap();
        assert!(lent_len.is_some_and(|len| len > LIMIT));
        assert!(
            kept.encode_buf.capacity() <= LIMIT,
            "a lent outlier is released once its borrower is done"
        );
        encode(&mut kept, "small");
        kept.lend_encoded(|lent| {
            assert_eq!(lent.bytes, owned.bytes.as_slice(), "same block bytes");
            assert_eq!(lent.tagdata, owned.tagdata.as_deref(), "same tagdata");
            assert_eq!(lent.index.count, owned.index.count);
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert!(
            kept.encode_buf.capacity() <= LIMIT,
            "outlier encode buffer released"
        );
        assert!(
            kept.group_buf.capacity() <= LIMIT,
            "outlier group buffer released"
        );
        assert!(
            kept.elem_scratch.capacity() <= LIMIT,
            "outlier scratch released"
        );

        let mut owning = BlockBuilder::new();
        encode(&mut owning, &big);
        let outlier = owning.take_owned().unwrap().unwrap();
        assert!(outlier.bytes.len() > LIMIT);
        assert!(
            owning.encode_reserve <= LIMIT,
            "reservation capped at the limit"
        );
        encode(&mut owning, "small");
        let next = owning.take_owned().unwrap().unwrap();
        assert_eq!(next.bytes, owned.bytes, "an owned take after an outlier");
        assert!(
            owning.encode_reserve < 1024,
            "the reservation follows the last block"
        );
    }

    /// A relation member whose type has no enum value must not be encoded.
    ///
    /// `member_type_value` mapped `Unknown(_)` to 0, i.e. NODE. That writes a
    /// file that is valid PBF and semantically wrong: no reader, no
    /// `check --index`, no osmium cross-validation catches a member that
    /// changed kind. The rejection has to happen at the encode boundary.
    #[test]
    fn a_relation_member_with_no_enum_value_never_reaches_a_block() {
        let mut bb = BlockBuilder::new();
        let members = [
            MemberData {
                id: MemberId::Node(1),
                role: "outer",
            },
            MemberData {
                id: MemberId::Unknown(7, 99),
                role: "future",
            },
        ];
        bb.add_relation(500, [("type", "multipolygon")], &members, None);
        let err = bb
            .take()
            .expect_err("a member of type 7 must not be encoded as a node");
        let msg = err.to_string();
        assert!(
            msg.contains("relation 500") && msg.contains("member type 7"),
            "the error must name the relation and the unmodelled wire value, got: {msg}"
        );
    }

    /// The three modelled member types must still encode - the guard above
    /// must reject exactly `Unknown`, not relations in general.
    #[test]
    fn a_relation_with_only_modelled_member_types_still_encodes() {
        let mut bb = BlockBuilder::new();
        let members = [
            MemberData {
                id: MemberId::Node(1),
                role: "a",
            },
            MemberData {
                id: MemberId::Way(2),
                role: "b",
            },
            MemberData {
                id: MemberId::Relation(3),
                role: "c",
            },
        ];
        bb.add_relation(500, [("type", "multipolygon")], &members, None);
        assert!(
            bb.take().unwrap().is_some(),
            "node/way/relation members must encode as they always did"
        );
        assert_eq!(
            MemberId::Unknown(7, 99).member_type(),
            MemberType::Unknown(7),
            "fixture sanity: Unknown must survive the MemberId round-trip, \
             or the rejection test above proves nothing"
        );
    }

    /// The pre-allocation bound must clamp only the pre-allocation.
    ///
    /// `cap` is a policy - how many elements go in a block before it is
    /// flushed - and it lands verbatim in the output's blob sizes.
    /// `prealloc_bound` is an allocation ceiling and nothing else. Conflating
    /// them would not be a memory bug but a *format* bug: a builder whose cap
    /// had silently become the byte-derived bound would write blobs of a
    /// different size than the caller asked for, on some inputs and not
    /// others. The split shipped with no test at all, which is how a
    /// conflation would have reached a release unnoticed.
    fn cap(n: usize) -> std::num::NonZeroUsize {
        std::num::NonZeroUsize::new(n).expect("test caps are non-zero")
    }

    #[test]
    fn the_prealloc_bound_clamps_allocation_without_moving_the_cap() {
        // Bound far below the cap: the flush point must still be the cap.
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap(5), 1);
        for id in 1..=5 {
            assert!(
                !bb.should_flush(),
                "the builder wanted to flush after {} elements with a cap of \
                 5, so the pre-allocation bound has been mistaken for the cap",
                id - 1
            );
            bb.add_node(id, 0, 0, [("k", "v")], None);
        }
        assert!(bb.should_flush(), "a cap of 5 must flush at 5 elements");

        // A bound of zero is still a legal request and must not panic or
        // collapse the cap.
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap(2), 0);
        bb.add_node(1, 0, 0, [("k", "v")], None);
        assert!(!bb.should_flush());
        bb.add_node(2, 0, 0, [("k", "v")], None);
        assert!(bb.should_flush());

        // Paired counterpart: a bound above the cap changes nothing, so the
        // assertions above are about the clamp rather than about the cap
        // having been broken generally.
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap(5), usize::MAX);
        for id in 1..=4 {
            bb.add_node(id, 0, 0, [("k", "v")], None);
        }
        assert!(!bb.should_flush());
        bb.add_node(5, 0, 0, [("k", "v")], None);
        assert!(bb.should_flush());
    }

    /// A cap no input can reach must not be pre-allocated.
    ///
    /// `repack --elements-per-blob N` takes N off the command line and builds
    /// one `BlockBuilder` per worker plus a central one. Before the bound
    /// existed, a single integer requested several unbounded allocations
    /// before a byte of input had been read. This constructs at the shape that
    /// used to abort the process and asserts the builder is merely empty.
    #[test]
    fn an_absurd_cap_does_not_allocate_absurdly() {
        let bb = BlockBuilder::with_element_cap_prealloc(cap(usize::MAX / 2), 64);
        assert!(
            !bb.should_flush(),
            "a fresh builder holds nothing regardless of its cap"
        );
    }

    #[test]
    fn tagdata_keeps_an_explicit_empty_tag_key() {
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, [("", "value")], None);

        let block = bb.take_owned().unwrap().unwrap();
        let tagdata = block.tagdata.as_deref().expect("tagged block has tagdata");
        let tag_index = crate::blob_meta::TagIndex::deserialize(tagdata)
            .expect("builder tagdata must be readable");
        assert_eq!(tag_index.keys().len(), 1);
        assert!(
            tag_index.keys()[0].is_empty(),
            "an empty string is a real tag key when a tag uses it"
        );
    }

    #[test]
    fn dense_node_id_deltas_wrap_at_signed_boundaries() {
        let mut bb = BlockBuilder::new();
        bb.add_node(-1, 0, 0, [], None);
        bb.add_node(i64::MAX, 0, 0, [], None);

        assert_eq!(bb.dense_ids, [-1, i64::MIN]);
        assert!(bb.take().unwrap().is_some());
    }

    #[test]
    fn dense_metadata_deltas_wrap_at_signed_boundaries() {
        let mut bb = BlockBuilder::new();
        let first = Metadata {
            version: 1,
            timestamp: i64::MAX,
            changeset: i64::MAX,
            uid: i32::MAX,
            user: "first",
            visible: true,
        };
        let next = Metadata {
            version: 1,
            timestamp: -1,
            changeset: -1,
            uid: -1,
            user: "next",
            visible: true,
        };
        bb.add_node(1, 0, 0, [], Some(&first));
        bb.add_node(2, 0, 0, [], Some(&next));

        assert_eq!(bb.dense_timestamps, [i64::MAX, i64::MIN]);
        assert_eq!(bb.dense_changesets, [i64::MAX, i64::MIN]);
        assert_eq!(bb.dense_uids, [i32::MAX, i32::MIN]);
        assert!(bb.take().unwrap().is_some());
    }

    #[test]
    fn raw_dense_metadata_deltas_wrap_at_signed_boundaries() {
        let mut bb = BlockBuilder::new();
        let first = RawMetadata {
            version: 1,
            timestamp: i64::MAX,
            changeset: i64::MAX,
            uid: i32::MAX,
            user_sid: i32::MAX,
            visible: true,
        };
        let next = RawMetadata {
            version: 1,
            timestamp: -1,
            changeset: -1,
            uid: -1,
            user_sid: -1,
            visible: true,
        };
        bb.add_dense_metadata_raw(&first);
        bb.add_dense_metadata_raw(&next);

        assert_eq!(bb.dense_timestamps, [i64::MAX, i64::MIN]);
        assert_eq!(bb.dense_changesets, [i64::MAX, i64::MIN]);
        assert_eq!(bb.dense_uids, [i32::MAX, i32::MIN]);
        assert_eq!(bb.dense_user_sids, [i32::MAX, i32::MIN]);
    }

    #[test]
    fn missing_dense_metadata_wraps_the_transition_back_to_zero() {
        let mut bb = BlockBuilder::new();
        let min_metadata = Metadata {
            version: 1,
            timestamp: i64::MIN,
            changeset: i64::MIN,
            uid: i32::MIN,
            user: "minimum",
            visible: true,
        };
        bb.add_node(1, 0, 0, [], Some(&min_metadata));
        bb.add_node(2, 0, 0, [], None);

        assert_eq!(bb.dense_timestamps, [i64::MIN, i64::MIN]);
        assert_eq!(bb.dense_changesets, [i64::MIN, i64::MIN]);
        assert_eq!(bb.dense_uids, [i32::MIN, i32::MIN]);
        assert!(bb.take().unwrap().is_some());
    }
}
