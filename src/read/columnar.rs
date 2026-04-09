//! Columnar dense node decode — batch decode IDs, lats, lons into contiguous
//! arrays for cache-friendly classification passes.
//!
//! The PBF wire format already stores dense nodes as three parallel packed
//! delta-encoded arrays (IDs, lats, lons). This module decodes them into
//! contiguous `Vec<i64>` / `Vec<i32>` slices that classification closures
//! can operate on directly, enabling autovectorization and better cache
//! utilization vs element-by-element iteration.

use super::wire::{PackedSint64Iter, WireDenseNodes};

/// Decoded dense node columns — contiguous arrays of IDs and coordinates.
///
/// All three arrays have the same length (`count`). Coordinates are in
/// decimicrodegrees (10^-7 degrees), matching `DenseNode::decimicro_lat/lon`.
pub(crate) struct DenseNodeColumns {
    /// Absolute node IDs (delta-decoded).
    pub ids: Vec<i64>,
    /// Latitude in decimicrodegrees.
    pub lats: Vec<i32>,
    /// Longitude in decimicrodegrees.
    pub lons: Vec<i32>,
}

impl DenseNodeColumns {
    /// Create empty columns.
    pub fn new() -> Self {
        Self {
            ids: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
        }
    }

    /// Number of nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Clear all columns for reuse.
    pub fn clear(&mut self) {
        self.ids.clear();
        self.lats.clear();
        self.lons.clear();
    }

    /// Batch-decode dense nodes from wire format, appending to columnar arrays.
    ///
    /// Delta-decodes IDs, lats, lons and converts coordinates to
    /// decimicrodegrees using the block's granularity and offsets.
    ///
    /// **Does not clear** — call `clear()` before the first group if needed.
    /// This allows multiple dense groups in a single block to be appended.
    #[allow(clippy::cast_possible_truncation)]
    pub fn decode_append(
        &mut self,
        dense: &WireDenseNodes<'_>,
        granularity: i32,
        lat_offset: i64,
        lon_offset: i64,
    ) {
        let gran = i64::from(granularity);

        // Decode IDs (delta-encoded sint64).
        let mut cumulative_id: i64 = 0;
        for delta in PackedSint64Iter::new(dense.id_data) {
            cumulative_id += delta;
            self.ids.push(cumulative_id);
        }

        // Fast path for standard granularity (100, the default for nearly all
        // PBFs): eliminates the per-node multiply + divide entirely.
        // nano = offset + 100 * cumulative → decimicro = nano / 100
        //      = cumulative + offset / 100
        if granularity == 100 && lat_offset % 100 == 0 && lon_offset % 100 == 0 {
            #[allow(clippy::cast_possible_wrap)]
            let lat_off_e7 = (lat_offset / 100) as i32;
            #[allow(clippy::cast_possible_wrap)]
            let lon_off_e7 = (lon_offset / 100) as i32;

            let mut cumulative_lat: i64 = 0;
            for delta in PackedSint64Iter::new(dense.lat_data) {
                cumulative_lat += delta;
                self.lats.push(cumulative_lat as i32 + lat_off_e7);
            }

            let mut cumulative_lon: i64 = 0;
            for delta in PackedSint64Iter::new(dense.lon_data) {
                cumulative_lon += delta;
                self.lons.push(cumulative_lon as i32 + lon_off_e7);
            }
        } else {
            // General path for non-standard granularity.
            let mut cumulative_lat: i64 = 0;
            for delta in PackedSint64Iter::new(dense.lat_data) {
                cumulative_lat += delta;
                let nano = lat_offset + gran * cumulative_lat;
                self.lats.push((nano / 100) as i32);
            }

            let mut cumulative_lon: i64 = 0;
            for delta in PackedSint64Iter::new(dense.lon_data) {
                cumulative_lon += delta;
                let nano = lon_offset + gran * cumulative_lon;
                self.lons.push((nano / 100) as i32);
            }
        }
    }

    /// Classify nodes against N bounding boxes in a single pass, collecting
    /// matching IDs into per-region output Vecs.
    ///
    /// Each bbox is `(min_lat, max_lat, min_lon, max_lon)` in decimicrodegrees.
    /// `out` must have length >= `bboxes.len()`. Single pass over the lat/lon
    /// arrays with N bbox tests per node.
    #[inline]
    pub fn collect_matching_ids_multi_bbox(
        &self,
        bboxes: &[(i32, i32, i32, i32)],
        out: &mut [Vec<i64>],
    ) {
        let n = self.len();
        let lats = &self.lats;
        let lons = &self.lons;
        let ids = &self.ids;

        for i in 0..n {
            let lat = lats[i];
            let lon = lons[i];
            let id = ids[i];
            for (j, &(min_lat, max_lat, min_lon, max_lon)) in bboxes.iter().enumerate() {
                let hit = (lat >= min_lat) as u8
                        & (lat <= max_lat) as u8
                        & (lon >= min_lon) as u8
                        & (lon <= max_lon) as u8;
                if hit != 0 {
                    out[j].push(id);
                }
            }
        }
    }

    /// Classify nodes against a bounding box, collecting matching IDs
    /// into a caller-provided Vec (scratch reuse).
    ///
    /// Uses branchless bitwise AND for the 4 comparisons to enable
    /// autovectorization. The comparison loop is a pure function over
    /// contiguous i32 arrays — no data-dependent branches.
    #[inline]
    pub fn collect_matching_ids_bbox(
        &self,
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
        out: &mut Vec<i64>,
    ) {
        let n = self.len();
        let lats = &self.lats;
        let lons = &self.lons;
        let ids = &self.ids;

        for i in 0..n {
            // Branchless: bitwise AND instead of short-circuit &&.
            // All four comparisons execute unconditionally — enables
            // autovectorization (no data-dependent branches).
            let hit = (lats[i] >= min_lat) as u8
                    & (lats[i] <= max_lat) as u8
                    & (lons[i] >= min_lon) as u8
                    & (lons[i] <= max_lon) as u8;
            if hit != 0 {
                out.push(ids[i]);
            }
        }
    }
}
