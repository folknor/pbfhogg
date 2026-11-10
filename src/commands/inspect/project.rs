//! The inspect scan of one data blob: its statistics gathered as the block
//! validator (`read::block::validate_block`) passes each piece, with no
//! `PrimitiveBlock` built, no offset tables appended to the payload and no
//! per-element `Element` values.
//!
//! Validation is block construction's own call (`BlockBuffer::project`,
//! which construction runs with an empty projection), so a blob is refused
//! here exactly when building its block would refuse it. Each statistic is a function of one validated column or
//! one validated element, decoded with the readers' own decoders and
//! arithmetic (wrapping delta sums, `offset + granularity * raw` and
//! `timestamp * date_granularity` wrapping, the packed iterators' narrowing),
//! so it is the value the element walk gives. Element order matters only to
//! the ID ranges, and the validator hands a group's plain nodes over before
//! its dense message, so each group keeps the two apart and joins them dense
//! first, as the element iterator yields them.

use crate::read::block::{BlockBuffer, GroupProjection};
use crate::read::wire::{
    BlockFields, PackedInt32Iter, PackedSint32Iter, PackedSint64Iter, PackedUint32Iter,
    WireDenseInfo, WireDenseNodes, WireInfo, WireNode, WireRelation, WireWay,
};
use crate::{ElementKind, Result};

use super::types::{BlockKind, ScanState, TypeIdRange, UserNames, classify_block, count_metadata};

/// Scan one decompressed data block into a fresh [`ScanState`] with the
/// collectors the flags name, validating it through the inspect projection
/// (`BlockBuffer::project`, which also applies the buffer's kind-claim
/// refusal); returns the state with the block's kind and element count.
///
/// Measured once per data blob, so a `--hotpath` profile of the full-decode
/// inspect splits validation plus projection from decompression.
#[hotpath::measure]
pub(super) fn scan_block_buffer(
    buffer: BlockBuffer<'_>,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
) -> Result<(ScanState, BlockKind, u64)> {
    let (projection, accepted) = buffer.project(|fields, st| {
        let state = ScanState::new(show_id_ranges, show_locations, extended);
        let users = state
            .extended
            .is_some()
            .then(|| UserNames::from_lengths(st.iter().map(|&(_, len)| len as usize)));
        InspectProjection {
            state,
            fields: *fields,
            st_len: st.len(),
            users,
            dense_ids: TypeIdRange::new(),
            plain_ids: TypeIdRange::new(),
        }
    })?;
    let scan = *accepted.scan();
    let kinds = scan.kinds;
    let kind = classify_block(
        kinds.has(ElementKind::Node),
        kinds.has(ElementKind::Way),
        kinds.has(ElementKind::Relation),
    );
    Ok((projection.state, kind, scan.element_count))
}

struct InspectProjection {
    state: ScanState,
    fields: BlockFields,
    st_len: usize,
    /// Built only for `--extended`, which counts user names.
    users: Option<UserNames>,
    /// The current group's dense and plain node IDs, joined dense first.
    dense_ids: TypeIdRange,
    plain_ids: TypeIdRange,
}

impl InspectProjection {
    /// Whether a tag pair's two string indices both fall inside the table:
    /// what makes `tags().next()` yield. Bounds only, the table's UTF-8 is
    /// checked later.
    fn resolves(&self, key: u64, val: u64) -> bool {
        usize::try_from(key).is_ok_and(|k| k < self.st_len)
            && usize::try_from(val).is_ok_and(|v| v < self.st_len)
    }

    /// A plain element's tags: does its first pair resolve?
    fn first_tag_resolves(&self, keys: &[u8], vals: &[u8]) -> bool {
        match (
            PackedUint32Iter::new(keys).next(),
            PackedUint32Iter::new(vals).next(),
        ) {
            (Some(k), Some(v)) => self.resolves(u64::from(k), u64::from(v)),
            _ => false,
        }
    }

    fn nano(&self, offset: i64, raw: i64) -> i64 {
        offset.wrapping_add(i64::from(self.fields.granularity).wrapping_mul(raw))
    }

