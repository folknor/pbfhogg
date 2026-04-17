//! Interpolation endpoint resolution (mmap-based).

use rustc_hash::FxHashMap;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use super::pass2::SlimInterpWay;
use super::strings::{StringPool, read_string_from_pool};

use super::super::format::*;

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
    if end > mmap.len() { return None; }
    Some(AddrPoint::from_bytes(mmap[offset..end].try_into().ok()?))
}

/// Read a NodeCoord from a node mmap by byte offset.
#[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
pub(super) fn read_node_at(mmap: &[u8], byte_offset: u64) -> Option<(i32, i32)> {
    let off = byte_offset as usize;
    let end = off + NODE_COORD_SIZE;
    if end > mmap.len() { return None; }
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

    // Build spatial index: S2 cell -> list of addr point indices
    let mut cell_to_addrs: FxHashMap<u64, Vec<u32>> = FxHashMap::default();
    for idx in 0..addr_count {
        if let Some(pt) = read_addr_point_mmap(addr_mmap, idx as u32) {
            let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
            let cell = CellID::from(ll).parent(street_level as u64).0;
            cell_to_addrs.entry(cell).or_default().push(idx as u32);
        }
    }

    let mut resolved = 0u32;

    for iw in interp_ways.iter_mut() {
        if iw.node_count < 2 { continue; }

        let Some(start_coord) = read_node_at(interp_nodes_mmap, iw.node_file_offset) else {
            continue;
        };
        let last_offset = iw.node_file_offset + (iw.node_count as u64 - 1) * NODE_COORD_SIZE as u64;
        let Some(end_coord) = read_node_at(interp_nodes_mmap, last_offset) else {
            continue;
        };

        let start_hn = find_endpoint_house_number_mmap(
            start_coord, iw.street_offset, addr_mmap, strings, &cell_to_addrs, street_level,
        );
        let end_hn = find_endpoint_house_number_mmap(
            end_coord, iw.street_offset, addr_mmap, strings, &cell_to_addrs, street_level,
        );

        if let (Some(s), Some(e)) = (start_hn, end_hn) {
            iw.start_number = s;
            iw.end_number = e;
            resolved += 1;
        }
        // KNOWN LIMITATION: if either endpoint fails to match an addr point,
        // we leave `start_number = 0, end_number = 0`. That collides with a
        // real OSM interpolation way that genuinely starts and ends at house
        // number 0. See `SlimInterpWay` doc. If this becomes an observed
        // correctness problem, introduce a `resolved: bool` persisted into
        // `InterpWay` and bump `FORMAT_VERSION`.
    }

    resolved
}

/// Find the house number of an address point near an interpolation endpoint.
#[allow(clippy::cast_possible_truncation)]
fn find_endpoint_house_number_mmap(
    endpoint: (i32, i32),
    street_offset: u32,
    addr_mmap: &[u8],
    strings: &StringPool,
    cell_to_addrs: &FxHashMap<u64, Vec<u32>>,
    street_level: u8,
) -> Option<u32> {
    let (lat_e7, lon_e7) = endpoint;
    let ll = LatLng::from_degrees(lat_e7 as f64 * 1e-7, lon_e7 as f64 * 1e-7);
    let center = CellID::from(ll).parent(street_level as u64);

    let mut best_idx: Option<u32> = None;
    let mut best_dist_sq = i64::MAX;
    let mut found_exact = false;

    let mut check_cell = |cell_id: u64| {
        let Some(indices) = cell_to_addrs.get(&cell_id) else { return };
        for &idx in indices {
            let Some(pt) = read_addr_point_mmap(addr_mmap, idx) else { continue };
            if pt.street_offset != street_offset { continue; }
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
