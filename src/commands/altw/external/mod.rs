//! External join for add-locations-to-ways: bounded-memory coordinate resolution
//! via double radix permutation.
//!
//! Instead of building a giant random-access node index (16 GB mmap at planet
//! scale), this module pre-computes the way-node join using sequential I/O and
//! bounded memory:
//!
//! 1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into
//!    256 node buckets partitioned by high bits of node_id.
//! 2. **Node join**: per bucket, sort pairs by node_id in RAM (~500 MB),
//!    merge-join with matching node stream, emit `(slot_pos, lat, lon)` into
//!    256 slot buckets partitioned by high bits of slot_pos.
//! 3. **Slot reorder**: per bucket, sort by slot_pos, emit final blob-ordered
//!    delta-varint `coord_payloads` (see `coord_payloads.rs`). The flat
//!    `coord_slots` array is a historical intermediate, retired in 2026-04.
//! 4. **Assembly**: stream original PBF + per-blob coord_payloads preads,
//!    emit enriched ways.
//!
//! Memory at every stage: <1 GB. All I/O sequential. No mmap, no random access.

use std::path::Path;

use crate::ElementReader;
use crate::writer::Compression;

use super::Stats;
use crate::BoxResult as Result;
use crate::commands::{HeaderOverrides, require_indexdata};

mod blob_bucket_index;
mod blob_meta;
mod coord_payloads;
mod radix;
mod relation_scan;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

// Under the `test-hooks` feature, expose the stage 3 bucket-panic
// hook so integration tests can arm it. Other stages can add
// sibling re-exports here as they grow hooks.
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    pub use super::stage3::test_hooks as stage3;
}

