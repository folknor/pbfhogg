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
#[hotpath::measure]
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

#[allow(clippy::too_many_lines)]
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
                // Reusable buffers for element data, hoisted outside the element loop.
                //
                // WHY: Without hoisting, each element allocates fresh Vecs via .collect(),
                // producing N allocations where N = number of elements. For Denmark (~50M
                // elements), that is ~150M alloc/dealloc pairs across the 3 buffer types
                // (tags + refs + members), plus ~8M more for the locations buffer on ways.
                //
                // HOW: Vec::clear() sets len to 0 but keeps the underlying heap allocation.
                // The subsequent extend() refills the buffer without reallocating once the
                // capacity is warm (i.e. after the first few elements in each block).
                //
                // These buffers grow to the size of the largest element in the block and
                // stabilize — there is no unbounded growth because PBF blocks have a max
                // of 8000 entities. They are scoped to the OsmData arm so that the borrowed
                // string references (which point into `block`) do not outlive the block.
                let mut tags_buf: Vec<(&str, &str)> = Vec::new();
                let mut refs_buf: Vec<i64> = Vec::new();
                let mut members_buf: Vec<MemberData<'_>> = Vec::new();
                let mut locations_buf: Vec<(i32, i32)> = Vec::new();

                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            stats.nodes_read += 1;
                            let has_tags = dn.tags().next().is_some();
                            if keep_untagged_nodes || has_tags {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(dn.tags());
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
                                bb.add_node(
                                    dn.id(),
                                    dn.decimicro_lat(),
                                    dn.decimicro_lon(),
                                    &tags_buf,
                                    meta.as_ref(),
                                );
                                stats.nodes_written += 1;
                            } else {
                                stats.nodes_dropped += 1;
                            }
                        }
                        Element::Node(n) => {
                            stats.nodes_read += 1;
                            let has_tags = n.tags().next().is_some();
                            if keep_untagged_nodes || has_tags {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(n.tags());
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
                                bb.add_node(
                                    n.id(),
                                    n.decimicro_lat(),
                                    n.decimicro_lon(),
                                    &tags_buf,
                                    meta.as_ref(),
                                );
                                stats.nodes_written += 1;
                            } else {
                                stats.nodes_dropped += 1;
                            }
                        }
                        Element::Way(w) => {
                            if !bb.can_add_way() {
                                flush_block(&mut bb, &mut writer)?;
                            }
                            tags_buf.clear();
                            tags_buf.extend(w.tags());
                            refs_buf.clear();
                            refs_buf.extend(w.refs());
                            locations_buf.clear();
                            locations_buf.extend(refs_buf.iter().map(|node_id| {
                                match index.get(node_id) {
                                    Some(&loc) => loc,
                                    None => {
                                        stats.missing_locations += 1;
                                        (0, 0)
                                    }
                                }
                            }));
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
                            bb.add_way_with_locations(
                                w.id(),
                                &tags_buf,
                                &refs_buf,
                                &locations_buf,
                                meta.as_ref(),
                            );
                            stats.ways_written += 1;
                        }
                        Element::Relation(r) => {
                            if !bb.can_add_relation() {
                                flush_block(&mut bb, &mut writer)?;
                            }
                            tags_buf.clear();
                            tags_buf.extend(r.tags());
                            members_buf.clear();
                            members_buf.extend(r.members().map(|m| MemberData {
                                id: m.id,
                                role: m.role().unwrap_or(""),
                            }));
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
                            bb.add_relation(
                                r.id(),
                                &tags_buf,
                                &members_buf,
                                meta.as_ref(),
                            );
                            stats.relations_written += 1;
                        }
                    }
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
