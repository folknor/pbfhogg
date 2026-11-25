//! External join for add-locations-to-ways: bounded-memory coordinate resolution
//! via double radix permutation.
//!
//! Instead of building a large random-access node index, this module
//! pre-computes the way-node join using sequential I/O and bounded memory:
//!
//! 1. **Way pass**: stream ways, emit `(node key, slot_pos)` COO pairs into
//!    256 node buckets partitioned by node key (signed node IDs fall into two
//!    key families sharing the buckets; see `keys.rs`).
//! 2. **Node join**: per bucket, sort pairs by node key in memory, merge-join
//!    with matching node stream, emit `(slot_pos, lat, lon)` into
//!    256 slot buckets partitioned by high bits of slot_pos.
//! 3. **Slot reorder**: per bucket, sort by slot_pos, emit final blob-ordered
//!    delta-varint `coord_payloads` (see `coord_payloads.rs`). The flat
//!    `coord_slots` array is a historical intermediate, retired in 2026-04.
//! 4. **Assembly**: stream original PBF + per-blob coord_payloads preads,
//!    emit enriched ways.
//!
//! All I/O is sequential. Temporary-disk demand and peak memory depend on the
//! input; see `reference/performance/add-locations-to-ways.md` for measurements. This path does not
//! build a random-access node index.

use super::Stats;
use crate::commands::HeaderOverrides;
use crate::commands::input_front::InputFront;
use crate::error::Result;
use crate::read::input::PinnedInput;

mod blob_bucket_index;
mod blob_meta;
mod coord_payloads;
mod keys;
mod radix;
mod relation_scan;
mod space;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

use crate::tunables::{
    ALTW_STAGE3_WORKERS_CAP, DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED,
    NUM_BUCKETS,
};

use stage1::stage1_way_pass;
use stage2::{SlotBuckets, stage2_node_join};
use stage3::{IntegratedInputs, stage3_slot_reorder};
use stage4::stage4_assembly;

/// What this backend would refuse in an input's indexdata, or would not be
/// chosen for, judged blob by blob in the one header walk auto routing
/// already makes, so auto never routes here an input the join then refuses
/// on those same claims. The checks are the join's own, not copies:
/// [`blob_meta::check_range`] over every data blob (reversed ranges and
/// canonical order), the `u32` way-blob ordinal of `stage1`, and the
/// node-key plan ([`keys::KeyPlan`]) the join builds from the same claims.
/// A plan needing the wide shard record (node keys past what the packed
/// record encodes, about 2^38) is not one auto chooses: the join takes it,
/// but auto keeps such an input on sparse, which it routed to before the
/// external index could take it. A node blob holding both key families is
/// decoded by the join's scan, not here, so a disorder hidden inside its
/// range is the join's refusal of the payload, as for any other blob.
/// Refusals that need the way pass (a slot bucket's width, scratch space)
/// stay with the join, as does the trust placed in a false but well-formed
/// claim (`reference/indexdata-trust.md`).
pub(super) struct HeaderAdmission {
    order: crate::osm_id::OrderWatermark,
    nodes: keys::NodeClaims,
    way_blobs: usize,
    refused: Option<crate::Error>,
}

impl HeaderAdmission {
    pub(super) fn new() -> Self {
        Self {
            order: crate::osm_id::OrderWatermark::default(),
            nodes: keys::NodeClaims::default(),
            way_blobs: 0,
            refused: None,
        }
    }

    /// One data blob's indexdata, in file order. After the first refusal
    /// the rest are not judged.
    pub(super) fn observe(&mut self, idx: &crate::blob_meta::BlobIndex) {
        if self.refused.is_some() {
            return;
        }
        if let Err(e) = blob_meta::check_range(idx, None, &mut self.order) {
            self.refused = Some(e);
            return;
        }
        match idx.kind {
            crate::ElementKind::Node => self.nodes.observe(idx.min_id, idx.max_id, idx.count),
            crate::ElementKind::Way => self.way_blobs += 1,
            crate::ElementKind::Relation => {}
        }
    }

