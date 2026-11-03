//! Interpolation endpoint resolution (mmap-based).
//!
//! An interpolation way's start and end numbers are the house numbers of the
//! address points at its two end nodes. OSM tags an interpolation way between
//! two address nodes, so its end nodes are themselves the address points: the
//! address of an end is the one whose stored coordinate equals the end node's,
//! not the nearest one. Nothing here depends on a cell level or a distance.
//!
//! An endpoint resolves only when the same-street address points at its
//! coordinate all carry one house-number string, and that string is a
//! positive decimal number. A way resolves only when both ends do, its
//! `addr:interpolation` is `all`, `even` or `odd`, and an `even` or `odd` way
//! has both ends of that parity. Anything else leaves both numbers at the
//! zero sentinel (see CORRECTNESS.md), so the way never answers a query: a
//! borrowed, ambiguous or truncated number would be a silent wrong answer.
//! ADR-0024 records the rule and the nearest-address search it replaced.

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use super::pass2::SlimInterpWay;
use super::strings::StringPool;

use super::super::format::*;

/// A coordinate as an exact lookup key, longitude folded by
/// [`crate::geo::normalize_longitude_e7`]: +180 and -180 degrees are one
/// meridian, so an end node and an address point either side of that
/// spelling are still at the same place.
type CoordKey = (i32, i64);

fn coord_key(lat_e7: i32, lon_e7: i32) -> CoordKey {
    (
        lat_e7,
        crate::geo::normalize_longitude_e7(i64::from(lon_e7)),
    )
}

/// The address points that sit exactly on some interpolation end node,
/// sorted by `(coordinate, address index)`. Built from the end-node
/// coordinates first, so it holds only matched points, never the whole
/// address table.
struct EndpointAddrs {
    rows: Vec<(CoordKey, u32)>,
}

impl EndpointAddrs {
    /// The address-point indices whose coordinate equals `key`.
    fn at(&self, key: CoordKey) -> impl Iterator<Item = u32> + '_ {
        let first = self.rows.partition_point(|&(k, _)| k < key);
        self.rows[first..]
            .iter()
            .take_while(move |&&(k, _)| k == key)
            .map(|&(_, index)| index)
    }
}

/// The first and last node of a way that has at least two.
fn way_ends(
    way: &SlimInterpWay,
    interp_nodes: Table<'_, NodeCoord>,
) -> Option<(CoordKey, CoordKey)> {
    // Not just a bounds guard: a one-node way would resolve both ends to
    // the same point.
    if way.node_count < 2 {
        return None;
    }
    let nodes = interp_nodes
        .span_at(way.node_file_offset, u64::from(way.node_count))
        .ok()?;
    let start = nodes.get(0)?;
    let end = nodes.get(usize::from(way.node_count) - 1)?;
    Some((
        coord_key(start.lat_e7, start.lon_e7),
        coord_key(end.lat_e7, end.lon_e7),
    ))
}

fn build_endpoint_addrs(
    interp_ways: &[SlimInterpWay],
    addr_points: Table<'_, AddrPoint>,
    interp_nodes: Table<'_, NodeCoord>,
    executor: &crate::host::Executor,
) -> EndpointAddrs {
    let mut ends: FxHashSet<CoordKey> = FxHashSet::default();
    for way in interp_ways {
        if way.interpolation.is_some()
            && let Some((start, end)) = way_ends(way, interp_nodes)
        {
            ends.insert(start);
            ends.insert(end);
        }
    }
    if ends.is_empty() {
        return EndpointAddrs { rows: Vec::new() };
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
                ends.contains(&key).then(|| {
                    (
                        key,
                        u32::try_from(index).expect("address count checked during Pass 2"),
                    )
                })
            })
            .collect()
    });
    rows.sort_unstable();
    EndpointAddrs { rows }
}

/// Parse a house number that is entirely a positive decimal number. `42A`,
/// `18-20` and `0` are refused rather than read as 42, 18 or a sentinel.
fn parse_house_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok().filter(|&n| n > 0)
}

/// Resolve start/end house numbers for interpolation ways from the address
/// points at their end nodes (see the module documentation for the rule).
/// Reads address points from mmap'd addr_points.bin. Returns the number of
/// ways resolved.
#[hotpath::measure]
pub(super) fn resolve_interpolation_endpoints_mmap(
    interp_ways: &mut [SlimInterpWay],
    addr_points: Table<'_, AddrPoint>,
    interp_nodes: Table<'_, NodeCoord>,
    strings: &StringPool,
    executor: &crate::host::Executor,
    invocation: &crate::host::Recorder,
) -> u32 {
    if interp_ways.is_empty() || addr_points.is_empty() {
        return 0;
    }

    invocation.marker("GEOCODE_INTERP_RESOLVE_INDEX_START");
    let addrs = build_endpoint_addrs(interp_ways, addr_points, interp_nodes, executor);
    invocation.marker("GEOCODE_INTERP_RESOLVE_INDEX_END");
    invocation.marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_START");
    let resolved = executor.install(|| {
        interp_ways
            .par_iter_mut()
            .map(|way| resolve_one_way(way, interp_nodes, addr_points, strings, &addrs))
            .sum()
    });
    invocation.marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_END");
    resolved
}

