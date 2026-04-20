//! Shared OsmData blob metadata scan for ALTW external join.
//!
//! Replaces repeated header-only scans in stage 1 and stage 4 with one
//! reusable pass over blob headers.

use std::path::Path;

use crate::blob_meta::ElemKind;

use super::super::Result;

#[derive(Clone, Copy, Debug)]
pub(super) struct BlobMeta {
    pub frame_offset: u64,
    pub data_offset: u64,
    pub data_size: usize,
    pub kind: ElemKind,
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
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn scan_blob_metadata(
    input: &Path,
    parse_tagdata: bool,
) -> Result<Vec<BlobMeta>> {
    crate::debug::emit_marker("ALTW_BLOB_META_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;

    let mut metas = Vec::new();
    let mut node_blobs: u64 = 0;
    let mut way_blobs: u64 = 0;
    let mut relation_blobs: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let idx = meta.index.as_ref()
            .ok_or("external join metadata scan: OsmData blob missing indexdata")?;
        let tag_index = if parse_tagdata {
            meta.tagdata.as_deref()
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
            ElemKind::Node => node_blobs += 1,
            ElemKind::Way => way_blobs += 1,
            ElemKind::Relation => relation_blobs += 1,
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

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("altw_meta_node_blobs", node_blobs as i64);
        crate::debug::emit_counter("altw_meta_way_blobs", way_blobs as i64);
        crate::debug::emit_counter("altw_meta_relation_blobs", relation_blobs as i64);
    }
    Ok(metas)
}
