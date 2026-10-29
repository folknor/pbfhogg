//! External join for add-locations-to-ways: bounded-memory coordinate resolution
//! via double radix permutation.
//!
//! Instead of building a large random-access node index, this module
//! pre-computes the way-node join using sequential I/O and bounded memory:
//!
//! 1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into
//!    256 node buckets partitioned by high bits of node_id.
//! 2. **Node join**: per bucket, sort pairs by node_id in memory, merge-join
//!    with matching node stream, emit `(slot_pos, lat, lon)` into
//!    256 slot buckets partitioned by high bits of slot_pos.
//! 3. **Slot reorder**: per bucket, sort by slot_pos, emit final blob-ordered
//!    delta-varint `coord_payloads` (see `coord_payloads.rs`). The flat
//!    `coord_slots` array is a historical intermediate, retired in 2026-04.
//! 4. **Assembly**: stream original PBF + per-blob coord_payloads preads,
//!    emit enriched ways.
//!
//! All I/O is sequential. Temporary-disk demand and peak memory depend on the
//! input; see `reference/performance.md` for measurements. This path does not
//! build a random-access node index.

use std::path::Path;

use crate::write::WriteOptions;

use super::Stats;
use crate::commands::HeaderOverrides;
use crate::commands::input_front::InputFront;
use crate::error::Result;
use crate::read::input::PinnedInput;

mod blob_bucket_index;
mod blob_meta;
mod coord_payloads;
mod radix;
mod relation_scan;
mod space;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

use crate::tunables::{
    ALTW_STAGE3_WORKERS_CAP, DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED,
    MAX_BUCKET_WIDTH, NUM_BUCKETS,
};

use stage1::stage1_way_pass;
use stage2::{SlotBuckets, stage2_node_join};
use stage3::{IntegratedInputs, SlotBucketRef, stage3_slot_reorder};
use stage4::stage4_assembly;

/// Width (in node-id space) of one ID bucket.
///
/// `bucket_width = ceil(max_node_id / num_buckets)`, with a floor of 1
/// so empty inputs (`max_node_id = 0`) still produce a usable mapping.
/// The `div_ceil` rounds up so every node id in `[0, max_node_id]` maps
/// into the first `num_buckets` buckets when paired with the
/// `min(num_buckets - 1)` clamp inside [`BucketLayout::locate`].
///
/// Stage-1 pass A computes `local_node_id = (node_id - bucket_lo) as
/// u32`. [`BucketLayout::new`] refuses (with an error) any layout whose
/// widest bucket-local id would not fit [`LOCAL_ID_MASK`], so the cast is
/// lossless for any `node_id` inside `[0, max_node_id]`. At
/// planet (max_node_id ~= 14e9, 256 buckets) `bucket_width ~= 55M`,
/// six orders of magnitude under 2^32.
fn bucket_width(max_node_id: u64, num_buckets: usize) -> u64 {
    debug_assert!(num_buckets > 0, "num_buckets must be > 0");
    max_node_id.div_ceil(num_buckets as u64).max(1)
}

/// Layout used by stage-1 pass A and stage 2 to map a node id to its
/// `(bucket, local-id-within-bucket)` pair.
///
/// Constructed once per run from the largest `max_id` advertised by
/// any node blob's indexdata, so the bucket bounds are tight for the
/// actual input rather than the planet-wide constant.
///
/// [`Self::locate`] returns `None` for any `node_id > max_node_id`
/// instead of saturating into the last bucket. That is load-bearing:
/// out-of-range ids (stale indexdata, corrupt input, accidental
/// `i64 as u64` of a negative ref) would otherwise silently land in
/// the final bucket and be truncated to a 32-bit `local_node_id` in
/// pass A.
///
/// `None` means "this id cannot be encoded into the normal bucket
/// record." Pass A treats it as an unresolved slot and skips
/// emission. The slot's position is still consumed (per-way refcount
/// sidecars count every ref, including unresolved ones) so stage 4
/// fills zero coordinates and increments `missing_locations`. This
/// matches ALTW's existing behaviour for way refs to nodes that
/// aren't in the input - common with extracts and node-sparse
/// fixtures. See `missing_node_refs_get_zero_coordinates` in
/// `cli/tests/cli_add_locations_to_ways.rs` for the contract canary.
pub(super) struct BucketLayout {
    pub(super) max_node_id: u64,
    pub(super) bucket_width: u64,
    pub(super) num_buckets: usize,
}

impl BucketLayout {
    /// Build the layout for node ids `0..=max_node_id`.
    ///
    /// `max_node_id` comes from indexdata, which nothing verifies, so an
    /// unrepresentable value is an error rather than a panic (CORRECTNESS.md,
    /// "Blob sidecar metadata" residual: anything sized from indexdata must
    /// cope with it being wrong). With 256 buckets the limit is a `max_id` of
    /// 2^38 (~2.7e11, about 20x the live OSM node ID range). That is an
    /// encoding limit of this backend, not an OSM validity limit: the claim
    /// may be true, so the message says what to do either way. Clamping
    /// instead would be wrong here - clamping an honest claim turns every
    /// node above the clamp into a missing location.
    pub(super) fn new(max_node_id: u64, num_buckets: usize) -> Result<Self> {
        assert!(num_buckets > 0, "num_buckets must be > 0");
        let bw = bucket_width(max_node_id, num_buckets);
        // Largest bucket-local id `locate` can produce: `bw - 1` in every
        // bucket but the last, which absorbs the `div_ceil` remainder and
        // reaches `max_node_id - last_lo`.
        let last_lo = (num_buckets as u64 - 1).saturating_mul(bw);
        let max_local = (bw - 1).max(max_node_id.saturating_sub(last_lo));
        if bw > MAX_BUCKET_WIDTH || max_local > u64::from(LOCAL_ID_MASK) {
            return Err(crate::Error::invalid_data(format!(
                "add-locations-to-ways, external index: the input's node \
                 indexdata advertises max_id {max_node_id}, above the {} this \
                 backend can encode ({num_buckets} buckets of at most {} ids). \
                 If the input really has node ids that large, the sparse index \
                 has no such limit; if it does not, the indexdata is wrong, and \
                 checking the file's blob metadata reports which blobs disagree \
                 with their contents.",
                (num_buckets as u64).saturating_mul(MAX_BUCKET_WIDTH),
                MAX_BUCKET_WIDTH,
            )));
        }
        Ok(Self {
            max_node_id,
            bucket_width: bw,
            num_buckets,
        })
    }

