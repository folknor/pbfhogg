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

/// Enforces that input IDs of one element kind strictly increase across
/// blobs, in schedule order.
///
/// Renumber assigns a new ID by *position* (blob base plus offset) while
/// every reference is resolved by *rank* in the set of input IDs. The two
/// agree exactly when the IDs strictly increase over the whole kind section;
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
            && first <= prev
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
    S: Send,
    W: Fn(&mut S, usize) -> crate::Result<Rewritten> + Sync,
{
    let mut order = StrictOrder::new(kind);
    let mut totals = StageTotals {
        written: 0,
        orphans: 0,
    };
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
        |state, (), idx| work(state, idx),
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
                writer.write_primitive_block_owned(crate::block_builder::OwnedBlock {
                    bytes,
                    index,
                    tagdata: None,
                    way_members: None,
                })?;
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
        assert!(
            order.admit(Some((9, 12))).is_err(),
            "a repeat must be refused"
        );
        let mut order = StrictOrder::new("node");
        order.admit(Some((10, 20))).unwrap();
        assert!(
            order.admit(Some((3, 4))).is_err(),
            "a step back must be refused"
        );
    }
}
