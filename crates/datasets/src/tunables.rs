//! Transfer bounds for dataset downloads.

/// curl flags that bound a transfer without capping its length. The tool
/// runs under brokkr's machine lock, so a stalled transfer would hold it
/// indefinitely; a hard `--max-time` is wrong because a planet PBF
/// legitimately takes hours. Give up on a connection not opened in 30 s, or
/// on a transfer that averaged under 1 KiB/s for two minutes.
pub(crate) const CURL_STALL_ARGS: [&str; 6] = [
    "--connect-timeout",
    "30",
    "--speed-limit",
    "1024",
    "--speed-time",
    "120",
];

/// Retry transient connection and server errors. `--retry-max-time` bounds
/// the retry window, not a transfer that is making progress.
pub(crate) const CURL_RETRY_ARGS: [&str; 6] = [
    "--retry",
    "3",
    "--retry-delay",
    "1",
    "--retry-max-time",
    "30",
];

/// HEAD has no body and can use a hard total deadline, unlike transfers.
pub(crate) const CURL_HEAD_ARGS: [&str; 4] = ["--connect-timeout", "30", "--max-time", "60"];