    /// `Ok` when auto may route the input here, else why not: the join's
    /// own refusal, or the wide plan auto does not choose.
    pub(super) fn finish(self) -> Result<()> {
        if let Some(e) = self.refused {
            return Err(e);
        }
        if let Some(last) = self.way_blobs.checked_sub(1) {
            stage1::way_blob_ordinal(last)?;
        }
        let plan = keys::KeyPlan::new(&self.nodes);
        if !plan.compact {
            return Err(crate::Error::invalid_data(format!(
                "its node keys reach {} (positive) and {} (negative), past what the \
                 external index's packed record encodes, and auto does not choose its \
                 wide record",
                plan.pos.max_key, plan.neg.max_key,
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod admission_tests {
    use super::HeaderAdmission;
    use crate::ElementKind;
    use crate::blob_meta::BlobIndex;

    fn idx(kind: ElementKind, min_id: i64, max_id: i64) -> BlobIndex {
        BlobIndex {
            kind,
            min_id,
            max_id,
            count: 1,
            bbox: None,
        }
    }

    fn admit(blobs: &[BlobIndex]) -> crate::error::Result<()> {
        let mut admission = HeaderAdmission::new();
        for b in blobs {
            admission.observe(b);
        }
        admission.finish()
    }

    #[test]
    fn admits_what_the_join_admits() {
        admit(&[
            idx(ElementKind::Node, 1, 10),
            idx(ElementKind::Node, 10, 20),
            idx(ElementKind::Way, -5, 3),
            idx(ElementKind::Relation, 1, 1),
        ])
        .expect("sorted non-negative nodes");
        admit(&[]).expect("no data blob");
        admit(&[idx(ElementKind::Node, 0, 1 << 38)]).expect("2^38 is packed");
        admit(&[
            idx(ElementKind::Node, -2, 0),
            idx(ElementKind::Node, -9, -5),
            idx(ElementKind::Node, -11, 2),
            idx(ElementKind::Node, 3, 9),
        ])
        .expect("negative node blobs in canonical order");
    }

    #[test]
    fn refuses_what_the_join_refuses_or_auto_does_not_choose() {
        for (blobs, says) in [
            (
                vec![idx(ElementKind::Node, 5, 9), idx(ElementKind::Node, 1, 2)],
                "contradict the sorted header",
            ),
            (
                vec![idx(ElementKind::Node, -1, -1), idx(ElementKind::Node, 0, 0)],
                "contradict the sorted header",
            ),
            (
                vec![idx(ElementKind::Way, 1, 2), idx(ElementKind::Node, 1, 2)],
                "contradict the sorted header",
            ),
            (
                vec![idx(ElementKind::Node, 1, (1 << 38) + 1)],
                "packed record",
            ),
            (vec![idx(ElementKind::Node, i64::MIN, -1)], "packed record"),
        ] {
            let err = admit(&blobs).expect_err(says);
            assert!(err.to_string().contains(says), "{says}: {err}");
        }
    }
}
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

/// An id-bucketed occurrence record emitted by stage 1 pass A, in the
/// packed form ([`IdCodec`]).
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

/// An id-bucketed occurrence record in the wide form ([`WideCodec`]): the
/// whole bucket-local key, for layouts whose keys the packed record cannot
/// hold. The closure flag rides in the slot's top bit, which a slot never
/// uses: a way blob decodes to at most 32 MiB and every ref takes at least
/// one byte of it, so a blob-local slot stays below 2^25.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WideIdRecord {
    pub(super) local: u64,
    pub(super) blob_idx: u32,
    /// Blob-local slot, with [`CLOSURE_FLAG`] for a trailing closed-ring
    /// reference.
    pub(super) slot: u32,
}

/// What stage 2 reads of a record, in either form.
pub(super) trait ShardRecord: Copy + Send + Sync + std::fmt::Debug + 'static {
    /// The bucket-local key, without the closure flag.
    fn local(&self) -> u64;
    fn closure(&self) -> bool;
    fn blob_idx(&self) -> u32;
    fn blob_local_slot(&self) -> u32;
}

impl ShardRecord for IdRecord {
    #[inline]
    fn local(&self) -> u64 {
        u64::from(self.local_node_id & LOCAL_ID_MASK)
    }
    #[inline]
    fn closure(&self) -> bool {
        self.local_node_id & CLOSURE_FLAG != 0
    }
    #[inline]
    fn blob_idx(&self) -> u32 {
        self.blob_idx
    }
    #[inline]
    fn blob_local_slot(&self) -> u32 {
        self.blob_local_slot
    }
}

impl ShardRecord for WideIdRecord {
    #[inline]
    fn local(&self) -> u64 {
        self.local
    }
    #[inline]
    fn closure(&self) -> bool {
        self.slot & CLOSURE_FLAG != 0
    }
    #[inline]
    fn blob_idx(&self) -> u32 {
        self.blob_idx
    }
    #[inline]
    fn blob_local_slot(&self) -> u32 {
        self.slot & LOCAL_ID_MASK
    }
}

/// An id-shard wire format: how stage 1 writes a record and stage 2 reads
/// it back. Each format is its own type, so the emission loop and the join
/// are compiled once per format and the packed path carries no branch for
/// the wide one.
pub(super) trait ShardCodec: Copy + Send + Sync + 'static {
    type Record: ShardRecord;
    /// Whether the layout this format serves is a cut layout
    /// ([`keys::NodeKeyLayout::locate_wide`]).
    const WIDE: bool;
    /// Encode one record into `out`, returning the bytes used.
    fn encode(
        self,
        local: u64,
        closure: bool,
        blob_idx: u32,
        slot: u32,
        out: &mut [u8; 2 * ID_WORD_SIZE],
    ) -> Result<usize>;
    /// Decode one shard file's bytes, appending its records to `out`.
    fn decode_into(self, bytes: &[u8], out: &mut Vec<Self::Record>) -> Result<()>;
}

/// The run's id-shard format, chosen once from the key layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShardFormat {
    Packed(IdCodec),
    Wide(WideCodec),
}

impl ShardFormat {
    /// The format `layout` needs for `way_blobs` way blobs.
    fn for_layout(layout: &keys::NodeKeyLayout, way_blobs: u64) -> Result<Self> {
        if layout.wide() {
            Ok(Self::Wide(WideCodec))
        } else {
            Ok(Self::Packed(IdCodec::new(layout.max_local(), way_blobs)?))
        }
    }
}

/// Size of one id-shard word. A packed record is one word, or two when its
/// slot escapes (see [`IdCodec`]); a wide record is always two.
pub(super) const ID_WORD_SIZE: usize = size_of::<u64>();

/// The packed id-shard wire format, sized once per run.
///
/// Pass A is bound by scratch writes (149 GB of id shards at planet in the
/// old 12-byte form, more than four times the bench host's RAM), so how
/// long it takes under a degraded drive state scales with the bytes per
/// reference. A record is packed into one little-endian `u64` word, high to
/// low: `local_node_id` in `local_bits`, `blob_idx` in `blob_bits`,
/// `blob_local_slot` in `slot_bits`, then the closure bit. The widths come
/// from what the run can produce: `local_bits` from
/// the layout's largest local key, `blob_bits` from the way-blob count, and the
/// slot gets what is left of the 63 bits. No synchronisation between pass A
/// workers is needed, because each field is known to the worker decoding the
/// blob.
///
/// The slot width is not a bound on refs per blob: a slot that does not fit
/// (`slot >= 2^slot_bits - 1`, every slot when `slot_bits` is 0) writes an
/// all-ones slot field and a second word holding the full `u32` slot. At
/// planet the fields are 26, 18 and 19 bits, so a blob escapes only past
/// 524,286 refs; any input stays decodable, at worst at 16 bytes a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IdCodec {
    local_bits: u32,
    blob_bits: u32,
    slot_bits: u32,
}

/// Bits needed to hold every value in `0..=max` (at least 1, so no shift
/// ever reaches 64).
fn field_bits(max: u64) -> u32 {
    (u64::BITS - max.leading_zeros()).max(1)
}

impl IdCodec {
    /// The codec for local ids up to `max_local` and `way_blobs` way blobs.
    pub(super) fn new(max_local: u64, way_blobs: u64) -> Result<Self> {
        let local_bits = field_bits(max_local);
        let blob_bits = field_bits(way_blobs.saturating_sub(1));
        if local_bits > 31 || blob_bits > 32 {
            return Err(crate::Error::internal(format!(
                "external join id codec: local id max {max_local} or way blob count \
                 {way_blobs} out of the range the layout and schedule admit"
            )));
        }
        Ok(Self {
            local_bits,
            blob_bits,
            slot_bits: 63 - local_bits - blob_bits,
        })
    }

