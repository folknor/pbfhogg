//! Output helpers for gap creates, trailing creates at type transitions,
//! and passthrough coalescing. The drain calls these while processing items
//! in file order.
//!
//! ## Create runs
//!
//! Every create the drain writes (gap creates before a blob, the creates of
//! a kind left over at a type transition, the trailing creates at end of
//! input) leaves as part of a run: a slice of the sorted upsert vector the
//! drain has chosen, written as consecutive blocks of `DEFAULT_ELEMENT_CAP`
//! elements and a shorter last one. [`CreateEncoder`] decides where the
//! blocks are cut (every upsert id has its overlay record, so each create is
//! one element and the cuts are those a single builder filling to its cap
//! would make), encodes and frames each block the way the workers frame
//! theirs (`frame_blob_pipelined`), and writes the frames raw in order.
//!
//! A run of one block is encoded on the drain thread. A longer one is
//! encoded on the invocation's executor, `APPLY_CHANGES_CREATE_BATCH_BLOCKS`
//! blocks per call, except under an explicit `-j 1`, which runs
//! sequentially and encodes every block on the drain thread. At planet the
//! OSC's node creates past the last base node blob are about 300 blocks,
//! written at the node-to-way transition:
//! encoded serially that kept the drain busy for a third of a second while
//! the workers kept producing way output behind it, a backlog of framed
//! output the allocator then kept as free memory in the workers' arenas for
//! the rest of the run (reference/performance/apply-changes.md, "apply-changes' memory
//! step at the node-to-way transition").

use std::num::NonZeroUsize;
use std::ops::Range;

use rayon::prelude::*;

use crate::ElementKind;
use crate::block_builder::{BlockBuilder, DEFAULT_ELEMENT_CAP};
use crate::file_writer::FileWriter;
use crate::osc::sorted::SortedDiff;
use crate::tunables::APPLY_CHANGES_CREATE_BATCH_BLOCKS;
use crate::writer::{Compression, PbfWriter, frame_blob_pipelined};

use super::Result;
use super::element_writes::{add_osc_node, add_osc_relation, add_osc_way};
use super::loc_mode::LocMode;
use super::stats::MergeStats;
use super::upsert_cursors::UpsertCursors;

/// One encoded block: its frame (`None` when there is nothing to write) and
/// the number of elements it holds.
type FramedBlock = (Option<Vec<u8>>, u64);

/// Encodes, frames and writes the drain's create runs; see the module doc.
pub(super) struct CreateEncoder<'a> {
    /// Where runs longer than one block are encoded; `None` encodes every
    /// block on the drain thread (an explicit `-j 1`, which runs sequentially).
    executor: Option<&'a crate::host::Executor>,
    compression: Compression,
    /// The builder for blocks encoded on the drain thread. It builds nothing
    /// but creates and is empty at every block's start: each block is taken
    /// before the next is begun.
    bb: BlockBuilder,
}

impl<'a> CreateEncoder<'a> {
    pub(super) fn new(
        executor: Option<&'a crate::host::Executor>,
        compression: Compression,
    ) -> Self {
        Self {
            executor,
            compression,
            bb: BlockBuilder::new(),
        }
    }

    /// Write the creates `run` of `kind` (an index range into the kind's
    /// sorted upserts), in order, as blocks of `DEFAULT_ELEMENT_CAP`.
    /// Creates counted into `stats` per kind. The first failure in output
    /// order is returned, after every block before it has been written;
    /// nothing after it is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit(
        &mut self,
        kind: ElementKind,
        run: Range<usize>,
        diff: &SortedDiff,
        loc: LocMode<'_>,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        let invocation = writer.cancel_token().permit();
        let metrics = &invocation.metrics().writer;
        let compression = self.compression;
        let added = write_run(
            self.executor,
            create_batch_blocks(&invocation),
            run,
            &mut self.bb,
            &invocation,
            |bb, chunk| encode_and_frame(bb, kind, chunk, diff, loc, &compression, metrics),
            |frame| writer.write_raw_owned(frame),
        )?;
        match kind {
            ElementKind::Node => stats.diff_nodes += added,
            ElementKind::Way => stats.diff_ways += added,
            ElementKind::Relation => stats.diff_relations += added,
        }
        Ok(())
    }
}

