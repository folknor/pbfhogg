//! The single coordinate slot encoding used by every flat, rank-indexed
//! coordinate store in the crate.
//!
//! A slot is one 64-bit word. The low word is the latitude in
//! decimicrodegrees biased by 90 degrees, which puts every legal latitude in
//! `0 ..= 1.8e9` and therefore leaves bit 31 free; that bit is the explicit
//! presence flag. The high word is the longitude verbatim, reinterpreted as
//! `u32`.
//!
//! The bias exists so that absence has its own representation. Spelling
//! absence as all-zero makes a node at Null Island - a real place off the
//! African coast, and one that broken edits, GPS-zero errors and test
//! fixtures keep producing - indistinguishable from a slot that was never
//! written. Every store that zero-fills on creation needs a flag bit, and
//! biasing the latitude buys one for free: no extra memory, no extra pass.
//!
//! This module exists because the encoding was previously written out twice,
//! once in the ALTW sparse store and once in the geocode builder's way
//! coordinate collection. Two verbatim copies of a bit layout are two things
//! that can drift, and a drift here corrupts coordinates silently. Callers
//! use [`pack`] and [`unpack`]; nobody re-derives the layout.

/// Latitude bias, in decimicrodegrees: 90 degrees.
pub(crate) const LAT_BIAS: i64 = 900_000_000;
/// Presence flag. Set on every written slot; unreachable by any biased
/// latitude, since the biased range tops out at `1.8e9 < 2^31`.
pub(crate) const PRESENT: u64 = 1 << 31;
/// Longitude bound in decimicrodegrees: 180 degrees.
pub(crate) const LON_LIMIT: i64 = 1_800_000_000;

/// Whether an integer coordinate is in the geographic domain.
pub(crate) fn in_domain(lat: i32, lon: i32) -> bool {
    (-LAT_BIAS..=LAT_BIAS).contains(&i64::from(lat))
        && (-LON_LIMIT..=LON_LIMIT).contains(&i64::from(lon))
}

/// A coordinate outside the geographic range. A neutral leaf: the caller
/// classifies it, because the same bad number is an invalid argument for a
/// bounding box and invalid data in an OSC file or a GeoJSON polygon.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CoordRangeError {
    name: String,
    value: f64,
    limit: f64,
}

impl std::fmt::Display for CoordRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}) must be within -{} and {} degrees",
            self.name, self.value, self.limit, self.limit
        )
    }
}

impl std::error::Error for CoordRangeError {}

/// Validate a named latitude in degrees, retaining the caller's field name.
pub(crate) fn validate_lat(name: &str, value: f64) -> Result<(), CoordRangeError> {
    validate_degrees(name, value, LAT_BIAS)
}

/// Validate a named longitude in degrees, retaining the caller's field name.
pub(crate) fn validate_lon(name: &str, value: f64) -> Result<(), CoordRangeError> {
    validate_degrees(name, value, LON_LIMIT)
}

fn validate_degrees(name: &str, value: f64, limit_e7: i64) -> Result<(), CoordRangeError> {
    let limit = crate::coord_fmt::e7_to_deg(limit_e7 as f64);
    if !value.is_finite() || !(-limit..=limit).contains(&value) {
        return Err(CoordRangeError {
            name: name.to_owned(),
            value,
            limit,
        });
    }
    Ok(())
}

/// Pack a geographic coordinate into one slot word, with the presence bit
/// set. Returns `None` when the coordinate is outside the geographic domain
/// and therefore not representable - the caller must refuse rather than
/// store a value that would decode as some other place.
///
/// The range check is deliberately geographic, not merely representational:
/// every `(i32, i32)` pair is representable in 64 bits, but only latitudes
/// within +/-90 and longitudes within +/-180 are coordinates.
pub(crate) fn pack(lat: i32, lon: i32) -> Option<u64> {
    if !in_domain(lat, lon) {
        return None;
    }
    let lat = i64::from(lat);
    let lon = i64::from(lon);
    #[allow(clippy::cast_sign_loss)]
    let biased_lat = (lat + LAT_BIAS) as u64;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let lon_bits = u64::from(lon as i32 as u32);
    Some(biased_lat | PRESENT | (lon_bits << 32))
}

