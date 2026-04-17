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

pub(super) fn run_pass1_5(
    input_path: &Path,
    needed_admin_ways: &crate::commands::id_set_dense::IdSetDense,
) -> Result<crate::commands::id_set_dense::IdSetDense> {
    let mut referenced_nodes = crate::commands::id_set_dense::IdSetDense::new();
    {
        let (schedule, shared_file) = crate::commands::build_classify_schedule(
            input_path, Some(crate::blob_index::ElemKind::Way),
        )?;

        // CAVEAT: per-worker `IdSetDense` accumulation sits on the "borderline"
        // side of the `parallel_classify_accumulate` contract (see that fn's
        // docs). A worker can touch node IDs across the full planet range,
        // so the worst-case per-worker bitmap is ~1.3 GB at planet scale
        // (10.4 B node IDs × 1 bit). Measured peak RSS is 14.59 GB at
        // planet — tight but workable.
        //
        // The correct long-term shape is
        // [`parallel_classify_phase`]: per-blob `Vec<i64>` merged immediately
        // into the shared `referenced_nodes`, bounding memory by blob size
        // (~8 000 elements) rather than by total accumulated unique IDs.
        // Tracked in `notes/geocode-build-opportunities.md`.
        crate::commands::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            crate::commands::id_set_dense::IdSetDense::new,
            |block, node_ids| {
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
                            for r in way.refs() { node_ids.set(r); }
                        }
                    }
                }
            },
            |worker_node_ids| {
                referenced_nodes.merge(worker_node_ids);
            },
        )?;
    }
    Ok(referenced_nodes)
}
