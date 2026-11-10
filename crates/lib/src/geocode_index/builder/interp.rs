//! Interpolation way resolution (mmap-based).
//!
//! OSM draws an interpolation way between address nodes: its end nodes, and
//! any node inside it, can be address points themselves. The address of a
//! node is the one whose stored coordinate equals the node's, not the nearest
//! one. Nothing here depends on a cell level or a distance.
//!
//! **The street or place.** A way numbers along one parent: a street, or a
//! place (`addr:place`, for addresses that belong to a place, not a
//! street). An address is on street `S` when its `addr:street` is `S`, and
//! on place `P` when its `addr:place` is `P` and it names no street: an
//! address with a street is numbered along its street, so a place never
//! pulls a street-numbered house into a place range. A way that carries a
//! non-empty `addr:street` keeps it; else one that carries `addr:place`
//! keeps that. A way with neither (the OSM convention: the street is on the
//! end nodes, and the way "doesn't take addr:street=*") takes the one street
//! that the addresses at its first node and at its last node have in
//! common; if they share no street, the one place they share. More than one
//! at the deciding tier, or none at both, refuses the way. Streets match by
//! name alone. The parent is chosen before any house number is looked at,
//! so an unusable number on one street cannot decide between two.
//!
//! **The numbers.** At every node, the addresses on the way's street either
//! are absent, or all carry one house-number string. That string is an
//! anchor when it is a positive decimal number fitting a u32. An interior
//! node with one non-numeric string (`12b`) is passed over, as the wiki
//! allows; any other non-anchor (disagreeing strings, `10` beside `10a` or
//! `010`, an all-digit `0` or an overflow, a non-numeric end) refuses the
//! way. Both ends must be anchors.
//!
//! **The sections.** Consecutive anchors bound a section, and each section
//! becomes its own record over the shared node span (the split node is the
//! last node of one record and the first of the next, in place in
//! `interp_nodes.bin`). A section with equal numbers at both anchors
//! establishes no range and refuses the way; one whose `even` or `odd`
//! anchors have the other parity refuses it too. A section with no number
//! between its anchors (10 to 12 even, 10 to 11 all, 10 to 13 by 3) is
//! dropped, without refusing the rest of the way. Both rules are
//! [`InterpolationType`]'s, which the reader answers by too.
//!
//! A way is resolved whole or not at all: a refused way is kept as one
//! record over all its nodes with both numbers at the zero sentinel (see
//! CORRECTNESS.md), so it never answers a query - a borrowed, ambiguous or
//! truncated number would be a silent wrong answer. So is a way with an
//! unsupported `addr:interpolation` mode or with a node ref Pass 2 could not
//! resolve, whose shortened node list could put an interior address where
//! an end was. ADR-0024 records the end-node rule and the nearest-address
//! search it replaced; ADR-0026 the street inference and the sections.

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use super::pass2::SlimInterpWay;
use super::strings::{StringPool, read_string_from_pool};

use super::super::format::*;

/// A coordinate as an exact lookup key, longitude folded by
/// [`crate::geo::normalize_longitude_e7`]: +180 and -180 degrees are one
/// meridian, so a way node and an address point either side of that
/// spelling are still at the same place.
type CoordKey = (i32, i64);

fn coord_key(lat_e7: i32, lon_e7: i32) -> CoordKey {
    (
        lat_e7,
        crate::geo::normalize_longitude_e7(i64::from(lon_e7)),
    )
}

/// The address points that sit exactly on some interpolation way node,
/// sorted by `(coordinate, address index)`. Built from the way-node
/// coordinates first, so it holds only matched points, never the whole
/// address table.
struct NodeAddrs {
    rows: Vec<(CoordKey, u32)>,
}

impl NodeAddrs {
    /// The address-point indices whose coordinate equals `key`.
    fn at(&self, key: CoordKey) -> impl Iterator<Item = u32> + '_ {
        let first = self.rows.partition_point(|&(k, _)| k < key);
        self.rows[first..]
            .iter()
            .take_while(move |&&(k, _)| k == key)
            .map(|&(_, index)| index)
    }
}

