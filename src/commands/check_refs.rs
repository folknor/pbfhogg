//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use std::path::Path;

use roaring::RoaringTreemap;

use crate::{Element, ElementReader, MemberId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Result of a referential integrity check.
pub struct RefCheckResult {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub missing_node_refs: u64,
    pub missing_way_refs: u64,
    pub missing_node_members: u64,
    pub missing_relation_members: u64,
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
/// Relies on PBF sort order: nodes before ways before relations.
///
/// # Why this is NOT an ID-only consumer
///
/// Despite appearances, check_refs needs more than just element IDs:
/// - Way node refs (`w.refs()`) — the delta-decoded refs array
/// - Relation member IDs and types (`r.members()`) — the memids and types arrays
///
/// A pure "ID-only scan mode" that skips refs/members would not work here.
/// A **selective parse** that skips stringtable, tags, coordinates, and metadata
/// but keeps IDs + refs + members could help — this has not been benchmarked yet.
/// See `PrimitiveBlock` doc comment in block.rs and TODO.md for the full analysis.
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
/// of `u64` space, which is fine for set membership tests — the mapping just
/// needs to be injective, not order-preserving.
///
/// RoaringTreemap (not RoaringBitmap) is required because RoaringBitmap only
/// supports `u32` (max ~4.3B), which cannot hold current node IDs exceeding
/// 10 billion.
#[hotpath::measure]
pub fn check_refs(path: &Path, check_relations: bool) -> Result<RefCheckResult> {
    let reader = ElementReader::from_path(path)?;

    let mut node_ids = RoaringTreemap::new();
    let mut way_ids = RoaringTreemap::new();
    let mut relation_ids = RoaringTreemap::new();

    let mut result = RefCheckResult {
        node_count: 0,
        way_count: 0,
        relation_count: 0,
        missing_node_refs: 0,
        missing_way_refs: 0,
        missing_node_members: 0,
        missing_relation_members: 0,
    };

    reader.for_each_pipelined(|element| {
        match element {
            Element::DenseNode(dn) => {
                node_ids.insert(dn.id() .cast_unsigned());
                result.node_count += 1;
            }
            Element::Node(n) => {
                node_ids.insert(n.id() .cast_unsigned());
                result.node_count += 1;
            }
            Element::Way(w) => {
                let wid = w.id();
                if check_relations {
                    way_ids.insert(wid .cast_unsigned());
                }
                result.way_count += 1;
                for node_ref in w.refs() {
                    if !node_ids.contains(node_ref .cast_unsigned()) {
                        result.missing_node_refs += 1;
                    }
                }
            }
            Element::Relation(r) => {
                if check_relations {
                    relation_ids.insert(r.id() .cast_unsigned());
                }
                result.relation_count += 1;
                if check_relations {
                    for member in r.members() {
                        match member.id {
                            MemberId::Node(id) => {
                                if !node_ids.contains(id .cast_unsigned()) {
                                    result.missing_node_members += 1;
                                }
                            }
                            MemberId::Way(id) => {
                                if !way_ids.contains(id .cast_unsigned()) {
                                    result.missing_way_refs += 1;
                                }
                            }
                            MemberId::Relation(id) => {
                                // Relations can reference other relations that
                                // appear later in the file, so we can only check
                                // relations seen so far. This matches osmium behavior.
                                if !relation_ids.contains(id .cast_unsigned()) {
                                    result.missing_relation_members += 1;
                                }
                            }
                            // Unknown member types from newer PBF producers —
                            // skip for ref-checking since we don't know what
                            // collection to check against.
                            MemberId::Unknown(_, _) => {}
                        }
                    }
                }
            }
        }
    })?;

    Ok(result)
}
