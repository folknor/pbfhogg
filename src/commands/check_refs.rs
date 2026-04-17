//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use std::path::Path;
use std::time::Instant;

use roaring::RoaringTreemap;

use crate::blob_index::ElemKind;
use crate::{Element, MemberId};

use super::Result;

/// A single missing reference entry (populated when `show_ids` is true).
pub struct MissingRef {
    /// The missing element, e.g. "n123".
    pub missing_type: char,
    pub missing_id: i64,
    /// The element that references it, e.g. "w456".
    pub referencing_type: char,
    pub referencing_id: i64,
}

/// Result of a referential integrity check.
pub struct RefCheckResult {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub missing_node_refs: u64,
    pub missing_way_refs: u64,
    pub missing_node_members: u64,
    pub missing_relation_members: u64,
    /// Number of relation-to-relation references that point to missing IDs.
    /// May exceed `missing_relation_members` when multiple relations reference
    /// the same missing relation.
    pub missing_relation_member_occurrences: u64,
    /// Every missing reference occurrence (populated when `show_ids` is true).
    /// Not deduplicated - each occurrence is a separate entry.
    pub missing_refs: Vec<MissingRef>,
}

impl RefCheckResult {
    /// True if all references resolve.
    pub fn is_valid(&self) -> bool {
        self.missing_node_refs == 0
            && self.missing_way_refs == 0
            && self.missing_node_members == 0
            && self.missing_relation_members == 0
    }

    /// Total number of missing references.
    pub fn total_missing(&self) -> u64 {
        self.missing_node_refs
            + self.missing_way_refs
            + self.missing_node_members
            + self.missing_relation_members
    }
}

