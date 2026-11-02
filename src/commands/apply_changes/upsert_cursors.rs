//! The drain's per-kind upsert cursors.

use crate::ElementKind;
use crate::osc::sorted::SortedDiff;

/// Grouped per-type cursors tracking how far through each kind's sorted
/// upserts ([`SortedDiff::upserts`]) the drain has written creates, as
/// indexes into that list.
pub(super) struct UpsertCursors {
    node: usize,
    way: usize,
    rel: usize,
}

impl UpsertCursors {
    pub(super) fn new() -> Self {
        Self {
            node: 0,
            way: 0,
            rel: 0,
        }
    }

    /// Mutable cursor + sorted upsert IDs for the given element kind.
    pub(super) fn get_mut<'a>(
        &mut self,
        kind: ElementKind,
        diff: &'a SortedDiff,
    ) -> (&mut usize, &'a [i64]) {
        let cursor = match kind {
            ElementKind::Node => &mut self.node,
            ElementKind::Way => &mut self.way,
            ElementKind::Relation => &mut self.rel,
        };
        (cursor, diff.upserts(kind))
    }

    /// Immutable cursor value + sorted upsert IDs for the given element kind.
    pub(super) fn get<'a>(&self, kind: ElementKind, diff: &'a SortedDiff) -> (usize, &'a [i64]) {
        let cursor = match kind {
            ElementKind::Node => self.node,
            ElementKind::Way => self.way,
            ElementKind::Relation => self.rel,
        };
        (cursor, diff.upserts(kind))
    }
}
