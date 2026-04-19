//! `check` command: integrity checks over a PBF file.
//!
//! Two modes selected by CLI flags:
//! - `--refs`: referential integrity (way refs, relation members) via [`refs::check_refs`].
//! - `--ids --full`: ID uniqueness and ordering via [`verify_ids::verify_ids`].

pub mod refs;
pub mod verify_ids;
