//! OpenStreetMap change format (OSC).
//!
//! This package unifies the OSC concerns:
//!
//! - [`parse`]: input parser. `.osc.gz` files into [`CompactDiffOverlay`].
//! - [`write`]: XML output writers (owned element types are private impl detail).
//! - [`merge_join`]: streaming merge-join over sorted PBFs, used by `diff`
//!   and `derive_changes` to produce OSC output.
//!
//! The public API is re-exported at this module level so consumers continue to
//! use `pbfhogg::osc::CompactDiffOverlay`, `pbfhogg::osc::parse_osc_file`, etc.

mod compact;
mod interner;
pub mod parse;
pub(crate) mod write;
pub(crate) mod merge_join;

pub use parse::*;
