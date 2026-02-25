pub mod add_locations_to_ways;
pub mod cat;
pub mod check_refs;
pub mod derive_changes;
pub mod diff;
pub mod extract;
pub mod fileinfo;
pub mod getid;
pub mod merge;
pub(crate) mod owned_elements;
pub mod sort;
pub mod tags_count;
pub mod tags_filter;

use crate::block_builder::{build_header, BlockBuilder};
use crate::file_writer::FileWriter;
use crate::writer::PbfWriter;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Flush the current block from a [`BlockBuilder`] into a [`PbfWriter`].
///
/// If the builder has accumulated elements, `take()` serializes them into a
/// protobuf `PrimitiveBlock` and the bytes are written as a blob. If the
/// builder is empty, this is a no-op.
pub(crate) fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}

/// Re-encode a [`HeaderBlock`](crate::HeaderBlock) and write it to a [`PbfWriter`].
///
/// Preserves bbox, replication timestamp/sequence/URL from the input header.
/// Used by commands that copy a PBF while transforming its data blocks.
pub(crate) fn rebuild_header(
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    let bbox = header.bbox().map(|b| (b.left, b.bottom, b.right, b.top));
    let header_bytes = build_header(
        bbox,
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
        &[],
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}
