//! Blob schedule scan: read all OsmData blob headers once, partition by
//! element kind, and extract indexdata min/max IDs + element counts.

use std::path::Path;

use super::super::Result;

/// Per-blob task for the parallel pass pool; a task's index in its per-kind
/// schedule is its sequence number for the ordered writer. `data_offset` /
/// `data_size` address the compressed blob body for pread. `element_count`
/// comes from the indexdata `BlobIndex.count` and lets the caller precompute
/// base new_ids without racing decode; each blob verifies it after decode.
// Deliberately schedule payloads without tagdata header lengths or key lists.
// Renumber is terminal; carrying those adds per-blob work and changes output.
pub(super) struct BlobTask {
    /// File-wide ordinal of the blob, counted by the header walker (not the
    /// position in this per-kind schedule). Provenance for error locations.
    pub(super) blob_index: u64,
    /// Start of the blob's on-disk frame (its 4-byte length prefix).
    pub(super) frame_offset: u64,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
    pub(super) element_count: u64,
    /// Source blob's max element ID from indexdata - a claim, used only as
    /// one input to the node pre-allocation bound.
    pub(super) max_id: i64,
    /// Source blob's spatial bbox (node blobs only).
    pub(super) bbox: Option<crate::blob_meta::BlobBbox>,
}

impl BlobTask {
    /// Stamp `error` with this blob's true position in `source`. Workers
    /// capture `source` once, so no per-blob reference counting is needed.
    pub(super) fn locate(
        &self,
        source: &std::sync::Arc<Path>,
        error: crate::Error,
    ) -> crate::Error {
        error.locate(crate::error::Location::new(
            Some(source),
            Some(self.blob_index),
            Some(crate::ByteOffset(self.frame_offset)),
        ))
    }
}

/// Header-only scan building a per-kind schedule with element counts.
/// Requires indexed PBFs (all brokkr datasets are indexed): the per-blob
/// element count is read from `BlobIndex.count`, which is required to
/// precompute each blob's `base_new_id` without a full decode pass. If a
/// matching blob is missing indexdata, we error out with a pointer to
/// `brokkr cat` / indexed datasets.
/// Scan all blob headers once and build per-kind schedules.
/// Returns `(node_schedule, way_schedule, relation_schedule)`.
///
/// Walks via the pread-only `HeaderWalker` + `posix_fadvise(RANDOM)` so
/// blob bodies stay out of the page cache during this scan - renumber's
/// downstream pread workers open separate fds and touch only the blobs
/// they rewrite.
#[hotpath::measure]
pub(super) fn build_all_blob_schedules(
    input: &Path,
    host: &crate::host::Host,
) -> Result<(Vec<BlobTask>, Vec<BlobTask>, Vec<BlobTask>)> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    drop(
        walker
            .next_header()?
            .ok_or_else(|| crate::error::new_missing_header(input))?,
    );
    let mut nodes: Vec<BlobTask> = Vec::new();
    let mut ways: Vec<BlobTask> = Vec::new();
    let mut relations: Vec<BlobTask> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let Some(idx) = &meta.index else {
            return Err(walker.locate_meta(
                &meta,
                crate::error::new_missing_indexdata(
                    "renumber requires indexdata on every OsmData blob: each blob's new \
                     ids are precomputed from its indexdata element count",
                ),
            ));
        };
        let sched = match idx.kind {
            crate::ElementKind::Node => &mut nodes,
            crate::ElementKind::Way => &mut ways,
            crate::ElementKind::Relation => &mut relations,
        };
        sched.push(BlobTask {
            blob_index: meta.blob_index,
            frame_offset: meta.frame_start,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            element_count: idx.count,
            max_id: idx.max_id,
            bbox: idx.bbox,
        });
    }
    Ok((nodes, ways, relations))
}