use radix::{NUM_BUCKETS, ScratchDir};

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
/// u32`. [`BucketLayout::new`] asserts `bucket_width <= u32::MAX` so
/// the cast is lossless for any `node_id` inside `[0, max_node_id]`. At
/// planet (max_node_id ~= 14e9, 256 buckets) `bucket_width ~= 55M`,
/// six orders of magnitude under 2^32.
#[allow(dead_code)] // exposed for testing; production callers use BucketLayout.
pub(super) fn bucket_width(max_node_id: u64, num_buckets: usize) -> u64 {
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
/// `tests/cli_add_locations_to_ways.rs` for the contract canary.
#[allow(dead_code)] // wired up in step 2 of A1 (pass-A IdRecord emission).
pub(super) struct BucketLayout {
    pub(super) max_node_id: u64,
    pub(super) bucket_width: u64,
    pub(super) num_buckets: usize,
}

#[allow(dead_code)] // wired up in step 2 of A1 (pass-A IdRecord emission).
impl BucketLayout {
    pub(super) fn new(max_node_id: u64, num_buckets: usize) -> Self {
        assert!(num_buckets > 0, "num_buckets must be > 0");
        let bw = bucket_width(max_node_id, num_buckets);
        assert!(
            bw <= (1_u64 << 30),
            "altw external: bucket_width {bw} exceeds 1<<30 \
             (max_node_id={max_node_id}, num_buckets={num_buckets}); \
             local_node_id would no longer fit in u32",
        );
        Self {
            max_node_id,
            bucket_width: bw,
            num_buckets,
        }
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
/// This is the input to [`BucketLayout::new`]. Errors if any node
/// blob has a malformed range (negative `max_id` or `max_id <
/// min_id`); both are producer-side bugs that pass A would otherwise
/// silently absorb. Returns 0 when there are no node blobs (degenerate
/// input - bucket_layout will then reject any non-zero ref).
#[allow(dead_code, private_interfaces)] // wired up in step 2 of A1.
pub(super) fn max_node_id_from_blob_meta(blob_meta: &[blob_meta::BlobMeta]) -> Result<u64> {
    let mut max_id: i64 = 0;
    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_meta::ElemKind::Node) {
            continue;
        }
        if meta.max_id < meta.min_id {
            return Err(format!(
                "altw external: node blob at data_offset={} has reversed \
                 indexdata range [min_id={}, max_id={}]",
                meta.data_offset, meta.min_id, meta.max_id,
            )
            .into());
        }
        if meta.max_id < 0 {
            return Err(format!(
                "altw external: node blob at data_offset={} has negative \
                 max_id={}; pbfhogg rejects negative element ids",
                meta.data_offset, meta.max_id,
            )
            .into());
        }
        if meta.max_id > max_id {
            max_id = meta.max_id;
        }
    }
    #[allow(clippy::cast_sign_loss)]
    Ok(max_id as u64)
}

#[cfg(test)]
mod max_node_id_tests {
    use super::{blob_meta::BlobMeta, max_node_id_from_blob_meta};
    use crate::blob_meta::ElemKind;

    fn meta(kind: ElemKind, min_id: i64, max_id: i64) -> BlobMeta {
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
        assert_eq!(max_node_id_from_blob_meta(&[]).expect("ok"), 0);
    }

    #[test]
    fn no_node_blobs_is_zero() {
        let metas = [meta(ElemKind::Way, 1, 100), meta(ElemKind::Relation, 1, 50)];
        assert_eq!(max_node_id_from_blob_meta(&metas).expect("ok"), 0);
    }

    #[test]
    fn picks_max_across_node_blobs() {
        let metas = [
            meta(ElemKind::Node, 1, 999),
            meta(ElemKind::Way, 1, 5_000_000),
            meta(ElemKind::Node, 1000, 12_345),
            meta(ElemKind::Node, 12_346, 99_999),
        ];
        assert_eq!(max_node_id_from_blob_meta(&metas).expect("ok"), 99_999);
    }

    #[test]
    fn errors_on_reversed_range() {
        let metas = [meta(ElemKind::Node, 100, 50)];
        let err = max_node_id_from_blob_meta(&metas).expect_err("reversed range");
        assert!(err.to_string().contains("reversed indexdata"));
    }

    #[test]
    fn errors_on_negative_max() {
        let metas = [meta(ElemKind::Node, -10, -5)];
        let err = max_node_id_from_blob_meta(&metas).expect_err("negative max");
        assert!(err.to_string().contains("negative max_id"));
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
        let layout = BucketLayout::new(1000, 4);
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
        let layout = BucketLayout::new(1000, 4);
        assert_eq!(layout.locate(1001), None);
        assert_eq!(layout.locate(u64::MAX), None);
    }

    #[test]
    fn locate_returns_none_for_negative_refs_cast_via_u64() {
        // An i64 negative id reinterpreted as u64 lands near u64::MAX.
        // Pass A must not silently truncate it into the last bucket;
        // the layout returns None and pass A skips emission (treating
        // the slot as unresolved, same as a ref to an absent node).
        let layout = BucketLayout::new(14_000_000_000, 256);
        #[allow(clippy::cast_sign_loss)]
        let neg_as_u64 = (-1_i64) as u64;
        assert_eq!(layout.locate(neg_as_u64), None);
    }

    #[test]
    fn locate_handles_zero_max() {
        let layout = BucketLayout::new(0, 256);
        // max_node_id=0 means only node id 0 is in range.
        assert!(matches!(layout.locate(0), Some((b, _)) if b < 256));
        assert_eq!(layout.locate(1), None);
    }

    #[test]
    fn every_referenced_id_maps_to_one_bucket() {
        let max = 12_345_678u64;
        let layout = BucketLayout::new(max, 256);
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
        let layout = BucketLayout::new(10_000, 10);
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
        let layout = BucketLayout::new(10_000, 10);
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
        // BucketLayout::new asserts this; cross-check the bare fn and
        // confirm the constructor accepts the planet config.
        let bw = bucket_width(14_000_000_000, 256);
        assert!(bw <= u64::from(u32::MAX));
        let _ = BucketLayout::new(14_000_000_000, 256);
    }
}

/// Outcome of the `RLIMIT_NOFILE` self-raise attempt.
pub(super) enum FdRaiseStatus {
    /// `getrlimit` failed; caller is using the conservative fallback.
    ProbeFailed,
    /// Soft limit was already at the hard cap; nothing to raise.
    AlreadyAtCap,
    /// Soft raised from `from` up to the hard cap.
    Raised { from: u64 },
    /// Soft limit is below the hard cap but `setrlimit` rejected the
    /// raise. Rare: unprivileged soft-raise to hard cap should always
    /// succeed on Linux, but seccomp or LSM policies can block it.
    RaiseFailed,
}

/// Result of probing + raising `RLIMIT_NOFILE`. `Display` prints a
/// compact one-line summary suitable for stderr narration.
pub(super) struct FdBudget {
    /// Effective soft limit after the raise attempt, clamped to a sane
    /// ceiling if the kernel reports `RLIM_INFINITY`.
    pub(super) effective_soft: u64,
    pub(super) status: FdRaiseStatus,
}

impl std::fmt::Display for FdBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            FdRaiseStatus::ProbeFailed => write!(
                f,
                "RLIMIT_NOFILE: probe failed; assuming {}",
                self.effective_soft
            ),
            FdRaiseStatus::AlreadyAtCap => write!(
                f,
                "RLIMIT_NOFILE: {} (already at hard cap; no adjustment)",
                self.effective_soft
            ),
            FdRaiseStatus::Raised { from } => write!(
                f,
                "RLIMIT_NOFILE: {from} -> {} (soft raised to hard cap)",
                self.effective_soft
            ),
            FdRaiseStatus::RaiseFailed => write!(
                f,
                "RLIMIT_NOFILE: {} (raise to hard cap rejected; still at soft cap)",
                self.effective_soft
            ),
        }
    }
}

