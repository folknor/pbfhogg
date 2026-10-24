//! Elapsed-time helper shared by the metrics counters.

/// Nanoseconds since `start`, saturating at `u64::MAX`.
#[inline]
pub(crate) fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