    /// One plain element's metadata census, as `Info` reads it.
    fn census_info(&mut self, info: Option<&WireInfo>) {
        let (Some(ext), Some(users)) = (self.state.extended.as_mut(), self.users.as_ref()) else {
            return;
        };
        ext.metadata.total += 1;
        let date_granularity = i64::from(self.fields.date_granularity);
        let (version, millis, changeset, uid, user) = info.map_or((0, 0, 0, 0, false), |i| {
            (
                i.version.unwrap_or(0),
                i.timestamp
                    .map_or(0, |ts| ts.wrapping_mul(date_granularity)),
                i.changeset.unwrap_or(0),
                i.uid.unwrap_or(0),
                i.user_sid.is_some_and(|sid| users.names(sid)),
            )
        });
        count_metadata(ext, version, millis, changeset, uid, user);
    }

    /// The dense nodes' tagged count: a node is tagged when the first pair
    /// of its `keys_vals` run resolves. The array is validated: complete
    /// pairs, one run per node, a final run possibly unterminated.
    fn tagged_dense(&self, keys_vals: &[u8]) -> u64 {
        let mut cursor = crate::read::wire::Cursor::new(keys_vals);
        let mut tagged = 0u64;
        while !cursor.is_empty() {
            let Ok(key) = cursor.read_varint() else { break };
            if key == 0 {
                continue;
            }
            let Ok(val) = cursor.read_varint() else { break };
            tagged += u64::from(self.resolves(key, val));
            // Skip the rest of this run, through its terminator.
            while !cursor.is_empty() {
                let Ok(k) = cursor.read_varint() else { break };
                if k == 0 {
                    break;
                }
                if cursor.read_varint().is_err() {
                    break;
                }
            }
        }
        tagged
    }

    fn dense_census(&mut self, info: &WireDenseInfo<'_>, ids: usize) {
        let (Some(ext), Some(users)) = (self.state.extended.as_mut(), self.users.as_ref()) else {
            return;
        };
        let m = &mut ext.metadata;
        m.has_version += PackedInt32Iter::new(info.version_data)
            .take(ids)
            .filter(|&v| v >= 1)
            .count() as u64;
        let mut changeset: i64 = 0;
        for delta in PackedSint64Iter::new(info.changeset_data).take(ids) {
            changeset = changeset.wrapping_add(delta);
            m.has_changeset += u64::from(changeset > 0);
        }
        let mut uid: i32 = 0;
        for delta in PackedSint32Iter::new(info.uid_data).take(ids) {
            uid = uid.wrapping_add(delta);
            m.has_uid += u64::from(uid > 0);
        }
        let mut sid: i32 = 0;
        for delta in PackedSint32Iter::new(info.user_sid_data).take(ids) {
            sid = sid.wrapping_add(delta);
            m.has_user += u64::from(users.names(sid));
        }
        let date_granularity = i64::from(self.fields.date_granularity);
        let mut timestamp: i64 = 0;
        for delta in PackedSint64Iter::new(info.timestamp_data).take(ids) {
            timestamp = timestamp.wrapping_add(delta);
            let millis = timestamp.wrapping_mul(date_granularity);
            if millis != 0 {
                ext.metadata.has_timestamp += 1;
                ext.update_timestamp(millis);
            }
        }
    }
}

impl GroupProjection for InspectProjection {
    fn group_begin(&mut self) {
        self.dense_ids = TypeIdRange::new();
        self.plain_ids = TypeIdRange::new();
    }

    fn node(&mut self, node: &WireNode<'_>, info: Option<&WireInfo>) {
        self.state.node_count += 1;
        if self.first_tag_resolves(node.keys_data, node.vals_data) {
            self.state.tagged_node_count += 1;
        }
        if self.state.node_ids.is_some() {
            self.plain_ids.update(node.id);
        }
        if self.state.extended.is_some() {
            let lat = self.nano(self.fields.lat_offset, node.lat);
            let lon = self.nano(self.fields.lon_offset, node.lon);
            if let Some(ext) = self.state.extended.as_mut() {
                ext.data_bbox.update(lat, lon);
            }
            self.census_info(info);
        }
    }

    fn way(&mut self, way: &WireWay<'_>, info: Option<&WireInfo>) {
        self.state.way_count += 1;
        if let Some(ids) = self.state.way_ids.as_mut() {
            ids.update(way.id);
        }
        if let Some(stats) = self.state.loc_stats.as_mut() {
            // `node_locations()` zips the lat and lon arrays, which
            // validation made equal (or both empty).
            #[allow(clippy::cast_possible_truncation)]
            let count = PackedSint64Iter::new(way.lat_data).count() as u32;
            if count > 0 {
                stats.with_locations += 1;
                *stats.coord_counts.entry(count).or_default() += 1;
            } else {
                stats.without_locations += 1;
            }
        }
        self.census_info(info);
    }

