//! Sparse node location index for `--locations-on-ways`.

use std::path::Path;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::blob_meta::ElemKind;
use crate::osc::CompactDiffOverlay;

/// Sparse node coordinate index for maintaining LocationsOnWays through merges.
///
/// Only contains coordinates for nodes referenced by OSC ways. Populated in two
/// stages: (1) pre-seeded from OSC node creates/modifications, (2) filled from
/// base PBF during the merge pass for nodes not in the OSC.
pub(super) struct NodeLocationIndex {
    /// Coordinates indexed by node ID (decimicrodegrees).
    pub(super) locations: FxHashMap<i64, (i32, i32)>,
    /// Node IDs still needed from the base PBF (not found in OSC).
    /// Sorted for range overlap checks against BlobIndex.
    pub(super) needed_sorted: Vec<i64>,
    /// Same IDs as `needed_sorted` but as a set for O(1) membership tests.
    pub(super) needed_set: FxHashSet<i64>,
}

impl NodeLocationIndex {
    /// Build the index from an already-parsed OSC diff.
    ///
    /// 1. Collects all node IDs referenced by OSC ways
    /// 2. Seeds coordinates from OSC nodes (creates/modifications)
    /// 3. Remaining needed IDs stored for base PBF extraction
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn build_from_diff(diff: &CompactDiffOverlay) -> Self {
        // Collect all node IDs referenced by OSC ways
        let mut all_needed: FxHashSet<i64> = FxHashSet::default();
        for &way_id in diff.way_ids() {
            if let Some(way) = diff.get_way(way_id) {
                for node_id in way.refs() {
                    all_needed.insert(node_id);
                }
            }
        }

        // Seed from OSC nodes
        let mut locations: FxHashMap<i64, (i32, i32)> =
            FxHashMap::with_capacity_and_hasher(all_needed.len(), Default::default());
        let mut still_needed: FxHashSet<i64> = FxHashSet::default();

        for &node_id in &all_needed {
            if let Some(node) = diff.get_node(node_id) {
                locations.insert(node_id, (node.decimicro_lat(), node.decimicro_lon()));
            } else {
                still_needed.insert(node_id);
            }
        }

        let mut needed_sorted: Vec<i64> = still_needed.iter().copied().collect();
        needed_sorted.sort_unstable();

        let seeded = locations.len() as u64;
        let remaining = still_needed.len() as u64;
        let total = all_needed.len() as u64;
        eprintln!(
            "Locations-on-ways: {total} node IDs needed, {seeded} from diff, {remaining} from base"
        );

        Self {
            locations,
            needed_sorted,
            needed_set: still_needed,
        }
    }

    /// Check if a blob's ID range overlaps any still-needed node IDs.
    pub(super) fn overlaps_needed(&self, min_id: i64, max_id: i64) -> bool {
        if self.needed_sorted.is_empty() {
            return false;
        }
        // Find the first needed ID >= min_id
        let start = self.needed_sorted.partition_point(|&id| id < min_id);
        // If that ID is <= max_id, there's overlap
        start < self.needed_sorted.len() && self.needed_sorted[start] <= max_id
    }

    /// Check if all needed nodes have been found.
    pub(super) fn all_found(&self) -> bool {
        self.needed_set.is_empty()
    }

    /// Pre-scan the base PBF to fill all remaining needed node coordinates.
    ///
    /// Two phases: (1) header-only walk to build a schedule of node blobs that
    /// overlap `needed_set`, skipping non-node blobs and range-disjoint node
    /// blobs; (2) parallel pread + decompress + `extract_node_tuples` across
    /// the schedule, accumulating into per-worker maps, merged at the end.
    /// Returns `(nodes_found, blobs_scanned)`.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn prefill_from_base(
        &mut self,
        base_pbf: &Path,
        _direct_io: bool,
    ) -> super::Result<(u64, u64)> {
        if self.all_found() {
            return Ok((0, 0));
        }

        let NodeBlobSchedule {
            schedule,
            blobs_skipped_non_node,
            blobs_skipped_range,
        } = self.scan_node_blob_schedule(base_pbf)?;

        let blobs_scanned = schedule.len() as u64;
        if schedule.is_empty() {
            emit_prefill_counters(0, 0, blobs_skipped_non_node, blobs_skipped_range, 0);
            return Ok((0, 0));
        }

        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        let worker_maps = extract_node_coords_parallel(
            base_pbf, &schedule, &self.needed_set, decode_threads,
        )?;

        // Merge worker maps into self.locations and drain from self.needed_set.
        let mut nodes_found: u64 = 0;
        for map in worker_maps {
            for (id, coords) in map {
                if self.needed_set.remove(&id) {
                    self.locations.insert(id, coords);
                    nodes_found += 1;
                }
            }
        }

        emit_prefill_counters(
            nodes_found, blobs_scanned, blobs_skipped_non_node, blobs_skipped_range, decode_threads,
        );
        Ok((nodes_found, blobs_scanned))
    }

    /// Phase A: header-only walk.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn scan_node_blob_schedule(
        &self,
        base_pbf: &Path,
    ) -> super::Result<NodeBlobSchedule> {
        let mut scanner = crate::blob::BlobReader::seekable_from_path(base_pbf)?;
        scanner.set_parse_indexdata(true);
        scanner
            .next_header_skip_blob()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

        let mut schedule: Vec<(u64, usize)> = Vec::new();
        let mut blobs_skipped_non_node: u64 = 0;
        let mut blobs_skipped_range: u64 = 0;
        while let Some(result_item) = scanner.next_header_with_data_offset() {
            let (hdr, _frame_offset, data_offset, data_size) = result_item?;
            if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
                continue;
            }
            if let Some(idx) = hdr.index() {
                if idx.kind != ElemKind::Node {
                    // Sorted PBF: once past nodes, no more node blobs.
                    blobs_skipped_non_node += 1;
                    break;
                }
                if !self.overlaps_needed(idx.min_id, idx.max_id) {
                    blobs_skipped_range += 1;
                    continue;
                }
            }
            schedule.push((data_offset, data_size));
        }
        Ok(NodeBlobSchedule { schedule, blobs_skipped_non_node, blobs_skipped_range })
    }
}

