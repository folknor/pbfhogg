//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Uses blob-level permutation sort: index each blob's element type + ID range,
//! sort the index, write in sorted order. Non-overlapping blobs pass through as
//! raw bytes; overlapping blobs are decoded, sorted, and re-encoded.
//! Memory usage is O(num_blobs) instead of O(num_elements).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::blob::{
    decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
    parse_blob_header_with_index, BlobKind,
};
use crate::blob_index::{BlobIndex, ElemKind, scan_block_ids};
use crate::block_builder::{HeaderBuilder, BlockBuilder, MemberData, Metadata};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::{reframe_raw_with_index, Compression, PbfWriter};
use crate::{Element, MemberId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Statistics from a sort operation.
pub struct SortStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    pub blobs_total: u64,
}

impl SortStats {
    pub fn print_summary(&self) {
        eprintln!(
            "Sorted {} nodes, {} ways, {} relations ({} blobs: {} passthrough, {} rewritten)",
            self.nodes, self.ways, self.relations,
            self.blobs_total, self.blobs_passthrough, self.blobs_rewritten,
        );
    }
}

// ---------------------------------------------------------------------------
// Blob index
// ---------------------------------------------------------------------------

/// Blob-level index entry for permutation sort.
struct BlobEntry {
    /// Byte offset of the complete frame in the input file.
    file_offset: u64,
    /// Length of the complete frame (4 + header_len + data_size).
    frame_len: u64,
    /// Element type + ID range.
    index: BlobIndex,
    /// Whether the BlobHeader already has indexdata embedded.
    has_indexdata: bool,
}

