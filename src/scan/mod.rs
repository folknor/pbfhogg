//! Lightweight wire-format scanners that bypass full PrimitiveBlock construction.
//!
//! Used by performance-critical paths that need only a subset of element data
//! (e.g. ID + coordinate tuples for nodes, or ID + ref list for ways) and
//! cannot afford the string-table parsing and group allocation overhead of
//! the standard read pipeline.

pub(crate) mod node;
pub(crate) mod way;
