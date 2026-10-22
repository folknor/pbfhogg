//! Spatial primitives shared by commands.

#[cfg(test)]
use crate::geo::Bbox;

#[cfg(test)]
use crate::geo::to_degrees;

// Keep the command-facing path while geometry owns integer bounds and the exact
// forward-conversion rounding policy.
pub(crate) use crate::geo::BboxInt;

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