/// Blocks per executor call: `APPLY_CHANGES_CREATE_BATCH_BLOCKS`, or the
/// fault plan's nonzero override in a test-hooks build. Scheduling only:
/// the blocks are cut at the element cap whatever the batch.
fn create_batch_blocks(invocation: &crate::host::Invocation) -> NonZeroUsize {
    #[cfg(feature = "test-hooks")]
    if let Some(forced) = NonZeroUsize::new(
        invocation
            .faults()
            .apply_changes_create_batch_blocks
            .load(std::sync::atomic::Ordering::Relaxed),
    ) {
        return forced;
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = invocation;
    APPLY_CHANGES_CREATE_BATCH_BLOCKS
}

/// Cut `run` into blocks of `DEFAULT_ELEMENT_CAP`, `encode` each and `write`
/// every frame in block order; returns the elements encoded. `encode` gets an
/// empty builder and the block's index range, and returns the block's frame
/// (`None` for nothing to write) and its element count. A run longer than
/// one block, with an executor, is encoded on it in batches of
/// `batch_blocks` blocks; otherwise every block is
/// encoded on the calling thread into `bb`. `invocation` is checked before
/// each batch or block.
///
/// The first failure in block order, an encoding's or a write's, is
/// returned after every block before it has been written; nothing after it
/// is written. Inside a parallel batch that holds whatever order the blocks
/// finish in: the batch's results are collected indexed, one per block in
/// block order, and walked serially, so completion order never reaches the
/// choice. Every block of the batch is still encoded.
fn write_run<E, W>(
    executor: Option<&crate::host::Executor>,
    batch_blocks: NonZeroUsize,
    run: Range<usize>,
    bb: &mut BlockBuilder,
    invocation: &crate::host::Invocation,
    encode: E,
    mut write: W,
) -> Result<u64>
where
    E: Fn(&mut BlockBuilder, Range<usize>) -> Result<FramedBlock> + Sync,
    W: FnMut(Vec<u8>) -> Result<()>,
{
    let cap = DEFAULT_ELEMENT_CAP.get();
    let mut added = 0u64;
    match executor {
        Some(executor) if run.len() > cap => {
            // Saturating: a batch past `usize::MAX` elements is the whole
            // run, never a zero step or a wrapped boundary.
            for batch in chunks(run, cap.saturating_mul(batch_blocks.get())) {
                invocation.check()?;
                let blocks: Vec<Range<usize>> = chunks(batch, cap).collect();
                let framed: Vec<Result<FramedBlock>> = executor.install(|| {
                    blocks
                        .into_par_iter()
                        .map(|chunk| {
                            // A builder per block, not the worker rule's
                            // builder kept for the run: an invocation-owned
                            // pool of builders, tested at planet, showed no
                            // wall or sampled-peak benefit and usually
                            // lengthened the transition (reference/performance/
                            // apply-changes.md, "apply-changes' create-run
                            // builders are made per block"). A thread-local
                            // is ruled out on its own grounds: the
                            // executor's threads belong to the caller and
                            // outlive the invocation, and the builders
                            // would stay on them.
                            let mut bb = BlockBuilder::new();
                            encode(&mut bb, chunk)
                        })
                        .collect()
                });
                for result in framed {
                    let (frame, n) = result?;
                    added += n;
                    if let Some(frame) = frame {
                        write(frame)?;
                    }
                }
            }
        }
        _ => {
            for chunk in chunks(run, cap) {
                invocation.check()?;
                let (frame, n) = encode(bb, chunk)?;
                added += n;
                if let Some(frame) = frame {
                    write(frame)?;
                }
            }
        }
    }
    Ok(added)
}

/// `range` cut into consecutive ranges of `size` elements, the last one
/// shorter: the index-range form of `slice::chunks`.
fn chunks(range: Range<usize>, size: usize) -> impl Iterator<Item = Range<usize>> {
    let end = range.end;
    range
        .step_by(size)
        .map(move |start| start..end.min(start.saturating_add(size)))
}

/// Encode the creates `run` (an index range into the kind's sorted upserts,
/// at most one block's worth) into the empty builder `bb` and frame the
/// block, returning the frame (`None` for an empty run) and the number of
/// elements added. The builder's poison, if an addition was refused,
/// surfaces through `lend_encoded`.
fn encode_and_frame(
    bb: &mut BlockBuilder,
    kind: ElementKind,
    run: Range<usize>,
    diff: &SortedDiff,
    loc: LocMode<'_>,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<FramedBlock> {
    // Each block starts on an empty builder (a flush always resets it, and
    // the parallel path makes a fresh one per block) and holds at most one
    // block's worth: `emit` cuts runs at the cap. Either broken would put
    // other elements into this block or overflow it.
    if !bb.is_empty() {
        return Err(crate::error::Error::internal(
            "apply-changes create block started on a builder that still holds elements",
        ));
    }
    if run.len() > DEFAULT_ELEMENT_CAP.get() {
        return Err(crate::error::Error::internal(format!(
            "apply-changes create run of {} elements is over the {}-element block cap",
            run.len(),
            DEFAULT_ELEMENT_CAP.get()
        )));
    }
    let added = run.len() as u64;
    for i in run {
        match kind {
            ElementKind::Node => add_osc_node(bb, &diff.node(i)),
            ElementKind::Way => {
                let osc = diff.way(i);
                // Fails only on `LocMode::Pending`, a way reaching the
                // writer before the coordinate map was published: a routing
                // invariant, not an input condition. A way create whose node
                // is missing never gets here failing: the drain refuses
                // missing nodes at the node-to-way barrier, before any way
                // create run, unless `--ignore-missing-nodes` asked for the
                // `(0, 0)` fallback `add_osc_way` gives. The block failure
                // input can reach in a create run is the frame's size
                // refusal below (a block over the PBF blob limit).
                let locs = loc.for_way(osc.id())?;
                add_osc_way(bb, &osc, locs);
            }
            ElementKind::Relation => add_osc_relation(bb, &diff.relation(i)),
        }
    }
    let frame = bb.lend_encoded(|block| {
        let indexdata = block.index.serialize();
        // No WayMembers, as `take_owned` gives none: the same frame the
        // writer's own framing job made of these blocks.
        frame_blob_pipelined(
            block.bytes,
            compression,
            Some(&indexdata),
            block.tagdata,
            None,
            metrics,
        )
    })?;
    Ok((frame, added))
}

/// Flush remaining upserts for the previous element type during a type
/// transition. Also handles skipped types (e.g., Node -> Relation flushes
/// all Way upserts).
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_remaining_upserts(
    prev: ElementKind,
    next: ElementKind,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    // Flush remaining creates of the previous type
    let mut emitted = emit_rest(prev, diff, cursors, creates, writer, stats, loc)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElementKind::Node && next == ElementKind::Relation {
        emitted += emit_rest(ElementKind::Way, diff, cursors, creates, writer, stats, loc)?;
    }

    Ok(emitted)
}

/// Write every upsert of `kind` from the cursor on and advance the cursor
/// to the end; returns how many ids that was.
fn emit_rest(
    kind: ElementKind,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<u64> {
    let (cursor, upserts) = cursors.get_mut(kind, diff);
    let run = *cursor..upserts.len();
    *cursor = upserts.len();
    let emitted = run.len() as u64;
    creates.emit(kind, run, diff, loc, writer, stats)?;
    Ok(emitted)
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    diff: &SortedDiff,
    cursors: &mut UpsertCursors,
    creates: &mut CreateEncoder<'_>,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc: LocMode<'_>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, diff);
    let start = *cursor;
    let len = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_cmp(id, min_id).is_lt());
    *cursor = start + len;
    creates.emit(blob_kind, start..start + len, diff, loc, writer, stats)
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
pub(super) fn has_gap_creates(
    blob_kind: ElementKind,
    min_id: i64,
    diff: &SortedDiff,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, diff);
    cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[cursor], min_id).is_lt()
}

