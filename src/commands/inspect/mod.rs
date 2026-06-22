//! Inspect PBF file: comprehensive metadata, block breakdown, ordering analysis.
//! Also provides `show_element` for displaying a single element by ID.

mod format;
#[cfg(feature = "commands")]
mod json;
pub mod node_stats;
mod report;
mod scan;
mod show_element;
mod types;

pub use scan::inspect;
pub use show_element::{ShowElementType, show_element};
pub use types::InspectReport;
