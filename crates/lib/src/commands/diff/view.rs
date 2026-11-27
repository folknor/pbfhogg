//! Borrowed comparison rows over bytes accepted by the block validator.
//!
//! Keep packed bytes as proofs of equality only. Byte inequality falls
//! through to the same content tokens used by digest, since distinct
//! encodings and distinct string IDs can represent the same content.

use std::cell::OnceCell;

use crate::ElementKind;
use crate::content::{Content, Equality, Relation, Token, content_equal};
use crate::error::{Result, new_stringtable_index_out_of_bounds, new_wire_error};
use crate::read::elements::{MemberIdIter, WayRefIter};
use crate::read::wire::{
    BlockFields, Cursor, PackedBoolIter, PackedInt32Iter, PackedUint32Iter, WireDenseInfo,
    WireDenseNodes, WireGroup, WireInfo, WireMessageIter, WireNode, WireRelation, WireWay,
};
use crate::scan::node::{
    scale_to_decimicro, scale_to_nano, unrepresentable_coordinate_err,
    unrepresentable_location_nano_err,
};

pub(crate) struct BlockView<'a> {
    bytes: &'a [u8],
    groups: &'a [(u32, u32)],
    strings: &'a [(u32, u32)],
    fields: BlockFields,
    resolved: OnceCell<Vec<&'a str>>,
}

impl<'a> BlockView<'a> {
    /// All four arguments come from the same successful block validation.
    pub(crate) fn new(
        bytes: &'a [u8],
        groups: &'a [(u32, u32)],
        strings: &'a [(u32, u32)],
        fields: BlockFields,
    ) -> Self {
        Self {
            bytes,
            groups,
            strings,
            fields,
            resolved: OnceCell::new(),
        }
    }

    fn slice(&self, span: (u32, u32)) -> &'a [u8] {
        slice(self.bytes, span)
    }

    fn strings(&self) -> &[&'a str] {
        self.resolved.get_or_init(|| {
            self.strings
                .iter()
                .map(|&span| accepted(std::str::from_utf8(self.slice(span))))
                .collect()
        })
    }

    fn coordinate_framing(&self) -> (i32, i64, i64) {
        (
            self.fields.granularity,
            self.fields.lat_offset,
            self.fields.lon_offset,
        )
    }

    pub(crate) fn rows(&self) -> Rows<'a> {
        Rows {
            bytes: self.bytes,
            fields: self.fields,
            groups: self.groups.iter().enumerate(),
            group: None,
        }
    }
}

fn slice(bytes: &[u8], (offset, len): (u32, u32)) -> &[u8] {
    let offset = offset as usize;
    &bytes[offset..offset + len as usize]
}

pub(crate) struct Rows<'a> {
    bytes: &'a [u8],
    fields: BlockFields,
    groups: std::iter::Enumerate<std::slice::Iter<'a, (u32, u32)>>,
    group: Option<GroupRows<'a>>,
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(group) = self.group.as_mut()
                && let Some(row) = group.next(self.fields)
            {
                return Some(row);
            }
            let (index, &span) = self.groups.next()?;
            let group = WireGroup::new(slice(self.bytes, span));
            self.group = Some(GroupRows {
                dense: accepted(group.dense()).map(|data| DenseRows::new(index, data)),
                nodes: group.nodes(),
                ways: group.ways(),
                relations: group.relations(),
            });
        }
    }
}

struct GroupRows<'a> {
    dense: Option<DenseRows<'a>>,
    nodes: WireMessageIter<'a>,
    ways: WireMessageIter<'a>,
    relations: WireMessageIter<'a>,
}

impl<'a> GroupRows<'a> {
    fn next(&mut self, fields: BlockFields) -> Option<Row<'a>> {
        self.dense
            .as_mut()
            .and_then(|dense| dense.next(fields))
            .or_else(|| self.nodes.next().map(|data| Row::node(data, fields)))
            .or_else(|| self.ways.next().map(|data| Row::way(data, fields)))
            .or_else(|| self.relations.next().map(Row::relation))
    }
}

