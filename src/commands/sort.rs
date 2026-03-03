//! Sort a PBF file into standard order. Equivalent to `osmium sort`.
//!
//! Uses blob-level permutation sort: index each blob's element type + ID range,
//! sort the index, write in sorted order. Non-overlapping blobs pass through as
//! raw bytes; overlapping blobs are decoded and merged via a streaming sweep.
//! Memory usage is O(num_blobs) for non-overlapping inputs. Overlapping blobs
//! use a streaming sweep merge with O(overlap_depth) memory.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::blob::{
    decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
    parse_blob_header_with_index, BlobKind,
};
use crate::blob_index::{BlobIndex, ElemKind, scan_block_ids};
use crate::block_builder::{BlockBuilder, MemberData, Metadata};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::{reframe_raw_with_index, Compression, PbfWriter};
use crate::{Element, MemberId};

use super::owned_elements::OwnedMember;

use super::{
    build_output_header, ensure_node_capacity, ensure_relation_capacity, ensure_way_capacity,
    flush_block, require_indexdata, Result, writer_from_header_bytes,
};

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
    /// Per-blob tag key data from BlobHeader field 4, preserved for passthrough.
    tagdata: Option<Box<[u8]>>,
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

impl PartialEq for OwnedNode {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}
impl Eq for OwnedNode {}
impl PartialOrd for OwnedNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.id.cmp(&other.id) }
}

struct OwnedWay {
    id: i64,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    metadata: Option<OwnedMetadata>,
}

impl PartialEq for OwnedWay {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}
impl Eq for OwnedWay {}
impl PartialOrd for OwnedWay {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedWay {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.id.cmp(&other.id) }
}

struct OwnedRelation {
    id: i64,
    tags: Vec<(String, String)>,
    members: Vec<OwnedMember>,
    metadata: Option<OwnedMetadata>,
}

impl PartialEq for OwnedRelation {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}
impl Eq for OwnedRelation {}
impl PartialOrd for OwnedRelation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedRelation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.id.cmp(&other.id) }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sort a PBF file into standard order (nodes → ways → relations, by ID).
///
/// Uses blob-level permutation sort: O(num_blobs) memory. Non-overlapping
/// blobs are passed through as raw bytes; overlapping blobs are decoded,
/// sorted, and re-encoded.
/// Options controlling sort I/O and compression behavior.
pub struct SortOptions {
    pub compression: Compression,
    pub direct_io: bool,
    pub io_uring: bool,
    pub sqpoll: bool,
    pub force: bool,
}

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn sort(input: &Path, output: &Path, opts: &SortOptions) -> Result<SortStats> {
    let SortOptions { compression, direct_io, io_uring, sqpoll, force } = *opts;
    require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to scan element IDs (significantly slower).")?;

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
    #[allow(clippy::redundant_closure_for_method_calls)]
    let header_bytes = build_output_header(&header, false, |hb| hb.sorted())?;
    let mut writer = writer_from_header_bytes(
        output,
        compression,
        &header_bytes,
        direct_io,
        io_uring,
        sqpoll,
    )?;

    // Open input for random-access reads
    let mut input_file = File::open(input)?;

    // copy_file_range / CopyRange setup
    #[cfg(feature = "linux-direct-io")]
    let input_fd = {
        use std::os::unix::io::AsRawFd;
        input_file.as_raw_fd()
    };
    #[cfg(not(feature = "linux-direct-io"))]
    let input_fd = 0i32;
    // io_uring uses linked ReadFixed+WriteFixed for CopyRange.
    // Buffered output supports copy_file_range. O_DIRECT cannot.
    #[allow(unused_variables)]
    let use_copy_range = io_uring || (!direct_io && cfg!(feature = "linux-direct-io"));

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
    let mut header_buf: Vec<u8> = Vec::new();
    let mut blob_buf: Vec<u8> = Vec::new();

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

        // Read BlobHeader (reuse buffer)
        header_buf.resize(header_len, 0);
        reader.read_exact(&mut header_buf)?;

        let (blob_type, data_size, raw_index, tagdata) =
            parse_blob_header_with_index(&header_buf)?;
        let index = raw_index.as_ref().and_then(|d| BlobIndex::deserialize(d));
        let has_indexdata = index.is_some();

        let frame_len = (4 + header_len + data_size) as u64;
        file_offset += frame_len;

        match &blob_type {
            BlobKind::OsmHeader if header.is_none() => {
                blob_buf.resize(data_size, 0);
                reader.read_exact(&mut blob_buf)?;
                header = Some(decode_blob_to_headerblock(&blob_buf)?);
            }
            BlobKind::OsmData if has_indexdata => {
                // Indexdata already in BlobHeader — skip blob payload entirely
                reader.skip(data_size as u64)?;
                #[allow(clippy::unwrap_used)]
                entries.push(BlobEntry {
                    file_offset: frame_start,
                    frame_len,
                    index: index.unwrap(),
                    has_indexdata,
                    tagdata,
                });
            }
            BlobKind::OsmData => {
                // No indexdata — must decompress and scan for element IDs
                blob_buf.resize(data_size, 0);
                reader.read_exact(&mut blob_buf)?;
                decompress_blob_data_into(&blob_buf, &mut decompress_buf)?;
                let blob_index = scan_block_ids(&decompress_buf).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "failed to scan block IDs",
                    )
                })?;
                entries.push(BlobEntry {
                    file_offset: frame_start,
                    frame_len,
                    index: blob_index,
                    has_indexdata,
                    tagdata,
                });
            }
            _ => {
                // Unknown or duplicate header blob — skip payload
                reader.skip(data_size as u64)?;
            }
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
        let reframed = reframe_raw_with_index(blob_bytes, &entry.index.serialize(), entry.tagdata.as_deref())?;
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