#[cfg(test)]
#[path = "../../../tests/create_run_fixture.rs"]
mod create_run_fixture;

#[cfg(test)]
mod tests {
    use super::*;

    // Share the tier-3 integration fixture's shape rather than guarding a
    // second handwritten count that can drift independently of the actual
    // run: that test is the one that runs the production batch.
    use super::create_run_fixture::BOUNDARY_RUN_FULL_BLOCKS;

    #[test]
    fn the_batch_boundary_fixture_spans_more_than_one_batch() {
        let cap = DEFAULT_ELEMENT_CAP.get();
        let run = 0..BOUNDARY_RUN_FULL_BLOCKS * cap + 1;
        assert!(
            chunks(run, cap * APPLY_CHANGES_CREATE_BATCH_BLOCKS.get()).count() > 1,
            "APPLY_CHANGES_CREATE_BATCH_BLOCKS holds the batch-boundary fixture's \
             whole run in one batch: grow the run in tests/apply_changes_create_runs.rs \
             by changing its shared fixture definition"
        );
    }

    use std::sync::Mutex;

    use crate::host::{Executor, Invocation};

    /// A run start that is not a multiple of the cap, as real gap and
    /// trailing runs start at any upsert offset.
    const START: usize = 3;

    /// The block of `run` an index range is, by its start.
    fn block_of(chunk: &Range<usize>) -> usize {
        (chunk.start - START) / DEFAULT_ELEMENT_CAP
    }

