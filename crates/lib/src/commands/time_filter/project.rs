use crate::read::block::{GroupProjection, PrimitiveGroup};
use crate::read::wire::{
    PackedBoolIter, PackedInt32Iter, PackedSint64Iter, WireDenseInfo, WireDenseNodes, WireInfo,
    WireNode, WireRelation, WireWay,
};
use crate::{Element, ElementKind, ElementKindSet, PrimitiveBlock, Result};

pub(super) struct Record {
    pub(super) kind: ElementKind,
    pub(super) id: i64,
    pub(super) version: Option<i32>,
    pub(super) visible: bool,
    timestamp: Option<i64>,
}

impl Record {
    pub(super) fn timestamp(&self, granularity: i64) -> Option<Result<i64>> {
        super::claim_seconds(
            self.timestamp
                .map(|raw| crate::read::claims::scale_milli_timestamp(raw, granularity)),
        )
    }
}

pub(super) struct Records {
    pub(super) records: Vec<Record>,
    pub(super) date_granularity: i64,
    group_lengths: Vec<usize>,
    group_start: usize,
}

impl Records {
    pub(super) fn new(date_granularity: i32) -> Self {
        Self {
            records: Vec::new(),
            date_granularity: i64::from(date_granularity),
            group_lengths: Vec::new(),
            group_start: 0,
        }
    }

    fn plain(&mut self, kind: ElementKind, id: i64, info: Option<&WireInfo>) {
        self.records.push(Record {
            kind,
            id,
            version: info.and_then(|info| info.version).filter(|&v| v >= 1),
            visible: info.and_then(|info| info.visible).unwrap_or(true),
            timestamp: info.and_then(|info| info.timestamp),
        });
    }

    /// Resolves ascending winner positions. Entire losing groups can be
    /// skipped. Within a dense group, the existing encoder needs cumulative
    /// coordinates, metadata and tag offsets, so its iterator must advance
    /// across preceding slots to reconstruct a winner.
    pub(super) fn encoder<'a>(
        &'a self,
        block: &'a PrimitiveBlock,
    ) -> impl FnMut(usize) -> Result<Element<'a>> + 'a {
        let mut groups = block.groups().zip(self.group_lengths.iter().copied());
        let mut active = None;
        let mut group_end = 0;
        let mut next = 0;
        move |position| {
            while position >= group_end {
                let (group, count) = groups.next().ok_or_else(position_error)?;
                next = group_end;
                group_end += count;
                if position < group_end {
                    active = Some(group_elements(group));
                }
            }
            let skip = position.checked_sub(next).ok_or_else(position_error)?;
            let element = active
                .as_mut()
                .and_then(|elements| elements.nth(skip))
                .ok_or_else(position_error)?;
            next = position + 1;
            Ok(element)
        }
    }
}

impl GroupProjection for Records {
    fn node(&mut self, node: &WireNode<'_>, info: Option<&WireInfo>) {
        self.plain(ElementKind::Node, node.id, info);
    }

    fn way(&mut self, way: &WireWay<'_>, info: Option<&WireInfo>) {
        self.plain(ElementKind::Way, way.id, info);
    }

    fn relation(&mut self, relation: &WireRelation<'_>, info: Option<&WireInfo>) {
        self.plain(ElementKind::Relation, relation.id, info);
    }

    fn dense(&mut self, dense: &WireDenseNodes<'_>, _: usize, info: Option<&WireDenseInfo<'_>>) {
        let mut versions = PackedInt32Iter::new(info.map_or(&[][..], |i| i.version_data));
        let mut timestamps = PackedSint64Iter::new(info.map_or(&[][..], |i| i.timestamp_data));
        let mut visibility = PackedBoolIter::new(info.map_or(&[][..], |i| i.visible_data));
        let mut id = 0_i64;
        let mut cumulative_timestamp = 0_i64;
        let dense_start = self.records.len();
        for delta in PackedSint64Iter::new(dense.id_data) {
            id = id.wrapping_add(delta);
            let version = versions.next().filter(|&v| v >= 1);
            let timestamp = timestamps.next().and_then(|delta| {
                cumulative_timestamp = cumulative_timestamp.wrapping_add(delta);
                // A versionless dense slot with zero scaled milliseconds is
                // padding. Overflow remains a claim and is checked later.
                (version.is_some() || (cumulative_timestamp != 0 && self.date_granularity != 0))
                    .then_some(cumulative_timestamp)
            });
            self.records.push(Record {
                kind: ElementKind::Node,
                id,
                version,
                visible: visibility.next().unwrap_or(true),
                timestamp,
            });
        }
        // The validator visits dense last, while elements() puts dense first.
        let dense_count = self.records.len() - dense_start;
        self.records[self.group_start..].rotate_right(dense_count);
    }

    fn group_end(&mut self, _: ElementKindSet) {
        self.group_lengths
            .push(self.records.len() - self.group_start);
        self.group_start = self.records.len();
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the chained iterators borrow the group for the returned lifetime"
)]
fn group_elements(group: PrimitiveGroup<'_>) -> impl Iterator<Item = Element<'_>> {
    group
        .dense_nodes()
        .map(Element::DenseNode)
        .chain(group.nodes().map(Element::Node))
        .chain(group.ways().map(Element::Way))
        .chain(group.relations().map(Element::Relation))
}

fn position_error() -> crate::Error {
    crate::Error::internal("time-filter: winner position disagrees with validated block")
}
