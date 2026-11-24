//! Element counts for comparison context, as osmium decodes them.
use crate::{error::Error, harness::Harness};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

/// osmium's object counts of `pbf` (and who wrote it), read from its OPL
/// decoding (`crate::opl_compare::order`).
pub(crate) fn osmium_counts(
    h: &mut Harness,
    pbf: crate::opl_compare::Operand<'_>,
) -> Result<Counts, Error> {
    let (_, [nodes, ways, relations]) = crate::opl_compare::order(h, pbf)?;
    Ok(Counts {
        nodes,
        ways,
        relations,
    })
}
