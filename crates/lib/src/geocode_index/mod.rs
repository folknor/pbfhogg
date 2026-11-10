//! Reverse geocoding index: binary format, reader, and builder.
//!
//! The index is a set of flat binary files optimized for mmap + binary search
//! queries. See the `format` module for the on-disk layout.
//!
//! # Feature gates
//!
//! - `geocode-reader`: enables [`Reader`] and its S2 dependency.
//! - `commands`: enables the builder (implies `geocode-reader`).

pub mod format;

#[cfg(feature = "geocode-reader")]
pub mod reader;

#[cfg(feature = "commands")]
pub mod builder;