    /// Returns `(bucket_idx, local_node_id)` for `node_id`, or `None`
    /// when `node_id > max_node_id`.
    pub(super) fn locate(&self, node_id: u64) -> Option<(usize, u32)> {
        if node_id > self.max_node_id {
            return None;
        }
        // For node_id <= max_node_id, raw_idx may equal num_buckets at
        // the boundary case (when max_node_id isn't a multiple of
        // num_buckets, div_ceil overshoots by one). Saturate into the
        // last bucket; the bucket_lo arithmetic still yields a
        // legitimate offset since the last bucket can extend up to
        // max_node_id.
        #[allow(clippy::cast_possible_truncation)]
        let raw_idx = (node_id / self.bucket_width) as usize;
        let bucket_idx = raw_idx.min(self.num_buckets - 1);
        let bucket_lo = (bucket_idx as u64) * self.bucket_width;
        let offset = node_id - bucket_lo;
        // For node_id <= max_node_id and bucket_width <= 1<<30, the
        // last bucket can extend to max_node_id - bucket_lo. With
        // num_buckets * bucket_width >= max_node_id, that distance is
        // at most 2 * bucket_width - 1 < 2 * u32::MAX. u32::try_from
        // makes the cast explicit; in practice this never fails for
        // an in-range id, but guard the contract anyway.
        u32::try_from(offset).ok().map(|local| (bucket_idx, local))
    }
}

/// Largest node id advertised by any node-kind blob's indexdata.
///
/// This is the input to [`BucketLayout::new`]. Returns 0 when there are no
/// node blobs (degenerate input - the layout then rejects any non-zero ref).
///
/// Infallible because the ranges were validated where they entered:
/// [`blob_meta::scan_blob_metadata`], the external join's one trust boundary
/// for indexdata ranges, refuses a reversed range and a node range reaching
/// below zero, located at the offending blob, before any meta reaches here.
/// The fold starts at 0, so even an unvalidated negative `max_id` could not
/// wrap the result.
fn max_node_id_from_blob_meta(blob_meta: &[blob_meta::BlobMeta]) -> u64 {
    blob_meta
        .iter()
        .filter(|meta| matches!(meta.kind, crate::ElementKind::Node))
        .fold(0_u64, |max, meta| {
            max.max(u64::try_from(meta.max_id).unwrap_or(0))
        })
}

#[cfg(test)]
mod max_node_id_tests {
    use super::{blob_meta::BlobMeta, max_node_id_from_blob_meta};
    use crate::ElementKind;

    fn meta(kind: ElementKind, min_id: i64, max_id: i64) -> BlobMeta {
        BlobMeta {
            frame_offset: 0,
            data_offset: 0,
            data_size: 0,
            kind,
            min_id,
            max_id,
            count: 0,
            has_tagindex: false,
            has_tags: false,
        }
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(max_node_id_from_blob_meta(&[]), 0);
    }

    #[test]
    fn no_node_blobs_is_zero() {
        let metas = [
            meta(ElementKind::Way, 1, 100),
            meta(ElementKind::Relation, 1, 50),
        ];
        assert_eq!(max_node_id_from_blob_meta(&metas), 0);
    }

    #[test]
    fn picks_max_across_node_blobs() {
        let metas = [
            meta(ElementKind::Node, 1, 999),
            meta(ElementKind::Way, 1, 5_000_000),
            meta(ElementKind::Node, 1000, 12_345),
            meta(ElementKind::Node, 12_346, 99_999),
        ];
        assert_eq!(max_node_id_from_blob_meta(&metas), 99_999);
    }
}

#[cfg(test)]
mod bucket_math_tests {
    use super::{BucketLayout, bucket_width};

    #[test]
    fn bucket_width_handles_zero_max() {
        assert_eq!(bucket_width(0, 256), 1);
    }

    #[test]
    fn bucket_width_handles_small_max() {
        assert_eq!(bucket_width(5, 256), 1);
        assert_eq!(bucket_width(255, 256), 1);
        assert_eq!(bucket_width(256, 256), 1);
        assert_eq!(bucket_width(257, 256), 2);
    }

    #[test]
    fn bucket_width_rounds_up() {
        assert_eq!(bucket_width(1000, 256), 4);
        assert_eq!(bucket_width(14_000_000_000, 256), 54_687_500);
    }

    #[test]
    fn locate_clamps_last_bucket_for_in_range_ids() {
        let layout = BucketLayout::new(1000, 4).expect("layout");
        assert_eq!(layout.bucket_width, 250);
        assert_eq!(layout.locate(0), Some((0, 0)));
        assert_eq!(layout.locate(249), Some((0, 249)));
        assert_eq!(layout.locate(250), Some((1, 0)));
        assert_eq!(layout.locate(999), Some((3, 249)));
        // node_id == max_node_id rounds raw_idx = 1000/250 = 4, clamped
        // to 3. bucket_lo = 3 * 250 = 750, so local = 1000 - 750 = 250.
        assert_eq!(layout.locate(1000), Some((3, 250)));
    }

    #[test]
    fn locate_rejects_ids_beyond_max() {
        let layout = BucketLayout::new(1000, 4).expect("layout");
        assert_eq!(layout.locate(1001), None);
        assert_eq!(layout.locate(u64::MAX), None);
    }

    #[test]
    fn locate_returns_none_for_negative_refs_cast_via_u64() {
        // An i64 negative id reinterpreted as u64 lands near u64::MAX.
        // Pass A must not silently truncate it into the last bucket;
        // the layout returns None and pass A skips emission (treating
        // the slot as unresolved, same as a ref to an absent node).
        let layout = BucketLayout::new(14_000_000_000, 256).expect("layout");
        #[allow(clippy::cast_sign_loss)]
        let neg_as_u64 = (-1_i64) as u64;
        assert_eq!(layout.locate(neg_as_u64), None);
    }

    #[test]
    fn locate_handles_zero_max() {
        let layout = BucketLayout::new(0, 256).expect("layout");
        // max_node_id=0 means only node id 0 is in range.
        assert!(matches!(layout.locate(0), Some((b, _)) if b < 256));
        assert_eq!(layout.locate(1), None);
    }

