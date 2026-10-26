//! Interpolation endpoint resolution (mmap-based).

use crate::coord_fmt::e7_to_deg;
use rayon::prelude::*;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use super::pass2::SlimInterpWay;
use super::strings::{StringPool, read_string_from_pool};

use super::super::format::*;

/// Compressed-sparse-row map from S2 cell IDs to address-point indices.
///
/// Built once per build, read-only during endpoint resolution, shareable
/// across rayon workers without locks. Replaces the old transient
/// `FxHashMap<u64, Vec<u32>>` (one heap `Vec` per cell) with three flat
/// arrays.
///
/// Rows retain ascending address-point index order because endpoint resolution
/// keeps the first candidate on equal-distance ties: on a distance tie (or
/// multiple exact matches) the lowest addr index must win, matching the old
/// hashmap which pushed indices in ascending scan order. A cell-only sort would
/// leave intra-cell order arbitrary and silently change which house number a
/// tied endpoint resolves to.
struct CellAddrCsr {
    /// Distinct cell ids, ascending. Binary-search key for `get`.
    cell_ids: Vec<u64>,
    /// Length `cell_ids.len() + 1`. Row `i` occupies
    /// `values[offsets[i]..offsets[i + 1]]`.
    /// This is u32 because every value is an address-point index and the
    /// on-disk header caps the total address-point count at u32::MAX.
    offsets: Vec<u32>,
    /// Address-point indices, grouped by cell in `cell_ids` order, ascending
    /// within each group.
    values: Vec<u32>,
}

#[expect(
    clippy::expect_used,
    reason = "Pass 2 refuses an address-point count over u32::MAX, and the CSR holds one value per address point"
)]
fn csr_offset(len: usize) -> u32 {
    u32::try_from(len)
        .expect("address CSR cannot exceed the u32 address-point count encoded by the index format")
}

impl CellAddrCsr {
    /// Return the address-point indices projected into `cell_id`.
    fn get(&self, cell_id: u64) -> &[u32] {
        match self.cell_ids.binary_search(&cell_id) {
            Ok(index) => {
                let start = self.offsets[index] as usize;
                let end = self.offsets[index + 1] as usize;
                &self.values[start..end]
            }
            Err(_) => &[],
        }
    }
}

/// Build the endpoint lookup index without per-cell allocations or locks.
fn build_cell_addr_csr(
    addr_points: Table<'_, AddrPoint>,
    street_level: u8,
    executor: &crate::host::Executor,
) -> CellAddrCsr {
    // Drive the projection from an INDEXED source (a `usize` range is an
    // IndexedParallelIterator, and `map` preserves that) so `collect` allocates
    // the final Vec once and each worker fills its own contiguous span. Do not
    // "simplify" this to `filter_map`: that is unindexed, so its `collect`
    // stitches per-worker fragments in a second copy pass that can transiently
    // hold ~2x the pair buffer. Every addr point projects to exactly one pair,
    // so no filter is needed. This also avoids the reverted fold/reduce hashmap
    // merge (commit 363c579) whose per-worker map union regressed the build.
    #[expect(
        clippy::expect_used,
        reason = "every index is below Table::len, and Pass 2 refuses an address-point count over u32::MAX"
    )]
    let mut pairs: Vec<(u64, u32)> = executor.install(|| {
        (0..addr_points.len())
            .into_par_iter()
            .map(|index| {
                let point = addr_points
                    .get(index)
                    .expect("an index below Table::len names a whole record");
                let lat_lng = LatLng::from_degrees(
                    e7_to_deg(point.lat_e7 as f64),
                    e7_to_deg(point.lon_e7 as f64),
                );
                let cell_id = CellID::from(lat_lng).parent(street_level as u64).0;
                (
                    cell_id,
                    u32::try_from(index).expect("address count checked during Pass 2"),
                )
            })
            .collect()
    });

    // The index tie-break preserves the ascending insertion order of the old
    // hashmap rows for exact and equal-distance endpoint candidates.
    executor.install(|| pairs.par_sort_unstable_by_key(|&(cell_id, index)| (cell_id, index)));

    let mut cell_ids = Vec::new();
    let mut offsets = vec![0_u32];
    let mut values = Vec::with_capacity(pairs.len());
    let mut index = 0;
    while index < pairs.len() {
        let cell_id = pairs[index].0;
        cell_ids.push(cell_id);
        while index < pairs.len() && pairs[index].0 == cell_id {
            values.push(pairs[index].1);
            index += 1;
        }
        offsets.push(csr_offset(values.len()));
    }

    CellAddrCsr {
        cell_ids,
        offsets,
        values,
    }
}

/// Parse leading digits from a house number string (e.g., "42" from "42A").
fn parse_house_number(s: &str) -> u32 {
    let mut n = 0u32;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
        } else {
            break;
        }
    }
    n
}

