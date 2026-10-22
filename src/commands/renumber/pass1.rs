//! Pass 1: parallel node rewrite on the shared ordered pool.
//!
//! Workers rewrite node blobs in any order; each blob's base new id comes
//! from a prefix sum over the schedule's element counts, so no worker waits
//! on another. Old node IDs go into one shared, pre-allocated `IdSet` via
//! `set_atomic_checked`; an ID outside the pre-allocation is collected per
//! worker and returned for the caller to insert after the pass.

use super::super::Result;
use super::pool::{Rewritten, run_rewrite_stage};
use super::schedule::BlobTask;
use super::wire_rewrite::{Scratch, reframe_dense_with_new_ids};
use super::{BlobBuffers, PASS1_WORKERS, StageCounters, add_ns, blob_index, positional_bases};
use crate::idset::IdSet;

/// Per-worker state: I/O and rewrite buffers plus the IDs this worker could
/// not record in the shared set.
struct Pass1Worker {
    buffers: BlobBuffers,
    scratch: Scratch,
    reframe_buf: Vec<u8>,
    overflow: Vec<i64>,
}

/// What pass 1 produced besides the output blobs.
pub(super) struct Pass1Result {
    pub(super) nodes_written: u64,
    /// Node IDs `node_id_set` had no pre-allocated chunk for. The caller
    /// must `set` them before building the rank index.
    pub(super) overflow: Vec<Vec<i64>>,
}

#[hotpath::measure]
pub(super) fn pass1_parallel_scan(
    schedule: &[BlobTask],
    start_node_id: i64,
    shared_file: &std::fs::File,
    node_id_set: &IdSet,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
) -> Result<Pass1Result> {
    let bases = positional_bases(start_node_id, schedule, "node")?;
    let counters = StageCounters::new();
    let mut workers: Vec<Pass1Worker> = (0..PASS1_WORKERS)
        .map(|_| Pass1Worker {
            buffers: BlobBuffers::default(),
            scratch: Scratch::default(),
            reframe_buf: Vec::new(),
            overflow: Vec::new(),
        })
        .collect();

    let totals = run_rewrite_stage(
        "node",
        "pass1",
        schedule.len(),
        &mut workers,
        &counters,
        writer,
        |w: &mut Pass1Worker, idx| {
            let task = &schedule[idx];
            let base = bases[idx];
            w.buffers.load(shared_file, task, &counters)?;

            let t = std::time::Instant::now();
            let outcome = reframe_dense_with_new_ids(
                &w.buffers.decompressed,
                base,
                node_id_set,
                &mut w.overflow,
                &mut w.reframe_buf,
                &mut w.scratch,
            )?;
            add_ns(&counters.reframe_ns, t);

            if outcome.count != task.element_count {
                return Err(format!(
                    "pass1 blob {idx} decoded {} nodes, indexdata said {}",
                    outcome.count, task.element_count
                ));
            }
            counters.bump_blobs();
            let block = if outcome.count == 0 {
                None
            } else {
                let bytes = std::mem::take(&mut w.reframe_buf);
                w.reframe_buf.reserve(bytes.len());
                let index = blob_index(
                    crate::blob_meta::ElemKind::Node,
                    base,
                    outcome.count,
                    task.bbox,
                )?;
                Some((bytes, index))
            };
            Ok(Rewritten {
                block,
                span: outcome.span,
                count: outcome.count,
                orphans: 0,
            })
        },
    )?;

    Ok(Pass1Result {
        nodes_written: totals.written,
        overflow: workers.into_iter().map(|w| w.overflow).collect(),
    })
}