    fn relation(&mut self, relation: &WireRelation<'_>, info: Option<&WireInfo>) {
        self.state.relation_count += 1;
        if let Some(ids) = self.state.relation_ids.as_mut() {
            ids.update(relation.id);
        }
        self.census_info(info);
    }

    fn dense(&mut self, dense: &WireDenseNodes<'_>, ids: usize, info: Option<&WireDenseInfo<'_>>) {
        self.state.node_count += ids as u64;
        self.state.tagged_node_count += self.tagged_dense(dense.keys_vals_data);
        if self.state.node_ids.is_some() {
            let mut id: i64 = 0;
            for delta in PackedSint64Iter::new(dense.id_data).take(ids) {
                id = id.wrapping_add(delta);
                self.dense_ids.update(id);
            }
        }
        if self.state.extended.is_none() {
            return;
        }
        // Each coordinate is scaled before its extremum: raw extrema do not
        // survive wrapping or a negative granularity.
        let (lat_offset, lon_offset) = (self.fields.lat_offset, self.fields.lon_offset);
        let mut lat_range = (i64::MAX, i64::MIN);
        let mut raw: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lat_data).take(ids) {
            raw = raw.wrapping_add(delta);
            let nano = self.nano(lat_offset, raw);
            lat_range = (lat_range.0.min(nano), lat_range.1.max(nano));
        }
        let mut lon_range = (i64::MAX, i64::MIN);
        raw = 0;
        for delta in PackedSint64Iter::new(dense.lon_data).take(ids) {
            raw = raw.wrapping_add(delta);
            let nano = self.nano(lon_offset, raw);
            lon_range = (lon_range.0.min(nano), lon_range.1.max(nano));
        }
        if let Some(ext) = self.state.extended.as_mut() {
            ext.metadata.total += ids as u64;
            if ids > 0 {
                let bb = &mut ext.data_bbox;
                bb.min_lat = bb.min_lat.min(lat_range.0);
                bb.max_lat = bb.max_lat.max(lat_range.1);
                bb.min_lon = bb.min_lon.min(lon_range.0);
                bb.max_lon = bb.max_lon.max(lon_range.1);
            }
        }
        if let Some(info) = info {
            self.dense_census(info, ids);
        }
    }

    fn group_end(&mut self, _kinds: crate::element_kind::KindSet) {
        if let Some(ids) = self.state.node_ids.as_mut() {
            ids.append(&self.dense_ids);
            ids.append(&self.plain_ids);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::type_complexity
)]
mod tests {
    use crate::block_builder::{BlockBuilder, MemberData, Metadata};
    use crate::commands::inspect::scan::scan_block;
    use crate::commands::inspect::types::{BlockKind, ScanState, TypeIdRange};
    use crate::{MemberId, PrimitiveBlock};

    /// The projection scan of `bytes` as the decode workers run it, on a
    /// claimless buffer.
    fn scan_block_bytes(
        bytes: &[u8],
        st: &mut Vec<(u32, u32)>,
        gr: &mut Vec<(u32, u32)>,
        ids: bool,
        locs: bool,
        ext: bool,
    ) -> crate::Result<((ScanState, BlockKind, u64), ())> {
        let buffer = crate::read::block::BlockBuffer::new(bytes.to_vec(), None, st, gr, None);
        super::scan_block_buffer(buffer, ids, locs, ext).map(|scan| (scan, ()))
    }

    fn range(r: Option<&TypeIdRange>) -> String {
        r.map_or_else(String::new, |r| {
            format!(
                "{} {} {} {} {} {:?}",
                r.min_id,
                r.max_id,
                r.monotonic,
                r.count,
                r.has_data(),
                r.bounds()
            )
        })
    }