    #[test]
    fn every_referenced_id_maps_to_one_bucket() {
        let max = 12_345_678u64;
        let layout = BucketLayout::new(max, 256).expect("layout");
        for id in (0..=max).step_by(50_000) {
            let (b, _) = layout.locate(id).unwrap_or_else(|| panic!("locate({id})"));
            assert!(b < 256, "id={id} -> bucket={b}");
        }
        let (lo_b, _) = layout.locate(0).expect("0 in range");
        let (hi_b, _) = layout.locate(max).expect("max in range");
        assert!(lo_b <= hi_b);
    }

    #[test]
    fn bucket_boundaries_are_exclusive_below_inclusive_above() {
        let layout = BucketLayout::new(10_000, 10).expect("layout");
        assert_eq!(layout.bucket_width, 1000);
        for k in 0u64..10 {
            let lo = k * 1000;
            let hi = (k + 1) * 1000 - 1;
            let expected = usize::try_from(k).expect("k fits in usize");
            assert_eq!(layout.locate(lo).map(|(b, _)| b), Some(expected), "lo={lo}",);
            assert_eq!(layout.locate(hi).map(|(b, _)| b), Some(expected), "hi={hi}",);
        }
    }

    #[test]
    fn local_node_id_round_trips_within_bucket() {
        let layout = BucketLayout::new(10_000, 10).expect("layout");
        for id in 0u64..=10_000 {
            let (bucket_idx, local) = layout.locate(id).expect("in range");
            let bucket_lo = (bucket_idx as u64) * layout.bucket_width;
            let recovered = bucket_lo + u64::from(local);
            assert_eq!(
                recovered, id,
                "id={id} bucket={bucket_idx} local={local} bucket_lo={bucket_lo}",
            );
        }
    }

    #[test]
    fn bucket_width_fits_in_u32_at_planet_scale() {
        // BucketLayout::new enforces this; cross-check the bare fn and
        // confirm the constructor accepts the planet config.
        let bw = bucket_width(14_000_000_000, 256);
        assert!(bw <= u64::from(u32::MAX));
        assert!(BucketLayout::new(14_000_000_000, 256).is_ok());
    }

    #[test]
    fn oversized_indexdata_max_id_is_an_error_not_a_panic() {
        // max_node_id comes from unverified indexdata. The largest
        // encodable claim is accepted, one past it is refused with an
        // error, and so is the adversarial extreme.
        let limit = 256 * (1_u64 << 30);
        let layout = BucketLayout::new(limit, 256).expect("2^38 is encodable");
        let (bucket, local) = layout.locate(limit).expect("max in range");
        assert_eq!(bucket, 255);
        assert!(local <= super::LOCAL_ID_MASK);

        let err = BucketLayout::new(limit + 1, 256)
            .err()
            .expect("2^38 + 1 must be refused");
        let msg = err.to_string();
        assert!(msg.contains("sparse index"), "{msg}");
        assert!(msg.contains("max_id 274877906945"), "{msg}");
        assert!(BucketLayout::new(u64::MAX, 256).is_err());
        assert!(BucketLayout::new(u64::MAX >> 1, 256).is_err());
    }
}

// Field widths and offsets of an id-bucketed occurrence record
// `(local_node_id: u32, blob_idx: u32, blob_local_slot: u32)`.
const ID_W_LOCAL_NODE_ID: usize = size_of::<u32>();
const ID_W_BLOB_IDX: usize = size_of::<u32>();
const ID_W_BLOB_LOCAL_SLOT: usize = size_of::<u32>();
const ID_OFF_LOCAL_NODE_ID: usize = 0;
const ID_OFF_BLOB_IDX: usize = ID_OFF_LOCAL_NODE_ID + ID_W_LOCAL_NODE_ID;
const ID_OFF_BLOB_LOCAL_SLOT: usize = ID_OFF_BLOB_IDX + ID_W_BLOB_IDX;

/// Size of an id-bucketed occurrence record.
pub(super) const ID_RECORD_SIZE: usize = ID_OFF_BLOB_LOCAL_SLOT + ID_W_BLOB_LOCAL_SLOT;
const _: () = assert!(ID_RECORD_SIZE == 12);

// Field widths and offsets of a resolved entry
// `(local_slot_pos: u32, lat: i32, lon: i32)`. Stage 3 decodes these
// records by the same offsets.
const RE_W_LOCAL_SLOT_POS: usize = size_of::<u32>();
const RE_W_LAT: usize = size_of::<i32>();
const RE_W_LON: usize = size_of::<i32>();
pub(super) const RE_OFF_LOCAL_SLOT_POS: usize = 0;
pub(super) const RE_OFF_LAT: usize = RE_OFF_LOCAL_SLOT_POS + RE_W_LOCAL_SLOT_POS;
pub(super) const RE_OFF_LON: usize = RE_OFF_LAT + RE_W_LAT;

/// Size of a resolved entry.
pub(super) const RESOLVED_ENTRY_SIZE: usize = RE_OFF_LON + RE_W_LON;
const _: () = assert!(RESOLVED_ENTRY_SIZE == 12);

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// Stage 1 → stage 2 hand-off describing one node blob: where it lives in
/// the input PBF and the id range its indexdata advertises.
///
/// Computed without decoding any blob. On sorted input the blobs' ranges
/// are ascending, so stage 2 finds the run of blobs intersecting each id
/// bucket by binary search over this vector.
#[derive(Clone, Copy, Debug)]
pub(super) struct NodeBlobInfo {
    pub data_offset: u64,
    pub data_size: usize,
    /// Indexdata-advertised id range, copied straight from blob_meta.
    /// Stage 2 partitions the slice by `[min_id, max_id]` to find
    /// blobs intersecting each id bucket; stage 2's merge walk uses
    /// `node_tuples.last().id` (the actual decoded last id) as the
    /// upper consumption bound rather than `max_id` itself, so loose
    /// indexdata can't cause silent record loss. Stage 2 also holds the
    /// claim exact for every node blob it decodes.
    pub min_id: i64,
    pub max_id: i64,
    /// Indexdata-advertised element count, held exact with the range.
    pub count: u64,
}

impl NodeBlobInfo {
    /// The node blob's indexdata claim.
    pub(super) fn claim(&self) -> crate::blob_meta::BlobIndex {
        crate::blob_meta::BlobIndex {
            kind: crate::ElementKind::Node,
            min_id: self.min_id,
            max_id: self.max_id,
            count: self.count,
            bbox: None,
        }
    }
}

