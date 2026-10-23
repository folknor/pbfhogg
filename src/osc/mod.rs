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
pub(crate) mod merge_join;
pub mod parse;
pub(crate) mod version_floor;
pub(crate) mod write;
pub(crate) mod xml_parse;

// OSC parsing reports typed `pbfhogg::Error`s: malformed OSC content is
// `InvalidData`, a failing read is `Io`.
type ParseResult<T> = crate::Result<T>;

pub use parse::*;
