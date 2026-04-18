//! Pass 3: S2 cell assignment + cell-index write.
//!
//! Fine + coarse S2 cell aggregation via a two-stage bucketed pipeline:
//! Stage A parses per-way S2 cells and partitions entries into 256 on-disk
//! buckets by top 8 bits of cell_id. Stage B processes one bucket at a time,
//! grouping by cell_id and writing the merged on-disk records. Also covers
//! admin-cell assignment (`assign_admin_cells`).

use std::io::{BufWriter, Write};
use std::path::Path;

use s2::cellid::CellID;
use s2::latlng::LatLng;

use super::Result;
use super::admin::AssembledPolygon;
use super::interp::{read_addr_point_mmap, read_node_at};
use super::pass2::SlimInterpWay;

use super::super::format::*;

// ---------------------------------------------------------------------------
// Cell entry types
// ---------------------------------------------------------------------------

pub(super) struct AdminCellEntry {
    pub(super) cell_id: u64,
    pub(super) poly_index: u32,
    pub(super) is_interior: bool,
}

// ---------------------------------------------------------------------------
// S2 cell covering for line segments
// ---------------------------------------------------------------------------

/// Cover a line segment by sampling intermediate points to find all S2 cells
/// the segment passes through at the given level.
///
/// Calls `emit(cell_id)` for each unique cell the segment crosses. No heap
/// allocation - uses a small stack buffer for deduplication (most segments
/// cross 1-4 cells).
fn cover_segment(
    lat1_e7: i32, lon1_e7: i32,
    lat2_e7: i32, lon2_e7: i32,
    level: u8,
    mut emit: impl FnMut(u64),
) {
    let lat1 = lat1_e7 as f64 * 1e-7;
    let lon1 = lon1_e7 as f64 * 1e-7;
    let lat2 = lat2_e7 as f64 * 1e-7;
    let lon2 = lon2_e7 as f64 * 1e-7;

    let c1 = CellID::from(LatLng::from_degrees(lat1, lon1)).parent(level as u64);
    let c2 = CellID::from(LatLng::from_degrees(lat2, lon2)).parent(level as u64);

    emit(c1.0);
    if c1.0 == c2.0 {
        return;
    }

    // Walk intermediate points. Step size < half cell edge to catch crossings.
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let seg_len_deg = (dlat.powi(2) + dlon.powi(2)).sqrt();

    let step_deg = match level {
        17 => 0.0003,
        14 => 0.003,
        10 => 0.005,
        _ => 0.001,
    };

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let steps = ((seg_len_deg / step_deg).ceil() as usize).clamp(2, 256);

    // Stack-based dedup for the common case (1-8 cells per segment)
    let mut seen = [0u64; 8];
    seen[0] = c1.0;
    let mut seen_count = 1usize;

    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let lat = lat1 + t * (lat2 - lat1);
        let lon = lon1 + t * (lon2 - lon1);
        let c = CellID::from(LatLng::from_degrees(lat, lon)).parent(level as u64).0;
        let already = seen[..seen_count].contains(&c);
        if !already {
            emit(c);
            if seen_count < seen.len() {
                seen[seen_count] = c;
                seen_count += 1;
            }
        }
    }
    if !seen[..seen_count].contains(&c2.0) {
        emit(c2.0);
    }
}

// ---------------------------------------------------------------------------
// S2 cell assignment
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Bucketed cell assignment
// ---------------------------------------------------------------------------

const NUM_BUCKETS: usize = 256;
const STREET_CHUNK: usize = 100_000;
const ADDR_CHUNK: usize = 500_000;

/// Tagged bucket record: 15 bytes on disk.
/// cell_id (8) + entry_type (1) + way_or_addr_index (4) + segment_index (2)
const BUCKET_RECORD_SIZE: usize = 15;
const ENTRY_TYPE_STREET: u8 = 0;
const ENTRY_TYPE_ADDR: u8 = 1;
const ENTRY_TYPE_INTERP: u8 = 2;

fn bucket_for_cell(cell_id: u64) -> usize {
    (cell_id >> 56) as usize
}

/// Ensure bucket writer exists, creating the file lazily.
fn ensure_bucket_writer(
    writers: &mut [Option<BufWriter<std::fs::File>>],
    bucket: usize,
    bucket_dir: &Path,
) -> Result<()> {
    if writers[bucket].is_none() {
        let path = bucket_dir.join(format!("{bucket:03}"));
        writers[bucket] = Some(BufWriter::new(std::fs::File::create(path)?));
    }
    Ok(())
}