/// The node coordinates of a way the resolver may resolve: a supported
/// mode, every node ref resolved, and at least two nodes (a one-node way
/// would resolve both ends to the same point).
fn way_nodes(way: &SlimInterpWay, interp_nodes: Table<'_, NodeCoord>) -> Option<Vec<CoordKey>> {
    if way.interpolation.is_none() || !way.complete || way.node_count < 2 {
        return None;
    }
    let nodes = interp_nodes
        .span_at(way.node_file_offset, u64::from(way.node_count))
        .ok()?;
    (0..usize::from(way.node_count))
        .map(|i| nodes.get(i).map(|n| coord_key(n.lat_e7, n.lon_e7)))
        .collect()
}

fn build_node_addrs(
    interp_ways: &[SlimInterpWay],
    addr_points: Table<'_, AddrPoint>,
    interp_nodes: Table<'_, NodeCoord>,
    executor: &crate::host::Executor,
) -> NodeAddrs {
    let mut keys: FxHashSet<CoordKey> = FxHashSet::default();
    for way in interp_ways {
        if let Some(nodes) = way_nodes(way, interp_nodes) {
            keys.extend(nodes);
        }
    }
    if keys.is_empty() {
        return NodeAddrs { rows: Vec::new() };
    }
    #[expect(
        clippy::expect_used,
        reason = "Pass 2 refuses an address-point count over u32::MAX"
    )]
    let mut rows: Vec<(CoordKey, u32)> = executor.install(|| {
        (0..addr_points.len())
            .into_par_iter()
            .filter_map(|index| {
                let point = addr_points.get(index)?;
                let key = coord_key(point.lat_e7, point.lon_e7);
                keys.contains(&key).then(|| {
                    (
                        key,
                        u32::try_from(index).expect("address count checked during Pass 2"),
                    )
                })
            })
            .collect()
    });
    rows.sort_unstable();
    NodeAddrs { rows }
}

/// Parse a house number that is entirely a positive decimal number. `42A`,
/// `18-20` and `0` are refused rather than read as 42, 18 or a sentinel.
fn parse_house_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok().filter(|&n| n > 0)
}

/// The interpolation records to write, and what happened to the source ways.
pub(super) struct Resolution {
    /// One record per section of a resolved way, one zero-sentinel record
    /// per refused way, in source-way order.
    pub(super) records: Vec<SlimInterpWay>,
    /// Source ways resolved, including those whose every section was
    /// adjacent numbers and so emitted no record. Resolved means the street
    /// and numbers resolved; the reader still refuses a section too short
    /// to measure.
    pub(super) resolved_ways: u64,
    /// Source ways refused.
    pub(super) unresolved_ways: u64,
    /// A sample of refused source way IDs.
    pub(super) unresolved_ids: Vec<i64>,
}

/// Resolve every interpolation way from the address points on its nodes
/// (see the module documentation for the rule). Reads address points from
/// mmap'd addr_points.bin.
#[hotpath::measure]
pub(super) fn resolve_interpolation_ways_mmap(
    interp_ways: Vec<SlimInterpWay>,
    addr_points: Table<'_, AddrPoint>,
    interp_nodes: Table<'_, NodeCoord>,
    strings: &StringPool,
    executor: &crate::host::Executor,
    invocation: &crate::host::Recorder,
) -> Resolution {
    invocation.marker("GEOCODE_INTERP_RESOLVE_INDEX_START");
    let addrs = if addr_points.is_empty() {
        NodeAddrs { rows: Vec::new() }
    } else {
        build_node_addrs(&interp_ways, addr_points, interp_nodes, executor)
    };
    invocation.marker("GEOCODE_INTERP_RESOLVE_INDEX_END");
    invocation.marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_START");
    let outcomes: Vec<Option<Resolved>> = executor.install(|| {
        interp_ways
            .par_iter()
            .map(|way| resolve_one_way(way, interp_nodes, addr_points, strings, &addrs))
            .collect()
    });
    invocation.marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_END");

    let mut resolution = Resolution {
        records: Vec::with_capacity(interp_ways.len()),
        resolved_ways: 0,
        unresolved_ways: 0,
        unresolved_ids: Vec::new(),
    };
    for (way, outcome) in interp_ways.into_iter().zip(outcomes) {
        let Some(resolved) = outcome else {
            resolution.unresolved_ways += 1;
            super::sample_id(&mut resolution.unresolved_ids, way.way_id);
            resolution.records.push(way);
            continue;
        };
        resolution.resolved_ways += 1;
        for section in resolved.sections {
            resolution.records.push(SlimInterpWay {
                way_id: way.way_id,
                node_file_offset: way.node_file_offset
                    + u64::from(section.first) * NodeCoord::SIZE as u64,
                parent_kind: resolved.parent.kind,
                parent_offset: resolved.parent.offset,
                interpolation: way.interpolation,
                node_count: section.last - section.first + 1,
                complete: true,
                start_number: section.start,
                end_number: section.end,
            });
        }
    }
    resolution
}

