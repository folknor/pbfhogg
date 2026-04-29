//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

pub mod external;

// Under the `test-hooks` feature, expose external-join fault-injection
// hooks so integration tests can arm them.
#[cfg(feature = "test-hooks")]
pub mod external_test_hooks {
    pub use super::external::test_hooks::stage3 as stage3;
}
mod dense;
mod passthrough;
mod sparse;

use std::path::Path;
use std::str::FromStr;

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementReader, MemberId, PrimitiveBlock};

use super::{
    drain_batch_results, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, require_indexdata, writer_from_header, HeaderOverrides,
};
use crate::idset::IdSet;

use super::{Result, BATCH_SIZE};

use self::dense::{build_node_index_dense, DenseMmapIndex};
use self::passthrough::write_output_passthrough;
use self::sparse::{build_node_index_sparse, SparseArrayIndex};

// ---------------------------------------------------------------------------
// Index type selection
// ---------------------------------------------------------------------------

/// Strategy for storing node coordinates during add-locations-to-ways.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexType {
    /// Direct-mapped array: `index[node_id] = (lat, lon)`. Fastest when the
    /// working set fits in RAM. At planet scale (~16 GB touched after pass 0
    /// filtering), this requires ~30+ GB of free memory to avoid page thrashing.
    #[default]
    Dense,
    /// Chunk-indexed sparse array with batched sorted lookups. Uses ~540 MB
    /// RAM for the chunk index plus a compact on-disk values file (~16 GB for
    /// planet). Way lookups are batched and sorted by file offset, converting
    /// random I/O into sequential scans. Works on memory-constrained hosts.
    Sparse,
    /// External join via double radix permutation. Bounded memory (<1 GB),
    /// all sequential I/O. Uses ~224 GB temp disk at planet scale. Best for
    /// memory-constrained hosts where dense thrashes and sparse is too slow.
    External,
    /// Auto-select: external if sorted + indexed, dense otherwise.
    Auto,
}

/// Parse error for [`IndexType`].
#[derive(Debug, Clone)]
pub struct ParseIndexTypeError(String);

impl std::fmt::Display for ParseIndexTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseIndexTypeError {}

impl FromStr for IndexType {
    type Err = ParseIndexTypeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "dense" => Ok(Self::Dense),
            "sparse" => Ok(Self::Sparse),
            "external" => Ok(Self::External),
            "auto" => Ok(Self::Auto),
            _ => Err(ParseIndexTypeError(format!(
                "unknown index type '{s}': expected 'dense', 'sparse', 'external', or 'auto'"
            ))),
        }
    }
}

impl std::fmt::Display for IndexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dense => f.write_str("dense"),
            Self::Sparse => f.write_str("sparse"),
            Self::External => f.write_str("external"),
            Self::Auto => f.write_str("auto"),
        }
    }
}

/// 4 bytes lat + 4 bytes lon = 8 bytes per entry. Shared between the dense
/// mmap layout and the sparse values file.
const ENTRY_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Unified node index
// ---------------------------------------------------------------------------

/// Unified node coordinate index dispatching to either dense or sparse.
enum NodeIndex {
    Dense(DenseMmapIndex),
    Sparse(SparseArrayIndex),
}

