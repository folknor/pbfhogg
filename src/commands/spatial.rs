//! Spatial primitives shared by commands.

use super::extract::Bbox;

use crate::coord_fmt::{deg_to_e7, e7_to_deg};

// Spatial membership deliberately uses multiplication, matching reader geometry.
// The text/OSC conversion divides for byte compatibility; sharing that function
// would change which boundary nodes extraction includes.
fn to_degrees(value: i32) -> f64 {
    e7_to_deg(f64::from(value))
}

/// Smallest decimicrodegree value whose degree form is `>= limit`.
///
/// The guess comes from scaling, then walks until the *forward* conversion
/// satisfies the comparison in both directions, so the result is exact
/// regardless of which way the two conversions round.
#[allow(clippy::cast_possible_truncation)]
fn ceil_e7(limit: f64) -> i32 {
    let mut v = deg_to_e7(limit).ceil().clamp(-2.1e9, 2.1e9) as i32;
    while to_degrees(v) < limit {
        match v.checked_add(1) {
            Some(next) => v = next,
            None => break,
        }
    }
    while let Some(prev) = v.checked_sub(1) {
        if to_degrees(prev) >= limit {
            v = prev;
        } else {
            break;
        }
    }
    v
}

/// Largest decimicrodegree value whose degree form is `<= limit`.
#[allow(clippy::cast_possible_truncation)]
fn floor_e7(limit: f64) -> i32 {
    let mut v = deg_to_e7(limit).floor().clamp(-2.1e9, 2.1e9) as i32;
    while to_degrees(v) > limit {
        match v.checked_sub(1) {
            Some(prev) => v = prev,
            None => break,
        }
    }
    while let Some(next) = v.checked_add(1) {
        if to_degrees(next) <= limit {
            v = next;
        } else {
            break;
        }
    }
    v
}

/// Integer bounding box in decimicrodegrees.
pub(crate) struct BboxInt {
    pub(crate) min_lon: i32,
    pub(crate) min_lat: i32,
    pub(crate) max_lon: i32,
    pub(crate) max_lat: i32,
}

impl BboxInt {
    /// Conservative integer bounds: a strict superset of [`Self::inside_bbox`],
    /// widened by one decimicrodegree on each side. Use for blob-level
    /// candidate selection and as a polygon prefilter, where a false positive
    /// costs work and a false negative loses data. Never for point membership.
    pub(crate) fn from_bbox(bbox: &Bbox) -> Self {
        let inside = Self::inside_bbox(bbox);
        Self {
            min_lon: inside.min_lon.saturating_sub(1),
            min_lat: inside.min_lat.saturating_sub(1),
            max_lon: inside.max_lon.saturating_add(1),
            max_lat: inside.max_lat.saturating_add(1),
        }
    }

    /// Exact integer bounds: `contains` on this box agrees, point for point,
    /// with `bbox.contains(lat as f64 * 1e-7, lon as f64 * 1e-7)`. Use for
    /// point membership, columnar classification, and for proving that a raw
    /// blob can be passed through untouched.
    pub(crate) fn inside_bbox(bbox: &Bbox) -> Self {
        Self {
            min_lon: ceil_e7(bbox.min_lon),
            min_lat: ceil_e7(bbox.min_lat),
            max_lon: floor_e7(bbox.max_lon),
            max_lat: floor_e7(bbox.max_lat),
        }
    }

    /// Return whether a decimicrodegree point is inside the box.
    pub(crate) fn contains(&self, lat: i32, lon: i32) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reviewer's worked example. `55.6 / 1e-7` is exactly `556_000_000`
    /// on both `ceil` and `floor`, so a division-derived lower bound admits
    /// `556_000_000` - whose forward conversion is strictly below `55.6`.
    /// A test at a boundary where the two conversions agree (56.0, 12.0)
    /// passes either way and proves nothing, so the fixture must use a
    /// boundary where they genuinely disagree.
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn boundary_where_the_two_conversions_disagree() {
        assert_eq!((55.6_f64 / 1e-7).ceil() as i32, 556_000_000);
        assert!(
            to_degrees(556_000_000) < 55.6,
            "fixture is only meaningful while this conversion asymmetry holds"
        );

        let bbox = Bbox {
            min_lon: 12.0,
            min_lat: 55.6,
            max_lon: 12.7,
            max_lat: 55.8,
        };
        let inside = BboxInt::inside_bbox(&bbox);
        assert_eq!(
            inside.min_lat, 556_000_001,
            "lower bound must be the first value whose forward conversion is inside"
        );
        assert!(!inside.contains(556_000_000, 120_000_000));
        assert!(inside.contains(556_000_001, 120_000_000));
    }

    /// The exact box must agree with the f64 comparison every consumer runs,
    /// at every boundary, in both directions.
    #[test]
    fn inside_bbox_agrees_with_the_f64_comparison() {
        for &(min_lon, min_lat, max_lon, max_lat) in &[
            (12.0_f64, 55.6_f64, 12.7_f64, 55.8_f64),
            (-0.1, -0.1, 0.1, 0.1),
            (8.7, 1.1, 8.9, 1.3),
            (-180.0, -90.0, 180.0, 90.0),
            (2.3, 48.85, 2.31, 48.86),
        ] {
            let bbox = Bbox {
                min_lon,
                min_lat,
                max_lon,
                max_lat,
            };
            let inside = BboxInt::inside_bbox(&bbox);
            let wide = BboxInt::from_bbox(&bbox);
            for edge in [
                inside.min_lat,
                inside.max_lat,
                inside.min_lon,
                inside.max_lon,
            ] {
                for delta in -2_i32..=2 {
                    let v = edge.saturating_add(delta);
                    for (lat, lon) in [(v, inside.min_lon), (inside.min_lat, v)] {
                        let exact = bbox.contains(to_degrees(lat), to_degrees(lon));
                        assert_eq!(
                            inside.contains(lat, lon),
                            exact,
                            "exact box disagrees at ({lat}, {lon}) for {min_lon},{min_lat},{max_lon},{max_lat}"
                        );
                        assert!(
                            !exact || wide.contains(lat, lon),
                            "conservative box lost a real point at ({lat}, {lon})"
                        );
                    }
                }
            }
        }
    }
}
