pub mod add_locations_to_ways;
pub mod cat;
#[cfg(feature = "commands")]
pub mod check_refs;
pub mod derive_changes;
pub mod diff;
#[cfg(feature = "commands")]
pub mod extract;
pub mod fileinfo;
pub mod getid;
pub mod merge;
pub mod node_stats;
pub(crate) mod owned_elements;
pub mod sort;
pub mod tags_count;
pub mod tags_filter;

use crate::block_builder::{HeaderBuilder, BlockBuilder, Metadata, RawMetadata};
use crate::file_writer::FileWriter;
use crate::writer::PbfWriter;

// Box<dyn Error> is intentional — commands are CLI internals, callers only display
// errors and exit. Typed error enums would add complexity with no matching benefit.
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
        writer.write_primitive_block(bytes)?;
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
    sorted: bool,
) -> Result<()> {
    let mut hb = HeaderBuilder::from_header(header);
    if sorted {
        hb = hb.sorted();
    }
    writer.write_header(&hb.build()?)?;
    Ok(())
}

/// Extract [`Metadata`] from an [`Info`](crate::Info) (Node/Way/Relation).
///
/// Returns `None` if the info block has no version. On `user()` error (string
/// table corruption), defaults to empty string.
pub(crate) fn element_metadata<'a>(info: &crate::Info<'a>) -> Option<Metadata<'a>> {
    info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info.user().and_then(std::result::Result::ok).unwrap_or(""),
        visible: info.visible(),
    })
}

/// Extract [`Metadata`] from a [`DenseNode`](crate::DenseNode).
///
/// Returns `None` if the node has no info block. On `user()` error (string
/// table corruption), defaults to empty string — consistent with the
/// Node/Way/Relation path.
pub(crate) fn dense_node_metadata<'a>(dn: &'a crate::DenseNode<'a>) -> Option<Metadata<'a>> {
    dn.info().map(|info| Metadata {
        version: info.version(),
        timestamp: info.milli_timestamp() / 1000,
        changeset: info.changeset(),
        uid: info.uid(),
        user: info.user().unwrap_or(""),
        visible: info.visible(),
    })
}

/// Extract [`RawMetadata`] from an [`Info`](crate::Info), preserving the raw
/// string table index for the user name.
pub(crate) fn element_raw_metadata(info: &crate::Info<'_>) -> Option<RawMetadata> {
    info.version().map(|v| RawMetadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user_sid: info.raw_user_sid().unwrap_or(0),
        visible: info.visible(),
    })
}

/// Extract [`RawMetadata`] from a [`DenseNode`](crate::DenseNode), preserving
/// the raw string table index for the user name.
pub(crate) fn dense_node_raw_metadata(dn: &crate::DenseNode<'_>) -> Option<RawMetadata> {
    dn.info().map(|info| RawMetadata {
        version: info.version(),
        timestamp: info.milli_timestamp() / 1000,
        changeset: info.changeset(),
        uid: info.uid(),
        user_sid: info.raw_user_sid(),
        visible: info.visible(),
    })
}