/// Self-raise `RLIMIT_NOFILE` soft limit to the current hard cap and
/// report both the effective soft limit and the narrative of what
/// happened.
///
/// External join's stage 1 pass B holds `num_workers * NUM_BUCKETS`
/// (up to ~4096 on a 17-core host) rank-shard files open concurrently.
/// Linux default soft ulimit is 1024 on many distros; some still cap
/// hard at 4096. Without this raise, `add-locations-to-ways
/// --index-type external` fails with EMFILE on a default-ulimit shell
/// even though the hard cap is usually high enough. The unprivileged
/// soft-raise up to hard cap is the standard pattern for servers
/// (PostgreSQL, Redis, nginx).
///
/// Returns 1024 as a conservative fallback `effective_soft` if the
/// probe itself fails, so callers can still compute a sensible cap.
pub(super) fn raise_nofile_to_hard_cap() -> FdBudget {
    // SAFETY: `getrlimit`/`setrlimit` are pure-data FFI calls. The
    // rlimit struct is fully initialised before being passed as
    // `&mut`, and we don't share it across threads.
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return FdBudget {
                effective_soft: 1024,
                status: FdRaiseStatus::ProbeFailed,
            };
        }
        let soft_before: u64 = rlim.rlim_cur;
        let hard: u64 = rlim.rlim_max;
        if soft_before >= hard {
            // `rlim_cur` can be `RLIM_INFINITY` (= `u64::MAX` on
            // Linux); clamp so downstream arithmetic doesn't overflow.
            return FdBudget {
                effective_soft: soft_before.min(1 << 30),
                status: FdRaiseStatus::AlreadyAtCap,
            };
        }
        let target = libc::rlimit {
            rlim_cur: hard,
            rlim_max: hard,
        };
        let raise_ok = libc::setrlimit(libc::RLIMIT_NOFILE, &target) == 0;
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return FdBudget {
                effective_soft: 1024,
                status: FdRaiseStatus::ProbeFailed,
            };
        }
        let effective: u64 = rlim.rlim_cur.min(1 << 30);
        if raise_ok {
            FdBudget {
                effective_soft: effective,
                status: FdRaiseStatus::Raised { from: soft_before },
            }
        } else {
            FdBudget {
                effective_soft: effective,
                status: FdRaiseStatus::RaiseFailed,
            }
        }
    }
}

/// Depth-gated stall span shared by concurrent blockers.
///
/// `brokkr sidecar --stalls` pairs `WAIT_<CATEGORY>_START/_END` markers
/// by name, which breaks if N threads emit interleaved pairs for the
/// same category. The gauge collapses concurrent blockers into one
/// non-overlapping span per busy period: START when depth goes 0 -> 1,
/// END when it returns to 0. The mutex is held only across the depth
/// transition + marker emit, and callers must gate `track()` behind a
/// try_* fast path so unblocked operations never touch it.
pub(super) struct StallGauge {
    start_marker: &'static str,
    end_marker: &'static str,
    depth: std::sync::Mutex<u64>,
}

