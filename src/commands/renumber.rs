//! Shared types for the renumber command.

/// Configuration for the renumber command.
pub struct RenumberOptions {
    pub start_node_id: i64,
    pub start_way_id: i64,
    pub start_relation_id: i64,
}

/// Statistics from a renumber operation.
#[derive(Debug, Clone)]
pub struct RenumberStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    /// Way refs and relation members whose old ID was not found in the
    /// corresponding ID set. These pass through with their old ID
    /// unchanged (orphan passthrough).
    pub orphan_refs: u64,
}

impl RenumberStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Renumbered {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
        if self.orphan_refs > 0 {
            eprintln!(
                "Warning: {} orphan refs preserved with old IDs (referenced \
                 elements not present in input)",
                self.orphan_refs,
            );
        }
    }
}
