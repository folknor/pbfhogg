//! Formatting helpers for the inspect report output.

#[allow(clippy::cast_precision_loss)]
pub(super) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn format_number(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

pub(super) fn format_number_signed(n: i64) -> String {
    if n < 0 {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let abs = (n as i128).unsigned_abs() as u64;
        format!("-{}", format_number(abs))
    } else {
        #[allow(clippy::cast_sign_loss)]
        format_number(n as u64)
    }
}

/// Format a millisecond epoch timestamp as ISO-8601 UTC.
pub(super) fn format_timestamp(millis: i64) -> String {
    let secs = millis / 1000;
    // Use manual formatting: seconds since epoch → date/time components
    // This avoids a chrono dependency for a single formatting call.
    const SECS_PER_DAY: i64 = 86400;
    let days = secs.div_euclid(SECS_PER_DAY);
    let day_secs = secs.rem_euclid(SECS_PER_DAY);
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
/// Algorithm from Howard Hinnant's `civil_from_days`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d)
}

pub(super) fn yes_no(b: bool) -> String {
    if b { "yes".to_string() } else { "no".to_string() }
}