    /// Every statistic a scan produces, as one comparable string.
    fn fingerprint(state: &ScanState, kind: BlockKind, elements: u64) -> String {
        let mut s = format!(
            "{kind:?} {elements} n{} t{} w{} r{} ids[{}|{}|{}]",
            state.node_count,
            state.tagged_node_count,
            state.way_count,
            state.relation_count,
            range(state.node_ids.as_ref()),
            range(state.way_ids.as_ref()),
            range(state.relation_ids.as_ref()),
        );
        if let Some(l) = &state.loc_stats {
            s += &format!(
                " loc {} {} {:?}",
                l.with_locations, l.without_locations, l.coord_counts
            );
        }
        if let Some(e) = &state.extended {
            let m = &e.metadata;
            let b = &e.data_bbox;
            s += &format!(
                " ext ts {} {} bbox {} {} {} {} meta {} {} {} {} {} {}",
                e.min_timestamp,
                e.max_timestamp,
                b.min_lat,
                b.max_lat,
                b.min_lon,
                b.max_lon,
                m.total,
                m.has_version,
                m.has_timestamp,
                m.has_changeset,
                m.has_uid,
                m.has_user
            );
        }
        s
    }

    /// The projection scan's answer for `bytes` against building the block
    /// and walking its elements: the same refusal message, or the same
    /// statistics in every mode.
    fn assert_same(bytes: &[u8], what: &str) {
        let oracle = PrimitiveBlock::from_vec(bytes.to_vec());
        for (ids, locs, ext) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let scanned = scan_block_bytes(bytes, &mut st, &mut gr, ids, locs, ext);
            match (&oracle, scanned) {
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{what}"),
                (Ok(block), Ok(((state, kind, elements), _))) => {
                    let mut expected = ScanState::new(ids, locs, ext);
                    let (ekind, eelements) = scan_block(block, &mut expected);
                    assert_eq!(
                        fingerprint(&state, kind, elements),
                        fingerprint(&expected, ekind, eelements),
                        "{what}, modes {ids} {locs} {ext}"
                    );
                }
                (Ok(_), Err(e)) => panic!("{what}: the scan refused a valid block: {e}"),
                (Err(e), Ok(_)) => {
                    panic!("{what}: the scan accepted a block construction refuses: {e}")
                }
            }
        }
    }

    fn meta(i: i64) -> Metadata<'static> {
        const USERS: [&str; 3] = ["", "alice", "bob"];
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Metadata {
            version: (i % 4) as i32 - 1,
            timestamp: (i * 37) % 5,
            changeset: (i % 3) - 1,
            uid: (i % 5) as i32 - 2,
            user: USERS[i.rem_euclid(3) as usize],
            visible: true,
        }
    }

    fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
        let encode = |build: &dyn Fn(&mut BlockBuilder)| {
            let mut b = BlockBuilder::new();
            build(&mut b);
            b.take().expect("encode").expect("bytes").to_vec()
        };
        vec![
            (
                "dense, metadata and tags, ids out of order",
                encode(&|b| {
                    for i in [5_i64, 3, 9, 9, 12, -4, 20] {
                        let tags: &[(&str, &str)] = if i % 2 == 0 { &[("k", "v")] } else { &[] };
                        b.add_node(
                            i,
                            (i * 1_000) as i32,
                            (-i * 777) as i32,
                            tags.iter().copied(),
                            Some(&meta(i)),
                        );
                    }
                }),
            ),
            (
                "dense without metadata",
                encode(&|b| {
                    for i in 1..40 {
                        b.add_node(i, (i * 13) as i32, (i * 17) as i32, [("a", "b")], None);
                    }
                }),
            ),
            (
                "ways with locations and metadata",
                encode(&|b| {
                    for i in 1..12_i64 {
                        if i % 3 == 0 {
                            b.add_way(i * 10, [("highway", "x")], &[1, 2, i], Some(&meta(i)));
                        } else {
                            #[allow(clippy::cast_possible_truncation)]
                            let at = (i * 100) as i32;
                            b.add_way_with_locations(
                                i * 10,
                                [("highway", "x")],
                                &[1, 2, i],
                                &[(at, -at), (at + 1, 3), (-at, at)],
                                Some(&meta(i)),
                            );
                        }
                    }
                }),
            ),
            (
                "relations",
                encode(&|b| {
                    for i in 1..8_i64 {
                        let members = [MemberData {
                            id: MemberId::Way(i),
                            role: "outer",
                        }];
                        b.add_relation(i, [("type", "multipolygon")], &members, Some(&meta(i)));
                    }
                }),
            ),
        ]
    }

    // Hand-encoded protobuf, for the shapes `BlockBuilder` never writes:
    // plain nodes, a group holding plain and dense nodes, several groups.
    use crate::read::wire_fixtures::{Group, dense, plain_node};

    /// A block of `strings` and `groups`, each group's plain nodes and
    /// dense message.
    fn block(strings: &[&str], groups: &[(Vec<Vec<u8>>, Option<Vec<u8>>)]) -> Vec<u8> {
        let groups: Vec<Group> = groups
            .iter()
            .map(|(nodes, dense)| Group {
                nodes: nodes.clone(),
                dense: dense.clone(),
                ..Group::default()
            })
            .collect();
        crate::read::wire_fixtures::block(strings, &groups)
    }

    fn wire_fixtures() -> Vec<(&'static str, Vec<u8>)> {
        let strings = ["", "k", "v", "alice", ""];
        vec![
            (
                // Validation order (plain, then dense) and element order
                // (dense first) differ: the IDs make that visible.
                "plain and dense nodes in one group",
                block(
                    &strings,
                    &[(
                        vec![
                            plain_node(5, 100, -100, &[(1, 2)], Some([2, 7, 9, 3, 3])),
                            plain_node(2, -50, 70, &[], None),
                        ],
                        Some(dense(&[(10, 1, 1), (3, 2, -2)], &[1, 2, 0, 0])),
                    )],
                ),
            ),
            (
                "several groups, boundaries across them",
                block(
                    &strings,
                    &[
                        (
                            vec![plain_node(7, 0, 0, &[(9, 2)], Some([1, 0, 0, 0, 4]))],
                            None,
                        ),
                        (vec![], Some(dense(&[(7, 5, 5), (8, 6, 6)], &[]))),
                        (
                            vec![plain_node(1, 9, 9, &[(1, 99)], None)],
                            Some(dense(&[], &[])),
                        ),
                        (vec![], None),
                    ],
                ),
            ),
            (
                // A first pair past the table is untagged even when a later
                // pair resolves; an unterminated final run still counts.
                "dense tag runs",
                block(
                    &strings,
                    &[(
                        vec![],
                        Some(dense(
                            &[(1, 0, 0), (2, 0, 0), (3, 0, 0)],
                            &[9, 2, 1, 2, 0, 0, 1, 2],
                        )),
                    )],
                ),
            ),
        ]
    }

    /// Valid blocks: identical statistics in every mode.
    #[test]
    fn statistics_match_the_element_walk() {
        for (what, bytes) in fixtures().into_iter().chain(wire_fixtures()) {
            assert!(
                PrimitiveBlock::from_vec(bytes.clone()).is_ok(),
                "{what}: fixture must be valid"
            );
            assert_same(&bytes, what);
        }
    }

    /// Successive blobs joined by `ScanState::append` give one serial
    /// scan's ID ranges, the projection's boundaries included.
    #[test]
    fn appended_blob_states_keep_their_boundaries() {
        let (mut st, mut gr) = (Vec::new(), Vec::new());
        let mut joined = ScanState::new(true, false, false);
        let mut serial = ScanState::new(true, false, false);
        for (_, bytes) in wire_fixtures() {
            let ((part, _, _), _) =
                scan_block_bytes(&bytes, &mut st, &mut gr, true, false, false).unwrap();
            joined.append(part);
            scan_block(&PrimitiveBlock::from_vec(bytes).unwrap(), &mut serial);
        }
        assert_eq!(
            fingerprint(&joined, BlockKind::Nodes, 0),
            fingerprint(&serial, BlockKind::Nodes, 0)
        );
    }

    /// Every single-byte change and every truncation of each fixture: the
    /// scan refuses exactly what construction refuses, with the same
    /// message, and agrees on everything it accepts.
    #[test]
    fn refusals_and_statistics_match_under_mutation() {
        for (what, bytes) in fixtures().into_iter().chain(wire_fixtures()) {
            for len in 0..bytes.len() {
                assert_same(&bytes[..len], &format!("{what}, truncated to {len}"));
            }
            for pos in 0..bytes.len() {
                for value in [0x00_u8, 0x01, 0x7f, 0x80, 0xff, bytes[pos] ^ 0x08] {
                    let mut mutated = bytes.clone();
                    mutated[pos] = value;
                    assert_same(&mutated, &format!("{what}, byte {pos} = {value:#x}"));
                }
            }
        }
    }
}
