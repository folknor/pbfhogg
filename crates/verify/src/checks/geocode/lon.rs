//! Longitude on the circle, as the documented geocode rules read it: a
//! difference is taken the shorter way round, and +180 and -180 are one
//! meridian. The model is written against these rules, not against pbfhogg's
//! code, so each is stated here once.

use std::f64::consts::{PI, TAU};

/// One turn of longitude in decimicrodegrees.
pub(super) const TURN_E7: i64 = 3_600_000_000;
/// Half a turn in decimicrodegrees.
pub(super) const HALF_E7: i64 = TURN_E7 / 2;

/// The shorter-way difference of a decimicrodegree longitude difference, in
/// `[-180, 180)` degrees: an exact half turn comes back as `-HALF_E7`.
pub(super) fn wrap_e7(d: i64) -> i64 {
    let r = d.rem_euclid(TURN_E7);
    if r >= HALF_E7 { r - TURN_E7 } else { r }
}

/// A decimicrodegree longitude folded into `(-180, 180]`, the antimeridian
/// spelled +180.
pub(super) fn fold_e7(lon: i64) -> i64 {
    let f = wrap_e7(lon);
    if f == -HALF_E7 { HALF_E7 } else { f }
}

/// The shorter-way difference of a radian longitude difference. A difference
/// already within half a turn is returned unchanged, so data off the seam is
/// measured exactly as by plain subtraction.
pub(super) fn wrap_rad(d: f64) -> f64 {
    if (-PI..=PI).contains(&d) {
        d
    } else {
        (d + PI).rem_euclid(TAU) - PI
    }
}

/// A degree longitude folded into `[-180, 180]` (no spelling is preferred:
/// the query is printed as folded).
pub(super) fn fold_deg(lon: f64) -> f64 {
    if (-180.0..=180.0).contains(&lon) {
        lon
    } else {
        (lon + 180.0).rem_euclid(360.0) - 180.0
    }
}

/// Grid columns `lo..=hi` (cell indices along longitude, any frame) reduced
/// onto a grid of `columns` cells per turn, each at most once.
pub(super) fn columns(lo: i64, hi: i64, columns: i64) -> Vec<i64> {
    if hi - lo + 1 >= columns {
        (0..columns).collect()
    } else {
        (lo..=hi).map(|c| c.rem_euclid(columns)).collect()
    }
}

/// The point `reach` radians from `(lat, lon)` (degrees) along `bearing`,
/// on the sphere, folded: a witness offset stays bounded near a pole.
pub(super) fn destination(lat: f64, lon: f64, reach: f64, bearing: f64) -> (f64, f64) {
    let (phi, lambda) = (lat.to_radians(), lon.to_radians());
    let sin_phi2 = phi.sin() * reach.cos() + phi.cos() * reach.sin() * bearing.cos();
    let phi2 = sin_phi2.clamp(-1.0, 1.0).asin();
    let lambda2 = lambda
        + (bearing.sin() * reach.sin() * phi.cos()).atan2(reach.cos() - phi.sin() * sin_phi2);
    (phi2.to_degrees(), fold_deg(lambda2.to_degrees()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seam_is_one_meridian() {
        assert_eq!(wrap_e7(HALF_E7), -HALF_E7);
        assert_eq!(wrap_e7(-HALF_E7 - 10), HALF_E7 - 10);
        assert_eq!(fold_e7(-HALF_E7), HALF_E7);
        assert_eq!(fold_e7(HALF_E7 + 5), -HALF_E7 + 5);
        assert_eq!(fold_e7(123), 123);
        assert_eq!(columns(-2, 1, 10), [8, 9, 0, 1]);
        assert_eq!(columns(-20, 20, 10).len(), 10);
        let (lat, lon) = destination(
            -17.0,
            179.9999,
            100.0 / 6_371_008.8,
            std::f64::consts::FRAC_PI_2,
        );
        assert!(
            (-17.0001..-16.9999).contains(&lat) && lon < -179.99,
            "{lat} {lon}"
        );
        let (lat, _) = destination(89.9999, 0.0, 1000.0 / 6_371_008.8, 0.0);
        assert!(lat <= 90.0);
    }
}
