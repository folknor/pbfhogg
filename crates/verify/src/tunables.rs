//! Sizes and bounds of the verifier, with why each has its value.

use std::time::Duration;

/// Bytes of a child's stderr kept for the report. A child that writes more
/// is a capture failure, never a silently cut report.
pub(crate) const STDERR_LIMIT: usize = 1 << 20;

/// Read buffer for draining a child's stderr pipe.
pub(crate) const DRAIN_BUFFER: usize = 64 * 1024;

/// How long killing and reaping a process tree may take before the verifier
/// gives up on establishing quiescence. Generous: a killed process only has
/// to be torn down by the kernel.
pub(crate) const SETTLE_DEADLINE: Duration = Duration::from_secs(30);

/// The longest single poll sleep while settling; readiness wakes it sooner.
pub(crate) const SETTLE_POLL: Duration = Duration::from_millis(50);

/// Lines and bytes per line a listing preview replays into check detail.
pub(crate) const PREVIEW_LINES: usize = 50;
pub(crate) const PREVIEW_LINE_BYTES: usize = 240;

/// Read buffer for hashing the executable under test.
pub(crate) const DIGEST_BUFFER: usize = 1 << 20;

/// Fail-stop bound for lifecycle tests, below brokkr's per-test watchdog.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "used by the separately compiled lifecycle integration test"
)]
pub(crate) const TEST_HANG_BOUND: Duration = Duration::from_secs(15);
/// Allow the watchdog to report before aborting a stuck test.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "used by the separately compiled lifecycle integration test"
)]
pub(crate) const TEST_REPORT_GRACE: Duration = Duration::from_millis(100);

/// Keep up to 20 GiB of completed reference artifacts by default.
pub(crate) const REFERENCE_BUDGET: u64 = 20 * 1024 * 1024 * 1024;

/// Records buffered by the bounded external sorter before spilling.
pub(crate) const SORT_CHUNK: usize = 1 << 20;
/// Maximum run files opened by one merge pass.
pub(crate) const SORT_FAN_IN: usize = 32;
/// Smallest distinct IDs shown per element-difference class and kind.
pub(crate) const MAX_REPORTED_IDS: usize = 10;

/// Streaming XML chunks, large enough to amortize reads without retaining a document.
pub(crate) const OSC_READ_BUFFER: usize = 16 * 1024;
/// Unicode characters retained when showing a change-listing mismatch.
pub(crate) const CHANGE_PREVIEW_CHARS: usize = 400;

/// Adjacent strips in the multi-extract check, matching the legacy verifier.
pub(crate) const MULTI_EXTRACT_REGIONS: usize = 5;

/// Maximum detailed text-diff mismatches retained before counting only.
pub(crate) const DIFF_SHOWN_MISMATCHES: usize = 20;
