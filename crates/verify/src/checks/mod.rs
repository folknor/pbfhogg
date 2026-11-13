//! One module per check. Each `run` makes its outputs in a fresh directory
//! of the run, records every comparison on the harness and returns its
//! claim.
pub(crate) mod apply_changes;
pub(crate) mod sort;

pub(crate) mod add_locations;
pub(crate) mod cat;
pub(crate) mod check_refs;
pub(crate) mod getparents;
pub(crate) mod tags_filter;
pub(crate) type ReadCheck = fn(
    &mut crate::harness::Harness,
    &std::path::Path,
    bool,
) -> Result<Option<String>, crate::error::Error>;

pub(crate) mod getid_removeid;

pub(crate) mod extract;

pub(crate) mod multi_extract;

pub(crate) mod check_ids;
pub(crate) mod degrade;
pub(crate) mod repack;

pub(crate) mod merge_changes;
pub(crate) mod tags_filter_osc;

pub(crate) mod derive_changes;
pub(crate) mod diff;
pub(crate) mod renumber;
pub(crate) type PairCheck = fn(
    &mut crate::harness::Harness,
    &std::path::Path,
    &std::path::Path,
    bool,
) -> Result<Option<String>, crate::error::Error>;