impl NodeIndex {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Dense(idx) => idx.get(node_id),
            Self::Sparse(idx) => idx.get(node_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
#[derive(Default)]
pub struct Stats {
    pub nodes_read: u64,
    pub nodes_written: u64,
    pub nodes_dropped: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    pub missing_locations: u64,
    pub blobs_passthrough: u64,
    pub blobs_decoded: u64,
}

impl Stats {
    /// Accumulate stats from another `Stats` instance into this one.
    pub fn merge(&mut self, src: &Stats) {
        self.nodes_read += src.nodes_read;
        self.nodes_written += src.nodes_written;
        self.nodes_dropped += src.nodes_dropped;
        self.ways_written += src.ways_written;
        self.relations_written += src.relations_written;
        self.missing_locations += src.missing_locations;
        self.blobs_passthrough += src.blobs_passthrough;
        self.blobs_decoded += src.blobs_decoded;
    }

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
        if self.blobs_passthrough > 0 {
            eprintln!(
                "  Blobs: {} passthrough, {} decoded",
                self.blobs_passthrough, self.blobs_decoded,
            );
        }
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
#[allow(clippy::too_many_arguments)]
pub fn add_locations_to_ways(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    index_type: IndexType,
) -> Result<Stats> {
    // Auto-select: external if sorted + indexed, dense otherwise.
    let index_type = if index_type == IndexType::Auto {
        let reader = crate::ElementReader::open(input, direct_io)?;
        let sorted = reader.header().is_sorted();
        drop(reader);
        // Check indexdata presence without erroring (peek at first blob).
        let has_index = (|| -> Option<bool> {
            let mut r = crate::blob::BlobReader::open(input, direct_io).ok()?;
            r.set_parse_indexdata(true);
            r.next()?.ok()?; // skip header
            let blob = r.next()?.ok()?;
            Some(blob.index().is_some())
        })().unwrap_or(false);

        let chosen = if sorted && has_index {
            IndexType::External
        } else {
            IndexType::Dense
        };
        eprintln!("auto-selected --index-type {chosen} (sorted={sorted}, indexed={has_index})");
        chosen
    } else {
        index_type
    };

    // External join has its own pipeline - dispatch early.
    if index_type == IndexType::External {
        return external::external_join(
            input,
            output,
            keep_untagged_nodes,
            compression,
            direct_io,
            force,
            overrides,
        );
    }

    let indexdata_present = require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed and re-encoded (significantly slower).")?;

    // Suggest external index for sorted indexed PBFs on sparse selection.
    if index_type == IndexType::Sparse && indexdata_present {
        let reader = crate::ElementReader::open(input, direct_io)?;
        if reader.header().is_sorted() {
            eprintln!(
                "hint: this sorted indexed PBF is eligible for --index-type external, \
                 which uses bounded memory and sequential I/O (3.9x faster than dense \
                 at planet scale). Sparse is slower than both dense and external on \
                 sorted inputs."
            );
        }
    }

    let scratch_dir = output.parent().unwrap_or(Path::new("."));

    // Pass 0: collect the set of node IDs referenced by ways. Only these
    // nodes need coordinate lookups, so only these get indexed. At planet
    // scale this reduces touched mmap pages from ~80 GB to ~16 GB.
    crate::debug::emit_marker("ALTW_PASS0_START");
    let referenced = collect_way_referenced_node_ids(input, direct_io)?;
    crate::debug::emit_marker("ALTW_PASS0_END");
    // Cheap one-shot iter().count(); surfaces the way-ref IdSet size before
    // the index build so the counter survives SIGKILL during pass 1.
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "altw_referenced_node_ids",
            i64::try_from(referenced.iter().count()).unwrap_or(i64::MAX),
        );
        crate::debug::emit_counter(
            "altw_index_kind",
            match index_type {
                IndexType::Dense => 0,
                IndexType::Sparse => 1,
                IndexType::External | IndexType::Auto => i64::MIN,
            },
        );
    }

    crate::debug::emit_marker("ALTW_PASS1_START");
    let index = build_node_index(input, direct_io, scratch_dir, &referenced, index_type)?;
    crate::debug::emit_marker("ALTW_PASS1_END");
    drop(referenced);

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_START");
        let ids = collect_relation_member_node_ids(input, direct_io)?;
        crate::debug::emit_marker("ALTW_REL_MEMBER_SCAN_END");
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter(
                "altw_relation_member_node_ids",
                i64::try_from(ids.iter().count()).unwrap_or(i64::MAX),
            );
        }
        Some(ids)
    };
    crate::debug::emit_marker("ALTW_PASS2_START");
    let stats = write_output_checked(
        input,
        output,
        &index,
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        indexdata_present,
        overrides,
    )?;
    crate::debug::emit_marker("ALTW_PASS2_END");
    emit_stats_counters(&stats);
    Ok(stats)
}