    /// Width of the direct slot field.
    pub(super) fn slot_bits(self) -> u32 {
        self.slot_bits
    }

    /// The all-ones slot field marking an escaped record (0 when the slot
    /// has no bits, where every record escapes).
    fn slot_escape(self) -> u64 {
        (1u64 << self.slot_bits) - 1
    }

    /// Encode `rec` into `out`, returning the bytes used (8 or 16).
    #[cfg(test)]
    fn encode_record(self, rec: &IdRecord, out: &mut [u8; 2 * ID_WORD_SIZE]) -> Result<usize> {
        ShardCodec::encode(
            self,
            rec.local(),
            rec.closure(),
            rec.blob_idx,
            rec.blob_local_slot,
            out,
        )
    }
}

impl ShardCodec for IdCodec {
    type Record = IdRecord;
    const WIDE: bool = false;

    /// Pass A's ids come from the layout and its ordinals from the schedule
    /// this codec was sized from, so neither can overflow its field; a value
    /// that does is refused rather than written into a neighbouring field.
    #[inline]
    fn encode(
        self,
        local: u64,
        closure: bool,
        blob_idx: u32,
        slot: u32,
        out: &mut [u8; 2 * ID_WORD_SIZE],
    ) -> Result<usize> {
        if local >> self.local_bits != 0 || u64::from(blob_idx) >> self.blob_bits != 0 {
            return Err(crate::Error::internal(format!(
                "external join id codec: record (local {local}, blob {blob_idx}) does not \
                 fit {self:?}"
            )));
        }
        let slot = u64::from(slot);
        let escape = self.slot_escape();
        let escaped = slot >= escape;
        let slot_field = if escaped { escape } else { slot };
        let word = (local << (self.blob_bits + self.slot_bits + 1))
            | (u64::from(blob_idx) << (self.slot_bits + 1))
            | (slot_field << 1)
            | u64::from(closure);
        out[..ID_WORD_SIZE].copy_from_slice(&word.to_le_bytes());
        if escaped {
            out[ID_WORD_SIZE..].copy_from_slice(&slot.to_le_bytes());
            Ok(2 * ID_WORD_SIZE)
        } else {
            Ok(ID_WORD_SIZE)
        }
    }

    /// Each file is checked on its own, so a truncated tail in one shard
    /// cannot borrow the next shard's first word: the length must be whole
    /// words and the last record must not be a dangling escape. An
    /// extension word wider than `u32` is refused, never truncated.
    fn decode_into(self, bytes: &[u8], out: &mut Vec<IdRecord>) -> Result<()> {
        let (words, rem) = bytes.as_chunks::<ID_WORD_SIZE>();
        if !rem.is_empty() {
            return Err(crate::Error::internal(format!(
                "id shard length {} is not a multiple of {ID_WORD_SIZE}",
                bytes.len()
            )));
        }
        let local_shift = self.blob_bits + self.slot_bits + 1;
        let blob_mask = (1u64 << self.blob_bits) - 1;
        let escape = self.slot_escape();
        let mut words = words.iter();
        while let Some(w) = words.next() {
            let word = u64::from_le_bytes(*w);
            let slot_field = (word >> 1) & escape;
            let slot = if slot_field == escape {
                let Some(ext) = words.next() else {
                    return Err(crate::Error::internal(
                        "id shard ends inside an escaped record",
                    ));
                };
                let ext = u64::from_le_bytes(*ext);
                u32::try_from(ext).map_err(|_| {
                    crate::Error::internal(format!("id shard escaped slot {ext} is wider than u32"))
                })?
            } else {
                // A direct slot was written from a u32; a wider field
                // (possible when slot_bits > 32) is a corrupt shard.
                u32::try_from(slot_field).map_err(|_| {
                    crate::Error::internal(format!("id shard slot {slot_field} is wider than u32"))
                })?
            };
            #[allow(clippy::cast_possible_truncation)]
            let local = (word >> local_shift) as u32;
            #[allow(clippy::cast_possible_truncation)]
            let blob_idx = ((word >> (self.slot_bits + 1)) & blob_mask) as u32;
            out.push(IdRecord {
                local_node_id: local | if word & 1 == 1 { CLOSURE_FLAG } else { 0 },
                blob_idx,
                blob_local_slot: slot,
            });
        }
        Ok(())
    }
}

/// The wide id-shard wire format: two little-endian `u64` words per
/// record, the whole bucket-local key, then `blob_idx << 32 | slot` with the
/// closure flag in the slot's top bit (see [`WideIdRecord`]). Used only by
/// runs whose node keys the packed word cannot hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WideCodec;

impl ShardCodec for WideCodec {
    type Record = WideIdRecord;
    const WIDE: bool = true;

