//! Resource policy for the development tools.

/// Bounded streaming buffer for dataset hashing; independent of file size.
pub(crate) const DIGEST_BUFFER_BYTES: usize = 256 * 1024;