/// Phase A output: blob-read schedule plus skip-reason counters.
struct NodeBlobSchedule {
    schedule: Vec<(u64, usize)>,
    blobs_skipped_non_node: u64,
    blobs_skipped_range: u64,
}

/// Phase B: parallel pread + decompress + `extract_node_tuples` across the
/// schedule. Each worker accumulates into its own map; caller merges them.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn extract_node_coords_parallel(
    base_pbf: &Path,
    schedule: &[(u64, usize)],
    needed_set: &FxHashSet<i64>,
    decode_threads: usize,
) -> super::Result<Vec<FxHashMap<i64, (i32, i32)>>> {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(base_pbf)
            .map_err(|e| format!("failed to open {}: {e}", base_pbf.display()))?,
    );
    let mut worker_maps: Vec<FxHashMap<i64, (i32, i32)>> = (0..decode_threads)
        .map(|_| FxHashMap::default())
        .collect();

    let next_idx = AtomicUsize::new(0);
    let first_err: Mutex<Option<String>> = Mutex::new(None);
    let schedule_ref = schedule;
    let next_ref = &next_idx;
    let first_err_ref = &first_err;

    std::thread::scope(|scope| {
        for local_map in &mut worker_maps {
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut tuples: Vec<crate::scan::node::NodeTuple> = Vec::new();
                let mut group_starts: Vec<(usize, usize)> = Vec::new();

                loop {
                    if first_err_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        return;
                    }
                    let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                    if idx >= schedule_ref.len() {
                        break;
                    }
                    let (data_offset, data_size) = schedule_ref[idx];

                    let result: std::result::Result<(), String> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| format!("pread at {data_offset}: {e}"))?;
                        decompress_buf.clear();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                            .map_err(|e| e.to_string())?;
                        tuples.clear();
                        group_starts.clear();
                        // Match sequential behaviour: swallow extract errors.
                        if crate::scan::node::extract_node_tuples(
                            &decompress_buf,
                            &mut tuples,
                            &mut group_starts,
                        )
                        .is_ok()
                        {
                            for t in &tuples {
                                if needed_set.contains(&t.id) {
                                    local_map.insert(t.id, (t.lat, t.lon));
                                }
                            }
                        }
                        Ok(())
                    })();

                    if let Err(e) = result {
                        let mut slot = first_err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        return;
                    }
                }
            });
        }
    });

    if let Some(e) = first_err
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(e.into());
    }
    Ok(worker_maps)
}

/// Emit the six prefill counters. `decode_threads == 0` signals the
/// empty-schedule early-exit path.
fn emit_prefill_counters(
    nodes_found: u64,
    blobs_scanned: u64,
    blobs_skipped_non_node: u64,
    blobs_skipped_range: u64,
    decode_threads: usize,
) {
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("merge_prefill_nodes_found", nodes_found as i64);
        crate::debug::emit_counter("merge_prefill_blobs_scanned", blobs_scanned as i64);
        crate::debug::emit_counter(
            "merge_prefill_blobs_skipped_non_node",
            blobs_skipped_non_node as i64,
        );
        crate::debug::emit_counter(
            "merge_prefill_blobs_skipped_range",
            blobs_skipped_range as i64,
        );
        crate::debug::emit_counter("merge_prefill_early_exit", 0);
        if decode_threads > 0 {
            crate::debug::emit_counter("merge_prefill_decode_threads", decode_threads as i64);
        }
    }
}
