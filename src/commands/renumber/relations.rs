//! Relation rewrite pipeline.
//!
//! - **R1**: sequential relation scan to collect all relation IDs into
//!   an `IdSet` + rank index, refusing any relation ID that does not
//!   strictly exceed the one before it in file order.
//! - **R2d**: parallel wire-format splice rewriter for relations on the
//!   shared ordered pool. Resolves node/way/relation member refs inline.

use protohoggr::{Cursor, WIRE_LEN};

use super::super::Result;
use super::pool::{Rewritten, run_rewrite_stage};
use super::schedule::BlobTask;
use super::wire_rewrite::{
    RelationIdSpaces, Scratch, order_error, reframe_relations_with_new_ids, relation_id,
};
use super::{BlobBuffers, Phase, StageCounters};
use crate::idset::IdSet;

/// Reject negative ids at the entry of the external path.
///
/// Production OSM planet extracts don't contain negative ids (they're
/// JOSM-local editor staging identifiers resolved before upload).
/// Renumber rejects them with a clear error.
fn reject_negative_id(id: i64, kind: &str) -> Result<()> {
    if id < 0 {
        return Err(crate::Error::invalid_data(format!(
            "renumber requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Negative ids are JOSM editor-local staging identifiers \
             that should be resolved before processing."
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// R1: collect relation IDs
// ---------------------------------------------------------------------------

/// R1 pass: collect relation IDs into an `IdSet`. New IDs are derived via
/// `start_relation_id + rank(old_id)`, which equals each relation's position
/// only because this pass proves the IDs strictly increase.
///
/// Each id is read with [`relation_id`], the same rule R2d's rewriter
/// applies, so the set holds exactly the ids R2d will look up.
#[hotpath::measure]
pub(super) fn relation_r1_collect_ids(
    shared_file: &std::fs::File,
    source: &std::sync::Arc<std::path::Path>,
    relation_schedule: &[BlobTask],
    relation_id_set: &mut IdSet,
    cancel: &crate::host::Invocation,
) -> Result<()> {
    let counters = StageCounters::new();
    let mut buffers = BlobBuffers::default();
    // Strict increase over the whole relation section, in file order.
    let mut prev: Option<i64> = None;

    for task in relation_schedule {
        // Once per relation blob, before it is read.
        cancel.check()?;
        (|| -> Result<()> {
            buffers
                .load(shared_file, task, &counters)
                .map_err(|e| e.context("R1"))?;

            let t = crate::counters::Stopwatch::start();
            let mut cursor = Cursor::new(&buffers.decompressed);
            while let Some((field, wire_type)) = cursor
                .read_tag()
                .map_err(|e| crate::Error::from(e).context("R1 block"))?
            {
                if field != 2 || wire_type != WIRE_LEN {
                    cursor
                        .skip_field(wire_type)
                        .map_err(|e| crate::Error::from(e).context("R1 skip"))?;
                    continue;
                }
                let group = cursor
                    .read_len_delimited()
                    .map_err(|e| crate::Error::from(e).context("R1 group"))?;
                let mut gcursor = Cursor::new(group);
                while let Some((gfield, gwire)) = gcursor
                    .read_tag()
                    .map_err(|e| crate::Error::from(e).context("R1 gfield"))?
                {
                    if gfield != 4 || gwire != WIRE_LEN {
                        gcursor
                            .skip_field(gwire)
                            .map_err(|e| crate::Error::from(e).context("R1 gskip"))?;
                        continue;
                    }
                    let rel_bytes = gcursor
                        .read_len_delimited()
                        .map_err(|e| crate::Error::from(e).context("R1 rel"))?;
                    let rel_id = relation_id(rel_bytes)?;
                    reject_negative_id(rel_id, "relation")?;
                    if let Some(p) = prev
                        && rel_id <= p
                    {
                        return Err(order_error("relation", p, rel_id));
                    }
                    prev = Some(rel_id);
                    relation_id_set.set(rel_id);
                }
            }
            t.record_ns(&counters.reframe_ns);
            counters.bump_blobs();
            Ok(())
        })()
        .map_err(|e| task.locate(source, e))?;
    }

    counters.emit(Phase::R1, cancel);
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation pass R2d: parallel wire-format rewrite + write output
// ---------------------------------------------------------------------------

struct R2dWorker {
    buffers: BlobBuffers,
    scratch: Scratch,
    reframe_buf: Vec<u8>,
}

/// What R2d produced besides the output blobs.
pub(super) struct R2dResult {
    pub(super) relations_written: u64,
    pub(super) orphan_refs: u64,
}

/// Parallel R2d: wire-format splice rewriter for relation blobs.
#[hotpath::measure]
pub(super) fn relation_r2d_assembly(
    shared_file: &std::fs::File,
    source: &std::sync::Arc<std::path::Path>,
    relation_schedule: &[BlobTask],
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    ids: &RelationIdSpaces<'_>,
    host: &crate::host::Host,
) -> Result<R2dResult> {
    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    )
    .min(crate::tunables::RENUMBER_RELATIONS_WORKERS_CAP);
    let counters = StageCounters::new();
    let mut workers: Vec<R2dWorker> = (0..decode_threads.max(1))
        .map(|_| R2dWorker {
            buffers: BlobBuffers::default(),
            scratch: Scratch::default(),
            reframe_buf: Vec::new(),
        })
        .collect();

    let totals = run_rewrite_stage(
        "relation",
        Phase::R2d,
        source,
        relation_schedule.len(),
        &mut workers,
        &counters,
        writer,
        |w: &mut R2dWorker, idx| {
            let task = &relation_schedule[idx];
            (|| {
                // Deliberately load only payload bytes and emit no tagdata. Carrying
                // header key lists would change output and add per-blob rewrite work.
                w.buffers.load(shared_file, task, &counters)?;

                let t = crate::counters::Stopwatch::start();
                let outcome = reframe_relations_with_new_ids(
                    &w.buffers.decompressed,
                    ids,
                    &mut w.reframe_buf,
                    &mut w.scratch,
                )?;
                t.record_ns(&counters.reframe_ns);
                counters.bump_blobs();

                let block = match outcome.new_range {
                    Some((min_id, max_id)) if outcome.count > 0 => {
                        let bytes = std::mem::take(&mut w.reframe_buf);
                        w.reframe_buf.reserve(bytes.len());
                        let index = crate::blob_meta::BlobIndex {
                            kind: crate::ElementKind::Relation,
                            min_id,
                            max_id,
                            count: outcome.count,
                            bbox: None,
                        };
                        Some((bytes, index))
                    }
                    _ => None,
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

    Ok(R2dResult {
        relations_written: totals.written,
        orphan_refs: totals.orphans,
    })
}
