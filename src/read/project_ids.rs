//! Element IDs and reference arrays read through the block validator
//! ([`BlockBuffer::project`](super::block::BlockBuffer::project)) instead of
//! a built block: [`KindIds`] (`check --ids`, the node phase of
//! `check --refs`, `getid -I`), [`WaySpans`] (the way phase of
//! `check --refs`, `getid --add-referenced`'s first pass) and
//! [`RelationSpans`] (the relation phase of `check --refs`).
//!
//! Each records positions in the validated bytes, not copies, and reads them
//! back only from the accepted blob
//! ([`ValidatedBlock::bytes`](super::block::ValidatedBlock::bytes)), so
//! every judgement on what it
//! recorded runs after the blob was accepted and nothing a hook gathers
//! outlives a refusal.

use super::block::GroupProjection;
use super::elements::{MemberIdIter, WayRefIter};
use super::wire::{WireDenseInfo, WireDenseNodes, WireInfo, WireNode, WireRelation, WireWay};
use crate::ElementKind;
use crate::element_kind::KindSet;
use crate::idset::IdSet;

/// Where a validated array lies in a blob's bytes, recorded by a hook and
/// read back by [`Span::in_blob`] once the blob is accepted.
#[derive(Clone, Copy)]
pub(crate) struct Span {
    at: usize,
    len: usize,
}

impl Span {
    fn of(data: &[u8]) -> Self {
        Span {
            at: data.as_ptr() as usize,
            len: data.len(),
        }
    }

    /// The recorded array, rebased on `bytes`, the accepted blob it was
    /// recorded in. An empty array is empty wherever it was recorded: the
    /// wire parsers give an absent packed field a static empty slice, which
    /// lies in no blob (a dense message without an `id` field, a relation
    /// without members). A non-empty span outside `bytes` is a pbfhogg bug
    /// (a span read against another blob), reported as an internal error.
    pub(crate) fn in_blob(self, bytes: &[u8]) -> crate::error::Result<&[u8]> {
        if self.len == 0 {
            return Ok(&[]);
        }
        self.at
            .checked_sub(bytes.as_ptr() as usize)
            .and_then(|start| bytes.get(start..start.checked_add(self.len)?))
            .ok_or_else(|| {
                crate::error::Error::internal("block projection: a span lies outside its blob")
            })
    }
}

/// A way's ID and the position of its `refs` array.
#[derive(Clone, Copy)]
pub(crate) struct WaySpan {
    pub(crate) id: i64,
    refs: Span,
}

impl WaySpan {
    /// The way's node refs, decoded from `bytes` (the accepted blob) as
    /// [`Way::refs`](super::elements::Way::refs) decodes them.
    pub(crate) fn refs(self, bytes: &[u8]) -> crate::error::Result<WayRefIter<'_>> {
        Ok(WayRefIter::delta_decoded(self.refs.in_blob(bytes)?))
    }
}

/// The ways of a block, each as its ID and the position of its `refs`
/// array, in element-walk order (the validator hands ways over in the order
/// the walk yields them). With `only`, just the ways whose ID it holds: a
/// read-only test on a set nobody mutates during the scan, so the hook stays
/// provisional.
pub(crate) struct WaySpans<'s> {
    only: Option<&'s IdSet>,
    ways: &'s mut Vec<WaySpan>,
}

impl<'s> WaySpans<'s> {
    /// The projection recording into `ways`, which it clears.
    pub(crate) fn new(only: Option<&'s IdSet>, ways: &'s mut Vec<WaySpan>) -> Self {
        ways.clear();
        WaySpans { only, ways }
    }
}

impl GroupProjection for WaySpans<'_> {
    fn way(&mut self, way: &WireWay<'_>, _info: Option<&WireInfo>) {
        if self.only.is_none_or(|only| only.get(way.id)) {
            self.ways.push(WaySpan {
                id: way.id,
                refs: Span::of(way.refs_data),
            });
        }
    }
}

