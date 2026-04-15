//! Shared OsmData blob metadata scan for ALTW external join.
//!
//! Replaces repeated header-only scans in stage 1 and stage 4 with one
//! reusable pass over blob headers.

use std::path::Path;

use crate::blob_index::ElemKind;

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
/// external join actually reuses later.
pub(super) fn scan_blob_metadata(
    input: &Path,
    parse_tagdata: bool,
) -> Result<Vec<BlobMeta>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.set_parse_tagdata(parse_tagdata);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut metas = Vec::new();
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, frame_offset, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        let idx = hdr.index().ok_or("external join metadata scan: OsmData blob missing indexdata")?;
        let tag_index = if parse_tagdata { hdr.tag_index() } else { None };
        let has_tagindex = tag_index.is_some();
        let has_tags = if parse_tagdata {
            tag_index.is_none_or(|ti| !ti.keys_empty())
        } else {
            false
        };
        metas.push(BlobMeta {
            frame_offset,
            data_offset,
            data_size,
            kind: idx.kind,
            min_id: idx.min_id,
            max_id: idx.max_id,
            count: idx.count,
            has_tagindex,
            has_tags,
        });
    }

    Ok(metas)
}
