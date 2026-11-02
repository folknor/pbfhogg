//! A finished OSC parse in ID order, read-only: what apply-changes holds for
//! its whole run.
//!
//! [`CompactDiffOverlay`](super::CompactDiffOverlay) is built for parsing:
//! hash indexes for random overwrites, deleted sets, version floors for the
//! regression gate, and an interner lookup map for deduplication. None of
//! that is read once the last file is parsed. Everything apply-changes
//! reads afterwards walks the diff in OSM ID order (the scanner's range
//! test, the workers' per-block merge, the drain's create runs), so
//! [`CompactDiffOverlay::into_sorted`](super::CompactDiffOverlay::into_sorted)
//! turns the overlay into that order and drops the rest: per kind a sorted
//! ID vector with a parallel vector of arena offsets, and a sorted vector of
//! the deleted IDs, beside the unchanged arenas and the interned strings.
//!
//! At planet (daily diff seq 4913, bygg) the parse-time lookups were about
//! 120 MB (hash indexes 89.7 MB, deleted sets 10.0 MB, floors 19.0 MB, the
//! interner's lookup about 1 MB), and apply-changes kept them beside a 60 MB
//! set of sorted copies of the same IDs; this representation is the 60 MB
//! alone (reference/performance.md, "Apply-changes retained lookup
//! storage").
//!
//! Every ID order here is `osm_id_cmp` (0, -1, -2, ..., then the positive
//! IDs), the order of a sorted PBF.

use std::collections::HashSet;
use std::ops::Range;

use rustc_hash::FxHashMap;

use crate::ElementKind;
use crate::osm_id::{blob_osm_first_key, blob_osm_last_key, osm_id_cmp, osm_id_key};

use super::compact::{CompactNodeRef, CompactRelationRef, CompactWayRef};
use super::interner::StringTable;

/// One element kind of a [`SortedDiff`]: its arena, the final upserts in ID
/// order with each one's record offset, and the final deletes in ID order.
/// An ID is in at most one of the two lists: the overlay's push and delete
/// paths keep the live index and the deleted set disjoint.
pub(crate) struct KindTable {
    arena: Vec<u8>,
    /// Upserted (created or modified) IDs, sorted by `osm_id_cmp`.
    ids: Vec<i64>,
    /// `offsets[i]` is where the record of `ids[i]` starts in `arena`.
    offsets: Vec<usize>,
    /// Deleted IDs, sorted by `osm_id_cmp`.
    deleted: Vec<i64>,
}

impl KindTable {
    /// From the overlay's final state for one kind. Built from the live
    /// index and the deleted set, never from the arena: the arena also holds
    /// superseded and deleted records, and the index is where the parse
    /// already resolved which record wins.
    pub(super) fn build(
        arena: Vec<u8>,
        index: FxHashMap<i64, usize>,
        deleted: HashSet<i64>,
    ) -> Self {
        let mut pairs: Vec<(i64, usize)> = index.into_iter().collect();
        pairs.sort_unstable_by(|a, b| osm_id_cmp(a.0, b.0));
        let (ids, offsets): (Vec<i64>, Vec<usize>) = pairs.into_iter().unzip();
        let mut deleted: Vec<i64> = deleted.into_iter().collect();
        deleted.sort_unstable_by(|a, b| osm_id_cmp(*a, *b));
        Self {
            arena,
            ids,
            offsets,
            deleted,
        }
    }

    /// The upserted IDs, sorted.
    #[inline]
    pub(crate) fn upserts(&self) -> &[i64] {
        &self.ids
    }

    /// The deleted IDs, sorted.
    #[inline]
    pub(crate) fn deleted(&self) -> &[i64] {
        &self.deleted
    }

    /// The record of `upserts()[i]`.
    #[inline]
    fn record(&self, i: usize) -> &[u8] {
        &self.arena[self.offsets[i]..]
    }

    /// Whether any upserted or deleted ID has an OSM key in
    /// `[first, last]`.
    fn any_in(&self, first: crate::osm_id::OsmIdKey, last: crate::osm_id::OsmIdKey) -> bool {
        let hit = |ids: &[i64]| {
            let pos = ids.partition_point(|&id| osm_id_key(id) < first);
            pos < ids.len() && osm_id_key(ids[pos]) <= last
        };
        hit(&self.ids) || hit(&self.deleted)
    }

