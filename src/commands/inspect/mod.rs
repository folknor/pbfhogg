//! Inspect PBF file: comprehensive metadata, block breakdown, ordering analysis.
//! Also provides `show_element_to` for displaying a single element by ID.

mod format;
#[cfg(feature = "commands")]
mod json;
mod keys;
pub mod node_stats;
mod report;
mod scan;
mod show_element;
mod types;

pub use keys::{InspectGetKeyKind, InspectGetLookup, InspectGetNeed, inspect_get_lookup};
pub use scan::{InspectOptions, inspect};
pub use show_element::show_element_to;
pub use types::InspectReport;