/// Resolve start/end house numbers for interpolation ways by matching
/// their endpoints against nearby address points with the same street name.
/// Reads address points from mmap'd addr_points.bin.
#[hotpath::measure]
pub(super) fn resolve_interpolation_endpoints_mmap(
    interp_ways: &mut [SlimInterpWay],
    addr_points: Table<'_, AddrPoint>,
    interp_nodes: Table<'_, NodeCoord>,
    strings: &StringPool,
    street_level: u8,
    executor: &crate::host::Executor,
) -> u32 {
    if interp_ways.is_empty() || addr_points.is_empty() {
        return 0;
    }

    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_START");
    let csr = build_cell_addr_csr(addr_points, street_level, executor);
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_END");
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_START");
    let resolved = executor.install(|| {
        interp_ways
            .par_iter_mut()
            .map(|way| resolve_one_way(way, interp_nodes, addr_points, strings, &csr, street_level))
            .sum()
    });
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_END");
    resolved
}

fn resolve_one_way(
    way: &mut SlimInterpWay,
    interp_nodes: Table<'_, NodeCoord>,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    csr: &CellAddrCsr,
    street_level: u8,
) -> u32 {
    // Not just a bounds guard: a one-node way would resolve both endpoints to
    // the same point.
    if way.node_count < 2 {
        return 0;
    }
    let Ok(nodes) = interp_nodes.span_at(way.node_file_offset, u64::from(way.node_count)) else {
        return 0;
    };
    let (Some(start), Some(end)) = (nodes.get(0), nodes.get(usize::from(way.node_count) - 1))
    else {
        return 0;
    };
    let start_hn = find_endpoint_house_number_mmap(
        (start.lat_e7, start.lon_e7),
        way.street_offset,
        addr_points,
        strings,
        csr,
        street_level,
    );
    let end_hn = find_endpoint_house_number_mmap(
        (end.lat_e7, end.lon_e7),
        way.street_offset,
        addr_points,
        strings,
        csr,
        street_level,
    );
    if let (Some(start), Some(end)) = (start_hn, end_hn) {
        way.start_number = start;
        way.end_number = end;
        1
    } else {
        // Leave the zero sentinel when either endpoint does not match.
        0
    }
}