// ---------------------------------------------------------------------------
// Owned element types (needed for overlap-run decode + re-encode).
// Vec fields are not converted to Box<[T]> — these are transient (decoded,
// sorted, re-encoded per overlap run), not long-lived allocations.
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sort a PBF file into standard order (nodes → ways → relations, by ID).
///
/// Uses blob-level permutation sort: O(num_blobs) memory. Non-overlapping
/// blobs are passed through as raw bytes; overlapping blobs are decoded,
/// sorted, and re-encoded.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn sort(input: &Path, output: &Path, compression: Compression, direct_io: bool) -> Result<SortStats> {
    // Pass 1: Build blob index
    eprintln!("Pass 1: indexing blobs...");
    let (header, mut entries) = build_blob_index(input, direct_io)?;
    eprintln!("  {} OSMData blobs indexed", entries.len());

    // Sort by (type_order, min_id)
    entries.sort_by_key(|e| (type_order(e.index.kind), e.index.min_id));

    // Detect overlaps
    let overlaps = detect_overlaps(&entries);
    let overlap_count = overlaps.iter().filter(|&&b| b).count();
    if overlap_count > 0 {
        eprintln!("  {overlap_count} blobs in overlap runs (decode + re-encode)");
    }

    // Pass 2: Write in sorted order
    eprintln!("Pass 2: writing sorted output...");
    let header_bytes = HeaderBuilder::from_header(&header).sorted().build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    // Open input for random-access reads
    let mut input_file = File::open(input)?;

    // copy_file_range setup
    #[cfg(feature = "linux-direct-io")]
    let input_fd = {
        use std::os::unix::io::AsRawFd;
        input_file.as_raw_fd()
    };
    #[cfg(not(feature = "linux-direct-io"))]
    let input_fd = 0i32;
    // Sort always uses buffered output, so copy_file_range is always safe.
    #[allow(unused_variables)]
    let use_copy_range = cfg!(feature = "linux-direct-io");

    let mut stats = SortStats {
        nodes: 0,
        ways: 0,
        relations: 0,
        blobs_passthrough: 0,
        blobs_rewritten: 0,
        blobs_total: entries.len() as u64,
    };

    let mut bb = BlockBuilder::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    let mut i = 0;
    while i < entries.len() {
        if overlaps[i] {
            let start = i;
            while i < entries.len() && overlaps[i] {
                i += 1;
            }
            write_overlap_run(
                &entries[start..i],
                &mut input_file,
                &mut bb,
                &mut writer,
                &mut stats,
            )?;
        } else {
            write_passthrough_blob(
                &entries[i],
                &mut input_file,
                &mut writer,
                &mut frame_buf,
                input_fd,
                use_copy_range,
            )?;
            count_entry(&entries[i], &mut stats);
            stats.blobs_passthrough += 1;
            i += 1;
        }
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Pass 1: Build blob index
// ---------------------------------------------------------------------------

/// Build a blob-level index of the input file.
///
/// Reads sequentially, extracting element type + ID range for each OSMData
/// blob. Blobs with indexdata are classified without decompression; others
/// are decompressed and scanned with `scan_block_ids`.
fn build_blob_index(
    input: &Path,
    direct_io: bool,
) -> Result<(crate::HeaderBlock, Vec<BlobEntry>)> {
    let mut reader = FileReader::open(input, direct_io)?;
    let mut entries = Vec::new();
    let mut header: Option<crate::HeaderBlock> = None;
    let mut file_offset: u64 = 0;
    let mut decompress_buf: Vec<u8> = Vec::new();

    loop {
        let frame_start = file_offset;

        // Read 4-byte header length
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        #[allow(clippy::cast_possible_truncation)]
        let header_len = u32::from_be_bytes(len_buf) as usize;

        // Read BlobHeader
        let mut header_bytes = vec![0u8; header_len];
        reader.read_exact(&mut header_bytes)?;

        let (blob_type, data_size, raw_index) =
            parse_blob_header_with_index(&header_bytes)?;
        let index = raw_index.as_ref().and_then(|d| BlobIndex::deserialize(d));
        let has_indexdata = index.is_some();

        // Read Blob bytes
        let mut blob_bytes = vec![0u8; data_size];
        reader.read_exact(&mut blob_bytes)?;

        let frame_len = (4 + header_len + data_size) as u64;
        file_offset += frame_len;

        match &blob_type {
            BlobKind::OsmHeader
                if header.is_none() =>
            {
                header = Some(decode_blob_to_headerblock(&blob_bytes)?);
            }
            BlobKind::OsmData => {
                let blob_index = if let Some(idx) = index {
                    idx
                } else {
                    decompress_blob_data_into(&blob_bytes, &mut decompress_buf)?;
                    scan_block_ids(&decompress_buf).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "failed to scan block IDs",
                        )
                    })?
                };
                entries.push(BlobEntry {
                    file_offset: frame_start,
                    frame_len,
                    index: blob_index,
                    has_indexdata,
                });
            }
            _ => {}
        }
    }

    let header = header.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no OSMHeader blob found")
    })?;

    Ok((header, entries))
}

// ---------------------------------------------------------------------------
// Sort + overlap detection
// ---------------------------------------------------------------------------

fn type_order(kind: ElemKind) -> u8 {
    match kind {
        ElemKind::Node => 0,
        ElemKind::Way => 1,
        ElemKind::Relation => 2,
    }
}

/// Detect overlapping blob runs in sorted entries.
///
/// Two adjacent blobs of the same type overlap if the first's max_id >=
/// the second's min_id. Returns a boolean vec where `true` marks entries
/// that must be decoded and re-encoded.
fn detect_overlaps(entries: &[BlobEntry]) -> Vec<bool> {
    let mut overlaps = vec![false; entries.len()];
    for i in 0..entries.len().saturating_sub(1) {
        if entries[i].index.kind == entries[i + 1].index.kind
            && entries[i].index.max_id >= entries[i + 1].index.min_id
        {
            overlaps[i] = true;
            overlaps[i + 1] = true;
        }
    }
    overlaps
}

// ---------------------------------------------------------------------------
// Pass 2: Passthrough write
// ---------------------------------------------------------------------------