fn write_bucket_record(w: &mut BufWriter<std::fs::File>, cell_id: u64, etype: u8, index: u32, segment: u16) -> std::io::Result<()> {
    let mut buf = [0u8; BUCKET_RECORD_SIZE];
    buf[0..8].copy_from_slice(&cell_id.to_le_bytes());
    buf[8] = etype;
    buf[9..13].copy_from_slice(&index.to_le_bytes());
    buf[13..15].copy_from_slice(&segment.to_le_bytes());
    w.write_all(&buf)
}

struct ParsedBucketEntry {
    cell_id: u64,
    etype: u8,
    index: u32,
    segment: u16,
}

fn parse_bucket_file(data: &[u8]) -> Vec<ParsedBucketEntry> {
    let count = data.len() / BUCKET_RECORD_SIZE;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * BUCKET_RECORD_SIZE;
        let cell_id = u64::from_le_bytes([
            data[off], data[off+1], data[off+2], data[off+3],
            data[off+4], data[off+5], data[off+6], data[off+7],
        ]);
        let etype = data[off+8];
        let index = u32::from_le_bytes([data[off+9], data[off+10], data[off+11], data[off+12]]);
        let segment = u16::from_le_bytes([data[off+13], data[off+14]]);
        entries.push(ParsedBucketEntry { cell_id, etype, index, segment });
    }
    entries
}

/// Parameters for the fused fine + coarse bucketed cell assignment
/// (plan item #4). Does one Stage A pass over streets / addrs / interps
/// at the fine level, deriving coarse-level cells via S2 parent on the
/// fly and de-duplicating per-segment. Runs Stage B once per tree to
/// produce the final cell + entry files.
pub(super) struct FusedCellAssignmentParams<'a> {
    pub(super) output_dir: &'a Path,
    pub(super) street_ways_mmap: &'a [u8],
    pub(super) street_nodes_mmap: &'a [u8],
    pub(super) street_way_count: u32,
    pub(super) addr_points_mmap: &'a [u8],
    pub(super) addr_point_count: u32,
    pub(super) interp_ways: &'a [SlimInterpWay],
    pub(super) interp_nodes_mmap: &'a [u8],
    pub(super) fine_level: u8,
    pub(super) coarse_level: u8,
    pub(super) fine_cells_file: &'a str,
    pub(super) fine_street_entries_file: &'a str,
    pub(super) fine_addr_entries_file: &'a str,
    pub(super) fine_interp_entries_file: &'a str,
    pub(super) coarse_cells_file: &'a str,
    pub(super) coarse_street_entries_file: &'a str,
    pub(super) coarse_addr_entries_file: &'a str,
    pub(super) coarse_interp_entries_file: &'a str,
}

