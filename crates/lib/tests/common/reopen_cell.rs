//! Admission for the replacement matrix's two distinct reopen strategies.
//!
//! Automatic reopening is a procfs cell, not another path-fallback cell.
//! Off Unix there is no identity verification to assert after replacement.
//! Decide before arming a replacement or running a command, so an unavailable
//! strategy cannot execute a duplicate or leave an outcome unchecked.

#[must_use]
pub fn available(force_path: bool, procfs: bool) -> bool {
    if !cfg!(unix) {
        crate::common::skip(
            "input replacement cell: Unix file identity verification is unavailable",
        );
        return false;
    }
    if force_path {
        assert!(!procfs, "forced path reopening must not select procfs");
        return true;
    }
    if !procfs {
        crate::common::skip(
            "input replacement procfs cell: automatic reopening selected the already-covered path fallback",
        );
        return false;
    }
    true
}
