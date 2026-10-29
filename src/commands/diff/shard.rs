//! Blob walk and shard planner for the block-pair merge, shared by the text
//! diff and `diff --format osc` (derive).
//!
//! Every comparison of two indexed inputs walks both files' blob headers
//! here, plans the ID space into shards, and runs
//! [`crate::osc::merge_join::block_pair_merge_phase`] once per shard and
//! kind. `-j 1` is the same walk with a one-shard plan, so the answer and
//! the refusals do not depend on the shard count.
//!
//! # Correctness contract
//!
//! - **Whole-file structure first.** [`walk_pair`] walks every blob header
//!   of both files before any body is read: framing, indexdata on every
//!   data blob, and canonical blob order (`BlobOrderGuard`, run in file
//!   order before bucketing by kind, so the per-kind lists ascend). This
//!   holds whatever the type filter; the body of an excluded kind's blob is
//!   never read. The guard refuses disorder the indexdata ranges prove;
//!   within a blob, and inside a mixed-sign range, order is trusted.
//! - **Negative IDs get one unbounded shard.** Shard windows are raw `i64`
//!   comparisons, while elements are ordered by `osm_id_cmp` (`0, -1, -2,
//!   ..., 1, 2, ...`); the two agree only when no ID is negative. A file
//!   holding a blob whose `min_id` is negative is therefore compared as one
//!   shard with no window, whatever the shard count, and the merge orders
//!   everything by `osm_id_cmp`.
//! - **Shard upper bounds are old-side blob maxima.** Every element falls
//!   in exactly one window `(t_low, t_high]`. A blob whose claimed range
//!   crosses a window is read by every shard it touches and clipped to that
//!   shard's window when decoded (new blobs routinely; old blobs only when an
//!   equal ID spans two adjacent old blobs). A clipped block is bounded at
//!   the window's upper end, which also rules out the residual loop in which
//!   both current blocks extend past `t_high` with nothing left in range.
//! - **Index claims are trusted.** Ranges and counts come from indexdata, as
//!   everywhere in the block-pair engine (see reference/indexdata-trust.md).
//!   A lying claim can change the answer. What the merge does check is the
//!   one claim a residual decision rests on: a block holding elements past
//!   its claimed last ID is refused when the merge meets them, unless the
//!   block was clipped at the window, whose bound then hides them.

use std::path::Path;
use std::sync::Arc;

use crate::ElementKind;
use crate::error::Result;
use crate::osc::merge_join::{BlobDesc, Window};
use crate::read::header_walker::HeaderWalker;
use crate::read::input::InputSource;

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

impl WalkedFile {
    /// The blobs of `kind`.
    pub(super) fn kind(&self, kind: ElementKind) -> &[BlobDesc] {
        match kind {
            ElementKind::Node => &self.nodes,
            ElementKind::Way => &self.ways,
            ElementKind::Relation => &self.relations,
        }
    }
}

/// Both inputs walked.
pub(super) struct WalkedPair {
    pub(super) old: WalkedFile,
    pub(super) new: WalkedFile,
    /// Either file holds a negative ID: plan one unbounded shard (see the
    /// module contract).
    pub(super) has_negative: bool,
}

/// Walk both inputs' blob headers.
///
/// Errors when an OsmData blob lacks indexdata or a file's blobs are not in
/// canonical order. Each input is opened once, from its source (a pinned
/// run passes its pins), and the walker's descriptor is the one the shard
/// workers share.
pub(super) fn walk_pair<'a, 'b>(
    old_source: impl Into<InputSource<'a>>,
    new_source: impl Into<InputSource<'b>>,
    host: &crate::host::Entered<'_>,
) -> Result<WalkedPair> {
    let old = walk_file(old_source.into(), host)?;
    let new = walk_file(new_source.into(), host)?;
    let has_negative = old.has_negative || new.has_negative;
    Ok(WalkedPair {
        old: old.bucket()?,
        new: new.bucket()?,
        has_negative,
    })
}

/// One file's data blobs in file order, before the order check.
struct RawWalk {
    path: Arc<Path>,
    in_file_order: Vec<BlobDesc>,
    has_negative: bool,
    file: Arc<std::fs::File>,
}

fn walk_file(source: InputSource<'_>, host: &crate::host::Entered<'_>) -> Result<RawWalk> {
    let mut walker = HeaderWalker::open(source, host)?;
    let mut in_file_order: Vec<BlobDesc> = Vec::new();
    let mut has_negative = false;
    let mut first = true;

    while let Some(meta) = walker.next_header()? {
        // Skip the required leading OsmHeader blob; subsequent OsmData
        // blobs must carry indexdata for the block-pair merge.
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
                    "block-pair merge requires readable indexdata on every data blob \
                     (this blob has none, or its indexdata has an unknown version, \
                     an unknown kind or an inverted id range)",
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
        // blobs in file order first, so a misplaced blob is refused instead
        // of silently answered around.
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

/// One shard: an optional ID window plus the index ranges of old and new
/// blobs whose content intersects it. Straddling blobs are read by every
/// shard they touch; each shard's merge clips to its own window so every
/// element is emitted exactly once.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shard {
    /// `None` for a one-shard plan, which reads every blob whole.
    pub(super) window: Option<Window>,
    pub(super) old_start: usize,
    pub(super) old_end: usize,
    pub(super) new_start: usize,
    pub(super) new_end: usize,
}

impl Shard {
    /// Total blobs the shard reads, both sides.
    pub(super) fn blobs(&self) -> usize {
        (self.old_end - self.old_start) + (self.new_end - self.new_start)
    }
}

/// Plan up to `target_count` shards of one kind by ID range, thresholds at
/// evenly spaced old-blob maxima. One unbounded shard when `target_count`
/// is at most one, when the old side has no blob, or when `has_negative`
/// (see the module contract).
pub(super) fn plan_shards(
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    target_count: usize,
    has_negative: bool,
) -> Vec<Shard> {
    if has_negative || target_count <= 1 || old_descs.is_empty() {
        return vec![Shard {
            window: None,
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
    if thresholds.is_empty() {
        return plan_shards(old_descs, new_descs, 1, false);
    }

    let mut shards: Vec<Shard> = Vec::with_capacity(thresholds.len() + 1);
    let mut t_low = i64::MIN;
    for &t_high in &thresholds {
        shards.push(build_shard(old_descs, new_descs, Window { t_low, t_high }));
        t_low = t_high;
    }
    shards.push(build_shard(
        old_descs,
        new_descs,
        Window {
            t_low,
            t_high: i64::MAX,
        },
    ));
    shards
}

fn build_shard(old_descs: &[BlobDesc], new_descs: &[BlobDesc], window: Window) -> Shard {
    // A blob intersects (t_low, t_high] iff max_id > t_low and
    // min_id <= t_high. The lists ascend, so the intersecting range is
    // contiguous.
    let Window { t_low, t_high } = window;
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
        window: Some(window),
        old_start,
        old_end: old_end.max(old_start),
        new_start,
        new_end: new_end.max(new_start),
    }
}
