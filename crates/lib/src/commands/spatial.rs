//! Spatial primitives shared by commands.

#[cfg(test)]
use crate::geo::Bbox;

// Keep the command-facing path while geometry owns integer bounds and the
// decimicrodegree quantization rule.
pub(crate) use crate::geo::BboxInt;

#[cfg(test)]
mod tests {
    use super::*;

    /// `55.6 / 1e-7` is exactly `556_000_000`, while `556_000_000 * 1e-7`
    /// lands strictly below `55.6`. A membership rule decided by either f64
    /// product puts the node sitting on the typed 55.6 edge on one side or
    /// the other by float luck; the quantized rule includes it, as osmium
    /// does, because both the edge and the point name column 556000000.
    #[test]
    fn a_node_on_a_typed_decimal_edge_is_inside() {
        assert!(crate::coord_fmt::e7_to_deg(556_000_000.0) < 55.6);
        let bbox = Bbox {
            min_lon: 12.0,
            min_lat: 55.6,
            max_lon: 12.7,
            max_lat: 55.8,
        };
        let inside = BboxInt::inside_bbox(&bbox);
        assert_eq!(inside.min_lat, 556_000_000);
        assert!(inside.contains(556_000_000, 120_000_000));
        assert!(!inside.contains(555_999_999, 120_000_000));
    }

    /// The exact box agrees with the f64 point test at every edge, in both
    /// directions, and the conservative box never loses a point either
    /// accepts.
    #[test]
    fn inside_bbox_agrees_with_the_point_test() {
        for &(min_lon, min_lat, max_lon, max_lat) in &[
            (12.0_f64, 55.6_f64, 12.7_f64, 55.8_f64),
            (-0.1, -0.1, 0.1, 0.1),
            (8.7, 1.1, 8.9, 1.3),
            (-180.0, -90.0, 180.0, 90.0),
            (2.3, 48.85, 2.31, 48.86),
            (1.000_000_05, -1.000_000_05, 1.000_001_15, 1.000_000_15),
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
                        let point = bbox.contains(
                            crate::coord_fmt::e7_to_deg(f64::from(lat)),
                            crate::coord_fmt::e7_to_deg(f64::from(lon)),
                        );
                        assert_eq!(
                            inside.contains(lat, lon),
                            point,
                            "exact box disagrees at ({lat}, {lon}) for {min_lon},{min_lat},{max_lon},{max_lat}"
                        );
                        assert!(
                            !point || wide.contains(lat, lon),
                            "conservative box lost a real point at ({lat}, {lon})"
                        );
                    }
                }
            }
        }
    }
}