    #[inline]
    fn encode(
        self,
        local: u64,
        closure: bool,
        blob_idx: u32,
        slot: u32,
        out: &mut [u8; 2 * ID_WORD_SIZE],
    ) -> Result<usize> {
        if slot & CLOSURE_FLAG != 0 {
            return Err(crate::Error::internal(format!(
                "external join wide id codec: blob-local slot {slot} is past any way blob's \
                 32 MiB payload"
            )));
        }
        let flagged = if closure { slot | CLOSURE_FLAG } else { slot };
        out[..ID_WORD_SIZE].copy_from_slice(&local.to_le_bytes());
        let tail = (u64::from(blob_idx) << 32) | u64::from(flagged);
        out[ID_WORD_SIZE..].copy_from_slice(&tail.to_le_bytes());
        Ok(2 * ID_WORD_SIZE)
    }

    fn decode_into(self, bytes: &[u8], out: &mut Vec<WideIdRecord>) -> Result<()> {
        let (records, rem) = bytes.as_chunks::<{ 2 * ID_WORD_SIZE }>();
        if !rem.is_empty() {
            return Err(crate::Error::internal(format!(
                "wide id shard length {} is not a multiple of {}",
                bytes.len(),
                2 * ID_WORD_SIZE
            )));
        }
        out.extend(records.iter().map(|r| {
            let mut local = [0u8; ID_WORD_SIZE];
            local.copy_from_slice(&r[..ID_WORD_SIZE]);
            let mut tail = [0u8; ID_WORD_SIZE];
            tail.copy_from_slice(&r[ID_WORD_SIZE..]);
            let tail = u64::from_le_bytes(tail);
            #[allow(clippy::cast_possible_truncation)]
            WideIdRecord {
                local: u64::from_le_bytes(local),
                blob_idx: (tail >> 32) as u32,
                slot: tail as u32,
            }
        }));
        Ok(())
    }
}

#[cfg(test)]
mod id_record_tests {
    use super::{
        CLOSURE_FLAG, ID_WORD_SIZE, IdCodec, IdRecord, ShardCodec, ShardRecord, WideCodec,
        WideIdRecord, keys,
    };
    use crate::test_support::assert_err_kind;

    fn rec(local_node_id: u32, blob_idx: u32, blob_local_slot: u32) -> IdRecord {
        IdRecord {
            local_node_id,
            blob_idx,
            blob_local_slot,
        }
    }

    /// Encode `records` into one shard's bytes.
    fn shard(codec: IdCodec, records: &[IdRecord]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 2 * ID_WORD_SIZE];
        for r in records {
            let n = codec.encode_record(r, &mut buf).expect("fits");
            bytes.extend_from_slice(&buf[..n]);
        }
        bytes
    }

    fn round_trip(codec: IdCodec, records: &[IdRecord]) -> Vec<u8> {
        let bytes = shard(codec, records);
        let mut out = Vec::new();
        codec.decode_into(&bytes, &mut out).expect("decode");
        assert_eq!(out, records, "{codec:?}");
        bytes
    }

    fn max_local(max_key: u64) -> u64 {
        keys::FamilyLayout::equal(0, 256, max_key).max_local()
    }

    #[test]
    fn planet_widths_pack_one_word_per_record() {
        let max = max_local(14_000_000_000);
        let codec = IdCodec::new(max, 146_000).expect("codec");
        assert_eq!(
            codec,
            IdCodec {
                local_bits: 26,
                blob_bits: 18,
                slot_bits: 19
            }
        );
        let max = u32::try_from(max).expect("u32");
        let records = [
            rec(0, 0, 0),
            rec(max, 145_999, 524_286),
            rec(max | CLOSURE_FLAG, 7, 9),
        ];
        assert_eq!(round_trip(codec, &records).len(), 3 * ID_WORD_SIZE);
    }

    #[test]
    fn slots_from_the_escape_value_up_take_a_second_word() {
        let codec = IdCodec::new(1000, 4).expect("codec");
        // slot_bits is 51 here, so every u32 slot fits directly.
        assert_eq!(codec.slot_bits, 51);
        let records = [rec(5, 3, u32::MAX - 1), rec(5 | CLOSURE_FLAG, 3, u32::MAX)];
        assert_eq!(round_trip(codec, &records).len(), 2 * ID_WORD_SIZE);

        let tight = IdCodec {
            local_bits: 31,
            blob_bits: 30,
            slot_bits: 2,
        };
        let records = [
            rec(1, 1, 0),
            rec(1, 1, 2),
            rec(1, 1, 3),
            rec(0x7fff_ffff | CLOSURE_FLAG, (1 << 30) - 1, u32::MAX),
        ];
        assert_eq!(round_trip(tight, &records).len(), 6 * ID_WORD_SIZE);
    }

