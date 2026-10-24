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

/// The one rule every OSC producer (`merge-changes`, `tags-filter` on an OSC,
/// `diff --format osc`) uses to frame its output, from the name the caller
/// chose: a name ending `.osc` is written plain (`Ok(false)`), a name ending
/// `.osc.gz` is gzipped (`Ok(true)`), and any other name is refused with an
/// `InvalidArgument` error naming both suffixes.
///
/// The match is on the final path component's raw bytes, so a non-UTF-8 name
/// is judged like any other, and it is case-sensitive: `.OSC` is refused.
/// Producers call this before any work, and never with a staging name.
///
/// # Errors
///
/// `InvalidArgument` when `destination` ends in neither suffix.
pub fn output_is_gzip(destination: &std::path::Path) -> crate::Result<bool> {
    let name = destination.as_os_str().as_encoded_bytes();
    if name.ends_with(b".osc.gz") {
        Ok(true)
    } else if name.ends_with(b".osc") {
        Ok(false)
    } else {
        Err(crate::Error::invalid_argument(format!(
            "OSC output name {} must end in .osc (written plain) or .osc.gz (gzipped)",
            destination.display()
        )))
    }
}
