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
    input_path: &Path,
    needed_admin_ways: &crate::commands::id_set_dense::IdSetDense,
) -> Result<crate::commands::id_set_dense::IdSetDense> {
    // One shared pre-allocated IdSetDense; workers write concurrently via
    // `set_atomic`. Replaces the previous per-worker `IdSetDense`
    // accumulation that grew to ~20 GB anon on Germany (~29.5 GB at planet)
    // because each worker allocated independent chunks across the full
    // planet ID range. Pattern follows `renumber_external/pass1.rs`
    // (plan item #7 in notes/geocode-build-opportunities.md).
    //
    // Two prerequisites for `pre_allocate`: a max_node_id upper bound and
    // indexdata presence. We walk the blob headers once to collect both the
    // way schedule and the max node ID from indexdata; if a node blob
    // lacks indexdata we conservatively keep max_node_id at whatever has
    // been observed and rely on `set_atomic`'s diagnostic panic to surface
    // out-of-range IDs if the --force path ever reaches here.
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCHEDULE_START");
    let (way_schedule, max_node_id, shared_file) =
        build_way_schedule_and_max_node_id(input_path)?;
    crate::debug::emit_marker("GEOCODE_PASS1_5_SCHEDULE_END");

    let mut referenced_nodes = crate::commands::id_set_dense::IdSetDense::new();
    referenced_nodes.pre_allocate(max_node_id);

    crate::debug::emit_marker("GEOCODE_PASS1_5_SCAN_START");
    {
        let referenced_ref = &referenced_nodes;
        crate::commands::parallel_classify_phase(
            &shared_file,
            &way_schedule,
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

/// Single header pass that produces both outputs needed for Pass 1.5:
/// - the way-blob schedule that workers iterate over
/// - the max node ID across all node blobs (for `IdSetDense::pre_allocate`)
///
/// Replaces two separate passes (one for each) since the header walk itself
/// is ~15 s at planet scale and walking it twice is wasted work. Blobs
/// without indexdata are conservatively added to the way schedule
/// (matching `build_classify_schedule` semantics) but can't contribute to
/// the max-node-id estimate — which is fine since `require_indexdata` at
/// the caller rejects non-indexed input unless `--force` is set.
fn build_way_schedule_and_max_node_id(
    input_path: &Path,
) -> Result<(Vec<(usize, u64, usize)>, i64, std::sync::Arc<std::fs::File>)> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input_path)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut seq: usize = 0;
    let mut max_node_id: i64 = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        let Some(idx) = hdr.index() else {
            way_schedule.push((seq, data_offset, data_size));
            seq += 1;
            continue;
        };
        match idx.kind {
            crate::blob_index::ElemKind::Node => {
                if idx.max_id > max_node_id {
                    max_node_id = idx.max_id;
                }
            }
            crate::blob_index::ElemKind::Way => {
                way_schedule.push((seq, data_offset, data_size));
                seq += 1;
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
    crate::debug::emit_counter("pass1_5_way_blobs", way_schedule.len() as i64);
    crate::debug::emit_counter("pass1_5_max_node_id", max_node_id);

    Ok((way_schedule, max_node_id, shared_file))
}
