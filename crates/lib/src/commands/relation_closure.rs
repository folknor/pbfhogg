//! The two reads a relation-membership closure is built from, shared by
//! `getid --add-referenced` and default (reference-including) `tags-filter`.
//!
//! A closure over relations in relations takes at most two reads of the
//! relation blobs, whatever the nesting depth, as osmium's
//! `find_relations_in_relations` and `find_nodes_and_ways_in_relations`:
//! the first ([`relation_phase`] with `seed_edges`) collects the members of
//! a frozen starting set and every relation-to-relation edge of the other
//! relations into an [`EdgeStore`]; the caller walks the closure in memory
//! with an explicit worklist; the second ([`relation_phase`] without
//! `seed_edges`) runs only when the walk reached a relation outside the
//! starting set, and collects those relations' node and way members. Each
//! command keeps its own walk, because what "reached" means for its output
//! differs (getid keeps requested IDs out of its pulled set; tags-filter
//! keeps direct matches and reached relations apart for provenance).

use crate::MemberId;
use crate::error::Error;
use crate::idset::IdSet;
use crate::read::input::InputFile;
use crate::read::project_ids::RelationSpans;
use crate::scan::classify::ScheduledBlob;

use super::Result;

/// What one relation blob contributes to a closure.
#[derive(Default)]
pub(crate) struct RelationRefs {
    /// Node and way members of the relations the phase collects from.
    pub(crate) nodes: Vec<i64>,
    pub(crate) ways: Vec<i64>,
    /// Relation members of the collected relations: the closure's seeds.
    pub(crate) relations: Vec<i64>,
    /// `(parent, child)` for every relation member of a relation that is
    /// not collected, of either sign: the edges the closure walks.
    pub(crate) edges: Vec<(i64, i64)>,
}

