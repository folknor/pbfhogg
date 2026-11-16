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

/// IDs per kind the field-policies slice takes from each kind's first data
/// blob: enough for a tagged object, a way and a relation with members to be
/// among them on real data, few enough that the slice stays small.
pub(crate) const FIELD_POLICY_IDS: usize = 64;

/// Adjacent strips in the multi-extract check, matching the legacy verifier.
pub(crate) const MULTI_EXTRACT_REGIONS: usize = 5;

/// Maximum detailed text-diff mismatches retained before counting only.
pub(crate) const DIFF_SHOWN_MISMATCHES: usize = 20;

/// Geocode check: the fine and coarse search radii (meters) the index is
/// built with, passed explicitly so the model does not depend on the CLI's
/// defaults. Both are exact in `f32`, as the index header stores them.
pub(crate) const GEOCODE_FINE_RADIUS_M: f64 = 75.0;
pub(crate) const GEOCODE_COARSE_RADIUS_M: f64 = 1000.0;
/// Geocode check: queries drawn uniformly over the data's extent.
pub(crate) const GEOCODE_UNIFORM_QUERIES: usize = 1000;
/// Geocode check: witness queries kept per class and distance category
/// (within the fine radius, beyond it but within the coarse one, beyond both).
pub(crate) const GEOCODE_WITNESS_QUOTA: usize = 150;
/// Geocode check: candidate witness points drawn before the quotas are given up.
pub(crate) const GEOCODE_WITNESS_ATTEMPTS: usize = 40_000;
/// Geocode check: farthest a witness point is placed from its anchor (meters).
pub(crate) const GEOCODE_WITNESS_REACH_M: f64 = 1300.0;
/// Geocode check: distance agreement (meters) between the model and the
/// reader, and the band within which two candidates tie or a nearest
/// candidate sits on the coarse radius (and is not judged).
pub(crate) const GEOCODE_DISTANCE_TOLERANCE_M: f64 = 1e-4;
/// Geocode check: the model's grid cell, in decimicrodegrees.
pub(crate) const GEOCODE_GRID_E7: i32 = 100_000;
/// Geocode check: the seed of the query draw, fixed so a run is replayable.
pub(crate) const GEOCODE_SEED: u64 = 0x6765_6f63_6f64_6531;
/// Geocode check: the non-zero `--admin-tolerance` (meters) of the main
/// build, passed explicitly; a second build runs at 0. The model leaves a
/// query within this (plus 1 percent and 1 m) of a boundary unjudged.
pub(crate) const GEOCODE_ADMIN_TOLERANCE_M: f64 = 20.0;
/// Geocode check: the band (meters) round a boundary left unjudged at
/// `--admin-tolerance 0`, for the float crossing tests alone.
pub(crate) const GEOCODE_ADMIN_EXACT_BAND_M: f64 = 1e-3;
/// Geocode check: admin witness queries drawn near boundary vertices.
pub(crate) const GEOCODE_ADMIN_WITNESSES: usize = 600;
/// Geocode check: farthest an admin witness is placed from its vertex (meters).
pub(crate) const GEOCODE_ADMIN_WITNESS_REACH_M: f64 = 300.0;