#[allow(clippy::cast_possible_wrap)]
fn emit_stats_counters(stats: &Stats) {
    crate::debug::emit_counter("altw_nodes_read", stats.nodes_read as i64);
    crate::debug::emit_counter("altw_nodes_written", stats.nodes_written as i64);
    crate::debug::emit_counter("altw_nodes_dropped", stats.nodes_dropped as i64);
    crate::debug::emit_counter("altw_ways_written", stats.ways_written as i64);
    crate::debug::emit_counter("altw_relations_written", stats.relations_written as i64);
    crate::debug::emit_counter("altw_missing_locations", stats.missing_locations as i64);
    crate::debug::emit_counter("altw_blobs_passthrough", stats.blobs_passthrough as i64);
    crate::debug::emit_counter("altw_blobs_decoded", stats.blobs_decoded as i64);
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon
/// for parallel node index population.
fn build_node_index(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSet,
    index_type: IndexType,
) -> Result<NodeIndex> {
    match index_type {
        IndexType::Dense => {
            build_node_index_dense(input, direct_io, scratch_dir, referenced)
                .map(NodeIndex::Dense)
        }
        IndexType::Sparse => {
            build_node_index_sparse(input, direct_io, scratch_dir, referenced)
                .map(NodeIndex::Sparse)
        }
        IndexType::External | IndexType::Auto => unreachable!("resolved before build_node_index"),
    }
}

/// Collect all node IDs referenced by ways (pass 0).
///
/// Uses `build_classify_schedule(Way)` to obtain a way-only blob schedule
/// via the pread-only `HeaderWalker`, then fans work out to
/// `parallel_classify_phase` workers. Each worker decompresses one blob
/// and emits the set of referenced node IDs; the main thread unions them
/// into a single `IdSet`.
///
/// At planet scale (~2 B unique node refs) the union bitset costs ~1.6 GB.
/// Per-blob refs are emitted into fresh `Vec<i64>`s that the main thread
/// consumes and drops immediately after the union, so the transient
/// worker-side memory stays bounded to per-blob refs (~8 k ways × ~20
/// refs each × 8 B = ~1.3 MB) regardless of how many blobs have completed.
///
/// `direct_io` is intentionally dropped on this path: the blob bodies are
/// pread from the shared file handle on worker threads, which is
/// incompatible with `O_DIRECT`'s alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_way_referenced_node_ids(input: &Path, _direct_io: bool) -> Result<IdSet> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Way),
    )?;
    let mut referenced = IdSet::new();
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _| {
            let mut refs_vec: Vec<i64> = Vec::new();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = element {
                    for node_id in w.refs() {
                        if node_id >= 0 {
                            refs_vec.push(node_id);
                        }
                    }
                }
            }
            refs_vec
        },
        |_seq, refs_vec| {
            for &node_id in &refs_vec {
                referenced.set(node_id);
            }
        },
    )?;
    Ok(referenced)
}

/// Collect all node IDs referenced by relation members.
///
/// Uses `build_classify_schedule(Relation)` plus
/// `parallel_classify_accumulate` so each worker unions member-node IDs
/// into its own `IdSet` across the whole scan; the main thread merges
/// the per-worker bitsets at completion. Relation member-node-ID sets
/// are sparse (only members with type=Node, and only those that are
/// also node IDs rather than way/relation IDs), so per-worker `IdSet`
/// memory stays bounded - the scan/classify.rs doc comment estimates
/// ~68 MB per worker at planet scale.
///
/// `direct_io` is intentionally dropped on this path: blob bodies are
/// pread from the shared file handle on worker threads, incompatible
/// with `O_DIRECT` alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn collect_relation_member_node_ids(input: &Path, _direct_io: bool) -> Result<IdSet> {
    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Relation),
    )?;
    let mut member_node_ids = IdSet::new();
    crate::scan::classify::parallel_classify_accumulate(
        &shared_file,
        &schedule,
        None,
        IdSet::new,
        |block, set| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = element {
                    for member in r.members() {
                        if let MemberId::Node(id) = member.id
                            && id >= 0
                        {
                            set.set(id);
                        }
                    }
                }
            }
        },
        |worker_set| {
            member_node_ids.merge_from(&worker_set);
        },
    )?;
    Ok(member_node_ids)
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------