#[track_caller]
fn accepted<T, E: std::fmt::Display>(parsed: std::result::Result<T, E>) -> T {
    match parsed {
        Ok(value) => value,
        Err(err) => panic!("diff view failed to read a validated block: {err}"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tags<'a> {
    Dense(&'a [u8]),
    Split(&'a [u8], &'a [u8]),
}

impl<'a> Tags<'a> {
    fn is_empty(self) -> bool {
        match self {
            Self::Dense(data) | Self::Split(data, _) => data.is_empty(),
        }
    }

    fn sids(self) -> impl Iterator<Item = (usize, usize)> + use<'a> {
        let (mut dense, mut split) = match self {
            Self::Dense(data) => (Cursor::new(data), None),
            Self::Split(keys, vals) => (
                Cursor::new(&[]),
                Some(PackedUint32Iter::new(keys).zip(PackedUint32Iter::new(vals))),
            ),
        };
        std::iter::from_fn(move || {
            if let Some(split) = split.as_mut() {
                return split.next().map(|(key, val)| (key as usize, val as usize));
            }
            if dense.is_empty() {
                return None;
            }
            Some((sid(&mut dense), sid(&mut dense)))
        })
    }
}

fn sid(cursor: &mut Cursor<'_>) -> usize {
    match usize::try_from(accepted(cursor.read_varint())) {
        Ok(value) => value,
        Err(_) => panic!("diff view read a validated tag SID beyond usize"),
    }
}

#[derive(Clone, Copy)]
enum Geometry<'a> {
    Node {
        lat: i64,
        lon: i64,
        fields: BlockFields,
    },
    Way {
        refs: &'a [u8],
        lats: &'a [u8],
        lons: &'a [u8],
        fields: BlockFields,
    },
    Relation {
        roles: &'a [u8],
        ids: &'a [u8],
        types: &'a [u8],
    },
}

#[derive(Clone, Copy)]
enum Meta<'a> {
    Plain(Option<&'a [u8]>),
    Dense {
        group: usize,
        slot: usize,
        info: Option<&'a [u8]>,
    },
}

pub(crate) struct Row<'a> {
    pub(crate) id: i64,
    tags: Tags<'a>,
    geometry: Geometry<'a>,
    meta: Meta<'a>,
}

impl<'a> Row<'a> {
    fn node(data: &'a [u8], fields: BlockFields) -> Self {
        let node = accepted(WireNode::parse(data));
        Self {
            id: node.id,
            tags: Tags::Split(node.keys_data, node.vals_data),
            geometry: Geometry::Node {
                lat: node.lat,
                lon: node.lon,
                fields,
            },
            meta: Meta::Plain(node.info_data),
        }
    }

    fn way(data: &'a [u8], fields: BlockFields) -> Self {
        let way = accepted(WireWay::parse(data));
        Self {
            id: way.id,
            tags: Tags::Split(way.keys_data, way.vals_data),
            geometry: Geometry::Way {
                refs: way.refs_data,
                lats: way.lat_data,
                lons: way.lon_data,
                fields,
            },
            meta: Meta::Plain(way.info_data),
        }
    }

    fn relation(data: &'a [u8]) -> Self {
        let rel = accepted(WireRelation::parse(data));
        Self {
            id: rel.id,
            tags: Tags::Split(rel.keys_data, rel.vals_data),
            geometry: Geometry::Relation {
                roles: rel.roles_sid_data,
                ids: rel.memids_data,
                types: rel.types_data,
            },
            meta: Meta::Plain(rel.info_data),
        }
    }

    fn kind(&self) -> ElementKind {
        match self.geometry {
            Geometry::Node { .. } => ElementKind::Node,
            Geometry::Way { .. } => ElementKind::Way,
            Geometry::Relation { .. } => ElementKind::Relation,
        }
    }

    pub(crate) fn version(&self, dense: &mut MetadataCursor<'a>) -> Option<i32> {
        let version = match self.meta {
            Meta::Plain(info) => info.and_then(|data| accepted(WireInfo::parse(data)).version),
            Meta::Dense { group, slot, info } => dense.group(group, info).versions.at(slot),
        };
        version.filter(|&v| v >= 1)
    }

    fn hidden(&self, dense: &mut MetadataCursor<'a>) -> bool {
        let visible = match self.meta {
            Meta::Plain(info) => info.and_then(|data| accepted(WireInfo::parse(data)).visible),
            Meta::Dense { group, slot, info } => dense.group(group, info).visible.at(slot),
        };
        visible == Some(false)
    }
}

struct DenseRows<'a> {
    ids: WayRefIter<'a>,
    lats: WayRefIter<'a>,
    lons: WayRefIter<'a>,
    keys_vals: &'a [u8],
    tags: Cursor<'a>,
    info: Option<&'a [u8]>,
    group: usize,
    slot: usize,
}

impl<'a> DenseRows<'a> {
    fn new(group: usize, data: &'a [u8]) -> Self {
        let dense = accepted(WireDenseNodes::parse(data));
        Self {
            ids: WayRefIter::delta_decoded(dense.id_data),
            lats: WayRefIter::delta_decoded(dense.lat_data),
            lons: WayRefIter::delta_decoded(dense.lon_data),
            keys_vals: dense.keys_vals_data,
            tags: Cursor::new(dense.keys_vals_data),
            info: dense.info_data,
            group,
            slot: 0,
        }
    }

    fn next(&mut self, fields: BlockFields) -> Option<Row<'a>> {
        let id = self.ids.next()?;
        let (Some(lat), Some(lon)) = (self.lats.next(), self.lons.next()) else {
            panic!("diff view found short coordinates in a validated dense group");
        };
        let slot = self.slot;
        self.slot += 1;
        Some(Row {
            id,
            tags: Tags::Dense(dense_tags(self.keys_vals, &mut self.tags)),
            geometry: Geometry::Node { lat, lon, fields },
            meta: Meta::Dense {
                group: self.group,
                slot,
                info: self.info,
            },
        })
    }
}

