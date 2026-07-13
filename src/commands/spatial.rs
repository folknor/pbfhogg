//! Spatial primitives shared by commands.

use super::extract::Bbox;

/// Integer bounding box in decimicrodegrees.
pub(crate) struct BboxInt {
    pub(crate) min_lon: i32,
    pub(crate) min_lat: i32,
    pub(crate) max_lon: i32,
    pub(crate) max_lat: i32,
}

impl BboxInt {
    /// Convert a degree bounding box to conservative integer bounds.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn from_bbox(bbox: &Bbox) -> Self {
        Self {
            min_lon: (bbox.min_lon * 1e7).floor() as i32,
            min_lat: (bbox.min_lat * 1e7).floor() as i32,
            max_lon: (bbox.max_lon * 1e7).ceil() as i32,
            max_lat: (bbox.max_lat * 1e7).ceil() as i32,
        }
    }

    /// Return whether a decimicrodegree point is inside the box.
    pub(crate) fn contains(&self, lat: i32, lon: i32) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}
