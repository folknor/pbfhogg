//! PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//!
//! Descriptor-first streaming pipeline (see `rewrite::merge`): a scanner
//! walks blob headers, a worker pool decompresses and rewrites the blobs
//! that overlap the diff, and a single drain reorders the results by seq
//! and writes the output.
//!
//! There is no separate batch-classify pass; the scanner and workers
//! process descriptors as they flow through the pipeline.
//!
//! Key insight: we pass ALL upsert IDs in a blob's range to the rewrite function.
//! IDs that match base elements are modifications (handled by normal element processing);
//! IDs that don't match are creates (emitted by the cursor). This eliminates the need
//! for a separate pass to collect modification IDs and compute create lists.

mod classify;
mod descriptor;
mod diff_ranges;
mod drain;
mod element_writes;
mod failure;
mod loc_mode;
mod node_coords;
mod node_locations;
mod rewrite;
mod rewrite_block;
mod scanner;
mod stats;
mod stream_output;
mod streaming;

pub use rewrite::{MergeOptions, merge};
pub use stats::MergeStats;

type Result<T> = super::Result<T>;
