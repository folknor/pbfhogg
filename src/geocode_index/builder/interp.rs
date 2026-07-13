//! Interpolation endpoint resolution (mmap-based).

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
    offsets: Vec<u32>,
    /// Address-point indices, grouped by cell in `cell_ids` order, ascending
    /// within each group.
    values: Vec<u32>,
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
#[allow(clippy::cast_possible_truncation)] // Existing address-point indices are u32.
fn build_cell_addr_csr(addr_mmap: &[u8], street_level: u8) -> CellAddrCsr {
    // Drive the projection from an INDEXED source (`par_chunks_exact` is an
    // IndexedParallelIterator, and `map` preserves that) so `collect` allocates
    // the final Vec once and each worker fills its own contiguous span. Do not
    // "simplify" this to `filter_map`: that is unindexed, so its `collect`
    // stitches per-worker fragments in a second copy pass that can transiently
    // hold ~2x the pair buffer. Every addr point projects to exactly one pair,
    // so no filter is needed. This also avoids the reverted fold/reduce hashmap
    // merge (commit 363c579) whose per-worker map union regressed the build.
    let mut pairs: Vec<(u64, u32)> = addr_mmap
        .par_chunks_exact(ADDR_POINT_SIZE)
        .enumerate()
        .map(|(index, record)| {
            let record: &[u8; ADDR_POINT_SIZE] = record
                .try_into()
                .expect("par_chunks_exact always yields full AddrPoint records");
            let point = AddrPoint::from_bytes(record);
            let lat_lng =
                LatLng::from_degrees(point.lat_e7 as f64 * 1e-7, point.lon_e7 as f64 * 1e-7);
            let cell_id = CellID::from(lat_lng).parent(street_level as u64).0;
            (cell_id, index as u32)
        })
        .collect();

    // The index tie-break preserves the ascending insertion order of the old
    // hashmap rows for exact and equal-distance endpoint candidates.
    pairs.par_sort_unstable_by_key(|&(cell_id, index)| (cell_id, index));

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
        offsets.push(values.len() as u32);
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

/// Read an AddrPoint from the mmap'd addr_points.bin by index.
pub(super) fn read_addr_point_mmap(mmap: &[u8], index: u32) -> Option<AddrPoint> {
    let offset = index as usize * ADDR_POINT_SIZE;
    let end = offset + ADDR_POINT_SIZE;
    if end > mmap.len() {
        return None;
    }
    Some(AddrPoint::from_bytes(mmap[offset..end].try_into().ok()?))
}

/// Read a NodeCoord from a node mmap by byte offset.
#[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
pub(super) fn read_node_at(mmap: &[u8], byte_offset: u64) -> Option<(i32, i32)> {
    let off = byte_offset as usize;
    let end = off + NODE_COORD_SIZE;
    if end > mmap.len() {
        return None;
    }
    let nc = NodeCoord::from_bytes(mmap[off..end].try_into().ok()?);
    Some((nc.lat_e7, nc.lon_e7))
}

/// Resolve start/end house numbers for interpolation ways by matching
/// their endpoints against nearby address points with the same street name.
/// Reads address points from mmap'd addr_points.bin.
#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
pub(super) fn resolve_interpolation_endpoints_mmap(
    interp_ways: &mut [SlimInterpWay],
    addr_mmap: &[u8],
    interp_nodes_mmap: &[u8],
    strings: &StringPool,
    street_level: u8,
) -> u32 {
    let addr_count = addr_mmap.len() / ADDR_POINT_SIZE;
    if interp_ways.is_empty() || addr_count == 0 {
        return 0;
    }

    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_START");
    let csr = build_cell_addr_csr(addr_mmap, street_level);
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_END");
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_START");
    let resolved = interp_ways
        .par_iter_mut()
        .map(|way| {
            resolve_one_way(
                way,
                interp_nodes_mmap,
                addr_mmap,
                strings,
                &csr,
                street_level,
            )
        })
        .sum();
    crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_END");
    resolved
}

fn resolve_one_way(
    way: &mut SlimInterpWay,
    interp_nodes_mmap: &[u8],
    addr_mmap: &[u8],
    strings: &StringPool,
    csr: &CellAddrCsr,
    street_level: u8,
) -> u32 {
    if way.node_count < 2 {
        return 0;
    }
    let Some(start_coord) = read_node_at(interp_nodes_mmap, way.node_file_offset) else {
        return 0;
    };
    let last_offset = way.node_file_offset + (way.node_count as u64 - 1) * NODE_COORD_SIZE as u64;
    let Some(end_coord) = read_node_at(interp_nodes_mmap, last_offset) else {
        return 0;
    };
    let start_hn = find_endpoint_house_number_mmap(
        start_coord,
        way.street_offset,
        addr_mmap,
        strings,
        csr,
        street_level,
    );
    let end_hn = find_endpoint_house_number_mmap(
        end_coord,
        way.street_offset,
        addr_mmap,
        strings,
        csr,
        street_level,
    );
    if let (Some(start), Some(end)) = (start_hn, end_hn) {
        way.start_number = start;
        way.end_number = end;
        1
    } else {
        // KNOWN LIMITATION: if either endpoint fails to match an addr point,
        // we leave `start_number = 0, end_number = 0`. That collides with a
        // real OSM interpolation way that genuinely starts and ends at house
        // number 0. See `SlimInterpWay` doc. If this becomes an observed
        // correctness problem, introduce a `resolved: bool` persisted into
        // `InterpWay` and bump `FORMAT_VERSION`.
        0
    }
}

/// Find the house number of an address point near an interpolation endpoint.
#[allow(clippy::cast_possible_truncation)]
fn find_endpoint_house_number_mmap(
    endpoint: (i32, i32),
    street_offset: u32,
    addr_mmap: &[u8],
    strings: &StringPool,
    csr: &CellAddrCsr,
    street_level: u8,
) -> Option<u32> {
    let (lat_e7, lon_e7) = endpoint;
    let ll = LatLng::from_degrees(lat_e7 as f64 * 1e-7, lon_e7 as f64 * 1e-7);
    let center = CellID::from(ll).parent(street_level as u64);

    let mut best_idx: Option<u32> = None;
    let mut best_dist_sq = i64::MAX;
    let mut found_exact = false;

    let mut check_cell = |cell_id: u64| {
        for &idx in csr.get(cell_id) {
            let Some(pt) = read_addr_point_mmap(addr_mmap, idx) else {
                continue;
            };
            if pt.street_offset != street_offset {
                continue;
            }
            let dlat = (pt.lat_e7 - lat_e7) as i64;
            let dlon = (pt.lon_e7 - lon_e7) as i64;
            let dist_sq = dlat * dlat + dlon * dlon;
            let is_exact = dlat.abs() <= 1 && dlon.abs() <= 1;

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
    let pt = read_addr_point_mmap(addr_mmap, idx)?;
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
        let lat_lng = LatLng::from_degrees(lat_e7 as f64 * 1e-7, lon_e7 as f64 * 1e-7);
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

        let csr = build_cell_addr_csr(&bytes, street_level);
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
        let csr = build_cell_addr_csr(&[], 17);
        assert!(csr.cell_ids.is_empty());
        assert_eq!(csr.offsets, vec![0]);
        assert_eq!(csr.get(0), &[] as &[u32]);
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

        let csr = build_cell_addr_csr(&bytes, street_level);
        assert_eq!(
            find_endpoint_house_number_mmap(
                endpoint,
                street_offset,
                &bytes,
                &strings,
                &csr,
                street_level,
            ),
            Some(10)
        );
    }
}
