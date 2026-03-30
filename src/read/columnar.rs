//! Columnar dense node decode — batch decode IDs, lats, lons into contiguous
//! arrays for cache-friendly classification passes.
//!
//! The PBF wire format already stores dense nodes as three parallel packed
//! delta-encoded arrays (IDs, lats, lons). This module decodes them into
//! contiguous `Vec<i64>` / `Vec<i32>` slices that classification closures
//! can operate on directly, enabling autovectorization and better cache
//! utilization vs element-by-element iteration.

use super::wire::{Cursor, PackedSint64Iter, WireDenseNodes};

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

    /// Batch-decode dense nodes from wire format into columnar arrays.
    ///
    /// Delta-decodes IDs, lats, lons and converts coordinates to
    /// decimicrodegrees using the block's granularity and offsets.
    ///
    /// The columns are cleared before decoding. Capacities are retained
    /// across calls for scratch reuse.
    #[allow(clippy::cast_possible_truncation)]
    pub fn decode(
        &mut self,
        dense: &WireDenseNodes<'_>,
        granularity: i32,
        lat_offset: i64,
        lon_offset: i64,
    ) {
        self.clear();

        let gran = i64::from(granularity);

        // Decode IDs (delta-encoded sint64).
        let mut cumulative_id: i64 = 0;
        for delta in PackedSint64Iter::new(dense.id_data) {
            cumulative_id += delta;
            self.ids.push(cumulative_id);
        }

        // Decode lats (delta-encoded sint64 → decimicrodegrees i32).
        let mut cumulative_lat: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lat_data) {
            cumulative_lat += delta;
            let nano = lat_offset + gran * cumulative_lat;
            self.lats.push((nano / 100) as i32);
        }

        // Decode lons (delta-encoded sint64 → decimicrodegrees i32).
        let mut cumulative_lon: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lon_data) {
            cumulative_lon += delta;
            let nano = lon_offset + gran * cumulative_lon;
            self.lons.push((nano / 100) as i32);
        }
    }

    /// Classify nodes against a bounding box, returning indices of matching nodes.
    ///
    /// This is the hot inner loop that benefits from columnar layout:
    /// contiguous i32 arrays enable autovectorization of the 4 comparisons.
    #[inline]
    pub fn classify_bbox(
        &self,
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
    ) -> Vec<usize> {
        let mut matches = Vec::new();
        for i in 0..self.len() {
            let lat = self.lats[i];
            let lon = self.lons[i];
            if lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon {
                matches.push(i);
            }
        }
        matches
    }

    /// Classify nodes against a bounding box, collecting matching IDs.
    ///
    /// Convenience method for the common pattern in extract/classify phases.
    #[inline]
    pub fn matching_ids_bbox(
        &self,
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
    ) -> Vec<i64> {
        let mut ids = Vec::new();
        for i in 0..self.len() {
            let lat = self.lats[i];
            let lon = self.lons[i];
            if lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon {
                ids.push(self.ids[i]);
            }
        }
        ids
    }
}
