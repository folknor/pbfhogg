//! Blob walk and shard planner shared by the two parallel block-pair
//! drivers: `parallel` (text diff) and `derive_parallel` (`diff --format
//! osc`).
//!
//! # Correctness contract
//!
//! The drivers answer exactly what the sequential block-pair merge answers
//! only under these conditions, and this module is where they are enforced
//! or assumed:
//!
//! - **Non-negative IDs only.** Shard thresholds and the per-shard clips are
//!   raw `i64` comparisons, while elements are ordered by `osm_id_cmp`
//!   (`0, -1, -2, ..., 1, 2, ...`). The two agree only when no ID is
//!   negative. [`walk_pair`] therefore returns `None` when either file holds
//!   a blob whose `min_id` is negative, and the caller runs the sequential
//!   block-pair merge for the whole comparison instead. The order guard is
//!   not consulted for such a file here: the sequential merge runs the same
//!   guard over every blob of both files as it reads them, so the verdict
//!   does not depend on which path saw the file first.
//! - **Blobs in canonical order.** [`walk_pair`] runs `BlobOrderGuard` over
//!   each file in file order before bucketing by kind, so the per-kind lists
//!   ascend and the planner's binary-search-style scans are well defined.
//!   The guard refuses disorder the indexdata ranges prove; within a blob,
//!   and inside a mixed-sign range, order is trusted.
//! - **The same structural refusals as the sequential merge.** Both paths
//!   check framing, indexdata on every data blob and blob order over the
//!   whole of both files, whatever the type filter (the sequential merge
//!   walks the headers of a tail its phases never read), and neither
//!   interprets the body of an excluded kind's blob. Checks on the bodies of
//!   selected kinds are not identical: the sequential merge does not decode
//!   a byte-identical blob pair when it may skip equal blobs (`-c` and
//!   `--format osc`), so a corrupt body inside such a pair passes there
//!   while these drivers, which decode every selected blob, refuse it; and
//!   these drivers decide a block's leftover tail from its claimed `max_id`
//!   where the sequential merge checks the claim against what the block
//!   holds.
//! - **Shard upper bounds are old-side blob maxima.** With strictly ordered
//!   input no old blob straddles a threshold, so every old element falls in
//!   exactly one shard window, and new blobs that straddle are read by each
//!   shard they touch and clipped to `(t_low, t_high]`. It also rules out the
//!   residual loop in which both current blocks extend past `t_high` and no
//!   element in range is left to consume: the old block always ends at or
//!   before the boundary.
//! - **Index claims are trusted.** Ranges and counts come from indexdata, as
//!   everywhere in the block-pair engine (see reference/indexdata-trust.md).
//!   A lying claim can change the answer.

use std::path::Path;
use std::sync::Arc;

use crate::ElementKind;
use crate::blob_meta::BlobIndex;
use crate::error::Result;
use crate::read::header_walker::HeaderWalker;

/// One OsmData blob's location + index metadata, enough to pread and
/// classify without decompression.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlobDesc {
    /// File-wide ordinal and frame start from the header walker; provenance
    /// for errors, never used for scheduling.
    pub(super) blob_index: u64,
    pub(super) frame_offset: u64,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
    pub(super) index: BlobIndex,
}

/// Three per-kind blob lists plus a shared `File` handle for pread.
pub(super) struct WalkedFile {
    /// The path the walker was opened from; workers locate their errors
    /// against it.
    pub(super) path: Arc<Path>,
    pub(super) nodes: Vec<BlobDesc>,
    pub(super) ways: Vec<BlobDesc>,
    pub(super) relations: Vec<BlobDesc>,
    pub(super) file: Arc<std::fs::File>,
}

/// Walk both inputs' blob headers for the parallel path.
///
/// Returns `None` when either file holds a negative ID; the caller must
/// then run the sequential block-pair merge for the whole comparison (see
/// the module contract). Errors when an OsmData blob lacks indexdata or a
/// file's blobs are not in canonical order.
pub(super) fn walk_pair(
    old_path: &Path,
    new_path: &Path,
    host: &crate::host::Host,
) -> Result<Option<(WalkedFile, WalkedFile)>> {
    // Both files' negative-ID answers come before either file's order
    // check: the fallback covers the whole comparison, so an order-guard
    // error on one side must not preempt a negative ID on the other.
    let old = walk_file(old_path, host)?;
    let new = walk_file(new_path, host)?;
    if old.has_negative || new.has_negative {
        return Ok(None);
    }
    Ok(Some((old.bucket()?, new.bucket()?)))
}

/// One file's data blobs in file order, before the order check.
struct RawWalk {
    path: Arc<Path>,
    in_file_order: Vec<BlobDesc>,
    has_negative: bool,
    file: Arc<std::fs::File>,
}

