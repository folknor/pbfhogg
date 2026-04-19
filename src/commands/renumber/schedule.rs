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
    /// Source blob's min element ID from indexdata. Used for per-block
    /// negative-id skip: if min_id >= 0, all IDs are non-negative.
    pub(super) min_id: i64,
    /// Source blob's max element ID from indexdata.
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
#[hotpath::measure]
pub(super) fn build_all_blob_schedules(
    input: &Path,
) -> Result<(Vec<BlobTask>, Vec<BlobTask>, Vec<BlobTask>)> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner
        .next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut nodes: Vec<BlobTask> = Vec::new();
    let mut ways: Vec<BlobTask> = Vec::new();
    let mut relations: Vec<BlobTask> = Vec::new();
    let mut node_seq: usize = 0;
    let mut way_seq: usize = 0;
    let mut rel_seq: usize = 0;
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        let Some(idx) = hdr.index() else {
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
            data_offset,
            data_size,
            element_count: idx.count,
            min_id: idx.min_id,
            max_id: idx.max_id,
            bbox: idx.bbox,
        });
        *seq += 1;
    }
    Ok((nodes, ways, relations))
}
