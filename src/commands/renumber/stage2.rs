//! Stage 2d: parallel way rewrite - fused ref resolve + wire-format splice.
//!
//! Single pass over way blobs on the shared ordered pool: resolves refs
//! inline through the node `IdSpace` and splices new IDs into the wire
//! format. Each worker records the way IDs it sees in its own `IdSet` shard;
//! the caller merges the shards for relation member resolution.

use super::super::Result;
use super::pool::{Rewritten, run_rewrite_stage};
use super::schedule::BlobTask;
use super::wire_rewrite::{IdSpace, Scratch, reframe_ways_with_new_ids};
use super::{BlobBuffers, Phase, StageCounters, blob_index, positional_bases};
use crate::idset::IdSet;

struct Stage2dWorker<'a> {
    way_ids: &'a mut IdSet,
    buffers: BlobBuffers,
    scratch: Scratch,
    reframe_buf: Vec<u8>,
}

/// What stage 2d produced besides the output blobs.
pub(super) struct Stage2dResult {
    pub(super) ways_written: u64,
    pub(super) orphan_refs: u64,
}

#[hotpath::measure]
pub(super) fn stage2d_parallel_way_assembly(
    shared_file: &std::fs::File,
    source: &std::sync::Arc<std::path::Path>,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    way_id_sets: &mut [IdSet],
    way_schedule: &[BlobTask],
    nodes: &IdSpace<'_>,
    start_way_id: i64,
) -> Result<Stage2dResult> {
    let bases = positional_bases(start_way_id, way_schedule, "way")?;
    let counters = StageCounters::new();
    let mut workers: Vec<Stage2dWorker<'_>> = way_id_sets
        .iter_mut()
        .map(|way_ids| Stage2dWorker {
            way_ids,
            buffers: BlobBuffers::default(),
            scratch: Scratch::default(),
            reframe_buf: Vec::new(),
        })
        .collect();

    let totals = run_rewrite_stage(
        "way",
        Phase::Stage2d,
        source,
        way_schedule.len(),
        &mut workers,
        &counters,
        writer,
        |w: &mut Stage2dWorker<'_>, idx| {
            let task = &way_schedule[idx];
            let base = bases[idx];
            (|| {
                // Deliberately load only payload bytes and emit no tagdata. Carrying
                // header key lists would change output and add per-blob rewrite work.
                w.buffers.load(shared_file, task, &counters)?;

                let t = crate::counters::Stopwatch::start();
                let outcome = reframe_ways_with_new_ids(
                    &w.buffers.decompressed,
                    base,
                    nodes,
                    &mut *w.way_ids,
                    &mut w.reframe_buf,
                    &mut w.scratch,
                )?;
                t.record_ns(&counters.reframe_ns);

                if outcome.count != task.element_count {
                    return Err(crate::Error::invalid_data(format!(
                        "stage 2d decoded {} ways, indexdata said {}",
                        outcome.count, task.element_count
                    )));
                }
                counters.bump_blobs();
                let block = if outcome.count == 0 {
                    None
                } else {
                    let bytes = std::mem::take(&mut w.reframe_buf);
                    w.reframe_buf.reserve(bytes.len());
                    let index = blob_index(crate::ElementKind::Way, base, outcome.count, None)?;
                    Some((bytes, index))
                };
                Ok(Rewritten {
                    blob_index: task.blob_index,
                    frame_offset: task.frame_offset,
                    block,
                    span: outcome.span,
                    count: outcome.count,
                    orphans: outcome.orphans,
                })
            })()
            .map_err(|e| task.locate(source, e))
        },
    )?;

    Ok(Stage2dResult {
        ways_written: totals.written,
        orphan_refs: totals.orphans,
    })
}
