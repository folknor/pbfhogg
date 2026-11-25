//! The ordered rewrite stage every parallel renumber stage runs as.
//!
//! Pass 1, stage 2d and R2d share one shape: workers rewrite blobs on the
//! crate's ordered pool (`crate::ordered_pool`), and the calling thread
//! checks cross-blob ID order and writes the results strictly in schedule
//! order. The pool supplies the bounded window, the ordered failures and the
//! prompt exit this pool was first written to have (three hand-rolled copies
//! once parked every worker in `send` forever on a mid-stream error, and let
//! one slow blob grow the reorder buffer without limit); this module adds
//! what is renumber's own.

use super::super::Result;
use super::{Phase, StageCounters};
use crate::blob_meta::BlobIndex;
use crate::error::Error;
use crate::ordered_pool::{ConsumeFn, PoolConfig, Timer, Waits, default_window, run_ordered};

/// One rewritten blob, as a parallel stage's worker hands it to the writer.
pub(super) struct Rewritten {
    /// File-wide ordinal and frame start of the blob this was rewritten from.
    /// Plain integers: the consumer turns them into a location with the
    /// stage's single captured path, only if an order error needs one.
    pub(super) blob_index: u64,
    pub(super) frame_offset: u64,
    /// The block to write, or `None` for a blob that holds no elements.
    pub(super) block: Option<(Vec<u8>, BlobIndex)>,
    /// How the block's new ids were assigned, for indexing its pieces if it
    /// has to be cut (`super::split`).
    pub(super) numbering: super::split::Numbering,
    /// First and last *input* ID the blob held, `None` when empty.
    pub(super) span: Option<(i64, i64)>,
    pub(super) count: u64,
    pub(super) orphans: u64,
}

/// What a rewrite stage reports once every blob is written.
pub(super) struct StageTotals {
    pub(super) written: u64,
    pub(super) orphans: u64,
}

/// Enforces that input IDs of one element kind strictly ascend in canonical
/// order across blobs, in schedule order.
///
/// Renumber assigns a new ID by *position* (blob base plus offset) while
/// every reference is resolved by *canonical rank* in the set of input IDs.
/// The two agree exactly when the IDs strictly ascend over the whole kind
/// section;
/// a duplicate or a step backwards silently points references at the wrong
/// element. Within a blob the rewriters check each element against the one
/// before; this is the cross-blob half, fed with the IDs the blob actually
/// held rather than its indexdata range, which nothing verifies.
pub(super) struct StrictOrder {
    kind: &'static str,
    last: Option<i64>,
}

impl StrictOrder {
    pub(super) fn new(kind: &'static str) -> Self {
        Self { kind, last: None }
    }

    pub(super) fn admit(&mut self, span: Option<(i64, i64)>) -> std::result::Result<(), Error> {
        let Some((first, last)) = span else {
            return Ok(());
        };
        if let Some(prev) = self.last
            && crate::osm_id::osm_id_le(first, prev)
        {
            return Err(super::wire_rewrite::order_error(self.kind, prev, first));
        }
        self.last = Some(last);
        Ok(())
    }
}

/// Run one parallel rewrite stage: `work` rewrites blob `idx` on a worker,
/// and the calling thread checks cross-blob order and writes the results in
/// schedule order. Counters are emitted under `stage`'s names once the stage
/// ends.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_rewrite_stage<S, W>(
    kind: &'static str,
    stage: Phase,
    source: &std::sync::Arc<std::path::Path>,
    n_tasks: usize,
    states: &mut [S],
    counters: &StageCounters,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    work: W,
) -> Result<StageTotals>
where
    S: Send + super::RewriteWorker,
    W: Fn(&mut S, usize) -> crate::Result<Rewritten> + Sync,
{
    let mut order = StrictOrder::new(kind);
    let mut totals = StageTotals {
        written: 0,
        orphans: 0,
    };
    let limits = crate::commands::split::Limits::new(writer.compression());
    let sure = limits.sure();
    let cancel = writer.cancel_token().permit();
    run_ordered(
        n_tasks,
        states,
        PoolConfig {
            window: default_window(states.len()),
            cancel: &cancel,
            waits: Waits {
                window: Some(Timer::Ns(&counters.window_wait_ns)),
                recv: Some(Timer::Ns(&counters.consumer_recv_wait_ns)),
                ..Waits::default()
            },
        },
        |_| (),
        |state, (), idx| {
            // Nothing a result carries borrows the blob buffers, so they
            // follow their retention limits as soon as the work returns,
            // on success and failure alike.
            let result = work(state, idx);
            state.buffers().release_outliers(&cancel);
            result
        },
        &mut ConsumeFn(|_, item: Rewritten| {
            order.admit(item.span).map_err(|e| {
                e.locate(crate::error::Location::new(
                    Some(source),
                    Some(item.blob_index),
                    Some(crate::ByteOffset(item.frame_offset)),
                ))
            })?;
            if let Some((bytes, index)) = item.block {
                let t = crate::counters::Stopwatch::start();
                // Deliberately write no tagdata: terminal renumber output is not
                // tag-filtered. Carrying headers or rebuilding keys would add per-blob
                // or per-tag work and change the historical output bytes.
                let mut write = |bytes, index| {
                    writer.write_primitive_block_owned(crate::block_builder::OwnedBlock {
                        bytes,
                        index,
                        tagdata: None,
                        way_members: None,
                    })
                };
                if bytes.len() <= sure {
                    write(bytes, index)?;
                } else {
                    // The block may be past what the writer frames: `split`
                    // checks it and cuts it between elements if so, one input
                    // blob still one result for the order check and the totals.
                    for (bytes, index) in super::split::fit(bytes, index, item.numbering, &limits)?
                    {
                        write(bytes, index)?;
                    }
                }
                t.record_ns(&counters.consumer_write_ns);
            }
            // Counted after the write, so on a mid-stream error the totals cover
            // only output actually emitted.
            totals.written = totals.written.saturating_add(item.count);
            totals.orphans = totals.orphans.saturating_add(item.orphans);
            Ok(())
        }),
    )?;
    counters.emit(stage, &cancel);
    Ok(totals)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strict_order_refuses_a_step_back_or_a_repeat_across_blobs() {
        let mut order = StrictOrder::new("node");
        order.admit(Some((1, 5))).unwrap();
        order.admit(None).unwrap();
        order.admit(Some((6, 9))).unwrap();
        crate::test_support::assert_err_kind!(
            order.admit(Some((9, 12))),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("appears twice"),
            "a repeat must be refused"
        );
        let mut order = StrictOrder::new("node");
        order.admit(Some((10, 20))).unwrap();
        crate::test_support::assert_err_kind!(
            order.admit(Some((3, 4))),
            crate::ErrorKind::Unsorted { .. },
            "a step back must be refused"
        );
        // Canonical order: 0, then negatives by magnitude, then positives.
        let mut order = StrictOrder::new("node");
        order.admit(Some((0, -3))).unwrap();
        order.admit(Some((-4, 2))).unwrap();
        crate::test_support::assert_err_kind!(
            order.admit(Some((-9, -9))),
            crate::ErrorKind::Unsorted { .. }
        );
        let mut order = StrictOrder::new("node");
        order.admit(Some((-3, -3))).unwrap();
        crate::test_support::assert_err_kind!(
            order.admit(Some((-2, -2))),
            crate::ErrorKind::Unsorted { .. }
        );
    }
}