/// One read of the relation blobs: for each relation in `collect`, its
/// node and way members, and with `seed_edges` also its relation members
/// (as seeds) and every other relation's relation members (as edges).
///
/// `collect` is only read, and nobody mutates it during the read, so which
/// records are collected does not depend on the order blobs complete in.
/// Every record of a collected ID is expanded, a repeated ID's included.
///
/// Each blob is validated through `RelationSpans` (no block built; refused
/// exactly as construction refuses, and a blob whose kind claim its
/// contents contradict is refused) before anything it recorded is read.
/// Every member is taken by its exact signed ID. `threads` is the caller's
/// fixed worker count, `None` for the host-derived default.
///
/// `claims`, when given, holds each schedule entry's indexdata claim by
/// position, and every blob is held to its claim being exact for the
/// relations it holds (`ClaimWalk`: count, minimum and maximum), refused
/// before anything it recorded reaches `merge`. A caller passes them when a
/// later read selects relation blobs by their claimed range: this read
/// decodes every relation blob, so a claim that would hide a relation from
/// that read is refused here instead of believed there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn relation_phase(
    shared_file: &InputFile,
    schedule: &[ScheduledBlob],
    claims: Option<&[crate::blob_meta::BlobIndex]>,
    collect: &IdSet,
    seed_edges: bool,
    threads: Option<usize>,
    mut merge: impl FnMut(RelationRefs),
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    if let Some(claims) = claims
        && claims.len() != schedule.len()
    {
        return Err(Error::internal(
            "relation phase: claims do not match the schedule entry for entry",
        ));
    }
    crate::scan::classify::parallel_project_phase(
        shared_file,
        schedule,
        threads,
        Vec::new,
        |position, buffer, relations| -> Result<RelationRefs> {
            let (RelationSpans { .. }, accepted) =
                buffer.project(|_, _| RelationSpans::new(&mut *relations))?;
            if let Some(claim) = claims.and_then(|c| c.get(position)) {
                let mut walk =
                    crate::blob_meta::ClaimWalk::new(Some(claim), crate::blob_meta::Order::Free);
                for relation in relations.iter() {
                    walk.observe(relation.id)?;
                }
                walk.finish()?;
            }
            let mut refs = RelationRefs::default();
            for relation in relations.iter() {
                let collected = collect.get(relation.id);
                if !collected && !seed_edges {
                    continue;
                }
                for member in relation.members(accepted.bytes())? {
                    match member {
                        MemberId::Node(id) if collected => refs.nodes.push(id),
                        MemberId::Way(id) if collected => refs.ways.push(id),
                        MemberId::Relation(id) if seed_edges => {
                            if collected {
                                refs.relations.push(id);
                            } else {
                                refs.edges.push((relation.id, id));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(refs)
        },
        |_seq, refs| merge(refs),
        host,
    )
}

/// A closure's `(parent, child)` edges, in an anonymous mapping of its own,
/// grown geometrically by moving to a larger mapping, so that neither its
/// growth nor its release goes through the malloc arenas: glibc raises its
/// dynamic mmap threshold when a large mmapped block goes back through
/// `free`, after which the decompress buffers of every later phase came
/// from the malloc arenas instead of mmap and stayed resident once freed.
/// Measured at planet for getid (bygg, 2026-11-03), peak anonymous memory:
/// 402 MB without a relation read, 572 to 591 MB with the store a `Vec`
/// dropped after the closure. At planet the store is about a million
/// pairs, some 17 MB.
///
/// Each edge is 16 bytes, parent then child, both big-endian two's
/// complement. The records sort as byte arrays with no unsafe view of the
/// mapping; that order is numeric only among non-negative IDs (a negative
/// ID sorts after every non-negative one), but [`Self::children`] searches
/// by the same byte order and needs only that equal parents are contiguous,
/// which holds for every `i64`.
#[derive(Default)]
pub(crate) struct EdgeStore {
    map: Option<memmap2::MmapMut>,
    len: usize,
}

/// One stored edge.
type EdgeRecord = [u8; 16];

impl EdgeStore {
    const FIRST_CAPACITY: usize = crate::tunables::RELATION_EDGE_STORE_FIRST_RECORDS;

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Bytes mapped, used or not.
    pub(crate) fn mapped_bytes(&self) -> usize {
        self.map.as_ref().map_or(0, |m| m.len())
    }

    fn records(&self) -> &[EdgeRecord] {
        let bytes = self.map.as_deref().unwrap_or(&[]);
        &bytes.as_chunks::<16>().0[..self.len]
    }

    /// Append `edges`, growing the mapping when they do not fit.
    pub(crate) fn extend(&mut self, edges: &[(i64, i64)]) -> Result<()> {
        let needed = self
            .len
            .checked_add(edges.len())
            .ok_or_else(|| Error::internal("relation edge store: record count overflows"))?;
        let capacity = self.mapped_bytes() / size_of::<EdgeRecord>();
        if needed > capacity {
            let grown = needed
                .max(capacity.saturating_mul(2))
                .max(Self::FIRST_CAPACITY);
            let bytes = grown
                .checked_mul(size_of::<EdgeRecord>())
                .ok_or_else(|| Error::internal("relation edge store: size overflows"))?;
            let mut map = memmap2::MmapMut::map_anon(bytes)?;
            let used = self.len * size_of::<EdgeRecord>();
            if let Some(old) = &self.map {
                map[..used].copy_from_slice(&old[..used]);
            }
            // The old mapping is unmapped here, not freed into an arena.
            self.map = Some(map);
        }
        let Some(map) = self.map.as_mut() else {
            return Ok(());
        };
        let records = &mut map.as_chunks_mut::<16>().0[self.len..needed];
        for (record, &(parent, child)) in records.iter_mut().zip(edges) {
            record[..8].copy_from_slice(&parent.to_be_bytes());
            record[8..].copy_from_slice(&child.to_be_bytes());
        }
        self.len = needed;
        Ok(())
    }

    /// The store sorted by parent, ready for [`Self::children`].
    pub(crate) fn sorted(mut self) -> Self {
        if let Some(map) = self.map.as_mut() {
            map.as_chunks_mut::<16>().0[..self.len].sort_unstable();
        }
        self
    }

    /// The children of `parent`, from a [`Self::sorted`] store.
    pub(crate) fn children(&self, parent: i64) -> impl Iterator<Item = i64> + '_ {
        let key = parent.to_be_bytes();
        let records = self.records();
        let from = records.partition_point(|r| r[..8] < key[..]);
        records[from..]
            .iter()
            .take_while(move |r| r[..8] == key[..])
            .map(|r| {
                let mut child = [0_u8; 8];
                child.copy_from_slice(&r[8..]);
                i64::from_be_bytes(child)
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The edge store keeps every pair across its growth (several moves
    /// past the first mapping) and, sorted, answers each parent with
    /// exactly its children, an unknown parent with none.
    #[test]
    fn edge_store_grows_sorts_and_answers_children() {
        let mut store = EdgeStore::default();
        assert_eq!(store.len(), 0);
        let total = EdgeStore::FIRST_CAPACITY * 3 + 7;
        let last = i64::try_from(total).unwrap();
        // Parents descending, so sorting has work to do; parent p holds
        // children 2p and 2p + 1. Appended in uneven batches.
        let edges: Vec<(i64, i64)> = (0..last)
            .rev()
            .map(|i| (i / 2 + (1_i64 << 40), i))
            .collect();
        for batch in edges.chunks(10_007) {
            store.extend(batch).unwrap();
        }
        assert_eq!(store.len(), total);
        assert!(store.mapped_bytes() >= total * 16);
        let store = store.sorted();
        for parent in [0_i64, 1, 5_000, (last - 1) / 2] {
            let mut got: Vec<i64> = store.children(parent + (1 << 40)).collect();
            got.sort_unstable();
            let want: Vec<i64> = [2 * parent, 2 * parent + 1]
                .into_iter()
                .filter(|&c| c < last)
                .collect();
            assert_eq!(got, want, "parent {parent}");
        }
        assert_eq!(store.children(7).count(), 0);
        assert_eq!(EdgeStore::default().sorted().children(1).count(), 0);
    }

    /// Negative parents and children, the extremes and magnitude twins
    /// included, each answer with exactly their own children.
    #[test]
    fn edge_store_answers_signed_parents() {
        let mut store = EdgeStore::default();
        let edges = [
            (-1, 5),
            (1, -5),
            (-1, -6),
            (i64::MIN, i64::MAX),
            (0, -1),
            (i64::MAX, i64::MIN),
            (-1, 0),
        ];
        store.extend(&edges).unwrap();
        let store = store.sorted();
        let children = |parent| {
            let mut got: Vec<i64> = store.children(parent).collect();
            got.sort_unstable();
            got
        };
        assert_eq!(children(-1), vec![-6, 0, 5]);
        assert_eq!(children(1), vec![-5]);
        assert_eq!(children(0), vec![-1]);
        assert_eq!(children(i64::MIN), vec![i64::MAX]);
        assert_eq!(children(i64::MAX), vec![i64::MIN]);
        assert!(children(-5).is_empty());
    }
}
