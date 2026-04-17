//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use std::path::Path;
use std::time::Instant;

use crate::blob_index::ElemKind;
use crate::{Element, MemberId};

use super::id_set_dense::IdSetDense;
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
/// but keeps IDs + refs + members was considered but is **not worth it**
/// while the current consumer work dominates. Once `IdSetDense` drops consumer
/// cost by ~10×, the parse cost may be worth revisiting (see
/// [notes/check-refs-opportunities.md](../../../notes/check-refs-opportunities.md)
/// section #3).
///
/// # Planet-scale memory usage
///
/// Uses [`IdSetDense`] for all three ID sets. `IdSetDense` is a chunked
/// 4 MB-bitmap keyed by element ID, purpose-built for dense-monotonic OSM ID
/// spaces. Insert and contains are O(1) (chunk index + byte offset +
/// bitmask - no hashing, no tree walk) and ~10× faster than
/// `RoaringTreemap` for this workload. Pre-allocating to the known OSM ID
/// ceiling is a fixed cost regardless of population: ~1.6 GB for nodes
/// (14 B pre-allocated IDs), ~175 MB for ways (1.5 B), ~3 MB for
/// relations (25 M).
///
/// Planet scale (~10 B nodes, ~1 B ways, ~17 M relations): ~1.8 GB total,
/// vs ~2-3 GB for RoaringTreemap and ~400 GB for `HashSet<i64>`.
///
/// Missing-ID sets are small by construction (thousands to low millions
/// even on a broken input) and are stored as `Vec<i64>` with a final
/// `sort_unstable` + `dedup` to count unique missing IDs. This avoids
/// a second bitmap allocation for a set that fits comfortably in a
/// cache-friendly vector.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub fn check_refs(path: &Path, check_relations: bool, show_ids: bool, direct_io: bool) -> Result<RefCheckResult> {
    crate::debug::emit_marker("CHECKREFS_SCAN_START");
    // Sequential reader to avoid PrimitiveBlock cross-thread alloc/free
    // retention (25+ GB at Europe/planet scale). check-refs does lightweight
    // per-element work (IdSetDense set/get) - the pipelined reader's
    // parallel decode creates cross-thread churn that dominates at scale.
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(path, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();

    // IdSetDense pre-allocated to the current OSM ID ceilings. Memory is
    // bounded by the ID space (~1.8 GB total across all three), not the
    // number of IDs set, so pre-allocation is a fixed up-front cost with
    // no scaling risk.
    let mut node_ids = IdSetDense::new();
    node_ids.pre_allocate(14_000_000_000);
    let mut way_ids = IdSetDense::new();
    let mut relation_ids = IdSetDense::new();
    if check_relations {
        way_ids.pre_allocate(1_500_000_000);
        relation_ids.pre_allocate(25_000_000);
    }

    // Missing IDs are small (thousands to low millions); collect as Vec<i64>
    // and sort+dedup at the end to count unique misses.
    let mut missing_node_refs_vec: Vec<i64> = Vec::new();
    let mut missing_way_refs_vec: Vec<i64> = Vec::new();
    let mut missing_node_members_vec: Vec<i64> = Vec::new();

    // Deferred relation-to-relation references. Relations can reference other
    // relations that appear later in the file (forward references), so we
    // collect all relation member IDs during the pass and check them after
    // reading completes, when the full relation_ids set is available. This
    // matches osmium's two-pass approach for relation members.
    let mut deferred_relation_refs: Vec<i64> = Vec::new();
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

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
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
        blob.decompress_into(&mut decompress_buf)?;
        total_bytes_decompressed += decompress_buf.len() as u64;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;

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
                node_ids.set(dn.id());
                result.node_count += 1;
            }
            Element::Node(n) => {
                node_ids.set(n.id());
                result.node_count += 1;
            }
            Element::Way(w) => {
                let wid = w.id();
                if check_relations {
                    way_ids.set(wid);
                }
                result.way_count += 1;
                for node_ref in w.refs() {
                    way_refs_checked += 1;
                    if !node_ids.get(node_ref) {
                        missing_node_refs_occurrences += 1;
                        missing_node_refs_vec.push(node_ref);
                        if show_ids {
                            missing_refs.push(MissingRef {
                                missing_type: 'n', missing_id: node_ref,
                                referencing_type: 'w', referencing_id: wid,
                            });
                        }
                    }
                }
            }
            Element::Relation(r) => {
                let rid = r.id();
                if check_relations {
                    relation_ids.set(rid);
                }
                result.relation_count += 1;
                if check_relations {
                    for member in r.members() {
                        match member.id {
                            MemberId::Node(id) => {
                                rel_node_members_checked += 1;
                                if !node_ids.get(id) {
                                    missing_node_members_occurrences += 1;
                                    missing_node_members_vec.push(id);
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
                                if !way_ids.get(id) {
                                    missing_way_refs_occurrences += 1;
                                    missing_way_refs_vec.push(id);
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
                                deferred_relation_refs.push(id);
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
                }
            }
        }
    } } // for element, for blob_result

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

    // Dedup the per-kind missing-ref vecs to get the unique-missing counts.
    // Input sizes are small (thousands to low millions) so in-place
    // sort_unstable + dedup is cheaper than a second bitmap.
    let t_dedup = Instant::now();
    let unique_len = |v: &mut Vec<i64>| -> u64 {
        v.sort_unstable();
        v.dedup();
        v.len() as u64
    };
    result.missing_node_refs = unique_len(&mut missing_node_refs_vec);
    result.missing_way_refs = unique_len(&mut missing_way_refs_vec);
    result.missing_node_members = unique_len(&mut missing_node_members_vec);
    let missing_dedup_ns = t_dedup.elapsed().as_nanos();

    // Resolve deferred relation refs against the complete relation_ids set.
    // Deduplicate missing IDs via sort+dedup for the unique-missing count.
    crate::debug::emit_marker("CHECKREFS_DEFERRED_RESOLVE_START");
    let t_deferred = Instant::now();
    if check_relations {
        let mut missing_relation_members_vec: Vec<i64> = Vec::new();
        let mut occurrences: u64 = 0;
        for (i, &id) in deferred_relation_refs.iter().enumerate() {
            if !relation_ids.get(id) {
                missing_relation_members_vec.push(id);
                occurrences += 1;
                if show_ids {
                    missing_refs.push(MissingRef {
                        missing_type: 'r',
                        missing_id: id,
                        referencing_type: 'r',
                        referencing_id: deferred_relation_ref_sources[i],
                    });
                }
            }
        }
        result.missing_relation_members = unique_len(&mut missing_relation_members_vec);
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

        // One-shot post-pass timers (the only timers kept - these are
        // outside the hot per-element loop, so no Instant overhead
        // amortizes into the measurement).
        let ns_to_ms = |ns: u128| (ns / 1_000_000) as i64;
        crate::debug::emit_counter("checkrefs_missing_dedup_ms", ns_to_ms(missing_dedup_ns));
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
