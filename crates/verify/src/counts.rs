//! Element counts for comparison context, as osmium decodes them.
use std::path::Path;

use crate::{error::Error, harness::Harness};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

/// osmium's object counts of `pbf`, read from its OPL decoding
/// (`crate::opl_compare::order`).
pub(crate) fn osmium_counts(h: &mut Harness, pbf: &Path) -> Result<Counts, Error> {
    let (_, [nodes, ways, relations]) =
        crate::opl_compare::order(h, pbf, &pbf.with_extension("counts.opl"))?;
    Ok(Counts {
        nodes,
        ways,
        relations,
    })
}
