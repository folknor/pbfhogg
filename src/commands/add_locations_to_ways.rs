//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::block_builder::{build_header, BlockBuilder, MemberData, Metadata};
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
pub struct Stats {
    pub nodes_read: u64,
    pub nodes_written: u64,
    pub nodes_dropped: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    pub missing_locations: u64,
}

impl Stats {
    /// Print a summary of the operation to stderr.
    pub fn print_summary(&self) {
        eprintln!(
            "add-locations-to-ways: {} nodes read, {} written, {} dropped, \
             {} ways, {} relations, {} missing locations",
            self.nodes_read,
            self.nodes_written,
            self.nodes_dropped,
            self.ways_written,
            self.relations_written,
            self.missing_locations,
        );
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Embed node coordinates into ways.
///
/// Two-pass algorithm:
/// 1. Read all nodes and build a coordinate index.
/// 2. Re-read the input and write to output, attaching coordinates to ways.
///
/// If `keep_untagged_nodes` is false, nodes with zero tags are omitted from
/// the output (their coordinates are still used for ways).
pub fn add_locations_to_ways(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
) -> Result<Stats> {
    let index = build_node_index(input)?;
    write_output(input, output, &index, keep_untagged_nodes)
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

fn build_node_index(input: &Path) -> Result<HashMap<i64, (i32, i32)>> {
    let mut index: HashMap<i64, (i32, i32)> = HashMap::new();
    let reader = BlobReader::from_path(input)?;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(_) | BlobDecode::Unknown(_) => {}
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            index.insert(dn.id(), (dn.decimicro_lat(), dn.decimicro_lon()));
                        }
                        Element::Node(n) => {
                            index.insert(n.id(), (n.decimicro_lat(), n.decimicro_lon()));
                        }
                        Element::Way(_) | Element::Relation(_) => {}
                    }
                }
            }
        }
    }

    Ok(index)
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------

fn write_output(
    input: &Path,
    output: &Path,
    index: &HashMap<i64, (i32, i32)>,
    keep_untagged_nodes: bool,
) -> Result<Stats> {
    let mut stats = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
    };

    let mut writer = PbfWriter::to_path(output, Compression::default())?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;

    let reader = BlobReader::from_path(input)?;
    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                if !header_written {
                    write_header(&header, &mut writer)?;
                    header_written = true;
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    write_element(
                        &element,
                        index,
                        keep_untagged_nodes,
                        &mut bb,
                        &mut writer,
                        &mut stats,
                    )?;
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Element dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_element(
    element: &Element<'_>,
    index: &HashMap<i64, (i32, i32)>,
    keep_untagged_nodes: bool,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
    stats: &mut Stats,
) -> Result<()> {
    match element {
        Element::DenseNode(dn) => {
            stats.nodes_read += 1;
            let has_tags = dn.tags().next().is_some();
            if keep_untagged_nodes || has_tags {
                write_dense_node(dn, bb, writer)?;
                stats.nodes_written += 1;
            } else {
                stats.nodes_dropped += 1;
            }
        }
        Element::Node(n) => {
            stats.nodes_read += 1;
            let has_tags = n.tags().next().is_some();
            if keep_untagged_nodes || has_tags {
                write_node(n, bb, writer)?;
                stats.nodes_written += 1;
            } else {
                stats.nodes_dropped += 1;
            }
        }
        Element::Way(w) => {
            write_way_with_locations(w, index, bb, writer, stats)?;
        }
        Element::Relation(r) => {
            write_relation(r, bb, writer)?;
            stats.relations_written += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn write_header(
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    let bbox = header.bbox().map(|b| (b.left, b.bottom, b.right, b.top));
    let header_bytes = build_header(
        bbox,
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
        &["LocationsOnWays"],
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Element writers
// ---------------------------------------------------------------------------

fn write_dense_node(
    dn: &crate::DenseNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = dn.tags().collect();
    let meta = dn.info().and_then(|info| {
        let user = info.user().ok()?;
        Some(Metadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: info.changeset(),
            uid: info.uid(),
            user,
            visible: info.visible(),
        })
    });
    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags, meta.as_ref());
    Ok(())
}

fn write_node(
    n: &crate::Node,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = n.tags().collect();
    let info = n.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags, meta.as_ref());
    Ok(())
}

fn write_way_with_locations(
    w: &crate::Way,
    index: &HashMap<i64, (i32, i32)>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
    stats: &mut Stats,
) -> Result<()> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = w.tags().collect();
    let refs: Vec<i64> = w.refs().collect();
    let locations: Vec<(i32, i32)> = refs
        .iter()
        .map(|node_id| {
            match index.get(node_id) {
                Some(&loc) => loc,
                None => {
                    stats.missing_locations += 1;
                    (0, 0)
                }
            }
        })
        .collect();
    let info = w.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_way_with_locations(w.id(), &tags, &refs, &locations, meta.as_ref());
    stats.ways_written += 1;
    Ok(())
}

fn write_relation(
    r: &crate::Relation,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
    }
    let tags: Vec<(&str, &str)> = r.tags().collect();
    let members: Vec<MemberData<'_>> = r
        .members()
        .map(|m| MemberData {
            id: m.id,
            role: m.role().unwrap_or(""),
        })
        .collect();
    let info = r.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info
            .user()
            .and_then(std::result::Result::ok)
            .unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_relation(r.id(), &tags, &members, meta.as_ref());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<io::BufWriter<File>>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}
