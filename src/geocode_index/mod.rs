//! Reverse geocoding index: binary format, reader, and builder.
//!
//! The index is a set of flat binary files optimized for mmap + binary search
//! queries. See `REVERSE_GEOCODING_SPEC.md` for the full format specification.
//!
//! # Feature gates
//!
//! - `geocode-reader`: enables [`Reader`] and its S2 dependency.
//! - `commands`: enables the builder (implies `geocode-reader`).

pub mod format;

#[cfg(feature = "geocode-reader")]
pub mod reader;
