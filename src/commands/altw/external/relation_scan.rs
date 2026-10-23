//! Relation-member node-ID collection for ALTW external join.
//!
//! External join has a shared `BlobMeta` table, so this scan preads only
//! relation blobs rather than walking every blob. It decompresses each
//! selected blob via `decompress_blob_raw`.

use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Arc;

use crate::ElementKind;
use crate::block::PrimitiveBlock;

use super::super::Result;
use super::blob_meta::BlobMeta;
use crate::idset::IdSet;

pub(super) struct RelationScanOutput {
    pub(super) member_node_ids: Option<IdSet>,
    pub(super) member_way_ids: Option<IdSet>,
}

#[hotpath::measure]
pub(super) fn collect_relation_member_ids_indexed(
    input: &Path,
    blob_meta: &[BlobMeta],
    want_nodes: bool,
    want_ways: bool,
) -> Result<RelationScanOutput> {
    let file = Arc::new(
        std::fs::File::open(input)
            .map_err(|e| crate::Error::from(e).context("open pbf for relation scan"))?,
    );
    // Errors are located against the input once, not per blob.
    let source: Arc<Path> = Arc::from(input);

    let mut node_ids = want_nodes.then(IdSet::new);
    let mut way_ids = want_ways.then(IdSet::new);
    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    for meta in blob_meta
        .iter()
        .filter(|m| matches!(m.kind, ElementKind::Relation))
    {
        read_buf.resize(meta.data_size, 0);
        let locate = |e: crate::Error| {
            e.locate(crate::error::Location::new(
                Some(&source),
                None,
                Some(crate::ByteOffset(meta.frame_offset)),
            ))
        };
        file.read_exact_at(&mut read_buf, meta.data_offset)
            .map_err(|e| locate(crate::Error::from(e).context("relation scan pread")))?;
        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
            .map_err(|e| locate(e.context("relation scan decompress")))?;
        let block = PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf),
            &mut st_scratch,
            &mut gr_scratch,
        )
        .map_err(locate)?;
        super::super::relation_member_ids(&block, want_nodes, want_ways, |way, id| {
            if way {
                way_ids.as_mut().expect("requested").set(id);
            } else {
                node_ids.as_mut().expect("requested").set(id);
            }
        });
    }

    Ok(RelationScanOutput {
        member_node_ids: node_ids,
        member_way_ids: way_ids,
    })
}