/// A resolved way: its street and the sections that have numbers to infer.
struct Resolved {
    parent: Parent,
    sections: Vec<Section>,
}

/// Nodes `first..=last` of the source way, numbered `start` to `end`.
struct Section {
    first: u16,
    last: u16,
    start: u32,
    end: u32,
}

/// What the addresses on the way's street say at one node.
enum NodeNumber {
    /// No address on the street here.
    Absent,
    /// One non-numeric string at an interior node, which the wiki allows
    /// and interpolation passes over.
    PassedOver,
    Anchor(u32),
    /// Anything that makes the node unusable as an end or as a gap.
    Refused,
}

/// Resolve one source way, or `None` to refuse it.
fn resolve_one_way(
    way: &SlimInterpWay,
    interp_nodes: Table<'_, NodeCoord>,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    addrs: &NodeAddrs,
) -> Option<Resolved> {
    let interpolation = way.interpolation?;
    let nodes = way_nodes(way, interp_nodes)?;
    let (&first, &last) = (nodes.first()?, nodes.last()?);
    let parent = if way.parent_offset != 0 {
        Parent {
            kind: way.parent_kind,
            offset: way.parent_offset,
        }
    } else {
        common_end_parent(first, last, addr_points, addrs)?
    };

    let last_index = nodes.len() - 1;
    let mut sections = Vec::new();
    let mut previous: Option<(u16, u32)> = None;
    for (index, &key) in nodes.iter().enumerate() {
        let is_end = index == 0 || index == last_index;
        let number = match node_number(key, parent, addr_points, strings, addrs) {
            NodeNumber::Anchor(number) => number,
            NodeNumber::Absent | NodeNumber::PassedOver if !is_end => continue,
            NodeNumber::Absent | NodeNumber::PassedOver | NodeNumber::Refused => return None,
        };
        // Pass 2 refuses a way of more than u16::MAX nodes.
        let index = u16::try_from(index).ok()?;
        if let Some((first, start)) = previous {
            // Equal numbers establish no range; OSM requires an even or
            // odd way's anchors to follow its declared rule.
            if !interpolation.anchors_agree(start, number) {
                return None;
            }
            if interpolation.answers(start, number) {
                sections.push(Section {
                    first,
                    last: index,
                    start,
                    end: number,
                });
            }
        }
        previous = Some((index, number));
    }
    Some(Resolved { parent, sections })
}

/// The street or place a way numbers along.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Parent {
    kind: ParentKind,
    offset: u32,
}

impl Parent {
    /// Whether `point` is on this parent (see the module documentation).
    /// The builder stores an absent name as offset 0, never an empty
    /// string at another offset.
    fn holds(self, point: &AddrPoint) -> bool {
        match self.kind {
            ParentKind::Street => point.street_offset == self.offset,
            ParentKind::Place => point.street_offset == 0 && point.place_offset == self.offset,
        }
    }
}