    fn block_failure(block: usize) -> crate::error::Error {
        crate::error::Error::internal(format!("block {block} failed"))
    }

    /// The fixed-width test executor. Which order the batch's blocks finish
    /// in does not matter to these tests: `write_run` collects a parallel
    /// batch's results indexed, one per block in block order, before it
    /// walks them, so completion order cannot reach which failure is
    /// returned, and no block here waits for another.
    fn pool() -> Executor {
        Executor::for_tests()
    }

    /// An encoder failing blocks `earlier` and `later`. Every other block
    /// frames as its own index. Records each encoded block's range.
    fn encode_failing(
        earlier: usize,
        later: usize,
        seen: &Mutex<Vec<Range<usize>>>,
    ) -> impl Fn(&mut BlockBuilder, Range<usize>) -> Result<FramedBlock> + Sync {
        move |bb, chunk| {
            assert!(bb.is_empty(), "every block starts on an empty builder");
            seen.lock().expect("lock").push(chunk.clone());
            let block = block_of(&chunk);
            if block == later || block == earlier {
                return Err(block_failure(block));
            }
            Ok((Some(block.to_le_bytes().to_vec()), chunk.len() as u64))
        }
    }

    fn written_blocks(frames: &[Vec<u8>]) -> Vec<usize> {
        frames
            .iter()
            .map(|f| usize::from_le_bytes(f.as_slice().try_into().expect("block frame")))
            .collect()
    }

    /// The ranges the run's blocks are, in order.
    fn expected_ranges(blocks: usize, last_len: usize) -> Vec<Range<usize>> {
        let cap = DEFAULT_ELEMENT_CAP.get();
        (0..blocks)
            .map(|b| {
                let start = START + b * cap;
                start..start + if b + 1 == blocks { last_len } else { cap }
            })
            .collect()
    }