/// An id-bucketed occurrence record emitted by stage 1 pass A.
///
/// `(local_node_id, blob_idx, blob_local_slot)` is the parallel-safe
/// decomposition of the audit's `(local_node_id, slot_pos)` form: stage
/// 2 reconstructs `linear_slot_pos = blob_start_slot[blob_idx] +
/// blob_local_slot` from the per-blob refcount prefix-sum that pass A
/// produces in `ref_count_sidecar`. The stage-1 worker decoding a blob knows
/// `blob_local_slot`; a global slot position would need every earlier blob's
/// refcount first, serialising emission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IdRecord {
    pub(super) local_node_id: u32,
    pub(super) blob_idx: u32,
    pub(super) blob_local_slot: u32,
}

/// High bit in an [`IdRecord::local_node_id`] marks a trailing closed-ring
/// reference. It is metadata, never part of the bucket-local node id.
pub(super) const CLOSURE_FLAG: u32 = 0x8000_0000;
pub(super) const LOCAL_ID_MASK: u32 = 0x7fff_ffff;

impl IdRecord {
    pub(super) fn write_to(&self, buf: &mut [u8; ID_RECORD_SIZE]) {
        buf[ID_OFF_LOCAL_NODE_ID..][..ID_W_LOCAL_NODE_ID]
            .copy_from_slice(&self.local_node_id.to_le_bytes());
        buf[ID_OFF_BLOB_IDX..][..ID_W_BLOB_IDX].copy_from_slice(&self.blob_idx.to_le_bytes());
        buf[ID_OFF_BLOB_LOCAL_SLOT..][..ID_W_BLOB_LOCAL_SLOT]
            .copy_from_slice(&self.blob_local_slot.to_le_bytes());
    }

    #[expect(
        clippy::expect_used,
        reason = "each field is a four-byte subslice of a fixed-size record, so the conversion to [u8; 4] cannot fail"
    )]
    pub(super) fn read_from(buf: &[u8; ID_RECORD_SIZE]) -> Self {
        let local_node_id = u32::from_le_bytes(
            buf[ID_OFF_LOCAL_NODE_ID..][..ID_W_LOCAL_NODE_ID]
                .try_into()
                .expect("4 bytes"),
        );
        let blob_idx = u32::from_le_bytes(
            buf[ID_OFF_BLOB_IDX..][..ID_W_BLOB_IDX]
                .try_into()
                .expect("4 bytes"),
        );
        let blob_local_slot = u32::from_le_bytes(
            buf[ID_OFF_BLOB_LOCAL_SLOT..][..ID_W_BLOB_LOCAL_SLOT]
                .try_into()
                .expect("4 bytes"),
        );
        Self {
            local_node_id,
            blob_idx,
            blob_local_slot,
        }
    }
}

#[cfg(test)]
mod id_record_tests {
    use super::{ID_RECORD_SIZE, IdRecord};

    #[test]
    fn write_then_read_round_trips() {
        let cases = [
            IdRecord {
                local_node_id: 0,
                blob_idx: 0,
                blob_local_slot: 0,
            },
            IdRecord {
                local_node_id: 1,
                blob_idx: 2,
                blob_local_slot: 3,
            },
            IdRecord {
                local_node_id: u32::MAX,
                blob_idx: u32::MAX,
                blob_local_slot: u32::MAX,
            },
            IdRecord {
                local_node_id: 0xDEAD_BEEF,
                blob_idx: 0xCAFE_F00D,
                blob_local_slot: 0x1234_5678,
            },
        ];
        let mut buf = [0u8; ID_RECORD_SIZE];
        for r in cases {
            r.write_to(&mut buf);
            assert_eq!(IdRecord::read_from(&buf), r);
        }
    }

    #[test]
    fn write_emits_little_endian() {
        let r = IdRecord {
            local_node_id: 0x0403_0201,
            blob_idx: 0x0807_0605,
            blob_local_slot: 0x0C0B_0A09,
        };
        let mut buf = [0u8; ID_RECORD_SIZE];
        r.write_to(&mut buf);
        assert_eq!(
            buf,
            [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
            ]
        );
    }
}

/// A resolved coordinate ready to be scattered into a slot bucket for
/// stage 3's coord_payloads emission.
// Scratch wire records are 12 bytes (u32 bucket-local slot, i32 lat, i32
// lon), regardless of this Rust struct's padding. With inject_prepass, lat
// carries (geographic_lat << 1) | pin; otherwise it is geographic_lat.
// Stage 2 validates +/-900_000_000 before shifting, so both extremes fit
// i32. Stage 3 scatters these bytes unchanged; coord_payloads decodes them
// using the same run-wide inject_prepass option before stage 4 consumes
// coordinate varints and a separate pin bitmap. These files are private to
// one invocation, not a persistent format that can outlive that option.
// Keep the 12-byte scratch record: a separate field would grow each encoded
// entry by 1/3 at 16 bytes, and the slot's high bit is not spare (bucket
// widths can use the full u32 range). coord_pack's presence bit serves rank
// stores, where absence must remain distinct from Null Island. This scratch
// stream intentionally maps unwritten orphan slots to (0, 0) placeholders
// and counts absence in stage 2 independently of coordinate values. The
// command rejects missing locations unless `--ignore-missing-nodes` accepts
// those placeholders. With injection, coord_pack also leaves no bit for the
// pin; preserving it would need a side bitmap or a wider scratch record.
#[derive(Clone, Copy)]
pub(super) struct ResolvedEntry {
    slot_pos: u64,
    lat: i32,
    lon: i32,
}

impl ResolvedEntry {
    fn write_to(&self, bucket_start: u64, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        #[allow(clippy::cast_possible_truncation)]
        let local_slot_pos = (self.slot_pos - bucket_start) as u32;
        buf[RE_OFF_LOCAL_SLOT_POS..][..RE_W_LOCAL_SLOT_POS]
            .copy_from_slice(&local_slot_pos.to_le_bytes());
        buf[RE_OFF_LAT..][..RE_W_LAT].copy_from_slice(&self.lat.to_le_bytes());
        buf[RE_OFF_LON..][..RE_W_LON].copy_from_slice(&self.lon.to_le_bytes());
    }

