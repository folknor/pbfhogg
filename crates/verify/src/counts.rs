//! Shared inspect-based element counts for comparison context.
use crate::{error::Error, harness::Harness, inspect_report};
pub(crate) use inspect_report::InspectCounts as Counts;
use std::path::Path;
pub(crate) fn inspect_counts(h: &mut Harness, pbf: &Path) -> Result<Counts, Error> {
    Ok(inspect_report::inspect(h, pbf, false)?.counts())
}