    /// Run `write_run` over `blocks` blocks (the last `last_len` long) with
    /// `encode_failing(earlier, later)`; returns the error, the written
    /// blocks and the ranges encoded, sorted.
    fn run_failing(
        executor: Option<&Executor>,
        batch_blocks: NonZeroUsize,
        blocks: usize,
        last_len: usize,
        earlier: usize,
        later: usize,
    ) -> (crate::error::Error, Vec<usize>, Vec<Range<usize>>) {
        let cap = DEFAULT_ELEMENT_CAP.get();
        let seen = Mutex::new(Vec::new());
        let mut frames = Vec::new();
        let run = START..START + (blocks - 1) * cap + last_len;
        let result = write_run(
            executor,
            batch_blocks,
            run,
            &mut BlockBuilder::new(),
            &Invocation::new(),
            encode_failing(earlier, later, &seen),
            |frame| {
                frames.push(frame);
                Ok(())
            },
        );
        let Err(err) = result else {
            panic!("a run with failing blocks {earlier} and {later} succeeded")
        };
        let mut seen = seen.into_inner().expect("lock");
        seen.sort_by_key(|r| r.start);
        (err, written_blocks(&frames), seen)
    }

    /// Two failing blocks in the first parallel batch, finishing in whatever
    /// order: the earlier one's error is returned, the block before it is
    /// written, nothing after it is, and every block of the batch was
    /// encoded at its cut.
    #[test]
    fn the_first_failure_in_a_parallel_batch_is_returned_after_its_prefix() {
        let executor = pool();
        let (err, written, seen) = run_failing(
            Some(&executor),
            APPLY_CHANGES_CREATE_BATCH_BLOCKS,
            5,
            17,
            1,
            3,
        );
        assert!(err.to_string().contains("block 1 failed"), "got {err}");
        assert_eq!(written, vec![0]);
        assert_eq!(seen, expected_ranges(5, 17));
    }

    /// The same in a later batch: the first batch is written whole, then
    /// the batch's blocks before its earliest failure, and the earliest
    /// failure's error is returned.
    #[test]
    fn the_first_failure_in_a_later_parallel_batch_is_returned_after_its_prefix() {
        let executor = pool();
        let batch = APPLY_CHANGES_CREATE_BATCH_BLOCKS.get();
        let blocks = batch + 3;
        let (err, written, seen) = run_failing(
            Some(&executor),
            APPLY_CHANGES_CREATE_BATCH_BLOCKS,
            blocks,
            1,
            batch + 1,
            batch + 2,
        );
        assert!(
            err.to_string()
                .contains(&format!("block {} failed", batch + 1)),
            "got {err}"
        );
        assert_eq!(written, (0..=batch).collect::<Vec<_>>());
        assert_eq!(seen, expected_ranges(blocks, 1));
    }

    /// The same rules at small batches: two failures in the third batch of
    /// two, and a run walked one block per batch or in one saturated batch,
    /// all cut at the element cap and written whole and in order.
    #[test]
    fn small_and_saturated_batches_keep_the_cuts_and_the_order() {
        let executor = pool();
        let two = NonZeroUsize::MIN.saturating_add(1);
        let (err, written, seen) = run_failing(Some(&executor), two, 7, 17, 4, 5);
        assert!(err.to_string().contains("block 4 failed"), "got {err}");
        assert_eq!(written, vec![0, 1, 2, 3]);
        // Batches of two: the third batch (blocks 4 and 5) failed, so the
        // fourth (block 6) was never encoded.
        assert_eq!(seen, expected_ranges(7, 17)[..6].to_vec());

        let cap = DEFAULT_ELEMENT_CAP.get();
        let len = 5 * cap + 17;
        for batch_blocks in [NonZeroUsize::MIN, NonZeroUsize::MAX] {
            let seen = Mutex::new(Vec::new());
            let mut frames = Vec::new();
            let added = write_run(
                Some(&executor),
                batch_blocks,
                START..START + len,
                &mut BlockBuilder::new(),
                &Invocation::new(),
                encode_failing(usize::MAX, usize::MAX, &seen),
                |frame| {
                    frames.push(frame);
                    Ok(())
                },
            )
            .expect("run");
            assert_eq!(added, len as u64, "batch {batch_blocks}");
            assert_eq!(written_blocks(&frames), vec![0, 1, 2, 3, 4, 5]);
            let mut seen = seen.into_inner().expect("lock");
            seen.sort_by_key(|r| r.start);
            assert_eq!(seen, expected_ranges(6, 17), "batch {batch_blocks}");
        }
    }

