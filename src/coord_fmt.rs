//! Shared coordinate conversion and formatting.

/// Convert a decimicrodegree value to degrees, including fractional averages.
/// Multiplication order is part of the spatial boundary comparison contract:
/// `556_000_000 * 1e-7` is below `55.6`, even though `55.6 / 1e-7`
/// is exactly `556_000_000`. Bounds must check the forward conversion.
pub(crate) fn e7_to_deg(value: f64) -> f64 {
    value * 1e-7
}

/// Scale degrees to decimicrodegrees without choosing integer rounding.
/// This is not an exact floating-point inverse of [`e7_to_deg`]; spatial
/// bounds must correct their rounded guess against the forward conversion.
pub(crate) fn deg_to_e7(value: f64) -> f64 {
    value * 1e7
}

/// Convert an integer decimicrodegree coordinate to degrees.
pub(crate) fn from_decimicro(d: i32) -> f64 {
    // Deliberately divide: multiplication rounds differently for some values.
    // Sharing the spatial conversion would change diff text and OSC output bytes.
    f64::from(d) / 1e7
}

/// Format a coordinate at decimicrodegree precision, trimming trailing zeros.
pub(crate) fn format_coord(buf: &mut String, deg: f64) {
    use std::fmt::Write;

    buf.clear();
    write!(buf, "{deg:.7}").ok();
    let trimmed = buf.trim_end_matches('0').trim_end_matches('.');
    buf.truncate(trimmed.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_conversion_retains_division_rounding() {
        let value = 556_000_000;
        assert_eq!(
            from_decimicro(value).to_bits(),
            (f64::from(value) / 1e7).to_bits()
        );
        assert_ne!(
            from_decimicro(value).to_bits(),
            e7_to_deg(f64::from(value)).to_bits()
        );
    }

    #[test]
    fn trims_trailing_zeros() {
        let mut buf = String::new();
        format_coord(&mut buf, 12.5);
        assert_eq!(buf, "12.5");
        format_coord(&mut buf, -0.123_456_7);
        assert_eq!(buf, "-0.1234567");
    }
}