/// Decode all blobs in an overlap run and write elements in sorted order.
///
/// Uses a streaming sweep merge: walks entries by min_id, maintains a min-heap,
/// and flushes elements when their ID is guaranteed to be in final position
/// (i.e. smaller than all future blobs' min_id). Memory is O(overlap_depth)
/// rather than O(total_elements_in_run).
fn write_overlap_run(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut SortStats,
) -> Result<()> {
    // All entries in an overlap run have the same kind (entries are sorted by
    // type_order first, and detect_overlaps only marks adjacent same-type).
    let kind = entries[0].index.kind;
    match kind {
        ElemKind::Node => sweep_merge_nodes(entries, input_file, bb, writer, stats),
        ElemKind::Way => sweep_merge_ways(entries, input_file, bb, writer, stats),
        ElemKind::Relation => sweep_merge_rels(entries, input_file, bb, writer, stats),
    }?;
    stats.blobs_rewritten += entries.len() as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sweep merge per element type
// ---------------------------------------------------------------------------

fn sweep_merge_nodes(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut SortStats,
) -> Result<()> {
    let mut heap: BinaryHeap<Reverse<OwnedNode>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    for entry in entries {
        // Flush elements guaranteed in final position: ID < this blob's min_id
        flush_heap_below(&mut heap, entry.index.min_id, |node| {
            write_single_node(&node, bb, writer)?;
            stats.nodes += 1;
            Ok(())
        })?;

        // Decode blob and push elements into heap
        read_frame_into(input_file, entry, &mut frame_buf)?;
        let blob_bytes = extract_blob_bytes(&frame_buf)?;
        let block = decode_blob_to_primitiveblock(blob_bytes)?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn))),
                Element::Node(n) => heap.push(Reverse(read_node(n))),
                _ => {}
            }
        }
    }

    // Drain remaining
    while let Some(Reverse(node)) = heap.pop() {
        write_single_node(&node, bb, writer)?;
        stats.nodes += 1;
    }
    flush_block(bb, writer)
}

fn sweep_merge_ways(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut SortStats,
) -> Result<()> {
    let mut heap: BinaryHeap<Reverse<OwnedWay>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    for entry in entries {
        flush_heap_below(&mut heap, entry.index.min_id, |way| {
            write_single_way(&way, bb, writer)?;
            stats.ways += 1;
            Ok(())
        })?;

        read_frame_into(input_file, entry, &mut frame_buf)?;
        let blob_bytes = extract_blob_bytes(&frame_buf)?;
        let block = decode_blob_to_primitiveblock(blob_bytes)?;
        for element in block.elements() {
            if let Element::Way(w) = &element {
                heap.push(Reverse(read_way(w)));
            }
        }
    }

    while let Some(Reverse(way)) = heap.pop() {
        write_single_way(&way, bb, writer)?;
        stats.ways += 1;
    }
    flush_block(bb, writer)
}

fn sweep_merge_rels(
    entries: &[BlobEntry],
    input_file: &mut File,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut SortStats,
) -> Result<()> {
    let mut heap: BinaryHeap<Reverse<OwnedRelation>> = BinaryHeap::new();
    let mut frame_buf: Vec<u8> = Vec::new();

    for entry in entries {
        flush_heap_below(&mut heap, entry.index.min_id, |rel| {
            write_single_relation(&rel, bb, writer)?;
            stats.relations += 1;
            Ok(())
        })?;

        read_frame_into(input_file, entry, &mut frame_buf)?;
        let blob_bytes = extract_blob_bytes(&frame_buf)?;
        let block = decode_blob_to_primitiveblock(blob_bytes)?;
        for element in block.elements() {
            if let Element::Relation(r) = &element {
                heap.push(Reverse(read_relation(r)));
            }
        }
    }

    while let Some(Reverse(rel)) = heap.pop() {
        write_single_relation(&rel, bb, writer)?;
        stats.relations += 1;
    }
    flush_block(bb, writer)
}

/// Flush all elements from the min-heap whose ID is below `below`.
fn flush_heap_below<T: Ord>(
    heap: &mut BinaryHeap<Reverse<T>>,
    below: i64,
    mut emit: impl FnMut(T) -> Result<()>,
) -> Result<()>
where
    T: HasId,
{
    while heap.peek().map_or(false, |Reverse(e)| e.id() < below) {
        if let Some(Reverse(element)) = heap.pop() {
            emit(element)?;
        }
    }
    Ok(())
}

/// Trait for accessing the ID of owned element types.
trait HasId {
    fn id(&self) -> i64;
}

impl HasId for OwnedNode {
    fn id(&self) -> i64 { self.id }
}

impl HasId for OwnedWay {
    fn id(&self) -> i64 { self.id }
}

impl HasId for OwnedRelation {
    fn id(&self) -> i64 { self.id }
}

// ---------------------------------------------------------------------------
// Write single elements via BlockBuilder
// ---------------------------------------------------------------------------

fn write_single_node(
    node: &OwnedNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    ensure_node_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = node
        .tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let meta = owned_to_metadata(node.metadata.as_ref());
    bb.add_node(node.id, node.decimicro_lat, node.decimicro_lon, &tags, meta.as_ref());
    Ok(())
}

fn write_single_way(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way
        .tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let meta = owned_to_metadata(way.metadata.as_ref());
    bb.add_way(way.id, &tags, &way.refs, meta.as_ref());
    Ok(())
}

fn write_single_relation(
    rel: &OwnedRelation,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    ensure_relation_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = rel
        .tags
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m| MemberData { id: m.id, role: &m.role })
        .collect();
    let meta = owned_to_metadata(rel.metadata.as_ref());
    bb.add_relation(rel.id, &tags, &members, meta.as_ref());
    Ok(())
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

