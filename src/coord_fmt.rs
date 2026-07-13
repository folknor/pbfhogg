//! Shared coordinate conversion and formatting.

/// Convert decimicrodegrees to degrees.
pub(crate) fn from_decimicro(d: i32) -> f64 {
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
    fn trims_trailing_zeros() {
        let mut buf = String::new();
        format_coord(&mut buf, 12.5);
        assert_eq!(buf, "12.5");
        format_coord(&mut buf, -0.123_456_7);
        assert_eq!(buf, "-0.1234567");
    }
}
