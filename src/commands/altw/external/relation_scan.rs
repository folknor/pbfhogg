//! Relation-member node-ID collection for ALTW external join.
//!
//! The generic `collect_relation_member_node_ids` in `add_locations_to_ways.rs`
//! reads every blob sequentially via `BlobReader::next()` - at planet scale
//! that's ~100 GB of compressed bytes preread (not decompressed) just to find
//! the ~2K relation blobs. External join already has a full `BlobMeta` table,
//! so this variant preads only the relation blobs directly and decompresses
//! each via `decompress_blob_raw`.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Arc;

use crate::blob_meta::ElemKind;
use crate::block::PrimitiveBlock;
use crate::elements::{Element, MemberId};

use super::super::Result;
use super::blob_meta::BlobMeta;
use crate::idset::IdSet;

pub(super) fn collect_relation_member_node_ids_indexed(
    input: &Path,
    blob_meta: &[BlobMeta],
) -> Result<IdSet> {
    let file = Arc::new(
        std::fs::File::open(input).map_err(|e| format!("open pbf for relation scan: {e}"))?,
    );

    let mut ids = IdSet::new();
    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    for meta in blob_meta
        .iter()
        .filter(|m| matches!(m.kind, ElemKind::Relation))
    {
        read_buf.resize(meta.data_size, 0);
        file.read_exact_at(&mut read_buf, meta.data_offset)
            .map_err(|e| format!("relation scan pread: {e}"))?;
        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
            .map_err(|e| format!("relation scan decompress: {e}"))?;
        let block = PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf),
            &mut st_scratch,
            &mut gr_scratch,
        )?;
        for element in block.elements_skip_metadata() {
            if let Element::Relation(r) = element {
                for member in r.members() {
                    if let MemberId::Node(id) = member.id
                        && id >= 0
                    {
                        ids.set(id);
                    }
                }
            }
        }
    }

    Ok(ids)
}