fn dense_tags<'a>(data: &'a [u8], cursor: &mut Cursor<'a>) -> &'a [u8] {
    let start = data.len() - cursor.remaining();
    loop {
        let end = data.len() - cursor.remaining();
        if cursor.is_empty() || accepted(cursor.read_varint()) == 0 {
            return &data[start..end];
        }
        accepted(cursor.read_varint());
    }
}

/// The independent metadata columns that affect equality or common text.
/// Each advances only when its own field is requested, and none of the
/// other DenseInfo deltas is part of the comparison walk.
#[derive(Default)]
pub(crate) struct MetadataCursor<'a> {
    dense: Option<DenseMetadata<'a>>,
}

struct DenseMetadata<'a> {
    group: usize,
    versions: Column<PackedInt32Iter<'a>>,
    visible: Column<PackedBoolIter<'a>>,
}

impl<'a> MetadataCursor<'a> {
    fn group(&mut self, group: usize, info: Option<&'a [u8]>) -> &mut DenseMetadata<'a> {
        if self
            .dense
            .as_ref()
            .is_some_and(|prior| prior.group != group)
        {
            self.dense = None;
        }
        self.dense.get_or_insert_with(|| {
            let info = info.map(|data| accepted(WireDenseInfo::parse(data)));
            DenseMetadata {
                group,
                versions: Column::new(PackedInt32Iter::new(info.map_or(&[], |i| i.version_data))),
                visible: Column::new(PackedBoolIter::new(info.map_or(&[], |i| i.visible_data))),
            }
        })
    }
}

struct Column<I: Iterator> {
    iter: I,
    next: usize,
    last: Option<I::Item>,
}

impl<I: Iterator> Column<I>
where
    I::Item: Copy,
{
    fn new(iter: I) -> Self {
        Self {
            iter,
            next: 0,
            last: None,
        }
    }

    fn at(&mut self, slot: usize) -> Option<I::Item> {
        if slot.checked_add(1) == Some(self.next) {
            return self.last;
        }
        assert!(slot >= self.next, "diff metadata cursor moved backwards");
        self.last = self.iter.nth(slot - self.next);
        self.next = slot + 1;
        self.last
    }
}

struct RowContent<'a, 's> {
    row: &'s Row<'a>,
    block: &'s BlockView<'a>,
}

impl Content for RowContent<'_, '_> {
    fn content_kind(&self) -> ElementKind {
        self.row.kind()
    }

    fn content_tokens(&self) -> Result<impl Iterator<Item = Result<Token<'_>>>> {
        let (coord, refs, members) = match self.row.geometry {
            Geometry::Node { lat, lon, fields } => {
                (Some(node_coord(lat, lon, fields)?), None, None)
            }
            Geometry::Way { refs, .. } => (None, Some(WayRefIter::delta_decoded(refs)), None),
            Geometry::Relation { roles, ids, types } => (
                None,
                None,
                Some(MemberIdIter::new(ids, types).zip(PackedInt32Iter::new(roles))),
            ),
        };
        let tags = self.row.tags.sids().map(|(k, v)| {
            let strings = self.block.strings();
            Ok(Token::Tag(strings[k], strings[v]))
        });
        let members = members.into_iter().flatten().map(|(id, sid)| {
            role(self.block.strings(), sid).map(|role| Token::Member { id, role })
        });
        Ok(coord
            .into_iter()
            .map(|(lat, lon)| Ok(Token::Coord { lat, lon }))
            .chain(tags)
            .chain(refs.into_iter().flatten().map(|id| Ok(Token::Ref(id))))
            .chain(members))
    }
}

fn role<'a>(strings: &[&'a str], sid: i32) -> Result<&'a str> {
    let index = usize::try_from(sid)
        .map_err(|_| new_wire_error("relation member has negative role string index"))?;
    strings
        .get(index)
        .copied()
        .ok_or_else(|| new_stringtable_index_out_of_bounds(index))
}

fn node_coord(lat: i64, lon: i64, fields: BlockFields) -> Result<(i32, i32)> {
    let granularity = i64::from(fields.granularity);
    Ok((
        scale_to_decimicro(fields.lat_offset, granularity, lat)
            .ok_or_else(unrepresentable_coordinate_err)?,
        scale_to_decimicro(fields.lon_offset, granularity, lon)
            .ok_or_else(unrepresentable_coordinate_err)?,
    ))
}

fn locations(geometry: Geometry<'_>) -> impl Iterator<Item = Result<(i64, i64)>> {
    let Geometry::Way {
        lats, lons, fields, ..
    } = geometry
    else {
        unreachable!("inline locations are compared only for ways");
    };
    WayRefIter::delta_decoded(lats)
        .zip(WayRefIter::delta_decoded(lons))
        .map(move |(lat, lon)| {
            let granularity = i64::from(fields.granularity);
            let lat = scale_to_nano(fields.lat_offset, granularity, lat)
                .ok_or_else(unrepresentable_location_nano_err)?;
            let lon = scale_to_nano(fields.lon_offset, granularity, lon)
                .ok_or_else(unrepresentable_location_nano_err)?;
            Ok((lat / 100, lon / 100))
        })
}