    /// Bucket index for slot-pos partitioning.
    ///
    /// Uses floor division for `range_size` so the last bucket *absorbs*
    /// the remainder (and is wider than the others) instead of being
    /// truncated. This keeps every bucket's width ≥ `range_size`, which
    /// (together with the `slot_bucket_count = total_slots / max_blob_slots`
    /// floor in `external_join`) preserves the 2-piece straddler
    /// invariant for all input sizes. Out-of-range high slot_pos values
    /// (that would land past the nominal last bucket because the last
    /// is wider) get clamped to `slot_bucket_count - 1`.
    #[allow(clippy::cast_possible_truncation)]
    fn slot_bucket(&self, total_slots: u64, slot_bucket_count: usize) -> usize {
        let range_size = total_slots / slot_bucket_count as u64;
        if range_size == 0 {
            return 0;
        }
        let bucket = self.slot_pos / range_size;
        (bucket as usize).min(slot_bucket_count - 1)
    }
}

pub(super) fn slot_bucket_bounds(
    total_slots: u64,
    slot_bucket_count: usize,
    bucket_idx: usize,
) -> (u64, u64) {
    let range_size = total_slots / slot_bucket_count as u64;
    let bucket_start = bucket_idx as u64 * range_size;
    let bucket_end = if bucket_idx == slot_bucket_count - 1 {
        total_slots
    } else {
        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
    };
    (bucket_start, bucket_end)
}

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded working memory and sequential I/O. Temporary-disk demand depends
/// on the input; see `reference/performance.md` for measured resource use and
/// the module docs for the algorithm.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)): where a later
/// open cannot reach the pinned file, an input replaced mid-run fails with
/// `InputReplaced`.
#[allow(clippy::too_many_arguments)]
pub fn external_join(
    input: &Path,
    output: impl crate::output::BindOutput,
    keep_untagged_nodes: bool,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    inject_prepass: bool,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<Stats> {
    let host = &host.enter();
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    // Every stage's open of the input derives from this pin; see
    // `add_locations_to_ways_to`, which pins the same way.
    let pin = PinnedInput::pin(input, host)?;
    let front = InputFront::open(&pin, write.direct_reads, host)?;
    let result = external_join_in(
        &pin,
        front,
        &output,
        keep_untagged_nodes,
        write,
        overrides,
        inject_prepass,
        host,
        &scratch,
    )?;
    drop(scratch);
    output.publish()?;
    Ok(result)
}

/// Validate before stage 2 writes slot buckets. Each nonempty blob is no
/// wider than the smallest bucket, so it intersects at most two adjacent
/// buckets. Empty blobs publish no straddler half, including at total_slots.
fn validate_slot_layout(starts: &[u64], total: u64, buckets: usize) -> Result<()> {
    if buckets == 0 || starts.first().is_some_and(|&start| start != 0) {
        return Err(crate::Error::internal(
            "external join: invalid slot bucket layout",
        ));
    }
    let width = total / buckets as u64;
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(total);
        let length = end.checked_sub(start).ok_or_else(|| {
            crate::Error::internal("external join: way slot starts are not monotonic")
        })?;
        if end > total || length > width {
            return Err(crate::Error::internal(format!(
                "external join: way blob {i} spans {length} slots, exceeding slot bucket width {width}; two-piece straddler bound would be violated"
            )));
        }
    }
    if starts.is_empty() && total != 0 {
        return Err(crate::Error::internal(
            "external join: nonzero slots without way blobs",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod slot_layout_tests {
    use super::validate_slot_layout;

    #[test]
    fn checks_straddler_bound_before_bucket_writes() {
        assert!(validate_slot_layout(&[0, 3, 6, 9], 12, 4).is_ok());
        assert!(validate_slot_layout(&[0, 4], 12, 4).is_err());
        assert!(validate_slot_layout(&[0, 6, 3], 12, 2).is_err());
        assert!(validate_slot_layout(&[0, 0], 0, 256).is_ok());
        assert!(validate_slot_layout(&[], 0, 256).is_ok());
        assert!(validate_slot_layout(&[0], 1, 0).is_err());
    }
}

/// [`external_join`] writing to a borrowed output it does not publish, in
/// scratch the caller owns.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) fn external_join_in(
    input: &PinnedInput,
    mut front: InputFront<'_>,
    output: &crate::output::BoundOutput,
    keep_untagged_nodes: bool,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    inject_prepass: bool,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
) -> Result<Stats> {
    let (direct_io, force) = (write.direct_reads, write.force);
    // `front` is the caller's one read of the input's preconditions; it
    // answers the indexdata and sortedness refusals here and supplies the
    // header stage 4 writes from.
    let has_indexdata = front.require_indexdata(
        force,
        "external join requires indexdata for efficient blob filtering",
        host,
    )?;

    // External-join cannot function without indexdata: the stage-1/2
    // ID bucket ranges are computed from per-blob `(min_id,
    // max_id)` derived from indexdata, and `scan_blob_metadata` hard-
    // errors if any OsmData blob lacks it. Under `--force` the
    // indexdata check upstream returns `Ok(false)` instead of a clear
    // error, which previously let the command proceed and then fail
    // in `scan_blob_metadata` with an opaque "OsmData blob missing
    // indexdata" message. Reject the combination up front with a
    // clear migration hint.
    if !has_indexdata {
        return Err(crate::error::new_missing_indexdata(
            "add-locations-to-ways with the external index requires indexed input, \
             even when forced: its ID bucket ranges come from per-blob indexdata. \
             The sparse index decodes every blob and runs without it.",
        )
        .locate(crate::error::Location::in_file(input.path())));
    }

    let header = front.into_header()?;
    crate::commands::require_sorted(
        header.is_sorted(),
        input.path(),
        "Input PBF",
        "The external join's single-pass node merge depends on ascending node ID order.",
    )?;

    let ref_count_sidecar = scratch.file("way-ref-counts")?;
    let per_way_refcount_sidecar = scratch.file("per-way-refcounts")?;

    host.invocation.marker("EXTJOIN_META_SCAN_START");
    let t_meta = crate::counters::Stopwatch::start();
    let blob_meta = blob_meta::scan_blob_metadata(input, !keep_untagged_nodes, host)?;
    host.invocation
        .counter("extjoin_meta_scan_ms", t_meta.elapsed_ms());
    host.invocation
        .counter("extjoin_meta_blobs", blob_meta.len());
    host.invocation
        .counter("extjoin_meta_tag_scan_enabled", !keep_untagged_nodes);
    host.invocation.marker("EXTJOIN_META_SCAN_END");

    // Build the BucketLayout for IdRecord emission. Width is derived
    // from the largest node-blob max_id in indexdata, not the
    // planet-wide constant - tighter buckets on smaller fixtures
    // and exact for any real input.
    let max_node_id = max_node_id_from_blob_meta(&blob_meta);
    let bucket_layout = BucketLayout::new(max_node_id, NUM_BUCKETS)?;
    host.invocation
        .counter("extjoin_bucket_max_node_id", max_node_id);
    host.invocation
        .counter("extjoin_bucket_width", bucket_layout.bucket_width);

    // Stage 1: produces total_slots, id_bucket_counts, num_shard_workers
    // and the per-node-blob mapping (see `Stage1Output`).
    //
    // #9 layer 2: relation member-id scan runs concurrently with stage 1.
    // The scan reads relation blobs only (via blob_meta) and shares no
    // state with stage 1 - both read from the same input PBF via pread
    // (`File: Sync` on Unix) with no locking, each through its own open
    // derived from the pin. Running it here avoids the former serial gap
    // between stage 2 and stage 4.
    host.invocation.marker("EXTJOIN_STAGE1_START");
    let s1_faults_before = crate::debug::read_page_faults(&host.invocation);

    let input_ref_parallel: &PinnedInput = input;
    let blob_meta_ref_parallel = &blob_meta;
    let bucket_layout_ref = &bucket_layout;
    let (stage1_out, relation_member_node_ids) = std::thread::scope(
        |scope| -> Result<(
            super::external::stage1::Stage1Output,
            Option<relation_scan::RelationScanOutput>,
        )> {
            let s1_handle = scope.spawn(|| {
                stage1_way_pass(
                    blob_meta_ref_parallel,
                    input_ref_parallel,
                    direct_io,
                    scratch,
                    bucket_layout_ref,
                    &ref_count_sidecar,
                    &per_way_refcount_sidecar,
                    inject_prepass,
                    host,
                )
            });
            let rel_handle = if keep_untagged_nodes && !inject_prepass {
                None
            } else {
                host.invocation.marker("EXTJOIN_RELATION_SCAN_START");
                Some(scope.spawn(move || {
                    let t_relscan = crate::counters::Stopwatch::start();
                    let ids = relation_scan::collect_relation_member_ids_indexed(
                        input_ref_parallel,
                        blob_meta_ref_parallel,
                        !keep_untagged_nodes,
                        inject_prepass,
                        &host.invocation,
                    )?;
                    host.invocation
                        .counter("extjoin_relation_member_collect_ms", t_relscan.elapsed_ms());
                    host.invocation.marker("EXTJOIN_RELATION_SCAN_END");
                    Ok::<_, crate::Error>(ids)
                }))
            };

            // Error ordering: `s1_handle.join()??` short-circuits before
            // joining the relation-scan handle, but `thread::scope` still
            // waits for `rel_handle` to run to completion before the scope
            // returns. A stage-1 failure while the relation scan is
            // running therefore delays error reporting until the relation
            // scan completes. Accepted as diagnostic-quality: stage-1
            // failures are rare (only fire on adversarial or malformed
            // input, mostly closed by the defensive-input checks in
            // ADR-0004). A cancellation is not delayed: both threads
            // observe the invocation's token, the relation scan per blob.
            let s1_res = s1_handle.join().map_err(|payload| {
                crate::error::panic_error("stage 1 thread", payload.as_ref())
            })??;
            let rel_res = match rel_handle {
                Some(handle) => Some(handle.join().map_err(|payload| {
                    crate::error::panic_error("relation scan thread", payload.as_ref())
                })??),
                None => None,
            };
            Ok((s1_res, rel_res))
        },
    )?;

    let s1_faults_after = crate::debug::read_page_faults(&host.invocation);
    let total_id_records: u64 = stage1_out.id_bucket_counts.iter().sum();
    // Partition-balance diagnostics (max/min-per-shard convention, cf.
    // derivepar_*_shard_max/min): min is taken over nonempty buckets so
    // small fixtures that leave buckets unused still report the spread
    // of the buckets that actually carry records.
    let id_bucket_max_records: u64 = stage1_out
        .id_bucket_counts
        .iter()
        .copied()
        .max()
        .unwrap_or(0);
    let id_bucket_min_records: u64 = stage1_out
        .id_bucket_counts
        .iter()
        .copied()
        .filter(|&c| c > 0)
        .min()
        .unwrap_or(0);
    let id_buckets_nonempty = stage1_out
        .id_bucket_counts
        .iter()
        .filter(|&&c| c > 0)
        .count();
    host.invocation
        .counter("extjoin_total_slots", stage1_out.total_slots);
    host.invocation
        .counter("extjoin_total_id_records", total_id_records);
    host.invocation
        .counter("extjoin_id_bucket_max_records", id_bucket_max_records);
    host.invocation
        .counter("extjoin_id_bucket_min_records", id_bucket_min_records);
    host.invocation
        .counter("extjoin_id_buckets_nonempty", id_buckets_nonempty);
    crate::debug::emit_page_fault_deltas(
        &host.invocation,
        ("s1_minflt_delta", "s1_majflt_delta"),
        s1_faults_before,
        s1_faults_after,
    );
    host.invocation.marker("EXTJOIN_STAGE1_END");

    let total_slots = stage1_out.total_slots;
    let id_bucket_counts = stage1_out.id_bucket_counts;
    let num_shard_workers = stage1_out.num_shard_workers;
    let node_blob_mapping = stage1_out.node_blob_mapping;

    // Compute slot_bucket_count: scale down from NUM_BUCKETS so that
    // every bucket can fit at least one full blob's slot range. This
    // keeps the 2-piece straddler invariant (a blob spans at most two
    // adjacent buckets) for both planet-scale inputs and tiny test
    // fixtures where total_slots / NUM_BUCKETS would otherwise be < 1.
    let way_slot_starts = stage4::load_ref_count_sidecar(&ref_count_sidecar, total_slots)?;
    let max_blob_slots: u64 = (0..way_slot_starts.len())
        .map(|i| {
            let end = if i + 1 < way_slot_starts.len() {
                way_slot_starts[i + 1]
            } else {
                total_slots
            };
            end - way_slot_starts[i]
        })
        .max()
        .unwrap_or(0);
    // Each bucket must hold ≥ max_blob_slots so the SMALLEST bucket
    // (which can be smaller than range_size when total_slots is not
    // a multiple of bucket_count) still satisfies the 2-piece
    // straddler invariant. Equivalently: bucket_count ≤
    // total_slots / max_blob_slots, with floor division.
    #[allow(clippy::cast_possible_truncation)]
    let slot_bucket_count = total_slots
        .checked_div(max_blob_slots)
        .map(|n| n.max(1).min(NUM_BUCKETS as u64) as usize)
        .unwrap_or(NUM_BUCKETS);
    validate_slot_layout(&way_slot_starts, total_slots, slot_bucket_count)?;
    // Worker count: see `ALTW_STAGE3_WORKERS_CAP` for why it is capped.
    // The stage 3 worker pool and its router backing files share this count;
    // the scratch precheck counts one payload file per worker.
    let num_workers = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    )
    .min(ALTW_STAGE3_WORKERS_CAP);
    // Stage 1's exact ref count is the first point at which stage 2-4
    // scratch can be sized without an extra input pass; see `space`.
    space::precheck(
        scratch.path(),
        total_slots,
        slot_bucket_count,
        num_workers,
        &host.invocation,
    )?;
    let total_id_shard_files = num_shard_workers * NUM_BUCKETS;
    host.invocation
        .counter("extjoin_id_bucket_count", NUM_BUCKETS);
    host.invocation
        .counter("extjoin_slot_bucket_count", slot_bucket_count);
    host.invocation
        .counter("extjoin_max_blob_slots", max_blob_slots);
    host.invocation
        .counter("extjoin_num_shard_workers", num_shard_workers);
    host.invocation
        .counter("extjoin_total_id_shard_files", total_id_shard_files);

    // Stage boundary: the sidecar loads and the scratch precheck above have
    // no checkpoint of their own.
    host.invocation.check()?;
    host.invocation.marker("EXTJOIN_STAGE2_START");
    let s2_faults_before = crate::debug::read_page_faults(&host.invocation);
    let slot_buckets = SlotBuckets::create(scratch, slot_bucket_count)?;
    let input_pbf = crate::read::input::InputFile::open(input)?;
    let resolved_count = stage2_node_join(
        scratch,
        &id_bucket_counts,
        num_shard_workers,
        bucket_layout_ref,
        &way_slot_starts,
        &slot_buckets,
        slot_bucket_count,
        total_slots,
        &input_pbf,
        &node_blob_mapping,
        inject_prepass,
        host,
    )?;
    slot_buckets.finish(&host.invocation)?;
    let s2_faults_after = crate::debug::read_page_faults(&host.invocation);
    // Reclaim id-shard scratch before the heavier stage 3/4 streaming
    // handoff allocates its own temporary space.
    for worker_id in 0..num_shard_workers {
        for bucket_idx in 0..NUM_BUCKETS {
            scratch.remove(&radix::id_shard_path(scratch, worker_id, bucket_idx)?);
        }
    }
    host.invocation
        .counter("extjoin_resolved_count", resolved_count);
    crate::debug::emit_page_fault_deltas(
        &host.invocation,
        ("s2_minflt_delta", "s2_majflt_delta"),
        s2_faults_before,
        s2_faults_after,
    );
    host.invocation.marker("EXTJOIN_STAGE2_END");

    // Free the per-blob mapping - it was a stage 2 input only.
    drop(node_blob_mapping);

    // Prepare inputs for the streaming stage 3 + stage 4 handoff.
    let per_way_rcs = coord_payloads::load_per_way_refcount_sidecar_indexed(
        &per_way_refcount_sidecar,
        way_slot_starts.len(),
    )?;
    host.invocation.counter("s3_worker_count", num_workers);

    // Worker tmp files opened once here with read + write, wrapped in
    // Arc<File> so stage 3 can `write_all_at` and stage 4 can
    // `read_exact_at` on the same `&File`. `File` is `Sync` on Unix for
    // pread/pwrite so no extra locking is needed.
    let worker_tmp_paths: Vec<crate::scratch::ScratchPath> = (0..num_workers)
        .map(|i| radix::payload_path(scratch, i))
        .collect::<crate::Result<_>>()?;
    let worker_files: Vec<crate::read::input::InputFile> = worker_tmp_paths
        .iter()
        .map(|p| {
            crate::scratch::create_read_write(p, false)
                .map(|file| {
                    crate::read::input::InputFile::from_parts(
                        std::sync::Arc::new(file),
                        std::sync::Arc::from(p.as_ref()),
                    )
                })
                .map_err(|e| {
                    crate::Error::from(e).context(format!(
                        "failed to create stage 3 payload file {}",
                        p.display()
                    ))
                })
        })
        .collect::<crate::Result<_>>()?;

    let slot_entry_counts: Vec<u64> = (0..slot_bucket_count)
        .map(|i| {
            let path = radix::slot_bucket_path(scratch, i)?;
            Ok(std::fs::metadata(&path)
                .map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64)
                .unwrap_or(0))
        })
        .collect::<crate::Result<_>>()?;
    // Slot-bucket balance twin of the id-bucket counters above; min is
    // over nonempty buckets.
    {
        let max_entries: u64 = slot_entry_counts.iter().copied().max().unwrap_or(0);
        let min_entries: u64 = slot_entry_counts
            .iter()
            .copied()
            .filter(|&c| c > 0)
            .min()
            .unwrap_or(0);
        let nonempty = slot_entry_counts.iter().filter(|&&c| c > 0).count();
        host.invocation
            .counter("extjoin_slot_bucket_max_entries", max_entries);
        host.invocation
            .counter("extjoin_slot_bucket_min_entries", min_entries);
        host.invocation
            .counter("extjoin_slot_buckets_nonempty", nonempty);
    }
    let slot_paths: Vec<std::path::PathBuf> = (0..slot_bucket_count)
        .map(|i| radix::slot_bucket_path(scratch, i).map(|p| p.to_path_buf()))
        .collect::<crate::Result<_>>()?;
    let slot_bucket_ref = SlotBucketRef {
        paths: slot_paths,
        entry_counts: slot_entry_counts,
    };

    // The streaming router: pre-populates `Empty` entries for zero-ref
    // way blobs so stage 4 never waits on a blob that stage 3 would
    // never publish.
    let router = coord_payloads::ConcurrentBlobLocationRouter::new(
        &per_way_rcs,
        worker_files.clone(),
        &host.invocation,
    )?;

    // (#9 layer 2: relation scan already ran in parallel with stage 1
    // above; `relation_member_node_ids` is already bound. No serial
    // scan between stage 2 and stage 4.)

    // Streaming stage 3 + stage 4: run concurrently via a single
    // `thread::scope`. Stage 3 publishes per-blob entries to the router
    // as it encodes them; stage 4 workers block on `router.wait_ready`
    // ahead of any input pread so they never hold decompressed state
    // while waiting.
    host.invocation.check()?;
    host.invocation.marker("EXTJOIN_STREAMING_START");
    host.invocation.marker("EXTJOIN_STAGE3_START");
    host.invocation.marker("EXTJOIN_STAGE4_START");
    let s3_faults_before = crate::debug::read_page_faults(&host.invocation);

    let router_ref = &router;
    let per_way_rcs_ref = &per_way_rcs;
    let blob_meta_ref = &blob_meta;
    let header_ref = &header;
    let way_slot_starts_ref = way_slot_starts.as_slice();
    let rel_ids_ref = relation_member_node_ids
        .as_ref()
        .and_then(|scan| scan.member_node_ids.as_ref());
    let member_way_ids_ref = relation_member_node_ids
        .as_ref()
        .and_then(|scan| scan.member_way_ids.as_ref());
    let slot_bucket_ref_ref = &slot_bucket_ref;
    let inject_metrics = super::inject_metrics::InjectMetrics::default();
    let inject_metrics_ref = &inject_metrics;

    // Both stage threads return the typed crate::Error unchanged, so a
    // PlatformUnsupported raised by the output writer in stage 4 keeps its
    // kind (and exit code) across the join.
    let mut stats = std::thread::scope(|scope| -> crate::Result<Stats> {
        let s3_handle = scope.spawn(move || -> crate::Result<()> {
            let integrated = IntegratedInputs {
                way_slot_starts: way_slot_starts_ref,
                per_way_rcs: per_way_rcs_ref,
                router: router_ref,
                inject_prepass,
            };
            let result = stage3_slot_reorder(
                slot_bucket_ref_ref,
                slot_bucket_count,
                total_slots,
                &integrated,
                num_workers,
                &host.invocation,
            );
            // Signal the router that no more publishes are coming. Must
            // run whether stage 3 succeeded or errored - otherwise stage
            // 4 waiters on unpublished slots would hang. On error the
            // worker has already called `router.abort`, but
            // mark_producer_done is idempotent with abort and cheap.
            router_ref.mark_producer_done();
            result
        });
        // A stage 4 failure aborts the router, so stage 3 stops at its next
        // bucket claim instead of finishing the rest for nobody: on a returned
        // error here, and through the guard on a panic. Failures inside the
        // ordered pool abort earlier, before the pool joins its workers (see
        // `S4Consumer` and the worker guards in `stage4_assembly`); this covers everything else (the schedule, the
        // writer, the final flush). An abort already raised keeps its first
        // error.
        let s4_handle = scope.spawn(move || -> crate::Result<Stats> {
            let guard = coord_payloads::AbortOnDrop::new(router_ref, "stage 4");
            let result = stage4_assembly(
                input,
                output,
                header_ref,
                blob_meta_ref,
                router_ref,
                per_way_rcs_ref,
                way_slot_starts_ref,
                keep_untagged_nodes,
                rel_ids_ref,
                member_way_ids_ref,
                inject_prepass,
                inject_metrics_ref,
                write,
                overrides,
                host,
            );
            // Stage 4's workers have joined inside `stage4_assembly`, so the
            // total is final; emitted on failure too, where an aborted wait
            // is part of the story.
            host.invocation
                .counter("s4_readiness_wait_ns", router_ref.readiness_wait_ns());
            if let Err(error) = &result {
                router_ref.abort(error.clone());
            }
            guard.disarm();
            result
        });

        let s3_res = s3_handle
            .join()
            .map_err(|payload| crate::error::panic_error("stage 3 thread", payload.as_ref()))?;
        let s4_res = s4_handle
            .join()
            .map_err(|payload| crate::error::panic_error("stage 4 thread", payload.as_ref()))?;

        // Prefer the stage 3 error if both failed (it's usually the root
        // cause - stage 4 typically errors only because of an abort that
        // stage 3 or the writer raised).
        s3_res?;
        s4_res
    })?;

    let s3_faults_after = crate::debug::read_page_faults(&host.invocation);
    for i in 0..slot_bucket_count {
        scratch.remove(&radix::slot_bucket_path(scratch, i)?);
    }
    crate::debug::emit_page_fault_deltas(
        &host.invocation,
        ("s3_minflt_delta", "s3_majflt_delta"),
        s3_faults_before,
        s3_faults_after,
    );
    host.invocation.marker("EXTJOIN_STAGE4_END");
    host.invocation.marker("EXTJOIN_STAGE3_END");
    host.invocation.marker("EXTJOIN_STREAMING_END");

    // Emit router statistics for the streaming stage 3 path.
    {
        let s = router
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.invocation
            .counter("s3_router_num_worker", s.num_worker);
        host.invocation
            .counter("s3_router_num_straddlers", s.num_straddlers);
        host.invocation.counter("s3_router_num_empty", s.num_empty);
        host.invocation
            .counter("s3_router_worker_bytes", s.worker_bytes);
        host.invocation
            .counter("s3_router_straddler_bytes", s.straddler_bytes);
        host.invocation
            .counter("s3_straddler_encode_ms", s.straddler_encode_ns / 1_000_000);
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "altw.external.coord_payloads",
            "[coord_payloads] streaming router {} way blobs ({} worker / {} straddler / {} empty), \
             {} MB in worker tmps + {} KB straddler bytes in RAM",
            router.num_blobs(),
            s.num_worker,
            s.num_straddlers,
            s.num_empty,
            s.worker_bytes / 1_000_000,
            s.straddler_bytes / 1_000,
        );
    }

    // `resolved_count` counts slots whose node was found in the id-sorted
    // tuple stream - presence, not a non-zero coordinate. It used to count
    // `!= (0, 0)`, which reported every genuine node at Null Island as
    // missing and so made this backend disagree with the sparse one about
    // the same input. Absence is not zero; see the counting site in
    // `stage2.rs`.
    stats.missing_locations = total_slots.saturating_sub(resolved_count);

    if inject_prepass {
        let member_ways = member_way_ids_ref.map_or(0, |set| set.iter().count() as u64);
        inject_metrics.emit(member_ways, &host.invocation);
    }

    Ok(stats)
}