/// A relation's ID and the positions of its `memids` and `types` arrays.
#[derive(Clone, Copy)]
pub(crate) struct RelationSpan {
    pub(crate) id: i64,
    memids: Span,
    types: Span,
}

impl RelationSpan {
    /// The relation's typed member IDs, decoded from `bytes` (the accepted
    /// blob) as [`Relation::members`](super::elements::Relation::members)
    /// decodes them, roles aside.
    pub(crate) fn members(self, bytes: &[u8]) -> crate::error::Result<MemberIdIter<'_>> {
        Ok(MemberIdIter::new(
            self.memids.in_blob(bytes)?,
            self.types.in_blob(bytes)?,
        ))
    }
}

/// The relations of a block, each as its ID and its member arrays'
/// positions, in element-walk order.
pub(crate) struct RelationSpans<'s> {
    relations: &'s mut Vec<RelationSpan>,
}

impl<'s> RelationSpans<'s> {
    /// The projection recording into `relations`, which it clears.
    pub(crate) fn new(relations: &'s mut Vec<RelationSpan>) -> Self {
        relations.clear();
        RelationSpans { relations }
    }
}

impl GroupProjection for RelationSpans<'_> {
    fn relation(&mut self, relation: &WireRelation<'_>, _info: Option<&WireInfo>) {
        self.relations.push(RelationSpan {
            id: relation.id,
            memids: Span::of(relation.memids_data),
            types: Span::of(relation.types_data),
        });
    }
}

/// Where the IDs of the chosen kinds sit in a block, recorded as the block
/// validator passes each element: no block is built, no offset table
/// appended. A dense group's IDs are not copied out: the projection keeps
/// the position of its validated `id` array, decoded once after the blob is
/// accepted, where the element walk stepped `lat`, `lon` and `keys_vals`
/// beside it. (A copy would hold a whole blob's node IDs per worker: planet
/// node blobs of a few hundred thousand nodes made that tens of megabytes
/// across the pool.) Way, relation and plain node IDs, taken from the
/// messages the validator already parsed, are copied.
///
/// Validation is block construction's own call, so a blob is refused here
/// exactly when building its block would refuse it, with the same error,
/// and a contradicted kind claim is refused after it. What a hook records is
/// provisional: every judgement on the IDs runs after the blob has been
/// accepted, through [`IdScratch::each`].
///
/// Order: the element walk yields a group's dense nodes before its plain
/// ones while the validator hands over the plain ones first, so a group's
/// plain node IDs wait in `plain` until the group ends. Filtered to any one
/// kind, [`IdScratch::each`] yields that kind's IDs in element-walk order;
/// the order between kinds is unspecified. IDs are selected by the
/// element's own kind, never by the blob's claim: an unindexed blob may hold
/// anything.
pub(crate) struct KindIds<'s> {
    kinds: KindSet,
    scratch: &'s mut IdScratch,
}

impl<'s> KindIds<'s> {
    /// The projection of the IDs of `kinds`, recording into `scratch`, which
    /// it clears: a worker reuses one scratch across its blobs.
    pub(crate) fn new(kinds: KindSet, scratch: &'s mut IdScratch) -> Self {
        scratch.runs.clear();
        scratch.ids.clear();
        scratch.plain.clear();
        KindIds { kinds, scratch }
    }

    /// The projection of the IDs of one kind.
    pub(crate) fn of(kind: ElementKind, scratch: &'s mut IdScratch) -> Self {
        let mut kinds = KindSet::default();
        kinds.add(kind);
        Self::new(kinds, scratch)
    }

    fn copy(&mut self, kind: ElementKind, id: i64) {
        self.scratch.ids.push(id);
        // A copied run extends only a run of its own kind, so a group's way
        // and relation IDs never replay under one kind.
        if let Some(IdRun::Copied { kind: last, count }) = self.scratch.runs.last_mut()
            && *last == kind
        {
            *count += 1;
        } else {
            self.scratch.runs.push(IdRun::Copied { kind, count: 1 });
        }
    }
}

