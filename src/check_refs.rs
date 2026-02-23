//! Validate referential integrity in a PBF file. Equivalent to `osmium check-refs`.

use std::collections::HashSet;
use std::path::Path;

use crate::{Element, ElementReader, RelMemberType};

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
pub fn check_refs(path: &Path, check_relations: bool) -> Result<RefCheckResult> {
    let reader = ElementReader::from_path(path)?;

    let mut node_ids: HashSet<i64> = HashSet::new();
    let mut way_ids: HashSet<i64> = HashSet::new();
    let mut relation_ids: HashSet<i64> = HashSet::new();

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
                node_ids.insert(dn.id());
                result.node_count += 1;
            }
            Element::Node(n) => {
                node_ids.insert(n.id());
                result.node_count += 1;
            }
            Element::Way(w) => {
                let wid = w.id();
                if check_relations {
                    way_ids.insert(wid);
                }
                result.way_count += 1;
                for node_ref in w.refs() {
                    if !node_ids.contains(&node_ref) {
                        result.missing_node_refs += 1;
                    }
                }
            }
            Element::Relation(r) => {
                if check_relations {
                    relation_ids.insert(r.id());
                }
                result.relation_count += 1;
                if check_relations {
                    for member in r.members() {
                        match member.member_type {
                            RelMemberType::Node => {
                                if !node_ids.contains(&member.member_id) {
                                    result.missing_node_members += 1;
                                }
                            }
                            RelMemberType::Way => {
                                if !way_ids.contains(&member.member_id) {
                                    result.missing_way_refs += 1;
                                }
                            }
                            RelMemberType::Relation => {
                                // Relations can reference other relations that
                                // appear later in the file, so we can only check
                                // relations seen so far. This matches osmium behavior.
                                if !relation_ids.contains(&member.member_id) {
                                    result.missing_relation_members += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    })?;

    Ok(result)
}
