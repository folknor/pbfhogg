//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Standard PBF order: all nodes sorted by ID, then all ways sorted by ID,
//! then all relations sorted by ID.

use std::path::Path;

use crate::block_builder::{build_header, BlockBuilder, MemberData, Metadata};
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element, MemberId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Statistics from a sort operation.
pub struct SortStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

impl SortStats {
    pub fn print_summary(&self) {
        eprintln!(
            "Sorted {} nodes, {} ways, {} relations",
            self.nodes, self.ways, self.relations,
        );
    }
}

// ---------------------------------------------------------------------------
// Owned element types (needed because Element borrows are block-scoped)
// ---------------------------------------------------------------------------

struct OwnedMetadata {
    version: i32,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user: String,
    visible: bool,
}

struct OwnedNode {
    id: i64,
    decimicro_lat: i32,
    decimicro_lon: i32,
    tags: Vec<(String, String)>,
    metadata: Option<OwnedMetadata>,
}

struct OwnedWay {
    id: i64,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    metadata: Option<OwnedMetadata>,
}

struct OwnedMember {
    id: MemberId,
    role: String,
}

struct OwnedRelation {
    id: i64,
    tags: Vec<(String, String)>,
    members: Vec<OwnedMember>,
    metadata: Option<OwnedMetadata>,
}

/// All elements read from the input file.
struct ReadResult {
    header: Option<crate::HeaderBlock>,
    nodes: Vec<OwnedNode>,
    ways: Vec<OwnedWay>,
    relations: Vec<OwnedRelation>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sort a PBF file into standard order (nodes → ways → relations, by ID).
///
/// Reads the entire file into memory, sorts, and writes the output.
/// Suitable for files that fit in RAM (typically up to ~1 GB PBF).
#[hotpath::measure]
pub fn sort(input: &Path, output: &Path, direct_io: bool) -> Result<SortStats> {
    let mut data = read_elements(input, direct_io)?;

    // Sort each type by ID
    data.nodes.sort_by_key(|n| n.id);
    data.ways.sort_by_key(|w| w.id);
    data.relations.sort_by_key(|r| r.id);

    let stats = SortStats {
        nodes: data.nodes.len() as u64,
        ways: data.ways.len() as u64,
        relations: data.relations.len() as u64,
    };

    write_sorted(output, &data)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Phase 1: Read all elements into owned vectors
// ---------------------------------------------------------------------------

fn read_elements(input: &Path, direct_io: bool) -> Result<ReadResult> {
    let reader = BlobReader::open(input, direct_io)?;
    let mut nodes: Vec<OwnedNode> = Vec::new();
    let mut ways: Vec<OwnedWay> = Vec::new();
    let mut relations: Vec<OwnedRelation> = Vec::new();
    let mut header: Option<crate::HeaderBlock> = None;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(h) => {
                if header.is_none() {
                    header = Some(*h);
                }
            }
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            nodes.push(read_dense_node(dn));
                        }
                        Element::Node(n) => {
                            nodes.push(read_node(n));
                        }
                        Element::Way(w) => {
                            ways.push(read_way(w));
                        }
                        Element::Relation(r) => {
                            relations.push(read_relation(r));
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    Ok(ReadResult {
        header,
        nodes,
        ways,
        relations,
    })
}

fn read_dense_node(dn: &crate::DenseNode<'_>) -> OwnedNode {
    OwnedNode {
        id: dn.id(),
        decimicro_lat: dn.decimicro_lat(),
        decimicro_lon: dn.decimicro_lon(),
        tags: dn
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        metadata: dn.info().and_then(|info| {
            Some(OwnedMetadata {
                version: info.version(),
                timestamp: info.milli_timestamp() / 1000,
                changeset: info.changeset(),
                uid: info.uid(),
                user: info.user().ok()?.to_owned(),
                visible: info.visible(),
            })
        }),
    }
}

fn read_node(n: &crate::Node<'_>) -> OwnedNode {
    let info = n.info();
    OwnedNode {
        id: n.id(),
        decimicro_lat: n.decimicro_lat(),
        decimicro_lon: n.decimicro_lon(),
        tags: n
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

fn read_way(w: &crate::Way<'_>) -> OwnedWay {
    let info = w.info();
    OwnedWay {
        id: w.id(),
        tags: w
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        refs: w.refs().collect(),
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

fn read_relation(r: &crate::Relation<'_>) -> OwnedRelation {
    let info = r.info();
    OwnedRelation {
        id: r.id(),
        tags: r
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        members: r
            .members()
            .map(|m| OwnedMember {
                id: m.id,
                role: m.role().unwrap_or("").to_owned(),
            })
            .collect(),
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Write sorted output
// ---------------------------------------------------------------------------

fn write_sorted(output: &Path, data: &ReadResult) -> Result<()> {
    let mut writer = PbfWriter::to_path(output, Compression::default())?;

    // Header
    if let Some(ref header) = data.header {
        let bbox = header.bbox().map(|b| (b.left, b.bottom, b.right, b.top));
        let header_bytes = build_header(
            bbox,
            header.osmosis_replication_timestamp(),
            header.osmosis_replication_sequence_number(),
            header.osmosis_replication_base_url(),
            &[],
        )?;
        writer.write_header(&header_bytes)?;
    } else {
        let header_bytes = build_header(None, None, None, None, &[])?;
        writer.write_header(&header_bytes)?;
    }

    let mut bb = BlockBuilder::new();

    // Write nodes
    for node in &data.nodes {
        if !bb.can_add_node() {
            flush_block(&mut bb, &mut writer)?;
        }
        let tags: Vec<(&str, &str)> = node
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let meta = owned_to_metadata(node.metadata.as_ref());
        bb.add_node(
            node.id,
            node.decimicro_lat,
            node.decimicro_lon,
            &tags,
            meta.as_ref(),
        );
    }
    flush_block(&mut bb, &mut writer)?;

    // Write ways
    for way in &data.ways {
        if !bb.can_add_way() {
            flush_block(&mut bb, &mut writer)?;
        }
        let tags: Vec<(&str, &str)> = way
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let meta = owned_to_metadata(way.metadata.as_ref());
        bb.add_way(way.id, &tags, &way.refs, meta.as_ref());
    }
    flush_block(&mut bb, &mut writer)?;

    // Write relations
    for rel in &data.relations {
        if !bb.can_add_relation() {
            flush_block(&mut bb, &mut writer)?;
        }
        let tags: Vec<(&str, &str)> = rel
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let members: Vec<MemberData<'_>> = rel
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: &m.role,
            })
            .collect();
        let meta = owned_to_metadata(rel.metadata.as_ref());
        bb.add_relation(rel.id, &tags, &members, meta.as_ref());
    }
    flush_block(&mut bb, &mut writer)?;

    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn owned_to_metadata(meta: Option<&OwnedMetadata>) -> Option<Metadata<'_>> {
    meta.map(|m| Metadata {
        version: m.version,
        timestamp: m.timestamp,
        changeset: m.changeset,
        uid: m.uid,
        user: &m.user,
        visible: m.visible,
    })
}

fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}