    /// Without an executor (`-j 1`) blocks are encoded one at a time, so
    /// the run stops at its first failure: nothing past it is encoded.
    #[test]
    fn a_sequential_run_stops_at_its_first_failure() {
        let seen = Mutex::new(Vec::new());
        let mut frames = Vec::new();
        let cap = DEFAULT_ELEMENT_CAP.get();
        let result = write_run(
            None,
            APPLY_CHANGES_CREATE_BATCH_BLOCKS,
            START..START + 4 * cap + 17,
            &mut BlockBuilder::new(),
            &Invocation::new(),
            encode_failing(1, 3, &seen),
            |frame| {
                frames.push(frame);
                Ok(())
            },
        );
        let Err(err) = result else {
            panic!("a run with failing blocks succeeded")
        };
        assert!(err.to_string().contains("block 1 failed"), "got {err}");
        assert_eq!(written_blocks(&frames), vec![0]);
        assert_eq!(
            seen.into_inner().expect("lock"),
            expected_ranges(5, 17)[..2].to_vec()
        );
    }

    /// A write failure ends the run where it happens: its error is
    /// returned and no later frame is offered to the writer.
    #[test]
    fn a_write_failure_stops_the_run_at_that_block() {
        let executor = pool();
        let cap = DEFAULT_ELEMENT_CAP.get();
        let mut offered = Vec::new();
        let result = write_run(
            Some(&executor),
            APPLY_CHANGES_CREATE_BATCH_BLOCKS,
            START..START + 4 * cap + 17,
            &mut BlockBuilder::new(),
            &Invocation::new(),
            |_, chunk| {
                Ok((
                    Some(block_of(&chunk).to_le_bytes().to_vec()),
                    chunk.len() as u64,
                ))
            },
            |frame| {
                offered.push(frame);
                if offered.len() == 2 {
                    return Err(crate::error::Error::internal("write 1 failed"));
                }
                Ok(())
            },
        );
        let Err(err) = result else {
            panic!("a run whose second write failed succeeded")
        };
        assert!(err.to_string().contains("write 1 failed"), "got {err}");
        assert_eq!(written_blocks(&offered), vec![0, 1]);
    }

    /// A successful run writes every frame in block order, skips a block
    /// that frames to nothing, and returns every element encoded, on the
    /// executor and on the calling thread alike; a run of one block with an
    /// executor present is encoded on the calling thread.
    #[test]
    fn a_successful_run_writes_every_block_in_order_and_counts_its_elements() {
        let executor = pool();
        let cap = DEFAULT_ELEMENT_CAP.get();
        let len = 4 * cap + 17;
        for executor in [Some(&executor), None] {
            let mut frames = Vec::new();
            let added = write_run(
                executor,
                APPLY_CHANGES_CREATE_BATCH_BLOCKS,
                START..START + len,
                &mut BlockBuilder::new(),
                &Invocation::new(),
                |_, chunk| {
                    let block = block_of(&chunk);
                    let frame = (block != 2).then(|| block.to_le_bytes().to_vec());
                    Ok((frame, chunk.len() as u64))
                },
                |frame| {
                    frames.push(frame);
                    Ok(())
                },
            )
            .expect("run");
            assert_eq!(added, len as u64, "executor={}", executor.is_some());
            assert_eq!(written_blocks(&frames), vec![0, 1, 3, 4]);
        }

        let caller = std::thread::current().id();
        let added = write_run(
            Some(&executor),
            APPLY_CHANGES_CREATE_BATCH_BLOCKS,
            START..START + cap,
            &mut BlockBuilder::new(),
            &Invocation::new(),
            |_, chunk| {
                assert_eq!(
                    std::thread::current().id(),
                    caller,
                    "a one-block run is encoded on the calling thread"
                );
                Ok((None, chunk.len() as u64))
            },
            |_| Ok(()),
        )
        .expect("one-block run");
        assert_eq!(added, cap as u64);
    }
}
