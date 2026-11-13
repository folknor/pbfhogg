//! Driver policy bounds, separate from protocol format limits.
use std::time::Duration;
/// Describe must finish before a worker scenario can be selected.
pub(crate) const DESCRIBE_DEADLINE: Duration = Duration::from_secs(60);
/// Bound repeated adoption, signalling and reaping, even for hostile workers.
pub(crate) const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
/// Cap readiness waits so every capture and adoption check gets a turn.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Retain diagnostics without allowing a producer to exhaust driver memory.
pub(crate) const CAPTURE_LIMIT: usize = 1024 * 1024;
/// Bound each drain turn so a continuously writing child cannot starve reaping.
pub(crate) const DRAIN_TURN: usize = 64 * 1024;

/// Stack buffer for nonblocking pipe reads.
pub(crate) const DRAIN_BUFFER: usize = 8192;

/// Lifecycle test's fail-stop bound, independent of the worker deadlines.
#[cfg(test)]
pub(crate) const TEST_HANG_BOUND: Duration = Duration::from_secs(15);
/// Give the watchdog a short opportunity to report before aborting.
#[cfg(test)]
pub(crate) const TEST_REPORT_GRACE: Duration = Duration::from_millis(100);
/// Tests of forced termination deliberately park until the deadline kills them.
#[cfg(test)]
pub(crate) const TEST_KILL_DEADLINE: Duration = Duration::from_millis(200);
/// Ordinary test fixtures have ample time to start on a loaded host.
#[cfg(test)]
pub(crate) const TEST_WORKER_DEADLINE: Duration = Duration::from_secs(3);

/// Bounded stream diagnostics shown before a count of remaining violations.
pub(crate) const VIOLATIONS_SHOWN: usize = 5;
/// Harness FIFO line protocol acceptance limit.
pub(crate) const FIFO_LINE_LIMIT: usize = 1024;
/// Streaming identity read buffer.
pub(crate) const DIGEST_BUFFER: usize = 64 * 1024;

/// Permit clock-origin estimation error, never a different process clock.
pub(crate) const CLOCK_TOLERANCE: Duration = Duration::from_secs(1);