/// RAII exit for [`StallGauge::track`]; decrements depth on drop so
/// early returns and error paths cannot leak an open WAIT span.
pub(super) struct StallGuard<'a>(&'a StallGauge);

impl StallGauge {
    pub(super) fn new(start_marker: &'static str, end_marker: &'static str) -> Self {
        Self {
            start_marker,
            end_marker,
            depth: std::sync::Mutex::new(0),
        }
    }

    /// Enter the stall; the returned guard exits it on drop.
    pub(super) fn track(&self) -> StallGuard<'_> {
        let mut depth = self
            .depth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *depth == 0 {
            crate::debug::emit_marker(self.start_marker);
        }
        *depth += 1;
        StallGuard(self)
    }
}

impl Drop for StallGuard<'_> {
    fn drop(&mut self) {
        let mut depth = self
            .0
            .depth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *depth = depth.saturating_sub(1);
        if *depth == 0 {
            crate::debug::emit_marker(self.0.end_marker);
        }
    }
}

/// Size of an id-bucketed occurrence record:
/// `(local_node_id: u32, blob_idx: u32, blob_local_slot: u32)` = 12 bytes.
pub(super) const ID_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(local_slot_pos: u32, lat: i32, lon: i32)` = 12 bytes.
pub(super) const RESOLVED_ENTRY_SIZE: usize = 12;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// Stage 1 → stage 2 hand-off describing one node blob: where it lives in
/// the input PBF and the half-open rank range `[ref_rank_start, ref_rank_end)`
/// of referenced nodes it contains.
///
/// Computed without decoding any blob - uses indexdata `(min_id, max_id)`
/// plus `IdSet::rank` queries. Adjacent blobs' ranges are
/// non-overlapping and monotonic in rank (because the input PBF is sorted
/// by node ID and rank is monotonic in ID). Each rank bucket maps to a
/// contiguous run of blobs in this vector via binary search.
#[derive(Clone, Copy, Debug)]
pub(super) struct NodeBlobInfo {
    pub data_offset: u64,
    pub data_size: usize,
    /// Indexdata-advertised id range, copied straight from blob_meta.
    /// Stage 2 partitions the slice by `[min_id, max_id]` to find
    /// blobs intersecting each id bucket; stage 2's merge walk uses
    /// `node_tuples.last().id` (the actual decoded last id) as the
    /// upper consumption bound rather than `max_id` itself, so loose
    /// indexdata can't cause silent record loss.
    pub min_id: i64,
    pub max_id: i64,
}

/// An id-bucketed occurrence record emitted by stage 1 pass A.
///
/// `(local_node_id, blob_idx, blob_local_slot)` is the parallel-safe
/// decomposition of the audit's `(local_node_id, slot_pos)` form: stage
/// 2 reconstructs `linear_slot_pos = blob_start_slot[blob_idx] +
/// blob_local_slot` from the per-blob refcount prefix-sum that pass A
/// produces in `ref_count_sidecar`. See `.plans/merry-watching-lake.md`
/// for the rationale.
#[allow(dead_code)] // wired up in step 2 (emission) and step 3 (consumption).
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

#[allow(dead_code)] // wired up in step 2 (emission) and step 3 (consumption).
impl IdRecord {
    pub(super) fn write_to(&self, buf: &mut [u8; ID_RECORD_SIZE]) {
        buf[..4].copy_from_slice(&self.local_node_id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.blob_idx.to_le_bytes());
        buf[8..12].copy_from_slice(&self.blob_local_slot.to_le_bytes());
    }

