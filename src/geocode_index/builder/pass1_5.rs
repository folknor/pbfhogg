//! Pass 1.5: referenced-node collection (planet-scale memory optimization).
//!
//! Scan way blobs to collect node IDs referenced by geocode-relevant ways
//! (streets, building addresses, interpolation, admin members). The dense
//! node index in pass 2 only populates entries for these nodes, reducing
//! page cache working set from ~83 GB (all 10.4B nodes) to ~16 GB (~2B
//! referenced). Same pattern as ALTW pass 0.

use std::path::Path;

use crate::Element;

use super::Result;
use super::pass2::EXCLUDED_HIGHWAYS;

#[hotpath::measure]
pub(super) fn run_pass1_5(
    way_schedule: &[(usize, u64, usize)],
    max_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    needed_admin_ways: &crate::commands::id_set_dense::IdSetDense,
) -> Result<crate::commands::id_set_dense::IdSetDense> {
    // One shared pre-allocated IdSetDense; workers write concurrently via
    // `set_atomic`. Replaces the previous per-worker `IdSetDense`
    // accumulation that grew to ~20 GB anon on Germany (~29.5 GB at planet)
    // because each worker allocated independent chunks across the full
    // planet ID range. Pattern follows `renumber_external/pass1.rs`
    // (plan item #7 in notes/geocode-build-opportunities.md).
    //
    // `pre_allocate(max_node_id)` requires `max_node_id` to cover every ID
    // the classify closure will call `set_atomic` on. We get it from
    // `build_pass2_schedules` below, which reads indexdata from node blobs.
    // If a node blob lacks indexdata (-- force path only), the upper bound
    // is under-reported and `set_atomic` would panic via its diagnostic
    // path.
    let mut referenced_nodes = crate::commands::id_set_dense::IdSetDense::new();
    referenced_nodes.pre_allocate(max_node_id);

    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_START");
    {
        let referenced_ref = &referenced_nodes;
        crate::commands::parallel_classify_phase(
            shared_file,
            way_schedule,
            || (),
            |block, _state: &mut ()| {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(way) = element {
                        let mut highway = false;
                        let mut name = false;
                        let mut hn = false;
                        let mut addr_st = false;
                        let mut building = false;
                        let mut interp = false;
                        let mut highway_val: Option<&str> = None;

                        for (k, _v) in way.tags() {
                            match k {
                                "highway" => { highway = true; highway_val = Some(_v); }
                                "name" => name = true,
                                "addr:housenumber" => hn = true,
                                "addr:street" => addr_st = true,
                                "building" => building = true,
                                "addr:interpolation" => interp = true,
                                _ => {}
                            }
                        }

                        let is_street = highway && name
                            && !EXCLUDED_HIGHWAYS.contains(&highway_val.unwrap_or(""));
                        let is_building_addr = building && hn && addr_st;
                        let is_interp = interp && addr_st;
                        let is_admin = needed_admin_ways.get(way.id());

                        if is_street || is_building_addr || is_interp || is_admin {
                            for r in way.refs() {
                                referenced_ref.set_atomic(r);
                            }
                        }
                    }
                }
            },
            |_seq, ()| {},
        )?;
    }
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_END");
    Ok(referenced_nodes)
}

/// Single header pass that produces everything Pass 1.5 and Pass 2a need to
/// start: the way-blob schedule (for Pass 1.5 + unused by Pass 2a), the
/// node-blob schedule (for Pass 2a), the max node ID from indexdata (so
/// Pass 1.5 can `pre_allocate` the shared `IdSetDense`), and a single
/// shared file handle reused across both phases' `pread_at` workers.
///
/// Consolidates two previously-separate header walks (Pass 1.5's
/// `build_way_schedule_and_max_node_id` and Pass 2a's
/// `build_classify_schedule(Node)`). At Europe the consolidated walk
/// costs ~16-17 s once, vs 16.6 s + 26.5 s = 43 s for the two-walk
/// shape — measured 2026-04-18 on `bf8f2038`.
///
/// Each schedule's `seq` is local to that schedule (node and way are
/// separate `0..n` ranges, as `parallel_classify_phase`'s ReorderBuffer
/// expects).
///
/// Blobs without indexdata are added to both schedules (conservative
/// fallback matching `build_classify_schedule`'s semantics) but cannot
/// contribute to `max_node_id`. `require_indexdata` at the
/// `build_geocode_index` entry rejects non-indexed input unless `--force`
/// is set, so in practice every blob carries indexdata here.
#[allow(clippy::type_complexity)]
pub(super) fn build_pass2_schedules(
    input_path: &Path,
) -> Result<(
    Vec<(usize, u64, usize)>,  // node_schedule
    Vec<(usize, u64, usize)>,  // way_schedule
    i64,                        // max_node_id
    std::sync::Arc<std::fs::File>,
)> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input_path)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut node_seq: usize = 0;
    let mut way_seq: usize = 0;
    let mut max_node_id: i64 = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        let Some(idx) = hdr.index() else {
            // No indexdata: conservatively include in both schedules so
            // neither phase silently drops data from a --force run.
            node_schedule.push((node_seq, data_offset, data_size));
            node_seq += 1;
            way_schedule.push((way_seq, data_offset, data_size));
            way_seq += 1;
            continue;
        };
        match idx.kind {
            crate::blob_index::ElemKind::Node => {
                if idx.max_id > max_node_id {
                    max_node_id = idx.max_id;
                }
                node_schedule.push((node_seq, data_offset, data_size));
                node_seq += 1;
            }
            crate::blob_index::ElemKind::Way => {
                way_schedule.push((way_seq, data_offset, data_size));
                way_seq += 1;
            }
            crate::blob_index::ElemKind::Relation => {}
        }
    }

    drop(scanner);
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input_path)
            .map_err(|e| format!("failed to open {}: {e}", input_path.display()))?,
    );

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("pass2_node_blobs", node_schedule.len() as i64);
        crate::debug::emit_counter("pass2_way_blobs", way_schedule.len() as i64);
        crate::debug::emit_counter("pass2_max_node_id", max_node_id);
    }

    Ok((node_schedule, way_schedule, max_node_id, shared_file))
}