    /// Backing bytes held, by capacity: the arena (untouched growth
    /// headroom included), both upsert vectors and the deletes.
    fn heap_bytes(&self) -> usize {
        self.arena.capacity()
            + self.ids.capacity() * size_of::<i64>()
            + self.offsets.capacity() * size_of::<usize>()
            + self.deleted.capacity() * size_of::<i64>()
    }
}

/// The OSC diff in ID order; see the module doc.
pub(crate) struct SortedDiff {
    nodes: KindTable,
    ways: KindTable,
    relations: KindTable,
    strings: StringTable,
}

impl SortedDiff {
    pub(super) fn new(
        nodes: KindTable,
        ways: KindTable,
        relations: KindTable,
        strings: StringTable,
    ) -> Self {
        Self {
            nodes,
            ways,
            relations,
            strings,
        }
    }

    /// One kind's table.
    #[inline]
    pub(crate) fn kind(&self, kind: ElementKind) -> &KindTable {
        match kind {
            ElementKind::Node => &self.nodes,
            ElementKind::Way => &self.ways,
            ElementKind::Relation => &self.relations,
        }
    }

    /// The upserted IDs of `kind`, sorted.
    #[inline]
    pub(crate) fn upserts(&self, kind: ElementKind) -> &[i64] {
        self.kind(kind).upserts()
    }

    /// The deleted IDs of `kind`, sorted.
    #[inline]
    pub(crate) fn deleted(&self, kind: ElementKind) -> &[i64] {
        self.kind(kind).deleted()
    }

    /// The node `upserts(Node)[i]`.
    #[inline]
    pub(crate) fn node(&self, i: usize) -> CompactNodeRef<'_> {
        CompactNodeRef {
            data: self.nodes.record(i),
            strings: &self.strings,
        }
    }

    /// The way `upserts(Way)[i]`.
    #[inline]
    pub(crate) fn way(&self, i: usize) -> CompactWayRef<'_> {
        CompactWayRef {
            data: self.ways.record(i),
            strings: &self.strings,
        }
    }

    /// The relation `upserts(Relation)[i]`.
    #[inline]
    pub(crate) fn relation(&self, i: usize) -> CompactRelationRef<'_> {
        CompactRelationRef {
            data: self.relations.record(i),
            strings: &self.strings,
        }
    }

    /// Whether any ID of `kind` the diff upserts or deletes could lie in a
    /// blob claiming `[min_id, max_id]`: the conservative OSM-order span of
    /// that range (`blob_osm_first_key` .. `blob_osm_last_key`), so a false
    /// answer is safe for passthrough and a true one only means the blob
    /// must be looked at.
    pub(crate) fn range_overlaps(&self, kind: ElementKind, min_id: i64, max_id: i64) -> bool {
        self.kind(kind).any_in(
            blob_osm_first_key(min_id, max_id),
            blob_osm_last_key(min_id, max_id),
        )
    }

    /// The index range into `upserts(kind)` of the IDs whose OSM key lies
    /// in `[key(first), last]`.
    pub(crate) fn upsert_range(
        &self,
        kind: ElementKind,
        first: i64,
        last: crate::osm_id::OsmIdKey,
    ) -> Range<usize> {
        let ids = self.upserts(kind);
        let first = osm_id_key(first);
        let start = ids.partition_point(|&id| osm_id_key(id) < first);
        let end = ids[start..].partition_point(|&id| osm_id_key(id) <= last) + start;
        start..end
    }

    /// Backing bytes held, by capacity: arenas, sorted vectors and the
    /// interned strings. An allocation total, not resident memory: arena
    /// capacity includes growth headroom that was never touched.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.nodes.heap_bytes()
            + self.ways.heap_bytes()
            + self.relations.heap_bytes()
            + self.strings.heap_bytes()
    }

    /// Bytes appended to the three arenas: every record the parse wrote,
    /// superseded and deleted ones included, so not the surviving payload.
    pub(crate) fn arena_len_bytes(&self) -> usize {
        self.nodes.arena.len() + self.ways.arena.len() + self.relations.arena.len()
    }
}