    pub(super) fn read_from(buf: &[u8; ID_RECORD_SIZE]) -> Self {
        let local_node_id = u32::from_le_bytes(buf[..4].try_into().expect("4 bytes"));
        let blob_idx = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
        let blob_local_slot = u32::from_le_bytes(buf[8..12].try_into().expect("4 bytes"));
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
        buf[..4].copy_from_slice(&local_slot_pos.to_le_bytes());
        buf[4..8].copy_from_slice(&self.lat.to_le_bytes());
        buf[8..12].copy_from_slice(&self.lon.to_le_bytes());
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
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn external_join(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    inject_prepass: bool,
) -> Result<Stats> {
    let has_indexdata = require_indexdata(
        input,
        direct_io,
        force,
        "external join requires indexdata for efficient blob filtering",
    )?;

    // External-join cannot function without indexdata: the stage-1/2
    // rank-based bucket ranges are computed from per-blob `(min_id,
    // max_id)` derived from indexdata, and `scan_blob_metadata` hard-
    // errors if any OsmData blob lacks it. Under `--force` the
    // indexdata check upstream returns `Ok(false)` instead of a clear
    // error, which previously let the command proceed and then fail
    // at `blob_meta.rs:50` with an opaque "OsmData blob missing
    // indexdata" message. Reject the combination up front with a
    // clear migration hint.
    if !has_indexdata {
        return Err(
            "add-locations-to-ways --index-type external requires indexed input; \
             --force is not supported on this path because the rank-based bucket \
             ranges depend on per-blob indexdata.\n\n\
             Generate an indexed PBF first:\n\n\
             \x20 pbfhogg cat <input.osm.pbf> -o indexed.osm.pbf\n\n\
             Then run add-locations-to-ways against the indexed output, or use \
             --index-type sparse (which decodes every blob anyway and \
             therefore tolerates --force)."
                .into(),
        );
    }

    {
        let reader = ElementReader::open(input, direct_io)?;
        if !reader.header().is_sorted() {
            return Err("external join requires a sorted PBF (Sort.Type_then_ID). \
                        The single-pass node merge depends on ascending node ID order."
                .into());
        }
    }

    let scratch_dir = ScratchDir::new(output.parent().unwrap_or(Path::new(".")), "external-join")?;

    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let per_way_refcount_sidecar = scratch_dir.file_path("per-way-refcounts");

    crate::debug::emit_marker("EXTJOIN_META_SCAN_START");
    let t_meta = std::time::Instant::now();
    let blob_meta = blob_meta::scan_blob_metadata(input, !keep_untagged_nodes)?;
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("extjoin_meta_scan_ms", t_meta.elapsed().as_millis() as i64);
        crate::debug::emit_counter("extjoin_meta_blobs", blob_meta.len() as i64);
        crate::debug::emit_counter(
            "extjoin_meta_tag_scan_enabled",
            if keep_untagged_nodes { 0 } else { 1 },
        );
    }
    crate::debug::emit_marker("EXTJOIN_META_SCAN_END");

    // Build the BucketLayout for A1 step 2's IdRecord emission. Width
    // is derived from the largest node-blob max_id in indexdata, not
    // the planet-wide constant - tighter buckets on smaller fixtures
    // and exact for any real input.
    let max_node_id = max_node_id_from_blob_meta(&blob_meta)?;
    let bucket_layout = BucketLayout::new(max_node_id, NUM_BUCKETS);
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_bucket_max_node_id", max_node_id as i64);
        crate::debug::emit_counter("extjoin_bucket_width", bucket_layout.bucket_width as i64);
    }

    // Stage 1: produces total_slots, unique_nodes, rank_bucket_counts,
    // num_shard_workers, the live IdSet (kept alive through stage 2
    // for inline coord resolution), and the per-blob rank mapping.
    //
    // #9 layer 2: relation member-id scan runs concurrently with stage 1.
    // The scan reads relation blobs only (via blob_meta) and shares no
    // state with stage 1 - both read from the same input PBF via pread
    // (`File: Sync` on Unix) with no locking. On Europe the scan takes
    // ~4 s; it fits entirely inside stage 1's ~43 s wall, so the serial
    // gap the scan used to create between stage 2 and stage 4 goes away.
    crate::debug::emit_marker("EXTJOIN_STAGE1_START");
    let (s1_minflt_before, s1_majflt_before) = crate::debug::read_page_faults();

