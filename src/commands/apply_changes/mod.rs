//! PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//!
//! Single-pass streaming batch pipeline:
//!   Phase 1: Parallel classify              [rayon pool]
//!   Phase 2: Sequential inline assign       [main thread, O(log n) per blob]
//!   Phase 3+4: Parallel rewrite + streaming output [rayon pool + main thread]
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
mod node_locations;
mod parallel_reader;
mod rewrite;
mod rewrite_block;
mod scanner;
mod stats;
mod stream_output;
mod streaming;

pub use rewrite::{merge, MergeOptions};
pub use stats::MergeStats;

type Result<T> = super::Result<T>;