    #[test]
    fn widest_packed_fields_leave_no_slot_bits_and_escape_every_record() {
        // max_id 2^38 is the largest the packed layout takes; its last
        // bucket's local id is 2^30, which needs 31 bits.
        assert_eq!(max_local(1 << 38), 1 << 30);
        let codec = IdCodec::new(1 << 30, 1 << 32).expect("codec");
        assert_eq!(codec.slot_bits, 0);
        let records = [rec(1 << 30, u32::MAX, 0), rec(0, 0, u32::MAX)];
        assert_eq!(round_trip(codec, &records).len(), 4 * ID_WORD_SIZE);
    }

    #[test]
    fn exact_bucket_boundary_and_tiny_schedules() {
        // max_id 256 over 256 buckets: width 1, but node 256 is local id 1.
        assert_eq!(max_local(256), 1);
        for blobs in [0, 1, 2] {
            let codec = IdCodec::new(1, blobs).expect("codec");
            round_trip(codec, &[rec(1, 0, 0), rec(CLOSURE_FLAG, 0, 70_000)]);
        }
    }

    #[test]
    fn a_field_past_its_width_is_refused() {
        let codec = IdCodec::new(3, 2).expect("codec");
        let mut buf = [0u8; 2 * ID_WORD_SIZE];
        assert_err_kind!(
            codec.encode_record(&rec(4, 0, 0), &mut buf),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("does not fit"),
        );
        assert_err_kind!(
            codec.encode_record(&rec(0, 2, 0), &mut buf),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("does not fit"),
        );
        assert_err_kind!(
            codec.encode(1 << 40, false, 0, 0, &mut buf),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("does not fit"),
        );
        assert_err_kind!(
            IdCodec::new(1 << 31, 1),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("out of the range"),
        );
        assert_err_kind!(
            IdCodec::new(1, (1 << 32) + 1),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("out of the range"),
        );
    }

    #[test]
    fn a_shard_is_checked_on_its_own() {
        let tight = IdCodec {
            local_bits: 31,
            blob_bits: 30,
            slot_bits: 2,
        };
        let bytes = shard(tight, &[rec(1, 1, 0), rec(1, 1, 9)]);
        let mut out = Vec::new();
        // Whole words, but the escape's extension word is missing.
        assert_err_kind!(
            tight.decode_into(&bytes[..2 * ID_WORD_SIZE], &mut out),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("inside an escaped record")
        );
        assert_err_kind!(
            tight.decode_into(&bytes[..5], &mut out),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("not a multiple of"),
        );
        // An extension word wider than u32.
        let mut wide = bytes.clone();
        wide[2 * ID_WORD_SIZE..].copy_from_slice(&(1u64 << 32).to_le_bytes());
        assert_err_kind!(
            tight.decode_into(&wide, &mut out),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("wider than u32"),
        );
    }