/// Fused fine + coarse bucketed cell assignment. Returns
/// `(fine_cells_written, coarse_cells_written)`.
///
/// Savings come from halving the Stage A `cover_segment` work: each
/// segment is walked once at the fine S2 level, and every emitted fine
/// cell derives its coarse parent via `s2::CellID(cid).parent()`.
/// Multiple fine cells in a segment commonly share a coarse parent, so
/// per-segment dedup via a 4-entry stack set emits each coarse cell
/// once per segment. Addr points (single cell per point) trivially
/// derive coarse from fine without any cover work.
///
/// Stage B runs separately on each of the two bucket trees —
/// parallelised per plan item #3 (256-bucket parallel parse+sort,
/// serial group+write).
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
pub(super) fn bucketed_cell_assignment_fused(p: &FusedCellAssignmentParams<'_>) -> Result<(u32, u32)> {
    use rayon::prelude::*;

    let FusedCellAssignmentParams {
        output_dir, street_ways_mmap, street_nodes_mmap, street_way_count,
        addr_points_mmap, addr_point_count, interp_ways, interp_nodes_mmap,
        fine_level, coarse_level,
        fine_cells_file, fine_street_entries_file, fine_addr_entries_file, fine_interp_entries_file,
        coarse_cells_file, coarse_street_entries_file, coarse_addr_entries_file, coarse_interp_entries_file,
    } = p;
    let street_way_count = *street_way_count;
    let addr_point_count = *addr_point_count;
    let fine_level = *fine_level;
    let coarse_level = *coarse_level;

    // Create two bucket directories (one per level).
    let fine_bucket_dir = output_dir.join(format!(".buckets-level{fine_level}"));
    let coarse_bucket_dir = output_dir.join(format!(".buckets-level{coarse_level}"));
    for dir in [&fine_bucket_dir, &coarse_bucket_dir] {
        if dir.exists() { std::fs::remove_dir_all(dir)?; }
        std::fs::create_dir_all(dir)?;
    }

    // Two sets of bucket writers (lazy).
    let mut fine_writers: Vec<Option<BufWriter<std::fs::File>>> = (0..NUM_BUCKETS)
        .map(|_| None)
        .collect();
    let mut coarse_writers: Vec<Option<BufWriter<std::fs::File>>> = (0..NUM_BUCKETS)
        .map(|_| None)
        .collect();

    // Stage A: Chunked parallel compute at the FINE level; every emitted
    // fine cell derives its coarse parent via S2 `parent()` and
    // de-duplicates per-segment so each coarse cell is written at most
    // once per segment. Saves Europe's ~4.9 s coarse-streets Stage A
    // (measured pre-fusion UUID bf8f2038) — the coarse cover_segment
    // pass is entirely eliminated.

    // Streets (fused fine + coarse)
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_STREETS_START");
    let mut chunk_start = 0u32;
    while chunk_start < street_way_count {
        let chunk_end = (chunk_start + STREET_CHUNK as u32).min(street_way_count);
        #[allow(clippy::type_complexity)]
        let entries: Vec<(u64, Option<u64>, u32, u16)> = (chunk_start..chunk_end)
            .into_par_iter()
            .flat_map_iter(|way_idx| {
                let offset = way_idx as usize * STREET_WAY_SIZE;
                let rec = street_ways_mmap.get(offset..offset + STREET_WAY_SIZE)
                    .and_then(|b| <&[u8; STREET_WAY_SIZE]>::try_from(b).ok())
                    .map(StreetWay::from_bytes);
                let mut out = Vec::new();
                if let Some(rec) = rec {
                    let nc = rec.node_count as usize;
                    if nc >= 2 {
                        out.reserve(nc * 2);
                        for seg_idx in 0..nc - 1 {
                            let off1 = rec.node_offset as usize + seg_idx * NODE_COORD_SIZE;
                            let off2 = off1 + NODE_COORD_SIZE;
                            if let (Some(n1), Some(n2)) = (
                                read_node_at(street_nodes_mmap, off1 as u64),
                                read_node_at(street_nodes_mmap, off2 as u64),
                            ) {
                                let mut coarse_seen = [0u64; 4];
                                let mut coarse_n = 0usize;
                                cover_segment(n1.0, n1.1, n2.0, n2.1, fine_level, |fine_cid| {
                                    let coarse_cid = CellID(fine_cid)
                                        .parent(coarse_level as u64).0;
                                    let coarse_to_emit = if coarse_seen[..coarse_n].contains(&coarse_cid) {
                                        None
                                    } else {
                                        if coarse_n < coarse_seen.len() {
                                            coarse_seen[coarse_n] = coarse_cid;
                                            coarse_n += 1;
                                        }
                                        Some(coarse_cid)
                                    };
                                    out.push((fine_cid, coarse_to_emit, way_idx, seg_idx as u16));
                                });
                            }
                        }
                    }
                }
                out.into_iter()
            })
            .collect();

        for &(fine_cid, coarse_opt, wi, si) in &entries {
            let b = bucket_for_cell(fine_cid);
            ensure_bucket_writer(&mut fine_writers, b, &fine_bucket_dir)?;
            write_bucket_record(fine_writers[b].as_mut().expect("ensured"),
                fine_cid, ENTRY_TYPE_STREET, wi, si)?;
            if let Some(coarse_cid) = coarse_opt {
                let b = bucket_for_cell(coarse_cid);
                ensure_bucket_writer(&mut coarse_writers, b, &coarse_bucket_dir)?;
                write_bucket_record(coarse_writers[b].as_mut().expect("ensured"),
                    coarse_cid, ENTRY_TYPE_STREET, wi, si)?;
            }
        }
        chunk_start = chunk_end;
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_STREETS_END");

    // Addr points: single cell per point, no cover_segment. Derive fine
    // and coarse via CellID.parent() chain from the same LatLng.
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_ADDR_START");
    let addr_count = addr_point_count as usize;
    let mut chunk_start = 0usize;
    while chunk_start < addr_count {
        let chunk_end = (chunk_start + ADDR_CHUNK).min(addr_count);
        let addr_entries: Vec<(u64, u64, u32)> = (chunk_start..chunk_end)
            .into_par_iter()
            .filter_map(|idx| {
                let pt = read_addr_point_mmap(addr_points_mmap, idx as u32)?;
                let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
                let fine_cid = CellID::from(ll).parent(fine_level as u64).0;
                let coarse_cid = CellID(fine_cid).parent(coarse_level as u64).0;
                Some((fine_cid, coarse_cid, idx as u32))
            })
            .collect();
        for &(fine_cid, coarse_cid, idx) in &addr_entries {
            let b = bucket_for_cell(fine_cid);
            ensure_bucket_writer(&mut fine_writers, b, &fine_bucket_dir)?;
            write_bucket_record(fine_writers[b].as_mut().expect("ensured"),
                fine_cid, ENTRY_TYPE_ADDR, idx, 0)?;
            let b = bucket_for_cell(coarse_cid);
            ensure_bucket_writer(&mut coarse_writers, b, &coarse_bucket_dir)?;
            write_bucket_record(coarse_writers[b].as_mut().expect("ensured"),
                coarse_cid, ENTRY_TYPE_ADDR, idx, 0)?;
        }
        chunk_start = chunk_end;
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_ADDR_END");

    // Interpolation (fused fine + coarse, same shape as streets)
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_INTERP_START");
    #[allow(clippy::type_complexity)]
    let interp_entries: Vec<(u64, Option<u64>, u32, u16)> = (0..interp_ways.len())
        .into_par_iter()
        .flat_map_iter(|way_idx| {
            let iw = &interp_ways[way_idx];
            let nc = iw.node_count as usize;
            let mut out = Vec::new();
            if nc >= 2 {
                out.reserve(nc * 2);
                for seg_idx in 0..nc - 1 {
                    let off1 = iw.node_file_offset as usize + seg_idx * NODE_COORD_SIZE;
                    let off2 = off1 + NODE_COORD_SIZE;
                    if let (Some(n1), Some(n2)) = (
                        read_node_at(interp_nodes_mmap, off1 as u64),
                        read_node_at(interp_nodes_mmap, off2 as u64),
                    ) {
                        let mut coarse_seen = [0u64; 4];
                        let mut coarse_n = 0usize;
                        cover_segment(n1.0, n1.1, n2.0, n2.1, fine_level, |fine_cid| {
                            let coarse_cid = CellID(fine_cid)
                                .parent(coarse_level as u64).0;
                            let coarse_to_emit = if coarse_seen[..coarse_n].contains(&coarse_cid) {
                                None
                            } else {
                                if coarse_n < coarse_seen.len() {
                                    coarse_seen[coarse_n] = coarse_cid;
                                    coarse_n += 1;
                                }
                                Some(coarse_cid)
                            };
                            out.push((fine_cid, coarse_to_emit, way_idx as u32, seg_idx as u16));
                        });
                    }
                }
            }
            out.into_iter()
        })
        .collect();
    for &(fine_cid, coarse_opt, wi, si) in &interp_entries {
        let b = bucket_for_cell(fine_cid);
        ensure_bucket_writer(&mut fine_writers, b, &fine_bucket_dir)?;
        write_bucket_record(fine_writers[b].as_mut().expect("ensured"),
            fine_cid, ENTRY_TYPE_INTERP, wi, si)?;
        if let Some(coarse_cid) = coarse_opt {
            let b = bucket_for_cell(coarse_cid);
            ensure_bucket_writer(&mut coarse_writers, b, &coarse_bucket_dir)?;
            write_bucket_record(coarse_writers[b].as_mut().expect("ensured"),
                coarse_cid, ENTRY_TYPE_INTERP, wi, si)?;
        }
    }
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEA_INTERP_END");

    // Flush and drop both sets of bucket writers
    for writer in fine_writers.iter_mut().flatten() { writer.flush()?; }
    for writer in coarse_writers.iter_mut().flatten() { writer.flush()?; }
    drop(fine_writers);
    drop(coarse_writers);

    // Stage B, twice — once per bucket tree.
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_FINE_START");
    let fine_count = run_stage_b(
        output_dir, &fine_bucket_dir,
        fine_cells_file, fine_street_entries_file,
        fine_addr_entries_file, fine_interp_entries_file,
    )?;
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_FINE_END");

    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_COARSE_START");
    let coarse_count = run_stage_b(
        output_dir, &coarse_bucket_dir,
        coarse_cells_file, coarse_street_entries_file,
        coarse_addr_entries_file, coarse_interp_entries_file,
    )?;
    crate::debug::emit_marker("GEOCODE_PASS3_STAGEB_COARSE_END");

    Ok((fine_count, coarse_count))
}

/// Stage B helper (plan item #3): parallel per-bucket read+parse+sort,
/// serial group+write. Extracted so the fused fine+coarse driver
/// (`bucketed_cell_assignment_fused`) can run it once per tree.
///
/// Runs byte-identically to the pre-parallel sequential Stage B because
/// the serial group+write phase walks the bucket-ordered
/// `sorted_buckets` Vec, preserving the debug_assert on cross-bucket
/// cell_id monotonicity.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_stage_b(
    output_dir: &Path,
    bucket_dir: &Path,
    cells_file: &str,
    street_entries_file: &str,
    addr_entries_file: &str,
    interp_entries_file: &str,
) -> Result<u32> {
    use rayon::prelude::*;

    let mut cells_out = BufWriter::new(std::fs::File::create(output_dir.join(cells_file))?);
    let mut street_out = BufWriter::new(std::fs::File::create(output_dir.join(street_entries_file))?);
    let mut addr_out = BufWriter::new(std::fs::File::create(output_dir.join(addr_entries_file))?);
    let mut interp_out = BufWriter::new(std::fs::File::create(output_dir.join(interp_entries_file))?);

    let mut street_byte_offset: u64 = 0;
    let mut addr_byte_offset: u64 = 0;
    let mut interp_byte_offset: u64 = 0;
    let mut total_cells: u32 = 0;
    let mut prev_cell_id: u64 = 0;

    let sorted_buckets: Vec<Vec<ParsedBucketEntry>> = (0..NUM_BUCKETS)
        .into_par_iter()
        .map(|bucket_idx| -> std::io::Result<Vec<ParsedBucketEntry>> {
            let bucket_path = bucket_dir.join(format!("{bucket_idx:03}"));
            if !bucket_path.exists() { return Ok(Vec::new()); }
            let data = std::fs::read(&bucket_path)?;
            std::fs::remove_file(&bucket_path)?;
            if data.is_empty() { return Ok(Vec::new()); }
            let mut entries = parse_bucket_file(&data);
            drop(data);
            entries.sort_unstable_by_key(|e| e.cell_id);
            Ok(entries)
        })
        .collect::<std::io::Result<_>>()?;

    for entries in &sorted_buckets {
        if entries.is_empty() { continue; }

        let mut i = 0;
        let mut streets: Vec<&ParsedBucketEntry> = Vec::new();
        let mut addrs: Vec<&ParsedBucketEntry> = Vec::new();
        let mut interps: Vec<&ParsedBucketEntry> = Vec::new();
        while i < entries.len() {
            let cell_id = entries[i].cell_id;
            let group_start = i;
            while i < entries.len() && entries[i].cell_id == cell_id {
                i += 1;
            }
            let group = &entries[group_start..i];

            streets.clear();
            addrs.clear();
            interps.clear();
            for e in group {
                match e.etype {
                    ENTRY_TYPE_STREET => streets.push(e),
                    ENTRY_TYPE_ADDR => addrs.push(e),
                    ENTRY_TYPE_INTERP => interps.push(e),
                    _ => {}
                }
            }

            // Write street entries.
            // INVARIANT: the on-disk count field is u16 (see `format::SegmentEntryIter`
            // and `U32EntryIter` reader-side decoders). Hard-error if any cell's
            // per-type entry count exceeds `u16::MAX`, rather than silently truncating.
            // If this ever fires in practice, bump the on-disk count to u32 and
            // increment `FORMAT_VERSION`. Do NOT restore a `.min(u16::MAX)` here -
            // silent truncation made this a latent bug for a long time.
            let has_streets = !streets.is_empty();
            if has_streets {
                let count = u16::try_from(streets.len()).map_err(|_| format!(
                    "geocode Stage B: cell {cell_id} has {} street entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                    streets.len()
                ))?;
                street_out.write_all(&count.to_le_bytes())?;
                for e in &streets {
                    street_out.write_all(&SegmentRef {
                        way_index: e.index,
                        segment_index: e.segment,
                    }.to_bytes())?;
                }
            }

            let has_addrs = !addrs.is_empty();
            if has_addrs {
                let count = u16::try_from(addrs.len()).map_err(|_| format!(
                    "geocode Stage B: cell {cell_id} has {} addr entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                    addrs.len()
                ))?;
                addr_out.write_all(&count.to_le_bytes())?;
                for e in &addrs {
                    addr_out.write_all(&e.index.to_le_bytes())?;
                }
            }

            let has_interps = !interps.is_empty();
            if has_interps {
                let count = u16::try_from(interps.len()).map_err(|_| format!(
                    "geocode Stage B: cell {cell_id} has {} interp entries, exceeds u16::MAX. \
                     Bump on-disk count to u32 and increment FORMAT_VERSION.",
                    interps.len()
                ))?;
                interp_out.write_all(&count.to_le_bytes())?;
                for e in &interps {
                    interp_out.write_all(&SegmentRef {
                        way_index: e.index,
                        segment_index: e.segment,
                    }.to_bytes())?;
                }
            }

            let gc = GeoCell {
                cell_id,
                street_offset: if has_streets { street_byte_offset } else { NO_DATA_U64 },
                addr_offset: if has_addrs { addr_byte_offset } else { NO_DATA_U64 },
                interp_offset: if has_interps { interp_byte_offset } else { NO_DATA_U64 },
            };
            cells_out.write_all(&gc.to_bytes())?;
            debug_assert!(
                cell_id > prev_cell_id || total_cells == 0,
                "bucket ordering violated: cell {cell_id} <= prev {prev_cell_id}"
            );
            prev_cell_id = cell_id;
            total_cells += 1;

            // The per-type `u16::try_from` above guarantees `X.len() <= u16::MAX`
            // so these `* SIZE as u64` casts won't overflow.
            if has_streets {
                street_byte_offset += 2 + (streets.len() * SEGMENT_REF_SIZE) as u64;
            }
            if has_addrs {
                addr_byte_offset += 2 + (addrs.len() * 4) as u64;
            }
            if has_interps {
                interp_byte_offset += 2 + (interps.len() * SEGMENT_REF_SIZE) as u64;
            }
        }
    }

    cells_out.flush()?;
    street_out.flush()?;
    addr_out.flush()?;
    interp_out.flush()?;

    std::fs::remove_dir_all(bucket_dir).ok();

    Ok(total_cells)
}

#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
pub(super) fn assign_admin_cells(polygons: &[AssembledPolygon], admin_level: u8) -> Vec<AdminCellEntry> {
    use rayon::prelude::*;

    // Per-polygon work is independent: edge-cell cover + centroid flood-fill
    // read the polygon vertices in isolation and produce a disjoint set of
    // `AdminCellEntry`s. `par_iter().flat_map_iter().collect()` preserves
    // input ordering so downstream `entries.sort_unstable_by_key(cell_id)`
    // in `write_admin_index` gives a byte-identical result to the sequential
    // path. Plan item #6 — Europe admin flood-fill was 10.7 s sequential
    // (UUID bf8f2038); expected ~5× on 12 cores.
    polygons.par_iter().enumerate().flat_map_iter(|(poly_idx, poly)| {
        admin_cells_for_polygon(poly_idx, poly, admin_level).into_iter()
    }).collect()
}

#[allow(clippy::cast_possible_truncation)]
fn admin_cells_for_polygon(
    poly_idx: usize,
    poly: &AssembledPolygon,
    admin_level: u8,
) -> Vec<AdminCellEntry> {
    // Parse vertices into rings (exterior + holes) separated by RING_SENTINEL
    let (ext_f64, hole_rings) = parse_polygon_rings(&poly.vertices);
    if ext_f64.len() < 3 { return Vec::new(); }

    let hole_slices: Vec<&[(f64, f64)]> = hole_rings.iter().map(Vec::as_slice).collect();

    // Edge cells: cover each ring segment using cover_segment
    let mut edge_cells = rustc_hash::FxHashSet::default();
    for v in poly.vertices.windows(2) {
        if v[0] == RING_SENTINEL || v[1] == RING_SENTINEL { continue; }
        cover_segment(v[0].lat_e7, v[0].lon_e7, v[1].lat_e7, v[1].lon_e7, admin_level, |cid| {
            edge_cells.insert(cid);
        });
    }

    let mut entries: Vec<AdminCellEntry> = edge_cells.iter()
        .map(|&cid| AdminCellEntry { cell_id: cid, poly_index: poly_idx as u32, is_interior: false })
        .collect();

    // Interior cells: flood-fill from centroid using point_in_polygon (with holes)
    let exterior_end = poly.vertices.iter()
        .position(|v| *v == RING_SENTINEL)
        .unwrap_or(poly.vertices.len());
    let exterior = &poly.vertices[..exterior_end];
    if exterior.len() < 3 { return entries; }

    let (sum_lat, sum_lon, count) = exterior.iter()
        .fold((0i64, 0i64, 0i64), |(sl, sn, c), v| {
            (sl + v.lat_e7 as i64, sn + v.lon_e7 as i64, c + 1)
        });
    if count == 0 { return entries; }
    let clat = sum_lat as f64 / count as f64 * 1e-7;
    let clon = sum_lon as f64 / count as f64 * 1e-7;

    // Centroid must be inside the polygon (exterior AND not in any hole)
    if !crate::geo::point_in_polygon(clon, clat, &ext_f64, &hole_slices) { return entries; }

    let seed = CellID::from(LatLng::from_degrees(clat, clon)).parent(admin_level as u64);
    let mut visited = rustc_hash::FxHashSet::default();
    let mut queue = std::collections::VecDeque::new();
    visited.insert(seed.0);
    queue.push_back(seed);

    while let Some(cell) = queue.pop_front() {
        if edge_cells.contains(&cell.0) { continue; }

        let center_ll = s2::latlng::LatLng::from(cell);
        // Test with holes - cells inside enclaves are NOT interior
        if crate::geo::point_in_polygon(
            center_ll.lng.deg(), center_ll.lat.deg(), &ext_f64, &hole_slices,
        ) {
            entries.push(AdminCellEntry {
                cell_id: cell.0, poly_index: poly_idx as u32, is_interior: true,
            });
            for n in cell.all_neighbors(admin_level as u64) {
                if !visited.contains(&n.0) {
                    visited.insert(n.0);
                    queue.push_back(n);
                }
            }
        }
    }
    entries
}

/// Parse polygon vertices (with sentinel separators) into exterior + hole rings as f64.
#[allow(clippy::type_complexity)]
fn parse_polygon_rings(vertices: &[NodeCoord]) -> (Vec<(f64, f64)>, Vec<Vec<(f64, f64)>>) {
    super::super::format::parse_polygon_rings(vertices.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_segment_same_cell() {
        // Two very close points in Copenhagen - should be in the same S2 cell at level 17.
        let lat1 = 556_762_000; // 55.6762
        let lon1 = 125_683_000; // 12.5683
        let lat2 = 556_762_100; // 55.67621 - ~0.1m away
        let lon2 = 125_683_100;

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert_eq!(cells.len(), 1, "very close points should produce exactly 1 cell");
    }

    #[test]
    fn cover_segment_cross_cell() {
        // Two points about 1km apart - should cross multiple S2 cells at level 17.
        // Level 17 cells are roughly 150m on a side.
        let lat1 = 556_700_000; // 55.6700
        let lon1 = 125_600_000; // 12.5600
        let lat2 = 556_800_000; // 55.6800 - ~1.1km north
        let lon2 = 125_700_000; // 12.5700

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert!(cells.len() >= 2, "points ~1km apart should cross at least 2 level-17 cells, got {}", cells.len());
    }

    #[test]
    fn cover_segment_endpoints_included() {
        // Verify both endpoint cells are always in the output.
        let lat1 = 556_700_000;
        let lon1 = 125_600_000;
        let lat2 = 556_800_000;
        let lon2 = 125_700_000;

        let c1 = CellID::from(LatLng::from_degrees(55.6700, 12.5600)).parent(17).0;
        let c2 = CellID::from(LatLng::from_degrees(55.6800, 12.5700)).parent(17).0;

        let mut cells = Vec::new();
        cover_segment(lat1, lon1, lat2, lon2, 17, |c| cells.push(c));

        assert!(cells.contains(&c1), "first endpoint cell must be included");
        assert!(cells.contains(&c2), "second endpoint cell must be included");
    }
}