/// One stretch of a blob's IDs of one kind, in element order.
enum IdRun {
    /// A dense group's validated `id` array, decoded after acceptance.
    Dense(Span),
    /// The next `count` IDs of `IdScratch::ids`, all of `kind`.
    Copied { kind: ElementKind, count: usize },
}

impl GroupProjection for KindIds<'_> {
    fn group_begin(&mut self) {
        self.scratch.plain.clear();
    }

    fn node(&mut self, node: &WireNode<'_>, _info: Option<&WireInfo>) {
        if self.kinds.has(ElementKind::Node) {
            self.scratch.plain.push(node.id);
        }
    }

    fn way(&mut self, way: &WireWay<'_>, _info: Option<&WireInfo>) {
        if self.kinds.has(ElementKind::Way) {
            self.copy(ElementKind::Way, way.id);
        }
    }

    fn relation(&mut self, relation: &WireRelation<'_>, _info: Option<&WireInfo>) {
        if self.kinds.has(ElementKind::Relation) {
            self.copy(ElementKind::Relation, relation.id);
        }
    }

    fn dense(
        &mut self,
        dense: &WireDenseNodes<'_>,
        _ids: usize,
        _info: Option<&WireDenseInfo<'_>>,
    ) {
        if self.kinds.has(ElementKind::Node) {
            self.scratch
                .runs
                .push(IdRun::Dense(Span::of(dense.id_data)));
        }
    }

    fn group_end(&mut self) {
        let plain = std::mem::take(&mut self.scratch.plain);
        for &id in &plain {
            self.copy(ElementKind::Node, id);
        }
        self.scratch.plain = plain;
    }
}

/// A worker's scratch for [`KindIds`], reused across blobs: the current
/// blob's runs and copied IDs, and the current group's plain node IDs.
#[derive(Default)]
pub(crate) struct IdScratch {
    runs: Vec<IdRun>,
    ids: Vec<i64>,
    plain: Vec<i64>,
}