/// Write a non-overlapping blob as raw bytes, adding indexdata if missing.
#[allow(unused_variables)]
fn write_passthrough_blob(
    entry: &BlobEntry,
    input_file: &mut File,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
    input_fd: i32,
    use_copy_range: bool,
) -> Result<()> {
    if entry.has_indexdata {
        // Already has indexdata — pure passthrough
        #[cfg(feature = "linux-direct-io")]
        if use_copy_range {
            writer.write_raw_copy(input_fd, entry.file_offset, entry.frame_len)?;
            return Ok(());
        }
        read_frame_into(input_file, entry, frame_buf)?;
        writer.write_raw(frame_buf)?;
    } else {
        // Reframe with indexdata before writing
        read_frame_into(input_file, entry, frame_buf)?;
        let blob_bytes = extract_blob_bytes(frame_buf)?;
        let reframed = reframe_raw_with_index(blob_bytes, &entry.index.serialize())?;
        writer.write_raw(&reframed)?;
    }
    Ok(())
}

/// Read a complete frame from the input file at the given offset into `buf`.
fn read_frame_into(
    file: &mut File,
    entry: &BlobEntry,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(entry.file_offset))?;
    #[allow(clippy::cast_possible_truncation)]
    let len = entry.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    file.read_exact(buf)?;
    Ok(())
}

/// Extract the raw Blob bytes from a complete frame.
///
/// Frame layout: `[4-byte header_len][BlobHeader][Blob]`.
fn extract_blob_bytes(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 4 {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "frame too short").into(),
        );
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "invalid header length").into(),
        );
    }
    Ok(&frame[blob_start..])
}

/// Add element counts from a blob entry to stats.
fn count_entry(entry: &BlobEntry, stats: &mut SortStats) {
    match entry.index.kind {
        ElemKind::Node => stats.nodes += entry.index.count,
        ElemKind::Way => stats.ways += entry.index.count,
        ElemKind::Relation => stats.relations += entry.index.count,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: Overlap run rewrite
// ---------------------------------------------------------------------------

/// Decode all blobs in an overlap run, sort elements by ID, and write.
fn write_overlap_run(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut SortStats,
) -> Result<()> {
    let mut nodes: Vec<OwnedNode> = Vec::new();
    let mut ways: Vec<OwnedWay> = Vec::new();
    let mut relations: Vec<OwnedRelation> = Vec::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    // Decode all blobs in the run
    for entry in entries {
        read_frame_into(input_file, entry, &mut frame_buf)?;
        let blob_bytes = extract_blob_bytes(&frame_buf)?;
        let block = decode_blob_to_primitiveblock(blob_bytes)?;

        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => nodes.push(read_dense_node(dn)),
                Element::Node(n) => nodes.push(read_node(n)),
                Element::Way(w) => ways.push(read_way(w)),
                Element::Relation(r) => relations.push(read_relation(r)),
            }
        }
    }

    // Sort and write each type
    if !nodes.is_empty() {
        nodes.sort_by_key(|n| n.id);
        stats.nodes += nodes.len() as u64;
        write_owned_nodes(&nodes, bb, writer)?;
    }
    if !ways.is_empty() {
        ways.sort_by_key(|w| w.id);
        stats.ways += ways.len() as u64;
        write_owned_ways(&ways, bb, writer)?;
    }
    if !relations.is_empty() {
        relations.sort_by_key(|r| r.id);
        stats.relations += relations.len() as u64;
        write_owned_relations(&relations, bb, writer)?;
    }

    stats.blobs_rewritten += entries.len() as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// Write owned elements via BlockBuilder
// ---------------------------------------------------------------------------

fn write_owned_nodes(
    nodes: &[OwnedNode],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    for node in nodes {
        if !bb.can_add_node() {
            flush_block(bb, writer)?;
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
    flush_block(bb, writer)
}

fn write_owned_ways(
    ways: &[OwnedWay],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    for way in ways {
        if !bb.can_add_way() {
            flush_block(bb, writer)?;
        }
        let tags: Vec<(&str, &str)> = way
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let meta = owned_to_metadata(way.metadata.as_ref());
        bb.add_way(way.id, &tags, &way.refs, meta.as_ref());
    }
    flush_block(bb, writer)
}

fn write_owned_relations(
    relations: &[OwnedRelation],
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    for rel in relations {
        if !bb.can_add_relation() {
            flush_block(bb, writer)?;
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
    flush_block(bb, writer)
}

// ---------------------------------------------------------------------------
// Element readers (borrow → owned for overlap-run decode)
// ---------------------------------------------------------------------------

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

use super::flush_block;