/// Find the house number of an address point near an interpolation endpoint.
fn find_endpoint_house_number_mmap(
    endpoint: (i32, i32),
    street_offset: u32,
    addr_points: Table<'_, AddrPoint>,
    strings: &StringPool,
    csr: &CellAddrCsr,
    street_level: u8,
) -> Option<u32> {
    // Endpoint admission is currently cell-based: this scans the street-level
    // cell and its eight neighbors, so its geographic reach changes with
    // street_level. The configured fine search radius applies to Reader query
    // matching, not to endpoint resolution. Reusing it here would change which
    // endpoints are serialized and which interpolation answers exist; keep
    // this policy unchanged until endpoint matching has its own stated radius.
    let (lat_e7, lon_e7) = endpoint;
    let ll = LatLng::from_degrees(e7_to_deg(lat_e7 as f64), e7_to_deg(lon_e7 as f64));
    let center = CellID::from(ll).parent(street_level as u64);

    let mut best_idx: Option<u32> = None;
    let mut best_dist_sq = i64::MAX;
    let mut found_exact = false;

    let mut check_cell = |cell_id: u64| {
        for &idx in csr.get(cell_id) {
            let Some(pt) = addr_points.get(idx as usize) else {
                continue;
            };
            if pt.street_offset != street_offset {
                continue;
            }
            let dlat = i64::from(pt.lat_e7) - i64::from(lat_e7);
            // Same species as the pass3/reader antimeridian bugs: an S2 cell
            // straddling +/-180 degrees holds points whose raw E7 longitudes
            // differ by ~3.6e9. Unwrapped, the near candidate looks half a
            // world away (and the raw i32 subtraction overflows outright).
            let dlon =
                crate::geo::wrapped_longitude_delta_e7(i64::from(pt.lon_e7) - i64::from(lon_e7));
            let dist_sq = dlat * dlat + dlon * dlon;
            let is_exact = dlat.abs() <= crate::tunables::INTERP_ENDPOINT_EXACT_TOLERANCE_E7
                && dlon.abs() <= crate::tunables::INTERP_ENDPOINT_EXACT_TOLERANCE_E7;

            if is_exact && !found_exact {
                found_exact = true;
                best_idx = Some(idx);
                best_dist_sq = dist_sq;
            } else if (is_exact || !found_exact) && dist_sq < best_dist_sq {
                best_idx = Some(idx);
                best_dist_sq = dist_sq;
            }
        }
    };

    check_cell(center.0);
    for n in center.all_neighbors(street_level as u64) {
        check_cell(n.0);
    }

    let idx = best_idx?;
    let pt = addr_points.get(idx as usize)?;
    let hn_str = read_string_from_pool(strings, pt.housenumber_offset);
    let hn = parse_house_number(hn_str);
    if hn > 0 { Some(hn) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_point(bytes: &mut Vec<u8>, point: AddrPoint) {
        bytes.extend_from_slice(&point.to_bytes());
    }

    fn cell_id(lat_e7: i32, lon_e7: i32, street_level: u8) -> u64 {
        let lat_lng = LatLng::from_degrees(e7_to_deg(lat_e7 as f64), e7_to_deg(lon_e7 as f64));
        CellID::from(lat_lng).parent(street_level as u64).0
    }

    #[test]
    fn csr_get_matches_naive_hashmap_ordered() {
        let street_level = 17;
        let first_cell = (55_700_000, 125_030_000);
        let second_cell = (-33_868_800, 151_209_300);
        let mut bytes = Vec::new();
        for (lat_e7, lon_e7) in [first_cell, second_cell, first_cell, second_cell, first_cell] {
            append_point(
                &mut bytes,
                AddrPoint {
                    lat_e7,
                    lon_e7,
                    housenumber_offset: 0,
                    street_offset: 0,
                    postcode_offset: 0,
                },
            );
        }

        let csr = build_cell_addr_csr(
            Table::new(&bytes),
            street_level,
            &crate::host::Executor::Ambient,
        );
        assert_eq!(
            csr.get(cell_id(first_cell.0, first_cell.1, street_level)),
            &[0, 2, 4]
        );
        assert_eq!(
            csr.get(cell_id(second_cell.0, second_cell.1, street_level)),
            &[1, 3]
        );
        assert_eq!(csr.get(cell_id(0, 0, street_level)), &[] as &[u32]);
    }

    #[test]
    fn csr_get_empty_input() {
        let csr = build_cell_addr_csr(Table::new(&[]), 17, &crate::host::Executor::Ambient);
        assert!(csr.cell_ids.is_empty());
        assert_eq!(csr.offsets, vec![0]);
        assert_eq!(csr.get(0), &[] as &[u32]);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    #[should_panic(expected = "address CSR cannot exceed")]
    fn csr_offset_refuses_truncation() {
        let _ = csr_offset(u32::MAX as usize + 1);
    }

    /// An address point 0.0001 degrees away ACROSS the antimeridian must beat
    /// one 0.0009 degrees away on the same side. Before the wrap fix the raw
    /// i32 longitude subtraction saw the across-seam candidate as ~360 degrees
    /// away (and overflowed i32 outright in debug builds), so the decoy won.
    #[test]
    fn endpoint_matching_takes_the_short_arc_across_the_antimeridian() {
        let street_level = 17;
        let endpoint = (100_000_000, 1_799_999_000); // 10N, 179.9999E
        let mut strings = StringPool::new();
        let near_across_seam = strings.intern("20");
        let far_same_side = strings.intern("10");
        let street_offset = strings.intern("Dateline Road");
        let mut bytes = Vec::new();
        // True delta 2000 e7-units, raw delta ~3.6e9 (would overflow i32).
        append_point(
            &mut bytes,
            AddrPoint {
                lat_e7: endpoint.0,
                lon_e7: -1_799_999_000,
                housenumber_offset: near_across_seam,
                street_offset,
                postcode_offset: 0,
            },
        );
        // True delta 9000 e7-units, same side.
        append_point(
            &mut bytes,
            AddrPoint {
                lat_e7: endpoint.0,
                lon_e7: 1_799_990_000,
                housenumber_offset: far_same_side,
                street_offset,
                postcode_offset: 0,
            },
        );

        let csr = build_cell_addr_csr(
            Table::new(&bytes),
            street_level,
            &crate::host::Executor::Ambient,
        );
        assert_eq!(
            find_endpoint_house_number_mmap(
                endpoint,
                street_offset,
                Table::new(&bytes),
                &strings,
                &csr,
                street_level,
            ),
            Some(20)
        );
    }

    #[test]
    fn endpoint_tiebreak_picks_lowest_index() {
        let street_level = 17;
        let endpoint = (55_700_000, 125_030_000);
        let mut strings = StringPool::new();
        let low_index_house = strings.intern("10");
        let high_index_house = strings.intern("20");
        let street_offset = strings.intern("Interp Street");
        let mut bytes = Vec::new();
        for housenumber_offset in [low_index_house, high_index_house] {
            append_point(
                &mut bytes,
                AddrPoint {
                    lat_e7: endpoint.0,
                    lon_e7: endpoint.1,
                    housenumber_offset,
                    street_offset,
                    postcode_offset: 0,
                },
            );
        }

        let csr = build_cell_addr_csr(
            Table::new(&bytes),
            street_level,
            &crate::host::Executor::Ambient,
        );
        assert_eq!(
            find_endpoint_house_number_mmap(
                endpoint,
                street_offset,
                Table::new(&bytes),
                &strings,
                &csr,
                street_level,
            ),
            Some(10)
        );
    }
}