#[allow(clippy::too_many_arguments)]
fn write_output_checked(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    indexdata_present: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    if indexdata_present {
        write_output_passthrough(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            compression,
            direct_io,
            overrides,
        )
    } else {
        write_output_decode_all(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            compression,
            direct_io,
            overrides,
        )
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let reader = ElementReader::open(input, direct_io)?;
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        true,
        overrides,
        |hb| hb.optional_feature("LocationsOnWays"),
        direct_io,
        false,
    )?;

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    let mut batches_dispatched: i64 = 0;

    for block in reader.into_blocks_pipelined() {
        batch.push(block?);

        if batch.len() >= BATCH_SIZE {
            let batch_stats = process_batch(
                &batch,
                &mut writer,
                index,
                keep_untagged_nodes,
                relation_member_node_ids,
            )?;
            stats.merge(&batch_stats);
            batch.clear();
            batches_dispatched += 1;
            // Per-batch counter survives SIGKILL inside pass 2; sidecar
            // reads the latest value to know how far the loop got.
            crate::debug::emit_counter("altw_pass2_batches_dispatched", batches_dispatched);
        }
    }

    if !batch.is_empty() {
        let batch_stats = process_batch(
            &batch,
            &mut writer,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
        )?;
        stats.merge(&batch_stats);
        batches_dispatched += 1;
        crate::debug::emit_counter("altw_pass2_batches_dispatched", batches_dispatched);
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

use super::flush_local;
use crate::owned::{dense_node_metadata, element_metadata};


/// Process a single `PrimitiveBlock`, writing elements into the thread-local
/// `BlockBuilder` and flushing complete blocks into `output`.
///
/// Way refs resolve via inline `NodeIndex::get`. The earlier
/// `resolve_batch_locations` pre-pass that converted random sparse mmap
/// I/O to a sorted sequential scan was removed in favour of straight
/// parallel inline lookups: the parallelism win (sparse pass 2 went from
/// avg cores ~4 with the serial resolve to ~16 with inline lookups)
/// dominates whatever cache-friendliness the sort provided. Pages get
/// faulted in either way; only the order changes, and the amortised
/// page-touch cost is the same once each page is hot.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
) -> std::result::Result<Stats, String> {
    let mut stats = Stats::default();

    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(dn.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Node(n) => {
                stats.nodes_read += 1;
                let has_tags = n.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(n.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for node_id in refs_buf.iter() {
                    match node_index.get(*node_id) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                }
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// Way coordinate lookups happen inline in the per-block worker via
/// `NodeIndex::get`. Both dense (direct array) and sparse (chunk +
/// slot indirection through a file-backed mmap) handle random access
/// well at ~16+ cores; the prior sparse-only pre-resolve was a serial
/// step that capped pass 2 at ~4 cores.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
) -> Result<Stats> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            || (BlockBuilder::new(), Vec::<i64>::new(), Vec::<(i32, i32)>::new()),
            |(bb, refs_buf, locations_buf), block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_block(
                    block,
                    bb,
                    &mut output,
                    index,
                    keep_untagged_nodes,
                    relation_member_node_ids,
                    refs_buf, locations_buf,
                )?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    let mut total = Stats::default();

    drain_batch_results(results, writer, |s| total.merge(&s))?;

    Ok(total)
}
