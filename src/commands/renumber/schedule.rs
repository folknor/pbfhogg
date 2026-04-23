//! Blob schedule scan: read all OsmData blob headers once, partition by
//! element kind, and extract indexdata min/max IDs + element counts.

use std::path::Path;

use super::super::Result;

/// Per-blob task for the parallel pass pool. `seq` is the filtered-index
/// position (monotonic within the per-kind blob list, used for writer
/// reorder). `data_offset` / `data_size` address the compressed blob body
/// for pread. `element_count` comes from the indexdata `BlobIndex.count`
/// and lets the caller precompute base new_ids without racing decode.
pub(super) struct BlobTask {
    pub(super) seq: usize,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
    pub(super) element_count: u64,
    /// Source blob's max element ID from indexdata. Used to compute
    /// per-stage id-space offsets before decode.
    pub(super) max_id: i64,
    /// Source blob's spatial bbox (node blobs only).
    pub(super) bbox: Option<crate::blob_meta::BlobBbox>,
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
) -> Result<(Vec<BlobTask>, Vec<BlobTask>, Vec<BlobTask>)> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;
    let mut nodes: Vec<BlobTask> = Vec::new();
    let mut ways: Vec<BlobTask> = Vec::new();
    let mut relations: Vec<BlobTask> = Vec::new();
    let mut node_seq: usize = 0;
    let mut way_seq: usize = 0;
    let mut rel_seq: usize = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let Some(idx) = meta.index else {
            return Err(
                "renumber requires an indexed PBF - run `pbfhogg cat` to add \
                 indexdata or use the indexed variant"
                    .into(),
            );
        };
        let (sched, seq) = match idx.kind {
            crate::blob_meta::ElemKind::Node => (&mut nodes, &mut node_seq),
            crate::blob_meta::ElemKind::Way => (&mut ways, &mut way_seq),
            crate::blob_meta::ElemKind::Relation => (&mut relations, &mut rel_seq),
        };
        sched.push(BlobTask {
            seq: *seq,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            element_count: idx.count,
            max_id: idx.max_id,
            bbox: idx.bbox,
        });
        *seq += 1;
    }
    Ok((nodes, ways, relations))
}
