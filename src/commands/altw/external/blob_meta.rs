//! Shared OsmData blob metadata scan for ALTW external join.
//!
//! Replaces repeated header-only scans in stage 1 and stage 4 with one
//! reusable pass over blob headers.

use std::path::Path;

use crate::ElementKind;

use super::super::Result;

#[derive(Clone, Copy, Debug)]
pub(super) struct BlobMeta {
    pub frame_offset: u64,
    pub data_offset: u64,
    pub data_size: usize,
    pub kind: ElementKind,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
    pub has_tagindex: bool,
    pub has_tags: bool,
}

/// Scan all OsmData blob headers once and retain only the metadata ALTW
/// external join actually reuses later. Walks via the pread-only
/// `HeaderWalker` so blob bodies stay out of the page cache during the
/// scan - stage 1 / stage 4 open their own fds and pread the bodies they
/// need.
///
/// This is the external join's one trust boundary for indexdata ranges,
/// so every range check lives here, located at the offending blob:
///
/// - a reversed range (`max_id < min_id`) is malformed indexdata;
/// - a node range reaching below zero is refused because stage 2's bucket
///   partition and merge walk compare node ids as unsigned values;
/// - the ranges must follow canonical `Sort.Type_then_ID` order. Stage 2
///   partitions the node blobs by range with `partition_point` and walks
///   bucket records forward through them, so node blobs out of order or
///   overlapping turn present nodes into missing locations (zeroed, under
///   `--ignore-missing-nodes`); stage 4 also writes blobs in input order
///   under the input's sorted header. Equal boundaries pass, as in
///   `OrderWatermark`: duplicate ids are a separate concern. Indexdata
///   ranges are exact by contract (`check --index`), so ranges that break
///   the order mean the header's claim, or the indexdata, is false. That
///   includes an overstated `max_id` overlapping the next blob, which stage
///   2's walk alone would tolerate (it bounds each blob by its last decoded
///   id). Refusing it is deliberate: tolerating it while still catching
///   interleaved blobs would need each blob's true last id, a per-blob
///   decode in this header-only scan.
///
/// Header-level only: the order of elements inside one blob is not visible
/// here.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn scan_blob_metadata(
    input: &Path,
    parse_tagdata: bool,
    host: &crate::host::Host,
) -> Result<Vec<BlobMeta>> {
    crate::debug::emit_marker("ALTW_BLOB_META_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;

    let mut metas = Vec::new();
    let mut order = crate::osm_id::OrderWatermark::default();
    let mut node_blobs: u64 = 0;
    let mut way_blobs: u64 = 0;
    let mut relation_blobs: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let idx = meta.index.as_ref().ok_or_else(|| {
            walker.locate_meta(
                &meta,
                crate::Error::invalid_data(
                    "external join metadata scan: OsmData blob missing indexdata",
                ),
            )
        })?;
        check_range(idx, &mut order).map_err(|e| walker.locate_meta(&meta, e))?;
        let tag_index = if parse_tagdata {
            meta.tagdata
                .as_deref()
                .and_then(crate::blob_meta::TagIndex::deserialize)
        } else {
            None
        };
        let has_tagindex = tag_index.is_some();
        let has_tags = if parse_tagdata {
            tag_index.is_none_or(|ti| !ti.keys_empty())
        } else {
            false
        };
        match idx.kind {
            ElementKind::Node => node_blobs += 1,
            ElementKind::Way => way_blobs += 1,
            ElementKind::Relation => relation_blobs += 1,
        }
        metas.push(BlobMeta {
            frame_offset: meta.frame_start,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            kind: idx.kind,
            min_id: idx.min_id,
            max_id: idx.max_id,
            count: idx.count,
            has_tagindex,
            has_tags,
        });
    }
    crate::debug::emit_marker("ALTW_BLOB_META_SCAN_END");

    crate::debug::emit_counter("altw_meta_node_blobs", node_blobs);
    crate::debug::emit_counter("altw_meta_way_blobs", way_blobs);
    crate::debug::emit_counter("altw_meta_relation_blobs", relation_blobs);
    Ok(metas)
}

/// The range checks [`scan_blob_metadata`] documents, for one blob in file
/// order. Unlocated: the caller locates the error at the blob.
fn check_range(
    idx: &crate::blob_meta::BlobIndex,
    order: &mut crate::osm_id::OrderWatermark,
) -> Result<()> {
    if idx.max_id < idx.min_id {
        return Err(crate::Error::invalid_data(format!(
            "altw external: {} blob has reversed indexdata range [min_id={}, max_id={}]",
            idx.kind.as_str(),
            idx.min_id,
            idx.max_id,
        )));
    }
    if matches!(idx.kind, ElementKind::Node) && idx.min_id < 0 {
        return Err(crate::Error::invalid_data(format!(
            "altw external: node blob indexdata range [min_id={}, max_id={}] holds \
             negative node ids, which the external index cannot join; the sparse \
             index can",
            idx.min_id, idx.max_id,
        )));
    }
    if let Err(v) = order.observe(idx.kind.ordinal(), idx.min_id, idx.max_id) {
        return Err(crate::error::new_unsorted(format!(
            "altw external: indexdata ranges contradict the sorted header - a {} blob \
             holding id {} follows {} blobs already reaching id {}. \
             The external join needs Sort.Type_then_ID input with exact indexdata \
             ranges, and one of the two is false here.",
            idx.kind.as_str(),
            v.first_id,
            ElementKind::from_ordinal(v.previous_kind).map_or("unknown", ElementKind::as_str),
            v.previous_id,
        )));
    }
    Ok(())
}