    let input_ref_parallel: &Path = input;
    let blob_meta_ref_parallel = &blob_meta;
    let bucket_layout_ref = &bucket_layout;
    let (stage1_out, relation_member_node_ids) = std::thread::scope(
        |scope| -> std::result::Result<
            (
                super::external::stage1::Stage1Output,
                Option<relation_scan::RelationScanOutput>,
            ),
            String,
        > {
            let s1_handle = scope.spawn(|| {
                stage1_way_pass(
                    blob_meta_ref_parallel,
                    input_ref_parallel,
                    direct_io,
                    &scratch_dir,
                    bucket_layout_ref,
                    &ref_count_sidecar,
                    &per_way_refcount_sidecar,
                    inject_prepass,
                )
                .map_err(|e| e.to_string())
            });
            let rel_handle = if keep_untagged_nodes && !inject_prepass {
                None
            } else {
                crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_START");
                Some(scope.spawn(move || {
                    let t_relscan = std::time::Instant::now();
                    let ids = relation_scan::collect_relation_member_ids_indexed(
                        input_ref_parallel,
                        blob_meta_ref_parallel,
                        !keep_untagged_nodes,
                        inject_prepass,
                    )
                    .map_err(|e| e.to_string())?;
                    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                    crate::debug::emit_counter(
                        "extjoin_relation_member_collect_ms",
                        t_relscan.elapsed().as_millis() as i64,
                    );
                    crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_END");
                    Ok::<_, String>(ids)
                }))
            };

            // Error ordering: `s1_handle.join()??` short-circuits before
            // joining the relation-scan handle, but `thread::scope` still
            // waits for `rel_handle` to run to completion before the scope
            // returns. A stage-1 failure while the relation scan is
            // running therefore delays error reporting by up to the
            // scan's wall time (~4 s Europe, longer at planet). Accepted
            // as diagnostic-quality: stage-1 failures are rare (only fire
            // on adversarial or malformed input, mostly closed by the
            // defensive-input checks in ADR-0004), and cancelling the
            // relation scan would require plumbing a shutdown signal we
            // don't otherwise need.
            let s1_res = s1_handle
                .join()
                .map_err(|_| "stage 1 thread panicked".to_string())??;
            let rel_res = match rel_handle {
                Some(handle) => Some(
                    handle
                        .join()
                        .map_err(|_| "relation scan thread panicked".to_string())??,
                ),
                None => None,
            };
            Ok((s1_res, rel_res))
        },
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (s1_minflt_after, s1_majflt_after) = crate::debug::read_page_faults();
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
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_total_slots", stage1_out.total_slots as i64);
        crate::debug::emit_counter("extjoin_total_id_records", total_id_records as i64);
        crate::debug::emit_counter(
            "extjoin_id_bucket_max_records",
            id_bucket_max_records as i64,
        );
        crate::debug::emit_counter(
            "extjoin_id_bucket_min_records",
            id_bucket_min_records as i64,
        );
        crate::debug::emit_counter("extjoin_id_buckets_nonempty", id_buckets_nonempty as i64);
        crate::debug::emit_counter(
            "s1_minflt_delta",
            (s1_minflt_after - s1_minflt_before) as i64,
        );
        crate::debug::emit_counter(
            "s1_majflt_delta",
            (s1_majflt_after - s1_majflt_before) as i64,
        );
    }
    crate::debug::emit_marker("EXTJOIN_STAGE1_END");

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
    let total_id_shard_files = num_shard_workers * NUM_BUCKETS;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_id_bucket_count", NUM_BUCKETS as i64);
        crate::debug::emit_counter("extjoin_slot_bucket_count", slot_bucket_count as i64);
        crate::debug::emit_counter("extjoin_max_blob_slots", max_blob_slots as i64);
        crate::debug::emit_counter("extjoin_num_shard_workers", num_shard_workers as i64);
        crate::debug::emit_counter("extjoin_total_id_shard_files", total_id_shard_files as i64);
    }

    crate::debug::emit_marker("EXTJOIN_STAGE2_START");
    let (s2_minflt_before, s2_majflt_before) = crate::debug::read_page_faults();
    let slot_buckets = SlotBuckets::create(&scratch_dir, slot_bucket_count)?;
    let input_pbf = std::sync::Arc::new(
        std::fs::File::open(input).map_err(|e| format!("open input pbf for stage 2: {e}"))?,
    );
    let resolved_count = stage2_node_join(
        &scratch_dir,
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
    )?;
    slot_buckets.finish()?;
    let (s2_minflt_after, s2_majflt_after) = crate::debug::read_page_faults();
    // Reclaim id-shard scratch (workers × NUM_BUCKETS files;
    // 80+ GB at planet) before the heavier stage 3/4 streaming
    // handoff allocates its own tmp space.
    for worker_id in 0..num_shard_workers {
        for bucket_idx in 0..NUM_BUCKETS {
            let path = scratch_dir
                .path
                .join(format!("id-W{worker_id}-{bucket_idx:03}"));
            drop(std::fs::remove_file(&path));
        }
    }
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);
        crate::debug::emit_counter(
            "s2_minflt_delta",
            (s2_minflt_after - s2_minflt_before) as i64,
        );
        crate::debug::emit_counter(
            "s2_majflt_delta",
            (s2_majflt_after - s2_majflt_before) as i64,
        );
    }
    crate::debug::emit_marker("EXTJOIN_STAGE2_END");

    // Free the per-blob mapping - it was a stage 2 input only.
    drop(node_blob_mapping);

    // Prepare inputs for the streaming stage 3 + stage 4 handoff.
    let per_way_rcs = coord_payloads::load_per_way_refcount_sidecar_indexed(
        &per_way_refcount_sidecar,
        way_slot_starts.len(),
    )?;
    // Worker count: back off from the pre-streaming `.min(6)` because
    // stage 3 and stage 4 worker buffers are now both resident at the
    // same time (streaming stage 3 -> 4 landed in `beb7838` + `f93d896`
    // + `eecb46c`).
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(4);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("s3_worker_count", num_workers as i64);

    // Worker tmp files opened once here with read + write, wrapped in
    // Arc<File> so stage 3 can `write_all_at` and stage 4 can
    // `read_exact_at` on the same `&File`. `File` is `Sync` on Unix for
    // pread/pwrite so no extra locking is needed.
    let worker_tmp_paths: Vec<std::path::PathBuf> = (0..num_workers)
        .map(|i| scratch_dir.file_path(&format!("payloads-W{i}")))
        .collect();
    let worker_files: Vec<std::sync::Arc<std::fs::File>> = worker_tmp_paths
        .iter()
        .map(|p| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)
                .map(std::sync::Arc::new)
                .map_err(|e| format!("open worker tmp {p:?}: {e}"))
        })
        .collect::<std::result::Result<_, String>>()?;

    let slot_entry_counts: Vec<u64> = (0..slot_bucket_count)
        .map(|i| {
            let path = scratch_dir.bucket_path("slot", i);
            std::fs::metadata(&path)
                .map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64)
                .unwrap_or(0)
        })
        .collect();
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
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("extjoin_slot_bucket_max_entries", max_entries as i64);
            crate::debug::emit_counter("extjoin_slot_bucket_min_entries", min_entries as i64);
            crate::debug::emit_counter("extjoin_slot_buckets_nonempty", nonempty as i64);
        }
    }
    let slot_paths: Vec<std::path::PathBuf> = (0..slot_bucket_count)
        .map(|i| scratch_dir.bucket_path("slot", i))
        .collect();
    let slot_bucket_ref = SlotBucketRef {
        paths: slot_paths,
        entry_counts: slot_entry_counts,
    };

    // The streaming router: pre-populates `Empty` entries for zero-ref
    // way blobs so stage 4 never waits on a blob that stage 3 would
    // never publish.
    let router =
        coord_payloads::ConcurrentBlobLocationRouter::new(&per_way_rcs, worker_files.clone())?;

    // (#9 layer 2: relation scan already ran in parallel with stage 1
    // above; `relation_member_node_ids` is already bound. No serial
    // scan between stage 2 and stage 4.)

    // Streaming stage 3 + stage 4: run concurrently via a single
    // `thread::scope`. Stage 3 publishes per-blob entries to the router
    // as it encodes them; stage 4 workers block on `router.wait_ready`
    // ahead of any input pread so they never hold decompressed state
    // while waiting.
    crate::debug::emit_marker("EXTJOIN_STREAMING_START");
    crate::debug::emit_marker("EXTJOIN_STAGE3_START");
    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let (s3_minflt_before, s3_majflt_before) = crate::debug::read_page_faults();

    let router_ref = &router;
    let per_way_rcs_ref = &per_way_rcs;
    let blob_meta_ref = &blob_meta;
    let way_slot_starts_ref = way_slot_starts.as_slice();
    let rel_ids_ref = relation_member_node_ids
        .as_ref()
        .and_then(|scan| scan.member_node_ids.as_ref());
    let member_way_ids_ref = relation_member_node_ids
        .as_ref()
        .and_then(|scan| scan.member_way_ids.as_ref());
    let slot_bucket_ref_ref = &slot_bucket_ref;

    // Closures return Result<_, String> because BoxResult's error type
    // (Box<dyn Error>) is not Send and thread::scope requires Send
    // return values. Errors are stringified at the scope boundary and
    // converted back to BoxResult outside.
    let mut stats = std::thread::scope(|scope| -> std::result::Result<Stats, String> {
        let s3_handle = scope.spawn(move || -> std::result::Result<(), String> {
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
            )
            .map_err(|e| e.to_string());
            // Signal the router that no more publishes are coming. Must
            // run whether stage 3 succeeded or errored - otherwise stage
            // 4 waiters on unpublished slots would hang. On error the
            // worker has already called `router.abort`, but
            // mark_producer_done is idempotent with abort and cheap.
            router_ref.mark_producer_done();
            result
        });
        let s4_handle = scope.spawn(move || -> std::result::Result<Stats, String> {
            stage4_assembly(
                input,
                output,
                blob_meta_ref,
                router_ref,
                per_way_rcs_ref,
                way_slot_starts_ref,
                keep_untagged_nodes,
                rel_ids_ref,
                member_way_ids_ref,
                inject_prepass,
                compression,
                direct_io,
                overrides,
            )
            .map_err(|e| e.to_string())
        });

        let s3_res = s3_handle
            .join()
            .map_err(|_| "stage 3 thread panicked".to_string())?;
        let s4_res = s4_handle
            .join()
            .map_err(|_| "stage 4 thread panicked".to_string())?;

        // Prefer the stage 3 error if both failed (it's usually the root
        // cause - stage 4 typically errors only because of an abort that
        // stage 3 or the writer raised).
        s3_res?;
        s4_res
    })
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (s3_minflt_after, s3_majflt_after) = crate::debug::read_page_faults();
    for i in 0..slot_bucket_count {
        drop(std::fs::remove_file(scratch_dir.bucket_path("slot", i)));
    }
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "s3_minflt_delta",
            (s3_minflt_after - s3_minflt_before) as i64,
        );
        crate::debug::emit_counter(
            "s3_majflt_delta",
            (s3_majflt_after - s3_majflt_before) as i64,
        );
    }
    crate::debug::emit_marker("EXTJOIN_STAGE4_END");
    crate::debug::emit_marker("EXTJOIN_STAGE3_END");
    crate::debug::emit_marker("EXTJOIN_STREAMING_END");

    // Emit router stats that the deleted `build_blob_location_router`
    // used to report.
    {
        let s = router
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("s3_router_num_worker", s.num_worker as i64);
            crate::debug::emit_counter("s3_router_num_straddlers", s.num_straddlers as i64);
            crate::debug::emit_counter("s3_router_num_empty", s.num_empty as i64);
            crate::debug::emit_counter("s3_router_worker_bytes", s.worker_bytes as i64);
            crate::debug::emit_counter("s3_router_straddler_bytes", s.straddler_bytes as i64);
            crate::debug::emit_counter(
                "s3_straddler_encode_ms",
                (s.straddler_encode_ns / 1_000_000) as i64,
            );
        }
        eprintln!(
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

    // `resolved_count` counts slots whose coord tuple is `!= (0, 0)`. A
    // real OSM node at exactly 0.0000000, 0.0000000 is indistinguishable
    // from an unresolved slot (zero-initialized scratch) and gets
    // counted as missing. Accepted limitation; see
    // `stage2.rs` `is_resolved` comment and CORRECTNESS.md
    // "Null Island ambiguity in dense mmap index" for the rationale.
    stats.missing_locations = total_slots.saturating_sub(resolved_count);

    if inject_prepass {
        let member_ways = member_way_ids_ref.map_or(0, |set| set.iter().count() as u64);
        super::inject_metrics::emit(member_ways);
    }

    drop(scratch_dir);

    Ok(stats)
}
