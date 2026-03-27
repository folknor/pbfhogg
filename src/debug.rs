#![allow(dead_code)]

use std::fmt;

pub(crate) fn log(args: fmt::Arguments<'_>) {
    eprintln!("{args}");
}

/// Read current RSS in kilobytes from `/proc/self/statm`.
pub(crate) fn read_rss_kb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4)
}

/// Read RSS breakdown (anon vs file) from `/proc/self/status`.
pub(crate) fn read_rss_detail() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut anon_kb: u64 = 0;
    let mut file_kb: u64 = 0;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("RssAnon:") {
            anon_kb = v.trim().strip_suffix(" kB").unwrap_or("0").trim().parse().ok()?;
        }
        if let Some(v) = line.strip_prefix("RssFile:") {
            file_kb = v.trim().strip_suffix(" kB").unwrap_or("0").trim().parse().ok()?;
        }
    }
    Some(format!("anon={}MB file={}MB", anon_kb / 1024, file_kb / 1024))
}

pub(crate) fn rss_line() -> String {
    match (read_rss_kb(), read_rss_detail()) {
        (Some(rss_kb), Some(detail)) => format!("rss={}MB {detail}", rss_kb / 1024),
        (Some(rss_kb), None) => format!("rss={}MB", rss_kb / 1024),
        (None, Some(detail)) => detail,
        (None, None) => String::from("rss=unknown"),
    }
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {{
        #[cfg(feature = "debug-logging")]
        $crate::debug::log(::std::format_args!($($arg)*));
    }};
}