    #[test]
    fn wide_records_carry_the_whole_key_and_the_closure_flag() {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 2 * ID_WORD_SIZE];
        for (local, closure, blob, slot) in [
            (0, false, 0, 0),
            (1u64 << 63, true, u32::MAX, (1 << 25) - 1),
            (u64::MAX >> 1, false, 7, 9),
        ] {
            let n = WideCodec
                .encode(local, closure, blob, slot, &mut buf)
                .expect("fits");
            assert_eq!(n, 2 * ID_WORD_SIZE);
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut out: Vec<WideIdRecord> = Vec::new();
        WideCodec.decode_into(&bytes, &mut out).expect("decode");
        let seen: Vec<_> = out
            .iter()
            .map(|r| (r.local(), r.closure(), r.blob_idx(), r.blob_local_slot()))
            .collect();
        assert_eq!(
            seen,
            [
                (0, false, 0, 0),
                (1 << 63, true, u32::MAX, (1 << 25) - 1),
                (u64::MAX >> 1, false, 7, 9),
            ]
        );
        assert_err_kind!(
            WideCodec.encode(0, false, 0, CLOSURE_FLAG, &mut buf),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("past any way blob")
        );
        assert_err_kind!(
            WideCodec.decode_into(&bytes[..24], &mut out),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("not a multiple of"),
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
// widths can use the full u32 range; `validate_slot_layout` refuses a
// layout whose widest bucket a u32 local slot cannot cover, once per run,
// so `write_to`'s narrowing cast cannot wrap). coord_pack's presence bit serves rank
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
        // Lossless: `validate_slot_layout` refused any layout whose widest
        // bucket holds more than 2^32 slots before stage 2 began.
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
    /// floor in `external_join_in`) preserves the 2-piece straddler
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

/// Validate before stage 2 writes slot buckets. Each nonempty blob is no
/// wider than the smallest bucket, so it intersects at most two adjacent
/// buckets. Empty blobs publish no straddler half, including at total_slots.
///
/// The widest bucket (the last, which absorbs the remainder) must also be
/// encodable: [`ResolvedEntry::write_to`] stores a bucket-local slot as
/// `u32`, so a bucket of more than 2^32 slots would wrap a local slot into
/// another way's position with no later check able to tell. With 256
/// buckets that is about 2^40 way node refs in total, some 88 times planet.
/// The total is the way pass's real ref count, but highly compressible
/// refs reach it in a file far smaller than that, so it is an input limit
/// of this backend, refused like [`BucketLayout::new`]'s. Stage 3 then
/// sizes one bucket's scatter buffer (`width * COORD_SLOT_SIZE` bytes) and
/// its entry buffer (at most one `RESOLVED_ENTRY_SIZE` record per slot),
/// and indexes the scatter buffer by local slot: both sizes are checked
/// here to be representable allocations on this target, so stage 3's
/// `usize` arithmetic on them cannot wrap.
fn validate_slot_layout(starts: &[u64], total: u64, buckets: usize) -> Result<()> {
    if buckets == 0 || starts.first().is_some_and(|&start| start != 0) {
        return Err(crate::Error::internal(
            "external join: invalid slot bucket layout",
        ));
    }
    let (last_start, last_end) = slot_bucket_bounds(total, buckets, buckets - 1);
    let widest = last_end - last_start;
    if widest > 1u64 << 32 {
        return Err(crate::Error::invalid_data(format!(
            "add-locations-to-ways, external index: the input's ways carry {total} node \
             refs, which split over {buckets} slot buckets put {widest} in the widest, \
             above the {} a bucket of this backend can encode. The sparse index has no \
             such limit.",
            1u64 << 32,
        )));
    }
    let representable = |record: usize| {
        usize::try_from(widest)
            .ok()
            .and_then(|w| w.checked_mul(record))
            .is_some_and(|bytes| isize::try_from(bytes).is_ok())
    };
    if !representable(COORD_SLOT_SIZE) || !representable(RESOLVED_ENTRY_SIZE) {
        return Err(crate::Error::invalid_data(format!(
            "add-locations-to-ways, external index: the input's ways carry {total} node \
             refs, which split over {buckets} slot buckets put {widest} in the widest, \
             more than one buffer on this platform can hold. The sparse index has no \
             such limit.",
        )));
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
    use crate::test_support::assert_err_kind;

    #[test]
    fn checks_straddler_bound_before_bucket_writes() {
        assert!(validate_slot_layout(&[0, 3, 6, 9], 12, 4).is_ok());
        assert_err_kind!(
            validate_slot_layout(&[0, 4], 12, 4),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("straddler bound"),
        );
        assert_err_kind!(
            validate_slot_layout(&[0, 6, 3], 12, 2),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("not monotonic"),
        );
        assert!(validate_slot_layout(&[0, 0], 0, 256).is_ok());
        assert!(validate_slot_layout(&[], 0, 256).is_ok());
        assert_err_kind!(
            validate_slot_layout(&[0], 1, 0),
            crate::ErrorKind::Internal { msg, .. } if msg.contains("invalid slot bucket layout"),
        );
    }

    /// Blob starts every 2^31 slots, well under every bucket width below, so
    /// only the width check can refuse.
    #[cfg(target_pointer_width = "64")]
    fn starts_for(total: u64) -> Vec<u64> {
        (0..total.div_ceil(1 << 31)).map(|i| i << 31).collect()
    }

    /// The widest bucket is the last, `total / 256 + total % 256` slots, and
    /// its bucket-local slots must fit `u32`. The limit is not monotone in
    /// the total: the remainder can push a smaller total over it.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn widest_slot_bucket_must_fit_a_u32_local_slot() {
        // Widest 2^32 - 1, and exactly 2^32: both encodable.
        for total in [(1u64 << 40) - 256, 1 << 40] {
            assert!(
                validate_slot_layout(&starts_for(total), total, 256).is_ok(),
                "{total} slots"
            );
        }
        // Widest 2^32 + 1, from a smaller total than the accepted 2^40.
        let total = (1u64 << 40) - 254;
        let err = validate_slot_layout(&starts_for(total), total, 256)
            .expect_err("a bucket past 2^32 slots");
        assert!(
            matches!(err.kind(), crate::ErrorKind::InvalidData { .. }),
            "an input limit, not an internal error: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("{total} node refs"))
                && msg.contains(&((1u64 << 32) + 1).to_string())
                && msg.contains("sparse index"),
            "{msg}"
        );
        // One bucket: the whole total is the widest bucket.
        let total = (1u64 << 32) + 1;
        assert_err_kind!(
            validate_slot_layout(&starts_for(total), total, 1),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("above the"),
        );
    }
}

/// The external join for add-locations-to-ways, writing to a borrowed output
/// it does not publish, in scratch the caller owns.
///
/// Bounded working memory and sequential I/O. Temporary-disk demand depends
/// on the input; see `reference/performance/add-locations-to-ways.md` for
/// measured resource use and the module docs for the algorithm.
///
/// There is deliberately no public entry point of its own: the external
/// backend is reached only through
/// [`add_locations_to_ways`](super::add_locations_to_ways) with
/// [`IndexType::External`](super::IndexType::External) (or `Auto`), so
/// backend selection, option validation and the missing-location policy
/// have one owner and no route can publish `(0, 0)` placeholders the caller
/// did not ask for. The policy is applied here, right after stage 2: the
/// missing count is `total_slots - resolved_count`, and stages 3 and 4 only
/// scatter and encode the entries stage 2 resolved, so neither can change
/// it. Refusing there skips the coordinate encoding, way assembly and
/// output write a refused run would otherwise finish and throw away.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(crate) fn external_join_in(
    input: &PinnedInput,
    mut front: InputFront<'_>,
    output: &crate::output::BoundOutput,
    options: &super::AltwOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
) -> Result<Stats> {
    let (keep_untagged_nodes, write, inject_prepass) = (
        options.keep_untagged_nodes,
        &options.write,
        options.inject_prepass,
    );
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

    // The node-key layout for record emission and the join, from the node
    // blobs' claims (and the decoded split of a blob holding both key
    // families): tight for the actual input, not a planet-wide constant.
    let plan = keys::KeyPlan::new(&keys::NodeClaims::of(&blob_meta));
    let node_spans = keys::NodeSpans::build(&blob_meta, &plan)?;
    let bucket_layout = keys::NodeKeyLayout::new(&plan, &node_spans);
    host.invocation
        .counter("extjoin_bucket_max_node_id", bucket_layout.pos.max_key());
    host.invocation
        .counter("extjoin_bucket_width", bucket_layout.pos.width());
    host.invocation
        .counter("extjoin_neg_buckets", bucket_layout.neg.buckets());
    host.invocation
        .counter("extjoin_neg_max_key", bucket_layout.neg.max_key());
    host.invocation
        .counter("extjoin_id_shard_wide", bucket_layout.wide());

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
    let (stage1_out, relation_member_node_ids) = crate::threads::scope(
        &host.invocation,
        |scope| -> Result<(
            super::external::stage1::Stage1Output,
            Option<relation_scan::RelationScanOutput>,
        )> {
            // Stage 1 runs on this thread and the relation scan, when there
            // is one, on the scope's only spawned thread, started first. A
            // refusal of that spawn therefore leaves nothing born and nothing
            // run, and the no-scan case starts no thread at all.
            //
            // The scan's span nests inside EXTJOIN_S1_PASS_A and ends well
            // before it. A marker-segmented sidecar view labels the segment
            // that begins at EXTJOIN_RELATION_SCAN_END with that marker, so
            // that row measures the rest of pass A, not the scan; read the
            // scan's own time from the START/END pair. Wall swings seen in
            // this stage at planet scale have been in pass A, with the scan
            // steady (reference/performance/add-locations-to-ways.md, "altw external pass A swings
            // with run history").
            let rel_handle = if keep_untagged_nodes && !inject_prepass {
                None
            } else {
                host.invocation.marker("EXTJOIN_RELATION_SCAN_START");
                let spawned = scope.spawn(move || {
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
                });
                Some(spawned.map_err(|e| {
                    crate::Error::from(e).context("spawn the external relation scan")
                })?)
            };

            // Error ordering: a stage-1 failure returns here before the
            // relation-scan handle is joined, but the scope still waits for
            // the scan to run to completion before it returns. A stage-1
            // failure while the relation scan is running therefore delays
            // error reporting until the relation scan completes. Accepted
            // as diagnostic-quality: stage-1 failures are rare (only fire on
            // adversarial or malformed input, mostly closed by the
            // defensive-input checks in ADR-0004). A cancellation is not
            // delayed: both threads observe the invocation's token, the
            // relation scan per blob.
            let s1_res = stage1_way_pass(
                blob_meta_ref_parallel,
                input_ref_parallel,
                direct_io,
                scratch,
                bucket_layout_ref,
                &ref_count_sidecar,
                &per_way_refcount_sidecar,
                inject_prepass,
                host,
            )?;
            let rel_res = match rel_handle {
                Some(handle) => Some(handle.join().map_err(|payload| {
                    crate::error::panic_error("relation scan thread", payload)
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
    let stage1_out_format = stage1_out.format;
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
    // Plain, never RANDOM: each stage-2 worker preads its bucket's run of
    // node blobs in file order, which kernel readahead pipelines; RANDOM
    // tripled the summed pread time on europe (see
    // `InputSource::open_random_access`).
    let input_pbf = crate::read::input::InputFile::open(input)?;
    let (resolved_count, slot_bucket_manifest) = stage2_node_join(
        scratch,
        &id_bucket_counts,
        num_shard_workers,
        stage1_out_format,
        bucket_layout_ref,
        &way_slot_starts,
        slot_buckets,
        slot_bucket_count,
        total_slots,
        &input_pbf,
        &node_blob_mapping,
        &node_spans,
        inject_prepass,
        host,
    )?;
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

    // `resolved_count` counts slots whose node was found in the id-sorted
    // tuple stream - presence, not a non-zero coordinate. It used to count
    // `!= (0, 0)`, which reported every genuine node at Null Island as
    // missing and so made this backend disagree with the sparse one about
    // the same input. Absence is not zero; see the counting site in
    // `stage2.rs`. Final here: stage 2 checked it against its manifest, and
    // the later stages resolve nothing.
    let missing_locations = total_slots.saturating_sub(resolved_count);
    host.invocation.check()?;
    super::enforce_missing_locations_policy(missing_locations, options.ignore_missing_nodes)?;

    // Free the per-blob mapping - it was a stage 2 input only.
    drop(node_blob_mapping);

    // Prepare inputs for the streaming stage 3 + stage 4 handoff.
    let per_way_rcs = coord_payloads::load_per_way_refcount_sidecar_indexed(
        &per_way_refcount_sidecar,
        way_slot_starts.len(),
    )?;
    host.invocation.counter("s3_worker_count", num_workers);

    // Worker tmp files opened once here with read + write, wrapped in
    // Arc<ScratchFile> so stage 3 can `write_all_at` and stage 4 can
    // `read_exact_at` on the same descriptor. `File` is `Sync` on Unix for
    // pread/pwrite so no extra locking is needed.
    let worker_files: Vec<coord_payloads::PayloadFile> = (0..num_workers)
        .map(|i| {
            let path = radix::payload_path(scratch, i)?;
            let file = crate::scratch::create_read_write(&path, false).map_err(|e| {
                crate::Error::from(e).context(format!(
                    "failed to create stage 3 payload file {}",
                    path.display()
                ))
            })?;
            Ok(coord_payloads::PayloadFile {
                file: std::sync::Arc::new(file),
                path,
            })
        })
        .collect::<crate::Result<_>>()?;

    // Stage 3's per-bucket counts are stage 2's committed counts, never the
    // files' observed lengths (see `SharedSlotBuckets::into_manifest`).
    // Slot-bucket balance twin of the id-bucket counters above; min is
    // over nonempty buckets.
    {
        let counts = || slot_bucket_manifest.buckets.iter().map(|&(_, n)| n);
        let max_entries: u64 = counts().max().unwrap_or(0);
        let min_entries: u64 = counts().filter(|&c| c > 0).min().unwrap_or(0);
        let nonempty = counts().filter(|&c| c > 0).count();
        host.invocation
            .counter("extjoin_slot_bucket_max_entries", max_entries);
        host.invocation
            .counter("extjoin_slot_bucket_min_entries", min_entries);
        host.invocation
            .counter("extjoin_slot_buckets_nonempty", nonempty);
    }

    // The streaming router: pre-populates `Empty` entries for zero-ref
    // way blobs so stage 4 never waits on a blob that stage 3 would
    // never publish.
    let router = coord_payloads::ConcurrentBlobLocationRouter::new(
        &per_way_rcs,
        worker_files,
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
    let slot_bucket_ref_ref = &slot_bucket_manifest;
    let inject_metrics = super::inject_metrics::InjectMetrics::default();
    let inject_metrics_ref = &inject_metrics;

    // Both stage threads return the typed crate::Error unchanged, so a
    // PlatformUnsupported raised by the output writer in stage 4 keeps its
    // kind (and exit code) across the join.
    let mut stats = crate::threads::scope(&host.invocation, |scope| -> crate::Result<Stats> {
        // Stage 3 is the router's only producer, so its parent owns the
        // router's terminal state for every way it can end: a returned error
        // aborts, success marks the producer done, and an unwind (anything
        // that escapes `stage3_slot_reorder`, which cannot install its
        // workers' guards before they exist) aborts through the guard.
        // Without one of the three a stage 4 readiness wait never ends.
        let s3_spawned = scope.spawn(move || -> crate::Result<()> {
            let guard = coord_payloads::AbortOnDrop::new(router_ref, "stage 3");
            let integrated = IntegratedInputs {
                way_slot_starts: way_slot_starts_ref,
                per_way_rcs: per_way_rcs_ref,
                router: router_ref,
                inject_prepass,
            };
            let result = stage3_slot_reorder(
                slot_bucket_ref_ref,
                total_slots,
                &integrated,
                num_workers,
                &host.invocation,
            );
            // An abort already raised keeps its first error; done after an
            // abort is harmless (waiters check the abort first).
            if let Err(error) = &result {
                router_ref.abort(error.clone());
            }
            router_ref.mark_producer_done();
            guard.disarm();
            result
        });
        let s3_handle = match s3_spawned {
            Ok(handle) => handle,
            // Stage 4 does not exist yet, so nothing waits on the router.
            Err(e) => return Err(crate::Error::from(e).context("spawn stage 3 thread")),
        };
        // A stage 4 failure aborts the router, so stage 3 stops at its next
        // bucket claim instead of finishing the rest for nobody: on a returned
        // error here, and through the guard on a panic. Failures inside the
        // ordered pool abort earlier, before the pool joins its workers (see
        // `S4Consumer` and the worker guards in `stage4_assembly`); this covers everything else (the schedule, the
        // writer, the final flush). An abort already raised keeps its first
        // error.
        let s4_spawned = scope.spawn(move || -> crate::Result<Stats> {
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

        // Every started parent is joined explicitly and flattened to a
        // Result before any error is chosen: an early return here would
        // leave a parent to the scope's join pass, which re-raises its panic
        // over the error this body returns.
        let join_s3 = |handle: crate::threads::Handle<'_, '_, '_, crate::Result<()>>| {
            handle
                .join()
                .map_err(|payload| crate::error::panic_error("stage 3 thread", payload))
                .and_then(|r| r)
        };
        let s4_handle = match s4_spawned {
            Ok(handle) => handle,
            Err(e) => {
                // Stage 3 stops at its next bucket claim. A startup priority
                // rule, not a root-cause claim: the refusal is returned even
                // if stage 3 recorded an error of its own meanwhile.
                let e = crate::Error::from(e).context("spawn stage 4 thread");
                router_ref.abort(e.clone());
                drop(join_s3(s3_handle));
                return Err(e);
            }
        };
        let s3_res = join_s3(s3_handle);
        let s4_res = s4_handle
            .join()
            .map_err(|payload| crate::error::panic_error("stage 4 thread", payload))
            .and_then(|r| r);

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

    stats.missing_locations = missing_locations;

    if inject_prepass {
        let member_ways = member_way_ids_ref.map_or(0, crate::idset::IdSet::len);
        inject_metrics.emit(member_ways, &host.invocation);
    }

    Ok(stats)
}