fn resolve_one_way(
    way: &mut SlimInterpWay,
    interp_nodes: Table<'_, NodeCoord>,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    addrs: &EndpointAddrs,
) -> u32 {
    let Some(interpolation) = way.interpolation else {
        return 0;
    };
    let Some((start, end)) = way_ends(way, interp_nodes) else {
        return 0;
    };
    let number = |key| endpoint_house_number(key, way.street_offset, addr_points, strings, addrs);
    let (Some(start), Some(end)) = (number(start), number(end)) else {
        // Leave the zero sentinel when either endpoint does not resolve.
        return 0;
    };
    // OSM requires an even or odd way's ends to follow its declared rule.
    if !interpolation.steps(start, end) {
        return 0;
    }
    way.start_number = start;
    way.end_number = end;
    1
}

/// The house number of the address at an interpolation end node: the one
/// house-number string every same-street address point at exactly `key`
/// carries, parsed strictly. None when there is no such point, when they
/// disagree (`10` beside `20`, or beside `10a`), or when the string is not a
/// positive decimal number. Disagreement is judged on the strings before any
/// parsing, so a refused spelling cannot hide a conflict.
fn endpoint_house_number(
    key: CoordKey,
    street_offset: u32,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    addrs: &EndpointAddrs,
) -> Option<u32> {
    // Offset 0 is the empty string: a way without a street name has no
    // street for its ends to agree with.
    if street_offset == 0 {
        return None;
    }
    let mut housenumber: Option<u32> = None;
    for index in addrs.at(key) {
        let Some(point) = addr_points.get(index as usize) else {
            continue;
        };
        if point.street_offset != street_offset {
            continue;
        }
        // The pool interns each string once, so equal offsets are equal
        // strings.
        match housenumber {
            None => housenumber = Some(point.housenumber_offset),
            Some(offset) if offset == point.housenumber_offset => {}
            Some(_) => return None,
        }
    }
    // A string with an interior NUL reads back as a prefix (`10\0a` as
    // `10`), which the strict parse would then accept.
    parse_house_number(strings.read_whole(housenumber?)?)
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

        fn address(&mut self, (lat_e7, lon_e7): (i32, i32), housenumber: &str, street: &str) {
            let point = AddrPoint {
                lat_e7,
                lon_e7,
                housenumber_offset: self.strings.intern(housenumber),
                street_offset: self.strings.intern(street),
                postcode_offset: 0,
            };
            self.addr_bytes.extend_from_slice(&point.to_bytes());
        }

        fn way(&mut self, nodes: &[(i32, i32)], interpolation: Option<InterpolationType>) {
            let node_file_offset = self.node_bytes.len() as u64;
            for &(lat_e7, lon_e7) in nodes {
                self.node_bytes
                    .extend_from_slice(&NodeCoord { lat_e7, lon_e7 }.to_bytes());
            }
            self.ways.push(SlimInterpWay {
                way_id: i64::try_from(self.ways.len()).expect("test way count") + 1,
                node_file_offset,
                street_offset: self.strings.intern(STREET),
                interpolation,
                node_count: u16::try_from(nodes.len()).expect("test way"),
                start_number: 0,
                end_number: 0,
            });
        }

        /// Resolve every way; return each way's `(start, end)` numbers.
        fn resolve(mut self) -> Vec<(u32, u32)> {
            let host = crate::host::Host::entered_for_tests();
            resolve_interpolation_endpoints_mmap(
                &mut self.ways,
                Table::new(&self.addr_bytes),
                Table::new(&self.node_bytes),
                &self.strings,
                &host.executor,
                &host.invocation,
            );
            self.ways
                .iter()
                .map(|way| (way.start_number, way.end_number))
                .collect()
        }
    }

    const A: (i32, i32) = (557_000_000, 125_000_000);
    const B: (i32, i32) = (557_000_000, 125_060_000);

    #[test]
    fn ends_take_the_numbers_of_the_addresses_at_them() {
        let mut fx = Fixture::new();
        fx.address(A, "10", STREET);
        fx.address(B, "30", STREET);
        fx.way(
            &[A, (557_000_000, 125_030_000), B],
            Some(InterpolationType::Even),
        );
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
        for other in ["20", "10a"] {
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
}