/// Check referential integrity of a PBF file.
///
/// Verifies that all node references in ways point to existing nodes.
/// If `check_relations` is true, also verifies that relation members
/// (node, way, relation) point to existing elements.
///
/// Relies on PBF sort order: nodes before ways before relations. Reports
/// unique missing IDs for nodes and ways (since they precede relations and
/// are fully indexed before relation processing). Reports unique missing
/// relation IDs for relation-to-relation members (deferred to post-pass to
/// handle forward references, then deduplicated).
///
/// # Why this is NOT an ID-only consumer
///
/// Despite appearances, check_refs needs more than just element IDs:
/// - Way node refs (`w.refs()`) - the delta-decoded refs array
/// - Relation member IDs and types (`r.members()`) - the memids and types arrays
///
/// A pure "ID-only scan mode" that skips refs/members would not work here.
/// A selective parse that skips stringtable, tags, coordinates, and metadata
/// but keeps IDs + refs + members was considered but is **not worth it**: profiling
/// shows check-refs is consumer-bound (main thread 100% CPU on RoaringTreemap
/// insertions, decode workers idle at 1% CPU each). Faster parsing would not
/// reduce wall time.
///
/// # Planet-scale memory usage
///
/// Uses `roaring::RoaringTreemap` instead of `HashSet<i64>` for ID storage.
/// OSM node IDs are dense and roughly sequential (1 through ~13 billion with
/// gaps from deletions). RoaringTreemap exploits this density by compressing
/// runs of consecutive IDs into bitmap containers (~2 bits per entry for dense
/// chunks) instead of storing each ID individually (~40 bytes per entry in
/// HashSet). For the full planet (~10B nodes, ~1B ways, ~17M relations):
///
/// - `HashSet<i64>`: ~400 GB (infeasible)
/// - `RoaringTreemap`: ~2-3 GB (fits comfortably on any server)
///
/// The `i64→u64` mapping uses `i64::cast_unsigned()`. Planet files from official
/// servers contain only positive IDs, so the cast is lossless. Files with negative
/// IDs (e.g. from JOSM for uncommitted elements) will wrap to the upper half
/// of `u64` space, which is fine for set membership tests - the mapping just
/// needs to be injective, not order-preserving.
///
/// RoaringTreemap (not RoaringBitmap) is required because RoaringBitmap only
/// supports `u32` (max ~4.3B), which cannot hold current node IDs exceeding
/// 10 billion.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub fn check_refs(path: &Path, check_relations: bool, show_ids: bool, direct_io: bool) -> Result<RefCheckResult> {
    crate::debug::emit_marker("CHECKREFS_SCAN_START");
    // Sequential reader to avoid PrimitiveBlock cross-thread alloc/free
    // retention (25+ GB at Europe/planet scale). check-refs does lightweight
    // per-element work (RoaringTreemap inserts) - the pipelined reader's
    // parallel decode creates cross-thread churn that dominates at scale.
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(path, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();

    let mut node_ids = RoaringTreemap::new();
    let mut way_ids = RoaringTreemap::new();
    let mut relation_ids = RoaringTreemap::new();

    // Track unique missing IDs (not reference occurrences) to match osmium
    // semantics: "441 nodes missing" means 441 distinct node IDs that don't
    // exist, not 712 references that point to missing nodes.
    let mut missing_node_refs_set = RoaringTreemap::new();
    let mut missing_way_refs_set = RoaringTreemap::new();
    let mut missing_node_members_set = RoaringTreemap::new();

    // Deferred relation-to-relation references. Relations can reference other
    // relations that appear later in the file (forward references), so we
    // collect all relation member IDs during the pass and check them after
    // reading completes, when the full relation_ids set is available. This
    // matches osmium's two-pass approach for relation members.
    let mut deferred_relation_refs: Vec<u64> = Vec::new();
    let mut deferred_relation_ref_sources: Vec<i64> = Vec::new();

    let mut missing_refs: Vec<MissingRef> = Vec::new();

    let mut result = RefCheckResult {
        node_count: 0,
        way_count: 0,
        relation_count: 0,
        missing_node_refs: 0,
        missing_way_refs: 0,
        missing_node_members: 0,
        missing_relation_members: 0,
        missing_relation_member_occurrences: 0,
        missing_refs: Vec::new(),
    };
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    // Per-sub-operation wall-time accumulators in nanoseconds.
    // `.elapsed().as_millis()` per-op truncates to 0 for anything under 1 ms
    // (every node_insert is ~70 ns; summing those as millis never escapes 0).
    // Accumulate nanos, divide to millis at emit time.
    let mut pread_ns: u128 = 0;
    let mut decompress_ns: u128 = 0;
    let mut block_build_ns: u128 = 0;
    let mut node_insert_ns: u128 = 0;
    let mut way_insert_ns: u128 = 0;
    let mut way_ref_check_ns: u128 = 0;
    let mut rel_insert_ns: u128 = 0;
    let mut rel_member_check_ns: u128 = 0;

    // Structural counters for yardstick validation.
    let mut node_blobs: u64 = 0;
    let mut way_blobs: u64 = 0;
    let mut relation_blobs: u64 = 0;
    let mut total_bytes_decompressed: u64 = 0;
    let mut way_refs_checked: u64 = 0;
    let mut rel_node_members_checked: u64 = 0;
    let mut rel_way_members_checked: u64 = 0;
    let mut rel_rel_members_deferred: u64 = 0;
    let mut missing_node_refs_occurrences: u64 = 0;
    let mut missing_way_refs_occurrences: u64 = 0;
    let mut missing_node_members_occurrences: u64 = 0;

    // Track the element kind currently being processed so phase markers
    // (NODES / WAYS / RELATIONS) are emitted on kind transitions. Driven
    // off element kind rather than blob.index() so the instrumentation is
    // robust against non-indexed inputs.
    let mut current_phase: Option<ElemKind> = None;

    loop {
        let t_pread = Instant::now();
        let next = blob_reader.next();
        pread_ns += t_pread.elapsed().as_nanos();
        let blob = match next {
            None => break,
            Some(r) => r?,
        };
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if !check_relations {
            if let Some(idx) = blob.index() {
                if matches!(idx.kind, ElemKind::Relation) {
                    continue;
                }
            }
        }
        let t_dec = Instant::now();
        blob.decompress_into(&mut decompress_buf)?;
        decompress_ns += t_dec.elapsed().as_nanos();
        total_bytes_decompressed += decompress_buf.len() as u64;
        let t_build = Instant::now();
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        block_build_ns += t_build.elapsed().as_nanos();

        // Per-blob kind tally via blob.index() when available; falls back
        // to first-element inspection below via current_phase updates.
        if let Some(idx) = blob.index() {
            match idx.kind {
                ElemKind::Node => node_blobs += 1,
                ElemKind::Way => way_blobs += 1,
                ElemKind::Relation => relation_blobs += 1,
            }
        }

        for element in block.elements_skip_metadata() {
        let kind = match element {
            Element::DenseNode(_) | Element::Node(_) => ElemKind::Node,
            Element::Way(_) => ElemKind::Way,
            Element::Relation(_) => ElemKind::Relation,
        };
        if current_phase != Some(kind) {
            match current_phase {
                Some(ElemKind::Node) => {
                    crate::debug::emit_marker("CHECKREFS_NODES_END");
                    crate::debug::emit_mallinfo2("checkrefs_after_nodes");
                }
                Some(ElemKind::Way) => {
                    crate::debug::emit_marker("CHECKREFS_WAYS_END");
                    crate::debug::emit_mallinfo2("checkrefs_after_ways");
                }
                Some(ElemKind::Relation) => {
                    crate::debug::emit_marker("CHECKREFS_RELATIONS_END");
                    crate::debug::emit_mallinfo2("checkrefs_after_relations");
                }
                None => {}
            }
            match kind {
                ElemKind::Node => crate::debug::emit_marker("CHECKREFS_NODES_START"),
                ElemKind::Way => crate::debug::emit_marker("CHECKREFS_WAYS_START"),
                ElemKind::Relation => crate::debug::emit_marker("CHECKREFS_RELATIONS_START"),
            }
            current_phase = Some(kind);
        }
        match element {
            Element::DenseNode(dn) => {
                let t = Instant::now();
                node_ids.insert(dn.id() .cast_unsigned());
                node_insert_ns += t.elapsed().as_nanos();
                result.node_count += 1;
            }
            Element::Node(n) => {
                let t = Instant::now();
                node_ids.insert(n.id() .cast_unsigned());
                node_insert_ns += t.elapsed().as_nanos();
                result.node_count += 1;
            }
            Element::Way(w) => {
                let wid = w.id();
                if check_relations {
                    let t = Instant::now();
                    way_ids.insert(wid .cast_unsigned());
                    way_insert_ns += t.elapsed().as_nanos();
                }
                result.way_count += 1;
                let t_refs = Instant::now();
                for node_ref in w.refs() {
                    way_refs_checked += 1;
                    if !node_ids.contains(node_ref .cast_unsigned()) {
                        missing_node_refs_occurrences += 1;
                        missing_node_refs_set.insert(node_ref .cast_unsigned());
                        if show_ids {
                            missing_refs.push(MissingRef {
                                missing_type: 'n', missing_id: node_ref,
                                referencing_type: 'w', referencing_id: wid,
                            });
                        }
                    }
                }
                way_ref_check_ns += t_refs.elapsed().as_nanos();
            }
            Element::Relation(r) => {
                let rid = r.id();
                if check_relations {
                    let t = Instant::now();
                    relation_ids.insert(rid .cast_unsigned());
                    rel_insert_ns += t.elapsed().as_nanos();
                }
                result.relation_count += 1;
                if check_relations {
                    let t_mem = Instant::now();
                    for member in r.members() {
                        match member.id {
                            MemberId::Node(id) => {
                                rel_node_members_checked += 1;
                                if !node_ids.contains(id .cast_unsigned()) {
                                    missing_node_members_occurrences += 1;
                                    missing_node_members_set.insert(id .cast_unsigned());
                                    if show_ids {
                                        missing_refs.push(MissingRef {
                                            missing_type: 'n', missing_id: id,
                                            referencing_type: 'r', referencing_id: rid,
                                        });
                                    }
                                }
                            }
                            MemberId::Way(id) => {
                                rel_way_members_checked += 1;
                                if !way_ids.contains(id .cast_unsigned()) {
                                    missing_way_refs_occurrences += 1;
                                    missing_way_refs_set.insert(id .cast_unsigned());
                                    if show_ids {
                                        missing_refs.push(MissingRef {
                                            missing_type: 'w', missing_id: id,
                                            referencing_type: 'r', referencing_id: rid,
                                        });
                                    }
                                }
                            }
                            MemberId::Relation(id) => {
                                rel_rel_members_deferred += 1;
                                deferred_relation_refs.push(id .cast_unsigned());
                                if show_ids {
                                    // Deferred - store relation ID for later resolution
                                    // We store the referencing relation ID alongside
                                    deferred_relation_ref_sources.push(rid);
                                }
                            }
                            // Unknown member types from newer PBF producers -
                            // skip for ref-checking since we don't know what
                            // collection to check against.
                            MemberId::Unknown(_, _) => {}
                        }
                    }
                    rel_member_check_ns += t_mem.elapsed().as_nanos();
                }
            }
        }
    } } // for element, loop

    // Close whichever phase marker is current.
    match current_phase {
        Some(ElemKind::Node) => {
            crate::debug::emit_marker("CHECKREFS_NODES_END");
            crate::debug::emit_mallinfo2("checkrefs_after_nodes");
        }
        Some(ElemKind::Way) => {
            crate::debug::emit_marker("CHECKREFS_WAYS_END");
            crate::debug::emit_mallinfo2("checkrefs_after_ways");
        }
        Some(ElemKind::Relation) => {
            crate::debug::emit_marker("CHECKREFS_RELATIONS_END");
            crate::debug::emit_mallinfo2("checkrefs_after_relations");
        }
        None => {}
    }

    result.missing_node_refs = missing_node_refs_set.len();
    result.missing_node_members = missing_node_members_set.len();
    result.missing_way_refs = missing_way_refs_set.len();

    // Resolve deferred relation refs against the complete relation_ids set.
    // Deduplicate missing IDs via RoaringTreemap to count unique missing
    // relation IDs, consistent with node/way counting above.
    crate::debug::emit_marker("CHECKREFS_DEFERRED_RESOLVE_START");
    let t_deferred = Instant::now();
    if check_relations {
        let mut missing_relation_members_set = RoaringTreemap::new();
        let mut occurrences: u64 = 0;
        for (i, &id) in deferred_relation_refs.iter().enumerate() {
            if !relation_ids.contains(id) {
                missing_relation_members_set.insert(id);
                occurrences += 1;
                if show_ids {
                    missing_refs.push(MissingRef {
                        missing_type: 'r',
                        missing_id: id.cast_signed(),
                        referencing_type: 'r',
                        referencing_id: deferred_relation_ref_sources[i],
                    });
                }
            }
        }
        result.missing_relation_members = missing_relation_members_set.len();
        result.missing_relation_member_occurrences = occurrences;
    }
    let deferred_resolve_ns = t_deferred.elapsed().as_nanos();
    crate::debug::emit_marker("CHECKREFS_DEFERRED_RESOLVE_END");

    result.missing_refs = missing_refs;

    crate::debug::emit_marker("CHECKREFS_SCAN_END");
    crate::debug::emit_mallinfo2("checkrefs_final");
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("checkrefs_node_count", result.node_count as i64);
        crate::debug::emit_counter("checkrefs_way_count", result.way_count as i64);
        crate::debug::emit_counter("checkrefs_relation_count", result.relation_count as i64);
        crate::debug::emit_counter("checkrefs_missing_node_refs", result.missing_node_refs as i64);
        crate::debug::emit_counter("checkrefs_missing_way_refs", result.missing_way_refs as i64);
        crate::debug::emit_counter("checkrefs_missing_node_members", result.missing_node_members as i64);
        crate::debug::emit_counter("checkrefs_missing_relation_members", result.missing_relation_members as i64);

        // Per-sub-operation wall-time breakdown. Accumulated as nanos,
        // converted to millis for the sidecar counters.
        let ns_to_ms = |ns: u128| (ns / 1_000_000) as i64;
        crate::debug::emit_counter("checkrefs_pread_ms", ns_to_ms(pread_ns));
        crate::debug::emit_counter("checkrefs_decompress_ms", ns_to_ms(decompress_ns));
        crate::debug::emit_counter("checkrefs_block_build_ms", ns_to_ms(block_build_ns));
        crate::debug::emit_counter("checkrefs_node_insert_ms", ns_to_ms(node_insert_ns));
        crate::debug::emit_counter("checkrefs_way_insert_ms", ns_to_ms(way_insert_ns));
        crate::debug::emit_counter("checkrefs_way_ref_check_ms", ns_to_ms(way_ref_check_ns));
        crate::debug::emit_counter("checkrefs_rel_insert_ms", ns_to_ms(rel_insert_ns));
        crate::debug::emit_counter("checkrefs_rel_member_check_ms", ns_to_ms(rel_member_check_ns));
        crate::debug::emit_counter("checkrefs_deferred_resolve_ms", ns_to_ms(deferred_resolve_ns));

        // Structural counters (validate yardstick + plan cost model).
        crate::debug::emit_counter("checkrefs_node_blobs", node_blobs as i64);
        crate::debug::emit_counter("checkrefs_way_blobs", way_blobs as i64);
        crate::debug::emit_counter("checkrefs_relation_blobs", relation_blobs as i64);
        crate::debug::emit_counter("checkrefs_total_bytes_decompressed", total_bytes_decompressed as i64);
        crate::debug::emit_counter("checkrefs_way_refs_checked", way_refs_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_node_members_checked", rel_node_members_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_way_members_checked", rel_way_members_checked as i64);
        crate::debug::emit_counter("checkrefs_rel_rel_members_deferred", rel_rel_members_deferred as i64);
        crate::debug::emit_counter("checkrefs_missing_node_refs_occurrences", missing_node_refs_occurrences as i64);
        crate::debug::emit_counter("checkrefs_missing_way_refs_occurrences", missing_way_refs_occurrences as i64);
        crate::debug::emit_counter("checkrefs_missing_node_members_occurrences", missing_node_members_occurrences as i64);
    }

    Ok(result)
}
