//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

use std::collections::HashMap;
use std::path::Path;

use crate::block_builder::{HeaderBuilder, BlockBuilder, MemberData};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementReader};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Index type
// ---------------------------------------------------------------------------

/// Node location index type for add-locations-to-ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    /// HashMap-based index. ~24 bytes/node, good for country-scale.
    Hash,
    /// Dense mmap-based index. 8 bytes/slot with direct indexing by node ID.
    /// Uses anonymous mmap with lazy page allocation — virtual memory is
    /// allocated for the full ID range but physical memory is only committed
    /// for pages actually written. Required for planet-scale (8.5B nodes →
    /// ~68 GB physical vs HashMap's ~192 GB).
    ///
    /// The `capacity` field is the max number of entries (node IDs). For planet,
    /// use [`DENSE_INDEX_DEFAULT_CAPACITY`] (16 billion). Smaller values work
    /// for testing or country-scale files.
    Dense { capacity: usize },
}

/// Default dense index capacity: 16 billion entries (128 GB virtual).
/// Covers current OSM max node ID (~12.5B) with headroom for growth.
///
/// Requires `vm.overcommit_memory=1` or sufficient physical RAM + swap on
/// the host. On systems with heuristic overcommit (the default), this
/// allocation may be rejected. Use a smaller capacity or switch to
/// `--index-type hash` in that case.
pub const DENSE_INDEX_DEFAULT_CAPACITY: usize = 16_000_000_000;

// ---------------------------------------------------------------------------
// Node location index
// ---------------------------------------------------------------------------

/// Node location index abstraction supporting multiple backends.
pub enum NodeLocationIndex {
    Hash(HashMap<i64, (i32, i32)>),
    Dense(DenseMmapIndex),
}

impl NodeLocationIndex {
    fn insert(&mut self, node_id: i64, lat: i32, lon: i32) {
        match self {
            Self::Hash(map) => {
                map.insert(node_id, (lat, lon));
            }
            Self::Dense(dense) => dense.insert(node_id, lat, lon),
        }
    }

    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Hash(map) => map.get(&node_id).copied(),
            Self::Dense(dense) => dense.get(node_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Dense mmap index
// ---------------------------------------------------------------------------

/// Dense mmap-backed node location index.
///
/// Uses anonymous mmap with direct indexing: `mmap[node_id * 8 .. node_id * 8 + 8]`
/// stores `(lat: i32, lon: i32)` packed as 8 bytes (little-endian).
///
/// Zero-initialized by the OS. Pages are lazily allocated (demand-paged): a
/// 128 GB virtual mapping only consumes physical memory for pages actually
/// written. For planet (~8.5B nodes, max ID ~12.5B), physical RSS is ~68 GB.
///
/// Sentinel: `(0, 0)` means unset. ~116 nodes at exactly null island (0°N, 0°E)
/// will appear as missing — acceptable ambiguity for diagnostic counters.
pub struct DenseMmapIndex {
    mmap: memmap2::MmapMut,
    capacity: usize,
}

/// 4 bytes lat + 4 bytes lon = 8 bytes per entry.
const ENTRY_SIZE: usize = 8;

// Require 64-bit platform for dense index (32-bit cannot address 128 GB).
const _: () = assert!(std::mem::size_of::<usize>() >= 8);

impl DenseMmapIndex {
    fn new(capacity: usize) -> Result<Self> {
        let byte_len = capacity
            .checked_mul(ENTRY_SIZE)
            .ok_or("dense index capacity overflow")?;
        // String error is intentional — includes the allocation size and actionable
        // recovery advice that the underlying io::Error wouldn't provide.
        let mmap = memmap2::MmapMut::map_anon(byte_len).map_err(|e| {
            format!(
                "failed to create dense mmap index ({} GB virtual): {e}. \
                 Try --index-type hash or increase vm.overcommit_ratio.",
                byte_len / 1_000_000_000
            )
        })?;
        Ok(Self { mmap, capacity })
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn insert(&mut self, node_id: i64, lat: i32, lon: i32) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let offset = idx * ENTRY_SIZE;
        self.mmap[offset..offset + 4].copy_from_slice(&lat.to_le_bytes());
        self.mmap[offset + 4..offset + 8].copy_from_slice(&lon.to_le_bytes());
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        if node_id < 0 {
            return None;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return None;
        }
        let offset = idx * ENTRY_SIZE;
        let lat_bytes: [u8; 4] = self.mmap[offset..offset + 4]
            .try_into()
            .ok()?;
        let lon_bytes: [u8; 4] = self.mmap[offset + 4..offset + 8]
            .try_into()
            .ok()?;
        let lat = i32::from_le_bytes(lat_bytes);
        let lon = i32::from_le_bytes(lon_bytes);
        if lat == 0 && lon == 0 {
            return None;
        }
        Some((lat, lon))
    }
}

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
    compression: Compression,
    direct_io: bool,
    index_type: IndexType,
) -> Result<Stats> {
    let index = build_node_index(input, direct_io, index_type)?;
    write_output(input, output, &index, keep_untagged_nodes, compression, direct_io)
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

fn build_node_index(input: &Path, direct_io: bool, index_type: IndexType) -> Result<NodeLocationIndex> {
    let mut index = match index_type {
        IndexType::Hash => NodeLocationIndex::Hash(HashMap::new()),
        IndexType::Dense { capacity } => NodeLocationIndex::Dense(DenseMmapIndex::new(capacity)?),
    };
    let reader = ElementReader::open(input, direct_io)?;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    index.insert(dn.id(), dn.decimicro_lat(), dn.decimicro_lon());
                }
                Element::Node(n) => {
                    index.insert(n.id(), n.decimicro_lat(), n.decimicro_lon());
                }
                Element::Way(_) | Element::Relation(_) => {}
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
    index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<Stats> {
    let mut stats = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
    };

    let reader = ElementReader::open(input, direct_io)?;
    let mut hb = HeaderBuilder::from_header(reader.header()).optional_feature("LocationsOnWays");
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;
    let mut bb = BlockBuilder::new();
    for block in reader.into_blocks_pipelined() {
        let block = block?;
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
                        let meta = dense_node_metadata(dn);
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
                        let meta = element_metadata(&n.info());
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
                        match index.get(*node_id) {
                            Some(loc) => loc,
                            None => {
                                stats.missing_locations += 1;
                                (0, 0)
                            }
                        }
                    }));
                    let meta = element_metadata(&w.info());
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
                    let meta = element_metadata(&r.info());
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

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    Ok(stats)
}


// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use super::{dense_node_metadata, element_metadata, flush_block};