fn walk_file(path: &Path, host: &crate::host::Host) -> Result<RawWalk> {
    let mut walker = HeaderWalker::open(path, host)?;
    let mut in_file_order: Vec<BlobDesc> = Vec::new();
    let mut has_negative = false;
    let mut first = true;

    while let Some(meta) = walker.next_header()? {
        // Skip the required leading OsmHeader blob; subsequent OsmData
        // blobs must carry indexdata for the parallel path.
        if first {
            first = false;
            continue;
        }
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let Some(index) = meta.index else {
            return Err(walker.locate_meta(
                &meta,
                crate::error::new_missing_indexdata(
                    "block-pair merge requires indexdata on every data blob",
                ),
            ));
        };
        has_negative |= index.min_id < 0;
        in_file_order.push(BlobDesc {
            blob_index: meta.blob_index,
            frame_offset: meta.frame_start,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            index,
        });
    }

    Ok(RawWalk {
        path: Arc::clone(walker.path()),
        in_file_order,
        has_negative,
        file: Arc::clone(walker.shared_file()),
    })
}

impl RawWalk {
    /// Run the order check in file order, then bucket by kind.
    fn bucket(self) -> Result<WalkedFile> {
        // Bucketing by kind throws file order away. The guard sees the
        // blobs in file order first, so this path refuses exactly what the
        // sequential block-pair merge refuses instead of silently answering.
        let mut order = crate::osc::merge_join::BlobOrderGuard::new();
        let mut nodes: Vec<BlobDesc> = Vec::new();
        let mut ways: Vec<BlobDesc> = Vec::new();
        let mut relations: Vec<BlobDesc> = Vec::new();
        for desc in self.in_file_order {
            order.observe(&desc.index).map_err(|e| {
                e.locate(crate::error::Location::new(
                    Some(&self.path),
                    Some(desc.blob_index),
                    Some(crate::ByteOffset(desc.frame_offset)),
                ))
            })?;
            match desc.index.kind {
                ElementKind::Node => nodes.push(desc),
                ElementKind::Way => ways.push(desc),
                ElementKind::Relation => relations.push(desc),
            }
        }
        Ok(WalkedFile {
            path: self.path,
            nodes,
            ways,
            relations,
            file: self.file,
        })
    }
}

/// One shard: an ID range `(t_low, t_high]` plus the index ranges of old and
/// new blobs whose content intersects that ID range. Straddling new blobs
/// are read by every shard they touch; each shard's element merge clips to
/// its own window so every element is emitted exactly once.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shard {
    /// IDs strictly greater than this belong to this shard. `i64::MIN` for
    /// the first shard.
    pub(super) t_low: i64,
    /// IDs less than or equal to this belong to this shard. `i64::MAX` for
    /// the last shard.
    pub(super) t_high: i64,
    pub(super) old_start: usize,
    pub(super) old_end: usize,
    pub(super) new_start: usize,
    pub(super) new_end: usize,
}

/// Plan up to `target_count` shards by ID range, thresholds at evenly
/// spaced old-blob maxima. Requires non-negative IDs, which [`walk_pair`]
/// guarantees.
pub(super) fn plan_shards(
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    target_count: usize,
) -> Vec<Shard> {
    debug_assert!(
        old_descs
            .iter()
            .chain(new_descs.iter())
            .all(|d| d.index.min_id >= 0),
        "plan_shards requires non-negative IDs; walk_pair routes negative inputs elsewhere",
    );

    if target_count <= 1 || old_descs.is_empty() {
        return vec![Shard {
            t_low: i64::MIN,
            t_high: i64::MAX,
            old_start: 0,
            old_end: old_descs.len(),
            new_start: 0,
            new_end: new_descs.len(),
        }];
    }

    let n = target_count.min(old_descs.len()).max(1);
    // `old_descs` ascend, so the thresholds are non-decreasing and any
    // duplicate is adjacent to its twin; consecutive dedup suffices.
    let mut thresholds: Vec<i64> = (1..n)
        .map(|k| old_descs[(k * old_descs.len() / n) - 1].index.max_id)
        .collect();
    thresholds.dedup();

    let mut shards: Vec<Shard> = Vec::with_capacity(thresholds.len() + 1);
    let mut t_low = i64::MIN;
    for &t_high in &thresholds {
        shards.push(build_shard(old_descs, new_descs, t_low, t_high));
        t_low = t_high;
    }
    shards.push(build_shard(old_descs, new_descs, t_low, i64::MAX));
    shards
}

fn build_shard(old_descs: &[BlobDesc], new_descs: &[BlobDesc], t_low: i64, t_high: i64) -> Shard {
    // A blob intersects (t_low, t_high] iff max_id > t_low and
    // min_id <= t_high. The lists ascend, so the intersecting range is
    // contiguous.
    let old_start = old_descs
        .iter()
        .position(|b| b.index.max_id > t_low)
        .unwrap_or(old_descs.len());
    let old_end = old_descs
        .iter()
        .rposition(|b| b.index.min_id <= t_high)
        .map_or(old_start, |i| i + 1);
    let new_start = new_descs
        .iter()
        .position(|b| b.index.max_id > t_low)
        .unwrap_or(new_descs.len());
    let new_end = new_descs
        .iter()
        .rposition(|b| b.index.min_id <= t_high)
        .map_or(new_start, |i| i + 1);
    Shard {
        t_low,
        t_high,
        old_start,
        old_end: old_end.max(old_start),
        new_start,
        new_end: new_end.max(new_start),
    }
}
