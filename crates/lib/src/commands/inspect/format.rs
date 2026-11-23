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

/// Format a millisecond epoch timestamp of the census range. The census
/// judges timestamps in milliseconds, so the range keeps them: a sub-second
/// instant prints the second holding it, rounded down like every other
/// timestamp output, then its exact `.SSS` milliseconds. The report is descriptive, not read back, so a year
/// outside 0000 to 9999 prints in the formatter's expanded spelling instead
/// of refusing the file.
pub(super) fn format_timestamp(millis: i64) -> String {
    crate::commands::format_epoch_millis(millis)
}

pub(super) fn yes_no(b: bool) -> String {
    if b {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}