/// The one street that the addresses at both end nodes share; if they
/// share none, the one place that the place addresses (those naming no
/// street) at both share; `None` when the deciding tier holds more than one,
/// or neither holds any.
fn common_end_parent(
    first: CoordKey,
    last: CoordKey,
    addr_points: Table<'_, AddrPoint>,
    addrs: &NodeAddrs,
) -> Option<Parent> {
    let names_at = |key, name: fn(&AddrPoint) -> u32| {
        let mut names: Vec<u32> = addrs
            .at(key)
            .filter_map(|index| addr_points.get(index as usize))
            .map(|point| name(&point))
            .filter(|&offset| offset != 0)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let shared = |name: fn(&AddrPoint) -> u32| {
        let at_last = names_at(last, name);
        names_at(first, name)
            .into_iter()
            .filter(|offset| at_last.binary_search(offset).is_ok())
            .collect::<Vec<u32>>()
    };
    let streets = shared(|point| point.street_offset);
    let (kind, names) = if streets.is_empty() {
        let places = shared(|point| {
            if point.street_offset == 0 {
                point.place_offset
            } else {
                0
            }
        });
        (ParentKind::Place, places)
    } else {
        (ParentKind::Street, streets)
    };
    match names.as_slice() {
        &[offset] => Some(Parent { kind, offset }),
        _ => None,
    }
}

/// The house number the addresses on `parent` at exactly `key` give that
/// node. Disagreement is judged on the strings before any parsing, so a
/// refused spelling cannot hide a conflict.
fn node_number(
    key: CoordKey,
    parent: Parent,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    addrs: &NodeAddrs,
) -> NodeNumber {
    let mut housenumber: Option<u32> = None;
    for index in addrs.at(key) {
        let Some(point) = addr_points.get(index as usize) else {
            continue;
        };
        if !parent.holds(&point) {
            continue;
        }
        // The pool interns each string once, so equal offsets are equal
        // strings.
        match housenumber {
            None => housenumber = Some(point.housenumber_offset),
            Some(offset) if offset == point.housenumber_offset => {}
            Some(_) => return NodeNumber::Refused,
        }
    }
    let Some(offset) = housenumber else {
        return NodeNumber::Absent;
    };
    // The pool stores values whole, so `10\0a` reads back as itself and the
    // strict parse refuses it.
    let s = read_string_from_pool(strings, offset);
    if let Some(number) = parse_house_number(s) {
        NodeNumber::Anchor(number)
    } else if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        // `0` or a number past u32: a real number the index cannot hold,
        // not a non-integer to step over.
        NodeNumber::Refused
    } else {
        NodeNumber::PassedOver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREET: &str = "Interp Street";

    struct Fixture {
        strings: StringPool,
        addr_bytes: Vec<u8>,
        node_bytes: Vec<u8>,
        ways: Vec<SlimInterpWay>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut strings = StringPool::new();
            strings.intern(STREET);
            Self {
                strings,
                addr_bytes: Vec::new(),
                node_bytes: Vec::new(),
                ways: Vec::new(),
            }
        }

        fn address(&mut self, key: (i32, i32), housenumber: &str, street: &str) {
            self.address_named(key, housenumber, street, "");
        }

        /// An address naming `street` and `place`, an empty one absent as
        /// Pass 2 stores it.
        fn address_named(
            &mut self,
            (lat_e7, lon_e7): (i32, i32),
            housenumber: &str,
            street: &str,
            place: &str,
        ) {
            let point = AddrPoint {
                lat_e7,
                lon_e7,
                housenumber_offset: self.strings.intern(housenumber),
                street_offset: self.name(street),
                postcode_offset: 0,
                place_offset: self.name(place),
            };
            self.addr_bytes.extend_from_slice(&point.to_bytes());
        }

        fn name(&mut self, value: &str) -> u32 {
            if value.is_empty() {
                0
            } else {
                self.strings.intern(value)
            }
        }

        /// A way carrying `addr:street` STREET.
        fn way(&mut self, nodes: &[(i32, i32)], interpolation: Option<InterpolationType>) {
            self.way_on(nodes, interpolation, STREET, true);
        }

        /// A way tagged by the OSM convention: no street on the way.
        fn bare_way(&mut self, nodes: &[(i32, i32)], interpolation: Option<InterpolationType>) {
            self.way_on(nodes, interpolation, "", true);
        }

        fn way_on(
            &mut self,
            nodes: &[(i32, i32)],
            interpolation: Option<InterpolationType>,
            street: &str,
            complete: bool,
        ) {
            self.way_with(nodes, interpolation, ParentKind::Street, street, complete);
        }

        /// A way carrying `addr:place` `place`.
        fn place_way(
            &mut self,
            nodes: &[(i32, i32)],
            interpolation: Option<InterpolationType>,
            place: &str,
        ) {
            self.way_with(nodes, interpolation, ParentKind::Place, place, true);
        }

        fn way_with(
            &mut self,
            nodes: &[(i32, i32)],
            interpolation: Option<InterpolationType>,
            parent_kind: ParentKind,
            parent: &str,
            complete: bool,
        ) {
            let parent_offset = self.name(parent);
            let node_file_offset = self.node_bytes.len() as u64;
            for &(lat_e7, lon_e7) in nodes {
                self.node_bytes
                    .extend_from_slice(&NodeCoord { lat_e7, lon_e7 }.to_bytes());
            }
            self.ways.push(SlimInterpWay {
                way_id: i64::try_from(self.ways.len()).expect("test way count") + 1,
                node_file_offset,
                parent_kind,
                parent_offset,
                interpolation,
                node_count: u16::try_from(nodes.len()).expect("test way"),
                complete,
                start_number: 0,
                end_number: 0,
            });
        }

        fn run(self) -> (Resolution, StringPool) {
            let host = crate::host::Host::entered_for_tests();
            let resolution = resolve_interpolation_ways_mmap(
                self.ways,
                Table::new(&self.addr_bytes),
                Table::new(&self.node_bytes),
                &self.strings,
                &host.executor,
                &host.invocation,
            );
            (resolution, self.strings)
        }

        /// Resolve every way; return each record's `(start, end)` numbers.
        fn resolve(self) -> Vec<(u32, u32)> {
            self.run()
                .0
                .records
                .iter()
                .map(|way| (way.start_number, way.end_number))
                .collect()
        }
    }

    const A: (i32, i32) = (557_000_000, 125_000_000);
    const B: (i32, i32) = (557_000_000, 125_060_000);
    const M: (i32, i32) = (557_000_000, 125_030_000);

    #[test]
    fn ends_take_the_numbers_of_the_addresses_at_them() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, M, B], Some(InterpolationType::Even));
        fx.way(&[B, A], Some(InterpolationType::All));
        assert_eq!(fx.resolve(), vec![(10, 30), (30, 10)]);
    }

    /// The old resolver took the nearest same-street address within the
    /// street-level cell and its eight neighbours, so an end one node away
    /// from an address borrowed its number, and how far that reached moved
    /// with the cell level. An address 1 E7 unit (about a centimetre) away is
    /// not at the end.
    #[test]
    fn an_address_near_an_end_is_not_the_address_at_it() {
        let mut fx = Fixture::new();
        fx.address((A.0 + 1, A.1), "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    #[test]
    fn an_address_on_another_street_does_not_resolve_an_end() {
        let mut fx = Fixture::new();
        fx.address(A, "10", "Corner Street");
        fx.address(B, "30", STREET);
        fx.way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    #[test]
    fn coincident_addresses_that_disagree_refuse_the_end() {
        for other in ["20", "10a", "010"] {
            let mut fx = Fixture::new();
            fx.address(A, "10", STREET);
            fx.address(A, other, STREET);
            fx.address(B, "30", STREET);
            fx.way(&[A, B], Some(InterpolationType::Even));
            assert_eq!(fx.resolve(), vec![(0, 0)], "10 beside {other}");
        }
    }

    #[test]
    fn coincident_duplicates_of_one_number_resolve() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(A, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(10, 30)]);
    }

    #[test]
    fn a_number_that_is_not_a_positive_decimal_refuses_the_end() {
        for bad in ["57a", "18-20", "0", "99999999999", " 10", "10\0a"] {
            let mut fx = Fixture::new();
            fx.address(A, bad, STREET);
            fx.address(B, "30", STREET);
            fx.way(&[A, B], Some(InterpolationType::All));
            assert_eq!(fx.resolve(), vec![(0, 0)], "{bad:?}");
        }
    }

    #[test]
    fn ends_must_have_the_declared_parity() {
        let cases = [
            (InterpolationType::Even, "10", "13", false),
            (InterpolationType::Even, "11", "21", false),
            (InterpolationType::Even, "10", "20", true),
            (InterpolationType::Odd, "10", "20", false),
            (InterpolationType::Odd, "11", "21", true),
            (InterpolationType::All, "10", "13", true),
        ];
        for (interpolation, start, end, resolves) in cases {
            let mut fx = Fixture::new();
            fx.address(A, start, STREET);
            fx.address(B, end, STREET);
            fx.way(&[A, B], Some(interpolation));
            let got = fx.resolve();
            assert_eq!(
                got[0] != (0, 0),
                resolves,
                "{interpolation:?} {start}..{end}: {got:?}"
            );
        }
    }

    #[test]
    fn an_unsupported_interpolation_mode_stays_unresolved() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, B], None);
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    /// An address stored at -180 degrees is at the same place as an end
    /// node stored at +180.
    #[test]
    fn the_antimeridian_has_one_spelling() {
        let mut fx = Fixture::new();
        let east = (100_000_000, 1_800_000_000);
        let west = (100_000_000, -1_800_000_000);
        fx.address(west, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[east, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(10, 30)]);
    }

    #[test]
    fn parse_house_number_accepts_only_positive_decimals() {
        assert_eq!(parse_house_number("42"), Some(42));
        assert_eq!(parse_house_number("042"), Some(42));
        assert_eq!(parse_house_number("4294967295"), Some(u32::MAX));
        for bad in ["", "0", "00", "42A", "4294967296", "-1", "+1"] {
            assert_eq!(parse_house_number(bad), None, "{bad:?}");
        }
    }

    /// The OSM convention: the street is on the end nodes, not the way.
    #[test]
    fn a_way_without_a_street_takes_the_street_its_ends_share() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(A, "1", "Corner Street");
        fx.address(B, "30", STREET);
        fx.bare_way(&[A, M, B], Some(InterpolationType::Even));
        let (resolution, strings) = fx.run();
        assert_eq!(resolution.resolved_ways, 1);
        let record = &resolution.records[0];
        assert_eq!((record.start_number, record.end_number), (10, 30));
        assert_eq!(record.parent_kind, ParentKind::Street);
        assert_eq!(
            read_string_from_pool(&strings, record.parent_offset),
            STREET
        );
    }

    const PLACE: &str = "Lhota";

    /// Addresses that belong to a place, not a street, number a way along
    /// the place, whether the way names it or takes it from its ends.
    #[test]
    fn place_addresses_number_a_way_along_their_place() {
        for tagged in [true, false] {
            let mut fx = Fixture::new();
            fx.address_named(A, "10", "", PLACE);
            fx.address_named(B, "30", "", PLACE);
            if tagged {
                fx.place_way(&[A, M, B], Some(InterpolationType::Even), PLACE);
            } else {
                fx.bare_way(&[A, M, B], Some(InterpolationType::Even));
            }
            let (resolution, strings) = fx.run();
            assert_eq!(resolution.resolved_ways, 1, "tagged {tagged}");
            let record = &resolution.records[0];
            assert_eq!((record.start_number, record.end_number), (10, 30));
            assert_eq!(record.parent_kind, ParentKind::Place);
            assert_eq!(read_string_from_pool(&strings, record.parent_offset), PLACE);
        }
    }

    /// A street and a place of the same name are different parents.
    #[test]
    fn a_place_is_not_a_street_of_the_same_name() {
        let mut fx = Fixture::new();
        fx.address_named(A, "10", "", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, B], Some(InterpolationType::Even));
        fx.place_way(&[A, B], Some(InterpolationType::Even), STREET);
        assert_eq!(fx.resolve(), vec![(0, 0), (0, 0)]);
    }

    /// An address with a street is numbered along its street, so its place
    /// neither anchors a place range nor splits one.
    #[test]
    fn an_address_with_a_street_is_not_on_its_place() {
        let mut fx = Fixture::new();
        fx.address_named(A, "10", "Main Road", PLACE);
        fx.address_named(B, "30", "", PLACE);
        fx.place_way(&[A, B], Some(InterpolationType::Even), PLACE);
        assert_eq!(fx.resolve(), vec![(0, 0)]);

        let mut fx = Fixture::new();
        fx.address_named(A, "10", "", PLACE);
        fx.address_named(M, "80", "Other Road", PLACE);
        fx.address_named(B, "30", "", PLACE);
        fx.place_way(&[A, M, B], Some(InterpolationType::Even), PLACE);
        assert_eq!(fx.resolve(), vec![(10, 30)]);

        // Ends on different streets in one place share no parent.
        let mut fx = Fixture::new();
        fx.address_named(A, "10", "North Road", PLACE);
        fx.address_named(B, "30", "South Road", PLACE);
        fx.bare_way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    /// A shared street decides before places are looked at; two shared
    /// places with no shared street refuse.
    #[test]
    fn a_shared_street_outranks_a_shared_place() {
        let mut fx = Fixture::new();
        fx.address_named(A, "10", STREET, PLACE);
        fx.address_named(B, "30", STREET, PLACE);
        fx.bare_way(&[A, B], Some(InterpolationType::Even));
        let (resolution, _) = fx.run();
        assert_eq!(resolution.records[0].parent_kind, ParentKind::Street);
        assert_eq!(resolution.resolved_ways, 1);

        let mut fx = Fixture::new();
        for (key, number) in [(A, "10"), (B, "30")] {
            fx.address_named(key, number, "", PLACE);
            fx.address_named(key, number, "", "Other Place");
        }
        fx.bare_way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    /// A numeric step: its anchors need no common grid, and a section with
    /// no step between its anchors is dropped.
    #[test]
    fn a_numeric_step_resolves_sections_wider_than_the_step() {
        let every =
            |n| InterpolationType::Every(InterpolationStep::new(n).expect("a step in 2..=9"));
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(M, "13", STREET);
        fx.address(B, "21", STREET);
        fx.way(&[A, M, B], Some(every(3)));
        let (resolution, _) = fx.run();
        assert_eq!(resolution.resolved_ways, 1);
        let records: Vec<_> = resolution
            .records
            .iter()
            .map(|r| (r.start_number, r.end_number))
            .collect();
        assert_eq!(records, vec![(13, 21)]);
    }

    /// The street is chosen before the numbers: two shared streets refuse
    /// even when only one of them carries usable numbers.
    #[test]
    fn ends_sharing_no_street_or_two_refuse_a_way_without_one() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(B, "30", "Other Street");
        fx.bare_way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);

        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(A, "9a", "Other Street");
        fx.address(B, "30", STREET);
        fx.address(B, "9b", "Other Street");
        fx.bare_way(&[A, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    /// 10 at the start, 20 in the middle, 40 at the end is two ranges, not
    /// one 10..40 spread over the whole length.
    #[test]
    fn an_interior_address_splits_the_way() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(M, "20", STREET);
        fx.address(B, "40", STREET);
        fx.bare_way(
            &[A, (557_000_000, 125_010_000), M, B],
            Some(InterpolationType::Even),
        );
        let (resolution, _) = fx.run();
        let records: Vec<_> = resolution
            .records
            .iter()
            .map(|r| {
                (
                    r.start_number,
                    r.end_number,
                    r.node_count,
                    r.node_file_offset,
                )
            })
            .collect();
        let size = NodeCoord::SIZE as u64;
        assert_eq!(records, vec![(10, 20, 3, 0), (20, 40, 2, 2 * size)]);
        assert_eq!(resolution.resolved_ways, 1);
    }

    #[test]
    fn a_non_integer_interior_address_is_passed_over() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(M, "12b", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, M, B], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(10, 30)]);
    }

    #[test]
    fn an_unusable_interior_address_refuses_the_whole_way() {
        for (first, second) in [("20", "22"), ("0", "0"), ("99999999999", "99999999999")] {
            let mut fx = Fixture::new();
            fx.address(A, "10", STREET);
            fx.address(M, first, STREET);
            fx.address(M, second, STREET);
            fx.address(B, "40", STREET);
            fx.way(&[A, M, B], Some(InterpolationType::Even));
            let (resolution, _) = fx.run();
            assert_eq!(resolution.unresolved_ways, 1, "{first}/{second}");
            let records: Vec<_> = resolution
                .records
                .iter()
                .map(|r| (r.start_number, r.end_number, r.node_count))
                .collect();
            assert_eq!(records, vec![(0, 0, 3)], "{first}/{second}");
        }
    }

    /// A closed outline with one address at its closing node answered that
    /// one number along its whole perimeter.
    #[test]
    fn equal_numbers_establish_no_range() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.way(&[A, M, B, A], Some(InterpolationType::Even));
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }

    #[test]
    fn a_section_with_no_number_between_its_anchors_is_dropped() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(M, "12", STREET);
        fx.address(B, "30", STREET);
        fx.way(&[A, M, B], Some(InterpolationType::Even));
        fx.way(&[A, M], Some(InterpolationType::Even));
        let (resolution, _) = fx.run();
        assert_eq!(resolution.resolved_ways, 2);
        let records: Vec<_> = resolution
            .records
            .iter()
            .map(|r| (r.start_number, r.end_number))
            .collect();
        assert_eq!(records, vec![(12, 30)]);
    }

    #[test]
    fn a_way_with_an_unresolved_node_ref_is_refused() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way_on(&[A, B], Some(InterpolationType::Even), STREET, false);
        assert_eq!(fx.resolve(), vec![(0, 0)]);
    }
}