/// A proof cache for the two blocks in the current merge round.
pub(crate) struct Pair {
    same_table: OnceCell<bool>,
    same_coordinate_framing: bool,
}

impl Pair {
    pub(crate) fn new(old: &BlockView<'_>, new: &BlockView<'_>) -> Self {
        Self {
            same_table: OnceCell::new(),
            same_coordinate_framing: old.coordinate_framing() == new.coordinate_framing(),
        }
    }

    fn same_table(&self, (old, new): (&BlockView<'_>, &BlockView<'_>)) -> bool {
        *self.same_table.get_or_init(|| {
            old.strings.len() == new.strings.len()
                && old
                    .strings
                    .iter()
                    .zip(new.strings)
                    .all(|(&a, &b)| old.slice(a) == new.slice(b))
        })
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the pair holds two mutable borrows; taking it by value is how the caller lends both"
    )]
    pub(crate) fn equal<'old, 'new>(
        &self,
        blocks: (&BlockView<'old>, &BlockView<'new>),
        old: &Row<'old>,
        new: &Row<'new>,
        equality: Equality,
        metadata: (&mut MetadataCursor<'old>, &mut MetadataCursor<'new>),
    ) -> Result<bool> {
        let content = self.content_equal(blocks, old, new)?;
        match equality {
            Equality::ContentVisibility => {
                Ok(content && old.hidden(metadata.0) == new.hidden(metadata.1))
            }
            Equality::Relation(Relation::ContentLocations)
                if old.kind() == ElementKind::Way && new.kind() == ElementKind::Way =>
            {
                // Both sides' entire lists must be checked even if content
                // already differs or one list is longer than the other.
                let mut equal = content;
                let mut old = locations(old.geometry);
                let mut new = locations(new.geometry);
                loop {
                    let a = old.next().transpose()?;
                    let b = new.next().transpose()?;
                    if a.is_none() && b.is_none() {
                        return Ok(equal);
                    }
                    equal &= a == b;
                }
            }
            Equality::Relation(_) => Ok(content),
        }
    }

    fn content_equal<'old, 'new>(
        &self,
        blocks: (&BlockView<'old>, &BlockView<'new>),
        old: &Row<'old>,
        new: &Row<'new>,
    ) -> Result<bool> {
        if old.kind() != new.kind() {
            return Ok(false);
        }
        if let Some(equal) = self.packed_proof(blocks, old, new)? {
            return Ok(equal);
        }
        let (a, b) = blocks;
        content_equal(
            &RowContent { row: old, block: a },
            &RowContent { row: new, block: b },
        )
    }

    fn packed_proof(
        &self,
        blocks: (&BlockView<'_>, &BlockView<'_>),
        old: &Row<'_>,
        new: &Row<'_>,
    ) -> Result<Option<bool>> {
        if old.tags != new.tags || (!old.tags.is_empty() && !self.same_table(blocks)) {
            return Ok(None);
        }
        match (old.geometry, new.geometry) {
            (
                Geometry::Node {
                    lat: a_lat,
                    lon: a_lon,
                    fields: a,
                },
                Geometry::Node {
                    lat: b_lat,
                    lon: b_lon,
                    fields: b,
                },
            ) => {
                let coord = node_coord(a_lat, a_lon, a)?;
                let identical = self.same_coordinate_framing && a_lat == b_lat && a_lon == b_lon;
                Ok(Some(identical || coord == node_coord(b_lat, b_lon, b)?))
            }
            (Geometry::Way { refs: a, .. }, Geometry::Way { refs: b, .. }) if a == b => {
                Ok(Some(true))
            }
            (
                Geometry::Relation {
                    roles: a_roles,
                    ids: a_ids,
                    types: a_types,
                },
                Geometry::Relation {
                    roles: b_roles,
                    ids: b_ids,
                    types: b_types,
                },
            ) if a_ids == b_ids && a_types == b_types && a_roles == b_roles => {
                if a_roles.is_empty() {
                    return Ok(Some(true));
                }
                if !self.same_table(blocks) {
                    return Ok(None);
                }
                // Same bytes and same table may still produce Err tokens.
                let len = blocks.0.strings.len();
                Ok(Some(PackedInt32Iter::new(a_roles).all(|sid| {
                    usize::try_from(sid).is_ok_and(|index| index < len)
                })))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::PrimitiveBlock;
    use crate::osc::merge_join::{element_id, element_version};
    use crate::read::block::{NoProjection, validate_in_place};

    const TEXT: Equality = Equality::Relation(Relation::Content);
    const TEXT_LOCATIONS: Equality = Equality::Relation(Relation::ContentLocations);
    const OSC: Equality = Equality::ContentVisibility;

    struct Fixture {
        bytes: Vec<u8>,
        groups: Vec<(u32, u32)>,
        strings: Vec<(u32, u32)>,
        fields: BlockFields,
        full: PrimitiveBlock,
    }

    impl Fixture {
        fn new(bytes: Vec<u8>) -> Self {
            let (mut strings, mut groups) = (Vec::new(), Vec::new());
            let (_, scan) =
                validate_in_place(&bytes, &mut strings, &mut groups, None, |_, _| NoProjection)
                    .unwrap();
            let full = PrimitiveBlock::from_vec(bytes.clone()).unwrap();
            Self {
                bytes,
                groups,
                strings,
                fields: scan.fields,
                full,
            }
        }

        fn view(&self) -> BlockView<'_> {
            BlockView::new(&self.bytes, &self.groups, &self.strings, self.fields)
        }
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while value >= 128 {
            bytes.push(u8::try_from(value & 127).unwrap() | 128);
            value >>= 7;
        }
        bytes.push(u8::try_from(value).unwrap());
        bytes
    }

    fn field(out: &mut Vec<u8>, field: u64, value: u64) {
        out.extend(varint(field << 3));
        out.extend(varint(value));
    }

    fn message(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
        out.extend(varint((field << 3) | 2));
        out.extend(varint(bytes.len() as u64));
        out.extend_from_slice(bytes);
    }

    fn zigzag(value: i64) -> u64 {
        ((value << 1) ^ (value >> 63)).cast_unsigned()
    }

    fn block(strings: &[&str], groups: &[Vec<u8>], framing: (i32, i64, i64)) -> Fixture {
        let mut table = Vec::new();
        for s in strings {
            message(&mut table, 1, s.as_bytes());
        }
        let mut bytes = Vec::new();
        message(&mut bytes, 1, &table);
        for group in groups {
            message(&mut bytes, 2, group);
        }
        field(&mut bytes, 17, i64::from(framing.0).cast_unsigned());
        field(&mut bytes, 19, framing.1.cast_unsigned());
        field(&mut bytes, 20, framing.2.cast_unsigned());
        Fixture::new(bytes)
    }

    fn plain_node(
        group: &mut Vec<u8>,
        id: i64,
        coord: (i64, i64),
        tags: (&[u8], &[u8]),
        info: &[u8],
    ) {
        let mut node = Vec::new();
        field(&mut node, 1, zigzag(id));
        message(&mut node, 2, tags.0);
        message(&mut node, 3, tags.1);
        message(&mut node, 4, info);
        field(&mut node, 8, zigzag(coord.0));
        field(&mut node, 9, zigzag(coord.1));
        message(group, 1, &node);
    }

    fn dense_nodes(ids: &[i64], lats: &[i64], lons: &[i64], tags: &[u8], info: &[u8]) -> Vec<u8> {
        let mut dense = Vec::new();
        for (column, values) in [(1, ids), (8, lats), (9, lons)] {
            let deltas: Vec<u8> = values
                .iter()
                .copied()
                .scan(0_i64, |previous, current| {
                    let delta = current.wrapping_sub(*previous);
                    *previous = current;
                    Some(delta)
                })
                .flat_map(|delta| varint(zigzag(delta)))
                .collect();
            message(&mut dense, column, &deltas);
        }
        message(&mut dense, 5, info);
        message(&mut dense, 10, tags);
        let mut group = Vec::new();
        message(&mut group, 2, &dense);
        group
    }

    fn records(kind: u64, fields: &[(u64, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        field(&mut body, 1, 1);
        for &(column, values) in fields {
            message(&mut body, column, values);
        }
        let mut group = Vec::new();
        message(&mut group, kind, &body);
        group
    }

    fn check(old: &Fixture, new: &Fixture, verdicts: &[[Option<bool>; 3]]) {
        let old_view = old.view();
        let new_view = new.view();
        let pair = Pair::new(&old_view, &new_view);
        let old_rows: Vec<_> = old_view.rows().collect();
        let new_rows: Vec<_> = new_view.rows().collect();
        assert_eq!(old_rows.len(), verdicts.len());
        assert_eq!(new_rows.len(), verdicts.len());
        let old_full: Vec<_> = old.full.elements().collect();
        let new_full: Vec<_> = new.full.elements().collect();
        assert_eq!(old_full.len(), verdicts.len());
        assert_eq!(new_full.len(), verdicts.len());

        for (index, equality) in [TEXT, TEXT_LOCATIONS, OSC].into_iter().enumerate() {
            let (mut old_meta, mut new_meta) =
                (MetadataCursor::default(), MetadataCursor::default());
            for (slot, (old_row, new_row)) in old_rows.iter().zip(&new_rows).enumerate() {
                let (old_element, new_element) = (&old_full[slot], &new_full[slot]);
                assert_eq!(old_row.id, element_id(old_element));
                assert_eq!(new_row.id, element_id(new_element));
                let full = equality.equal(old_element, new_element);
                let view = pair.equal(
                    (&old_view, &new_view),
                    old_row,
                    new_row,
                    equality,
                    (&mut old_meta, &mut new_meta),
                );
                if let Some(equal) = verdicts[slot][index] {
                    assert_eq!(
                        full.unwrap(),
                        equal,
                        "full element at slot {slot}, {equality:?}"
                    );
                    assert_eq!(
                        view.unwrap(),
                        equal,
                        "view row at slot {slot}, {equality:?}"
                    );
                } else {
                    let full_error = full.unwrap_err();
                    let view_error = view.unwrap_err();
                    let (
                        crate::ErrorKind::WireFormat { msg: full_msg },
                        crate::ErrorKind::WireFormat { msg: view_msg },
                    ) = (full_error.kind(), view_error.kind())
                    else {
                        panic!(
                            "expected coordinate wire errors at slot {slot}, {equality:?}: {full_error:?}, {view_error:?}"
                        );
                    };
                    let reason = if old_row.kind() == ElementKind::Node {
                        "node coordinate does not fit in an i32 decimicrodegree"
                    } else {
                        "way node location does not fit in an i64 nanodegree"
                    };
                    assert!(full_msg.contains(reason), "{full_error}");
                    assert_eq!(view_msg, full_msg, "slot {slot}, {equality:?}");
                }
                if equality == TEXT {
                    // Common text without -c must print the old version.
                    assert_eq!(old_row.version(&mut old_meta), element_version(old_element));
                    assert_eq!(new_row.version(&mut new_meta), element_version(new_element));
                }
            }
        }
    }

    #[test]
    fn dense_and_plain_nodes_compare_normalized_content_and_lazy_metadata() {
        let extreme = i64::from(i32::MAX);
        let minimum = i64::from(i32::MIN);
        let mut d_info = Vec::new();
        message(&mut d_info, 1, &[2, 0, 5, 7]);
        message(&mut d_info, 2, &[8, 12, 0, 2]);
        message(&mut d_info, 3, &[6, 3, 1, 2]);
        message(&mut d_info, 4, &[4, 3, 0, 2]);
        message(&mut d_info, 5, &[2, 0, 2, 0]);
        message(&mut d_info, 6, &[1, 0, 1, 2]);
        let dense = dense_nodes(
            &[-1, -2, 1, 2],
            &[extreme, minimum, 10, 11],
            &[minimum, extreme, -10, -11],
            &[1, 2, 0, 1, 2, 0, 0, 1, 2],
            &d_info,
        );
        let old = block(&["", "key", "value"], &[dense], (100, 0, 0));

        let coord = |lat: i64, lon: i64| (lat * 2, lon * 2);
        let mut plain = Vec::new();
        plain_node(
            &mut plain,
            -1,
            coord(extreme, minimum),
            (&[2], &[1]),
            &[8, 9],
        );
        plain_node(
            &mut plain,
            -2,
            coord(minimum, extreme),
            (&[2], &[3]),
            &[8, 3],
        );
        plain_node(&mut plain, 1, coord(10, -10), (&[], &[]), &[48, 0]);
        plain_node(&mut plain, 2, coord(11, -11), (&[2], &[1]), &[8, 7, 48, 1]);
        let new = block(&["", "value", "key", "value"], &[plain], (50, 0, 0));
        check(
            &old,
            &new,
            &[
                [Some(true), Some(true), Some(true)],
                [Some(true), Some(true), Some(false)],
                [Some(true), Some(true), Some(false)],
                [Some(true), Some(true), Some(true)],
            ],
        );
    }

    #[test]
    fn dense_packed_tags_cannot_prove_equality_across_different_tables() {
        let group = dense_nodes(&[1, 2, 3], &[1, 2, 3], &[1, 2, 3], &[1, 2, 0, 0, 0], &[]);
        let old = block(
            &["", "key", "old"],
            std::slice::from_ref(&group),
            (100, 0, 0),
        );
        let new = block(&["", "key", "new"], &[group], (100, 0, 0));
        check(
            &old,
            &new,
            &[[Some(false); 3], [Some(true); 3], [Some(true); 3]],
        );
    }

    #[test]
    fn dense_metadata_cursors_skip_independent_columns_and_reset_at_group_boundaries() {
        let mut info = Vec::new();
        message(&mut info, 1, &[3, 4, 5]);
        message(&mut info, 6, &[0, 1, 0]);
        let a = dense_nodes(&[1, 2, 3], &[1, 2, 3], &[1, 2, 3], &[], &info);
        let b = dense_nodes(&[4], &[4], &[4], &[], &[]);
        let block = block(&[""], &[a, b], (100, 0, 0));
        let view = block.view();
        let rows: Vec<_> = view.rows().collect();
        let mut cursor = MetadataCursor::default();
        assert!(rows[2].hidden(&mut cursor));
        assert_eq!(rows[1].version(&mut cursor), Some(4));
        assert_eq!(rows[2].version(&mut cursor), Some(5));
        assert!(!rows[3].hidden(&mut cursor));
        assert_eq!(rows[3].version(&mut cursor), None);
    }

    #[test]
    fn different_varints_and_duplicate_strings_compare_semantically() {
        let old_group = records(
            3,
            &[
                (2, &[1]),
                (3, &[2]),
                (8, &[2, 1]),
                (9, &[2, 1]),
                (10, &[1, 2]),
            ],
        );
        let new_group = records(
            3,
            &[
                (2, &[1]),
                (3, &[3]),
                (8, &[0x82, 0, 1]),
                // At half the granularity, double each signed delta.
                (9, &[4, 3]),
                (10, &[3, 4]),
            ],
        );
        let old = block(&["", "key", "value", "value"], &[old_group], (100, 0, 0));
        let new = block(&["", "key", "value", "value"], &[new_group], (50, 0, 0));
        check(&old, &new, &[[Some(true); 3]]);

        let changed_group = records(
            3,
            &[
                (2, &[1]),
                (3, &[2]),
                (8, &[2, 1]),
                (9, &[2, 2]),
                (10, &[1, 2]),
            ],
        );
        let changed = block(
            &["", "key", "value", "value"],
            &[changed_group],
            (100, 0, 0),
        );
        check(&old, &changed, &[[Some(true), Some(false), Some(true)]]);
    }

    #[test]
    fn relation_members_use_text_roles_and_invalid_roles_are_unequal_even_to_themselves() {
        let relation = |role| records(4, &[(8, &[role]), (9, &[2]), (10, &[0])]);
        let old = block(&["", "outer", "outer"], &[relation(1)], (100, 0, 0));
        let new = block(&["", "inner", "outer"], &[relation(2)], (100, 0, 0));
        check(&old, &new, &[[Some(true); 3]]);
        let different = block(&["", "inner", "outer"], &[relation(1)], (100, 0, 0));
        check(&old, &different, &[[Some(false); 3]]);
        let bad = block(&["", "outer"], &[relation(2)], (100, 0, 0));
        check(&bad, &bad, &[[Some(false); 3]]);
        check(&bad, &new, &[[Some(false); 3]]);

        let negative = varint((-1_i64).cast_unsigned());
        let group = records(4, &[(8, &negative), (9, &[2]), (10, &[0])]);
        let bad = block(&["", "outer"], &[group], (100, 0, 0));
        check(&bad, &bad, &[[Some(false); 3]]);
    }

    #[test]
    fn coordinate_errors_are_refused_in_the_same_formats_as_full_elements() {
        let dense = dense_nodes(&[1], &[i64::MAX], &[0], &[], &[]);
        let bad = block(&[""], &[dense], (100, 0, 0));
        let mut good_group = Vec::new();
        plain_node(&mut good_group, 1, (0, 0), (&[], &[]), &[]);
        let good = block(&[""], &[good_group], (100, 0, 0));
        check(&good, &bad, &[[None; 3]]);
        check(&bad, &bad, &[[None; 3]]);

        let max = varint(zigzag(i64::MAX));
        let way = records(3, &[(8, &[2]), (9, &max), (10, &[0])]);
        let good_way = records(3, &[(8, &[4])]);
        let bad_way = block(&[""], &[way], (100, 0, 0));
        let good_way = block(&[""], &[good_way], (100, 0, 0));
        check(&good_way, &bad_way, &[[Some(false), None, Some(false)]]);
        check(&bad_way, &bad_way, &[[Some(true), None, Some(true)]]);
    }

    fn entrypoint_blocks() -> [Vec<Fixture>; 2] {
        let coord = (100, 0, 0);
        let table = ["", "alice", "bob", "carol"];
        let nodes = |ids: &[i64], lats: &[i64], versions: &[u8]| {
            let mut info = Vec::new();
            message(&mut info, 1, versions);
            let group = dense_nodes(ids, lats, &vec![0; ids.len()], &[], &info);
            block(&table, &[group], coord)
        };
        let mut late = Vec::new();
        message(&mut late, 1, &[10, 11, 12, 13, 14, 15]);
        message(&mut late, 2, &[10, 2, 2, 2, 2, 2]);
        message(&mut late, 3, &[20, 2, 2, 2, 2, 2]);
        message(&mut late, 4, &[6, 2, 2, 2, 2, 2]);
        message(&mut late, 5, &[2, 2, 0, 2, 1, 0]);
        message(&mut late, 6, &[1, 1, 1, 1, 0, 1]);
        let late = dense_nodes(
            &[3, 4, 5, 6, 7, 8],
            &[31, 40, 50, 65, 70, 80],
            &[0; 6],
            &[],
            &late,
        );
        let way = |location| {
            block(
                &table,
                &[records(3, &[(8, &[2]), (9, &[location]), (10, &[0])])],
                coord,
            )
        };
        let relation = |role| {
            block(
                &table,
                &[records(4, &[(8, &[role]), (9, &[2]), (10, &[0])])],
                coord,
            )
        };
        [
            vec![
                nodes(&[1, 2, 3, 3], &[10, 20, 30, 31], &[1, 2, 3, 4]),
                nodes(&[4, 6], &[40, 60], &[5, 6]),
                nodes(&[7], &[70], &[7]),
                way(2),
                relation(1),
            ],
            vec![
                nodes(&[1, 3], &[10, 30], &[9, 9]),
                block(&table, &[late], coord),
                way(4),
                relation(2),
            ],
        ]
    }

    fn write_fixture(path: &std::path::Path, blocks: &[Fixture], indexed: bool) {
        use crate::block_builder::HeaderBuilder;
        use crate::read::block::Feature;
        use crate::writer::{Compression, PbfWriter};

        let header = HeaderBuilder::new()
            .sorted()
            .historical()
            .optional_feature(Feature::LOCATIONS_ON_WAYS)
            .build()
            .unwrap();
        let mut bytes = Vec::new();
        let mut writer = PbfWriter::new(&mut bytes, Compression::None);
        writer.write_header(&header).unwrap();
        for block in blocks {
            if indexed {
                writer.write_primitive_block(&block.bytes).unwrap();
            } else {
                writer
                    .write_primitive_block_no_indexdata(&block.bytes)
                    .unwrap();
            }
        }
        writer.flush().unwrap();
        drop(writer);
        std::fs::write(path, bytes).unwrap();
    }

    fn input_pairs(dir: &std::path::Path) -> [[std::path::PathBuf; 2]; 2] {
        let blocks = entrypoint_blocks();
        [false, true].map(|indexed| {
            [0, 1].map(|side| {
                let path = dir.join(format!("side-{side}-{indexed}.osm.pbf"));
                write_fixture(&path, &blocks[side], indexed);
                path
            })
        })
    }

    fn entrypoint_host() -> crate::host::Host {
        let host = crate::host::Host::for_tests();
        #[cfg(pbfhogg_test_hooks)]
        // Cut at every available key, including inside the new dense blob.
        host.invocation
            .faults()
            .diff_unit_target_bytes
            .store(1, std::sync::atomic::Ordering::Relaxed);
        host
    }

    #[test]
    fn indexed_and_full_element_entry_points_write_identical_text() {
        let dir = crate::path_guard::tests::scratch();
        let pairs = input_pairs(dir.path());
        let parallelism = crate::host::Parallelism::from_jobs(2);
        for (suppress_common, locations, verbose) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let options = super::super::DiffOptions {
                suppress_common,
                verbose,
                type_filter: None,
                parallelism,
                locations,
            };
            let outputs = pairs.each_ref().map(|[old, new]| {
                let mut bytes = Vec::new();
                let stats = super::super::diff(
                    old.as_path(),
                    new.as_path(),
                    &mut bytes,
                    &options,
                    false,
                    &entrypoint_host(),
                )
                .unwrap();
                (
                    bytes,
                    [stats.common, stats.created, stats.modified, stats.deleted],
                )
            });
            assert_eq!(
                outputs[0], outputs[1],
                "{suppress_common} {locations} {verbose}"
            );
            let text = std::str::from_utf8(&outputs[1].0).unwrap();
            assert_eq!(text.contains(" n1 v1\n"), !suppress_common);
            assert_eq!(text.contains(" n3 v3\n n3 v4\n"), !suppress_common);
            assert!(text.contains("-n2 v2\n"), "{text}");
            assert!(text.contains("*n6 v6 -> v13\n"), "{text}");
            assert!(!text.contains("*n7 "), "{text}");
            assert_eq!(text.contains("*w1\n"), locations);
            assert!(text.contains("*r1\n"), "{text}");
        }
    }

    #[test]
    fn indexed_and_full_element_entry_points_write_identical_osc_xml() {
        use std::io::Read;

        let dir = crate::path_guard::tests::scratch();
        let pairs = input_pairs(dir.path());
        for suffix in ["osc", "osc.gz"] {
            let xml = [0, 1].map(|route| {
                let [old, new] = &pairs[route];
                let path = dir.path().join(format!("changes-{route}.{suffix}"));
                let host = entrypoint_host();
                let output = crate::output::Output::new(
                    &path,
                    crate::output::Overwrite::Refuse,
                    &host.diagnostics,
                )
                .unwrap();
                let scratch =
                    crate::scratch::RunScratch::create(dir.path(), &host.entered()).unwrap();
                let stats = super::super::derive::derive_changes(
                    old.as_path(),
                    new.as_path(),
                    output,
                    false,
                    true,
                    None,
                    crate::host::Parallelism::from_jobs(2),
                    &host,
                    scratch,
                )
                .unwrap();
                assert_eq!([stats.creates, stats.modifies, stats.deletes], [2, 3, 1]);
                let bytes = std::fs::read(path).unwrap();
                if suffix == "osc" {
                    bytes
                } else {
                    let mut xml = Vec::new();
                    flate2::read::MultiGzDecoder::new(bytes.as_slice())
                        .read_to_end(&mut xml)
                        .unwrap();
                    xml
                }
            });
            assert_eq!(xml[0], xml[1], "{suffix}");
            let text = std::str::from_utf8(&xml[1]).unwrap();
            assert!(text.contains("<node id=\"2\" version=\"3\"/>"), "{text}");
            assert!(text.contains("version=\"13\" timestamp=\"1970-01-01T00:00:08Z\" changeset=\"13\" uid=\"6\" user=\"carol\""), "{text}");
            assert!(text.contains("visible=\"false\""), "{text}");
        }
    }
}