impl IdScratch {
    /// Hand the recorded IDs with their kinds to `f`, read from `bytes`, the
    /// accepted blob the dense runs point into; within a kind in
    /// element-walk order (see [`KindIds`]). Stops at `f`'s first error.
    pub(crate) fn each(
        &self,
        bytes: &[u8],
        mut f: impl FnMut(ElementKind, i64) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        let mut copied = self.ids.iter();
        for run in &self.runs {
            match *run {
                IdRun::Dense(span) => WayRefIter::delta_decoded(span.in_blob(bytes)?)
                    .try_for_each(|id| f(ElementKind::Node, id))?,
                IdRun::Copied { kind, count } => copied
                    .by_ref()
                    .take(count)
                    .try_for_each(|&id| f(kind, id))?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{IdScratch, KindIds};
    use crate::element_kind::KindSet;
    use crate::{Element, ElementKind};

    /// The IDs of `kind` the element walk yields, in its order.
    fn walked_ids(block: &crate::PrimitiveBlock, kind: ElementKind) -> Vec<i64> {
        block
            .elements_skip_metadata()
            .filter_map(|el| match (&el, kind) {
                (Element::DenseNode(dn), ElementKind::Node) => Some(dn.id()),
                (Element::Node(n), ElementKind::Node) => Some(n.id()),
                (Element::Way(w), ElementKind::Way) => Some(w.id()),
                (Element::Relation(r), ElementKind::Relation) => Some(r.id()),
                _ => None,
            })
            .collect()
    }

    const KINDS: [ElementKind; 3] = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];

    /// Every subset of the three kinds.
    fn kind_sets() -> Vec<KindSet> {
        (1_u8..8)
            .map(|mask| {
                let mut set = KindSet::default();
                for (bit, kind) in KINDS.into_iter().enumerate() {
                    if mask & (1 << bit) != 0 {
                        set.add(kind);
                    }
                }
                set
            })
            .collect()
    }

    /// For every fixture, every truncation and every probed byte value, and
    /// every set of kinds: the projection refuses exactly what block
    /// construction refuses, with the same message, and where it accepts,
    /// each chosen kind's IDs filtered from it are the element walk's IDs of
    /// that kind in the walk's order (dense nodes before plain ones within a
    /// group, groups in order), and no other kind appears.
    #[test]
    fn kind_ids_refuse_as_construction_and_match_the_element_walk() {
        use crate::read::wire_fixtures::{mutations, projection_fixtures};

        for (what, bytes) in projection_fixtures() {
            assert!(
                crate::PrimitiveBlock::from_vec(bytes.clone()).is_ok(),
                "{what}: fixture must be valid"
            );
            // One worker's scratch across every case, as a phase reuses it.
            let mut scratch = IdScratch::default();
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let cases =
                std::iter::once(("as built".to_owned(), bytes.clone())).chain(mutations(&bytes));
            for (how, input) in cases {
                let oracle = crate::PrimitiveBlock::from_vec(input.clone());
                for kinds in kind_sets() {
                    let buffer = crate::read::block::BlockBuffer::new(
                        input.clone(),
                        None,
                        &mut st,
                        &mut gr,
                        None,
                    );
                    let projected = match buffer.project(|_, _| KindIds::new(kinds, &mut scratch)) {
                        Ok((KindIds { .. }, accepted)) => {
                            let mut ids = Vec::new();
                            scratch
                                .each(accepted.bytes(), |kind, id| {
                                    ids.push((kind, id));
                                    Ok(())
                                })
                                .map(|()| ids)
                        }
                        Err(e) => Err(e),
                    };
                    match (&oracle, projected) {
                        (Err(a), Err(b)) => {
                            assert_eq!(a.to_string(), b.to_string(), "{what}, {how}, {kinds:?}");
                        }
                        (Ok(block), Ok(ids)) => {
                            for kind in KINDS {
                                let of_kind: Vec<i64> = ids
                                    .iter()
                                    .filter(|(k, _)| *k == kind)
                                    .map(|&(_, id)| id)
                                    .collect();
                                let expected = if kinds.has(kind) {
                                    walked_ids(block, kind)
                                } else {
                                    Vec::new()
                                };
                                assert_eq!(of_kind, expected, "{what}, {how}, {kinds:?}, {kind}");
                            }
                        }
                        (Ok(_), Err(e)) => {
                            panic!("{what}, {how}, {kinds:?}: refused a valid block: {e}")
                        }
                        (Err(e), Ok(_)) => {
                            panic!(
                                "{what}, {how}, {kinds:?}: accepted a block construction refuses: {e}"
                            )
                        }
                    }
                }
            }
        }
    }

    /// For every fixture and mutation: the way and relation projections
    /// refuse exactly what block construction refuses, with the same
    /// message, and where they accept, replaying what they recorded yields
    /// the element walk's ways (ID and refs) and relations (ID and typed
    /// member IDs) in the walk's order; restricted to a way set, exactly the
    /// walk's ways in it.
    #[test]
    fn way_and_relation_spans_refuse_as_construction_and_match_the_element_walk() {
        use super::{RelationSpans, WaySpans};
        use crate::MemberId;
        use crate::idset::IdSet;
        use crate::read::wire_fixtures::{mutations, projection_fixtures};

        type Ways = Vec<(i64, Vec<i64>)>;
        type Relations = Vec<(i64, Vec<MemberId>)>;
        let mut only = IdSet::new();
        for id in [20, -41, 42, 30, 2] {
            only.set(id);
        }

        for (what, bytes) in projection_fixtures() {
            let (mut ways, mut relations) = (Vec::new(), Vec::new());
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let cases =
                std::iter::once(("as built".to_owned(), bytes.clone())).chain(mutations(&bytes));
            for (how, input) in cases {
                let oracle = crate::PrimitiveBlock::from_vec(input.clone());
                for filter in [None, Some(&only)] {
                    let buffer = crate::read::block::BlockBuffer::new(
                        input.clone(),
                        None,
                        &mut st,
                        &mut gr,
                        None,
                    );
                    let projected: crate::Result<Ways> =
                        match buffer.project(|_, _| WaySpans::new(filter, &mut ways)) {
                            Ok((WaySpans { .. }, accepted)) => ways
                                .iter()
                                .map(|w| Ok((w.id, w.refs(accepted.bytes())?.collect())))
                                .collect(),
                            Err(e) => Err(e),
                        };
                    match (&oracle, projected) {
                        (Err(a), Err(b)) => {
                            assert_eq!(a.to_string(), b.to_string(), "{what}, {how}");
                        }
                        (Ok(block), Ok(got)) => {
                            let walked: Ways = block
                                .elements_skip_metadata()
                                .filter_map(|el| match el {
                                    Element::Way(w)
                                        if filter.is_none_or(|only| only.get(w.id())) =>
                                    {
                                        Some((w.id(), w.refs().collect()))
                                    }
                                    _ => None,
                                })
                                .collect();
                            assert_eq!(got, walked, "{what}, {how}, filtered {}", filter.is_some());
                        }
                        (oracle, got) => panic!(
                            "{what}, {how}: construction accepted {}, ways projection accepted {}",
                            oracle.is_ok(),
                            got.is_ok()
                        ),
                    }
                }
                let buffer = crate::read::block::BlockBuffer::new(
                    input.clone(),
                    None,
                    &mut st,
                    &mut gr,
                    None,
                );
                let projected: crate::Result<Relations> =
                    match buffer.project(|_, _| RelationSpans::new(&mut relations)) {
                        Ok((RelationSpans { .. }, accepted)) => relations
                            .iter()
                            .map(|r| Ok((r.id, r.members(accepted.bytes())?.collect())))
                            .collect(),
                        Err(e) => Err(e),
                    };
                match (&oracle, projected) {
                    (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{what}, {how}"),
                    (Ok(block), Ok(got)) => {
                        let walked: Relations = block
                            .elements_skip_metadata()
                            .filter_map(|el| match el {
                                Element::Relation(r) => {
                                    Some((r.id(), r.members().map(|m| m.id).collect()))
                                }
                                _ => None,
                            })
                            .collect();
                        assert_eq!(got, walked, "{what}, {how}");
                    }
                    (oracle, got) => panic!(
                        "{what}, {how}: construction accepted {}, relations projection accepted {}",
                        oracle.is_ok(),
                        got.is_ok()
                    ),
                }
            }
        }
    }

    /// A blob whose claim its elements contradict is refused after
    /// validation, as the block path refuses it, and a malformed one for its
    /// malformation first.
    #[test]
    fn a_contradicted_claim_is_refused_like_construction() {
        use crate::read::wire_fixtures::projection_fixtures;

        let (_, bytes) = projection_fixtures().swap_remove(0);
        let claim = Some(ElementKind::Way);
        let project = |input: Vec<u8>| {
            let mut scratch = IdScratch::default();
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            crate::read::block::BlockBuffer::new(input, None, &mut st, &mut gr, claim)
                .project(|_, _| KindIds::of(ElementKind::Node, &mut scratch))
                .map(drop)
        };
        let build = |input: Vec<u8>| {
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            crate::read::block::BlockBuffer::new(input, None, &mut st, &mut gr, claim)
                .block()
                .map(drop)
        };
        let projected = project(bytes.clone()).expect_err("a node blob claiming ways");
        assert!(projected.kind_contradiction().is_some(), "{projected}");
        assert_eq!(
            projected.to_string(),
            build(bytes.clone())
                .expect_err("the block path refuses it")
                .to_string()
        );

        let truncated = bytes[..bytes.len() - 1].to_vec();
        let malformed = project(truncated.clone()).expect_err("truncated");
        assert!(malformed.kind_contradiction().is_none(), "{malformed}");
        assert_eq!(
            malformed.to_string(),
            build(truncated)
                .expect_err("the block path refuses it")
                .to_string()
        );
    }
}