/// A forward cursor finding IDs in a sorted ID vector, for IDs visited in
/// non-decreasing `osm_id_cmp` order (a sorted block's elements of one kind,
/// a sorted list of references). Seeded by binary search at the first ID it
/// is asked for, then advanced past IDs strictly before each one asked: an
/// ID asked twice in a row (a base block holding one ID twice) is found
/// both times.
#[derive(Default)]
pub(crate) struct Seek(Option<usize>);

impl Seek {
    /// The position of `id` in `ids`, if it is there.
    #[inline]
    pub(crate) fn find(&mut self, ids: &[i64], id: i64) -> Option<usize> {
        let pos = self
            .0
            .get_or_insert_with(|| ids.partition_point(|&x| osm_id_cmp(x, id).is_lt()));
        while *pos < ids.len() && osm_id_cmp(ids[*pos], id).is_lt() {
            *pos += 1;
        }
        (*pos < ids.len() && ids[*pos] == id).then_some(*pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::CompactDiffOverlay;

    fn overlay(xml: &str) -> CompactDiffOverlay {
        let scratch = crate::path_guard::tests::scratch();
        let path = scratch.path().join("diff.osc");
        std::fs::write(&path, xml).expect("write osc");
        crate::osc::parse_osc_file(&path).expect("parse osc")
    }

    #[test]
    fn upserts_and_deletes_are_disjoint_and_in_osm_order() {
        let diff = overlay(
            r#"<osmChange>
<create><node id="5" lat="1" lon="1" version="1"/><node id="-2" lat="2" lon="2"/></create>
<modify><node id="-1" lat="3" lon="3"/><node id="0" lat="4" lon="4"/></modify>
<delete><node id="7" version="2"/><node id="-3"/></delete>
<create><node id="7" lat="5" lon="5" version="3"/></create>
<delete><node id="5" version="2"/></delete>
</osmChange>"#,
        )
        .into_sorted();
        assert_eq!(diff.upserts(ElementKind::Node), &[0, -1, -2, 7]);
        assert_eq!(diff.deleted(ElementKind::Node), &[-3, 5]);
        let lats: Vec<i32> = (0..4).map(|i| diff.node(i).decimicro_lat()).collect();
        assert_eq!(lats, [40_000_000, 30_000_000, 20_000_000, 50_000_000]);
        assert_eq!(diff.node(3).id(), 7);
    }

    #[test]
    fn range_overlaps_sees_deletes_and_upserts() {
        let diff = overlay(
            r#"<osmChange>
<create><way id="10"><nd ref="1"/></way></create>
<delete><way id="20"/></delete>
</osmChange>"#,
        )
        .into_sorted();
        assert!(diff.range_overlaps(ElementKind::Way, 5, 10));
        assert!(diff.range_overlaps(ElementKind::Way, 15, 25));
        assert!(!diff.range_overlaps(ElementKind::Way, 11, 19));
        assert!(!diff.range_overlaps(ElementKind::Node, 0, 100));
        // A mixed-sign range spans keys 0 .. key(max): the negative side
        // is before every positive ID.
        assert!(diff.range_overlaps(ElementKind::Way, -4, 12));
        assert_eq!(
            diff.upsert_range(ElementKind::Way, 10, osm_id_key(30)),
            0..1
        );
        assert_eq!(
            diff.upsert_range(ElementKind::Way, 11, osm_id_key(30)),
            1..1
        );
    }

    #[test]
    fn seek_finds_repeated_and_mixed_sign_ids() {
        let ids = [0, -1, -5, 3, 8];
        let mut seek = Seek::default();
        let found: Vec<Option<usize>> = [-1, -1, -2, -5, 3, 3, 4, 8, 9]
            .iter()
            .map(|&id| seek.find(&ids, id))
            .collect();
        assert_eq!(
            found,
            [
                Some(1),
                Some(1),
                None,
                Some(2),
                Some(3),
                Some(3),
                None,
                Some(4),
                None
            ]
        );
        // Seeded past a prefix it never visits.
        let mut late = Seek::default();
        assert_eq!(late.find(&ids, 8), Some(4));
        assert_eq!(late.find(&ids, i64::MAX), None);
        let mut low = Seek::default();
        assert_eq!(low.find(&[i64::MIN, 1], i64::MIN), Some(0));
    }
}