/// Inverse of [`pack`]. `None` means the slot was never written.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn unpack(packed: u64) -> Option<(i32, i32)> {
    if packed & PRESENT == 0 {
        return None;
    }
    let lat = ((packed & 0x7fff_ffff) as i64 - LAT_BIAS) as i32;
    let lon = (packed >> 32) as u32 as i32;
    Some((lat, lon))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{LAT_BIAS, PRESENT, pack, unpack};

    /// The slot encoding is asserted on its bits, not on a round trip
    /// through itself: a `pack` / `unpack` pair can agree with each other
    /// while both being wrong about the layout everything else depends on.
    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn packed_layout_is_biased_latitude_presence_bit_and_raw_longitude() {
        let packed = pack(550_000_000, -120_000_000).unwrap();
        assert_ne!(packed & PRESENT, 0, "a written slot carries the flag bit");
        assert_eq!(
            (packed & 0x7fff_ffff) as i64 - LAT_BIAS,
            550_000_000,
            "the low word is the latitude biased by 90 degrees"
        );
        assert_eq!(
            (packed >> 32) as u32,
            (-120_000_000_i32) as u32,
            "the high word is the longitude verbatim"
        );
    }

    /// The whole point of the flag bit: Null Island is a real place, so a
    /// zero coordinate must decode as present while a zero *slot* - what
    /// `set_len` and a fresh mmap leave behind - decodes as absent.
    #[test]
    fn null_island_is_present_and_an_untouched_slot_is_absent() {
        assert_eq!(unpack(0), None, "an untouched slot is absent");
        let packed = pack(0, 0).unwrap();
        assert_ne!(packed, 0, "a node at Null Island must not encode as zero");
        assert_eq!(unpack(packed), Some((0, 0)));
    }

    /// Every legal coordinate survives exactly, including both extremes of
    /// each axis, where the bias is tightest against the flag bit.
    #[test]
    fn every_legal_coordinate_survives_exactly() {
        for lat in [-900_000_000, -1, 0, 1, 900_000_000] {
            for lon in [-1_800_000_000, -1, 0, 1, 1_800_000_000] {
                let packed = pack(lat, lon).unwrap_or_else(|| {
                    panic!("({lat}, {lon}) is a legal coordinate and must pack")
                });
                assert_eq!(unpack(packed), Some((lat, lon)), "({lat}, {lon})");
            }
        }
    }

    /// No legal coordinate can alias the presence bit or another coordinate:
    /// the biased latitude must stay strictly below the flag bit across the
    /// whole domain, which is the property the free flag bit rests on.
    #[test]
    #[allow(clippy::cast_sign_loss)]
    fn no_legal_latitude_reaches_the_presence_bit() {
        for lat in [-LAT_BIAS, 0, LAT_BIAS] {
            let biased = (lat + LAT_BIAS) as u64;
            assert!(
                biased < PRESENT,
                "biased latitude {biased} must stay below the flag bit {PRESENT}"
            );
        }
    }

    /// Out-of-domain values are refused rather than stored as some other
    /// place. Latitude and longitude have different domains and are checked
    /// separately - a longitude of 100 degrees is fine, a latitude of 100
    /// degrees is not.
    #[test]
    fn out_of_domain_coordinates_are_refused() {
        assert!(pack(900_000_001, 0).is_none(), "latitude past 90 degrees");
        assert!(pack(-900_000_001, 0).is_none(), "latitude past -90 degrees");
        assert!(pack(0, 1_800_000_001).is_none(), "longitude past 180");
        assert!(pack(0, -1_800_000_001).is_none(), "longitude past -180");
        assert!(
            pack(1_000_000_000, 0).is_none(),
            "a latitude of 100 degrees is not a latitude, even though the \
             same number is a legal longitude"
        );
        assert!(pack(0, 1_000_000_000).is_some(), "100 degrees east is real");
    }
}
