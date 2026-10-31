//! The one engine of the block-pair merge over two indexed inputs, for the
//! text diff and derive (`diff --format osc`) alike, at every `-j`.
//!
//! A planner thread reads each input once, in file order, as one
//! sequential stream ([`BlobCursor`]: every header checked as it arrives,
//! `--direct-io` and page-cache eviction honoured), and cuts each kind into
//! work units: an OSM-order key [`Window`] `[start, end)` and the frames of
//! both inputs that intersect it. Units run on the ordered pool
//! ([`run_ordered_streams_from`]); each worker merges its unit
//! ([`block_pair_merge_phase`]) and streams its output in bounded chunks,
//! which the calling thread hands to the command in unit order. Nothing is
//! written to scratch, and no header is walked ahead of the merge.
//!
//! # Cuts
//!
//! A unit of kind `k` takes frames from both inputs in order of their
//! claimed first keys, and closes once it holds [`DIFF_UNIT_TARGET_BYTES`]
//! at the next cut its closure rule allows: the end is the smaller of the
//! two inputs' next first keys, and it must lie past every first key the
//! unit took, so every frame intersecting `[start, end)` is in the unit and
//! every later frame lies at or past `end` (blobs ascend, and a claimed
//! first key bounds a blob's elements from below). A frame whose claim
//! reaches `end` or beyond is carried into the next unit too, sharing one
//! allocation, and each unit clips it to its own window, so every element
//! falls in exactly one unit and a carried blob is read once but decoded
//! once per unit it touches. Cuts may fall at either input's blobs, so a
//! long one-sided stretch, a kind one input lacks and a huge blob on one
//! side all cut normally. Keys rather than raw IDs order negative IDs the
//! way the merge does; a mixed-sign blob's claimed first key is
//! conservatively key 0, which can only delay a cut.
//!
//! The only stretch no cut can split is a run of blobs starting at one ID
//! (duplicate IDs across blobs). A unit that reaches
//! [`DIFF_UNIT_CAP_BYTES`] without a cut is refused as a resource limit,
//! naming the ID; that is far above what duplicate-free input reaches.
//!
//! # Trust
//!
//! Membership and clipping trust indexdata ranges, as everywhere in the
//! block-pair engine (reference/indexdata-trust.md). A block holding
//! elements past its claimed last ID is refused when the merge meets them,
//! unless a cut hides them: a block whose claim crosses a window's end is
//! clipped there, and what it holds past its claim is then in no unit.
//!
//! # Progress and failures
//!
//! The planner blocks only on its bounded queue; a worker holds its whole
//! unit and never waits on the planner; the calling thread pulls the next
//! unit only when the pool's window has room. A planner failure (an input
//! defect, a cancellation) becomes the run's failure at the position of the
//! unit it did not produce, after every earlier unit, so an earlier unit's
//! own failure is reported first. A planner that ends without a terminal
//! message panicked; its panic is surfaced at the join.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use crate::ElementKind;
use crate::error::{Error, Result};
use crate::ordered_pool::{Emitter, StreamConfig, StreamConsumer, run_ordered_streams_from};
use crate::osc::merge_join::{
    BlobCursor, BlockPairMergeStats, BlockPairSide, Frame, MergeSink, Window,
    block_pair_merge_phase,
};
use crate::osm_id::OsmIdKey;
use crate::read::input::PinnedInput;
use crate::tunables::{
    DIFF_OUTPUT_CHUNK_DEPTH, DIFF_UNIT_CAP_BYTES, DIFF_UNIT_QUEUE_DEPTH, DIFF_UNIT_TARGET_BYTES,
    DIFF_UNIT_WINDOW_SLACK,
};

/// One kind's pass and its sidecar markers.
pub(super) struct Phase {
    pub(super) kind: ElementKind,
    pub(super) start_marker: &'static str,
    pub(super) end_marker: &'static str,
}

/// A command's worker-side sink: it records one unit's merge actions,
/// gathering output it hands over in chunks.
pub(super) trait UnitSink: MergeSink {
    /// One chunk of output.
    type Item: Send;
    /// The unit's counts.
    type Stats: Send;

    /// A gathered chunk, once one is due; called after every action until it
    /// answers `None`.
    fn take_chunk(&mut self) -> Option<Self::Item>;

    /// The unit is merged: its remaining output and its counts.
    fn finish(self) -> (Vec<Self::Item>, Self::Stats);
}

/// A command's side of the run, on the calling thread: it receives every
/// unit's chunks and counts in unit order.
pub(super) trait UnitConsumer {
    type Sink: UnitSink;

    /// Names a worker panic in the error it becomes.
    const WORKER: &'static str;

    /// A chunk of the current unit's output.
    fn item(&mut self, item: <Self::Sink as UnitSink>::Item) -> Result<()>;

    /// The current unit's counts, after its last chunk.
    fn finish_unit(&mut self, stats: <Self::Sink as UnitSink>::Stats) -> Result<()>;

    /// A kind's units are all consumed: how many there were, and the most
    /// and fewest blobs one held (sidecar counters).
    fn on_kind_end(
        &mut self,
        _kind: ElementKind,
        _units: usize,
        _max_blobs: usize,
        _min_blobs: usize,
        _invocation: &crate::host::Recorder,
    ) {
    }

    /// The fault plan's unit-panic hook for this command.
    #[cfg(feature = "test-hooks")]
    fn panic_hook(faults: &crate::test_hooks::FaultPlan) -> &std::sync::atomic::AtomicUsize;

    /// The panic message of an armed unit.
    #[cfg(feature = "test-hooks")]
    fn panic_message(idx: usize) -> String;
}

/// One work unit: a window of one kind and both inputs' frames meeting it.
struct Unit {
    kind: ElementKind,
    window: Window,
    old: Vec<Frame>,
    new: Vec<Frame>,
}

/// What a finished unit hands back besides its chunks.
struct UnitEnd<S> {
    stats: S,
    merge: BlockPairMergeStats,
}

/// The planner's messages, in order: units, then exactly one terminal.
enum PlanMsg {
    Unit(Unit),
    Done,
    Failed(Error),
}

/// Merge both pinned inputs over `phases`, at up to `jobs` workers, into
/// `consumer`. Returns the merge's shadow counters summed over every unit.
/// A phase whose kind neither input holds is skipped, markers included.
#[allow(clippy::too_many_arguments)]
pub(super) fn run<C: UnitConsumer>(
    old: &PinnedInput,
    new: &PinnedInput,
    phases: &[Phase],
    jobs: usize,
    direct_io: bool,
    skip_equal_blobs: bool,
    make_sink: &(dyn Fn() -> C::Sink + Sync),
    consumer: &mut C,
    host: &crate::host::Entered<'_>,
) -> Result<BlockPairMergeStats> {
    let old_cursor = BlobCursor::open(old.into(), direct_io, host)?;
    let new_cursor = BlobCursor::open(new.into(), direct_io, host)?;
    let old_path = Arc::clone(old_cursor.path());
    let new_path = Arc::clone(new_cursor.path());
    let kinds: Vec<ElementKind> = phases.iter().map(|p| p.kind).collect();
    let target = unit_target(&host.invocation);
    let workers = jobs.max(1);
    let invocation = &host.invocation;

    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<PlanMsg>(DIFF_UNIT_QUEUE_DEPTH);
        let planner = scope.spawn(move || {
            let mut cursors = [old_cursor, new_cursor];
            let msg = match plan(&mut cursors, &kinds, target, &tx, invocation) {
                Ok(()) => PlanMsg::Done,
                Err(e) => PlanMsg::Failed(e),
            };
            // A refused send means the consumer is gone; nothing to tell.
            tx.send(msg).ok();
        });

        let result = drive::<C>(
            rx,
            phases,
            workers,
            skip_equal_blobs,
            make_sink,
            consumer,
            (&old_path, &new_path),
            invocation,
        );
        // `drive` consumed the receiver, so a planner blocked sending has
        // already been released.
        match planner.join() {
            Ok(()) => result,
            Err(payload) => Err(crate::error::panic_error(
                "diff unit planner",
                payload.as_ref(),
            )),
        }
    })
}

/// The unit target, or the fault plan's override in a test-hooks build.
fn unit_target(invocation: &crate::host::Recorder) -> usize {
    #[cfg(feature = "test-hooks")]
    {
        let forced = invocation
            .faults()
            .diff_unit_target_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        if forced > 0 {
            return forced;
        }
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = invocation;
    DIFF_UNIT_TARGET_BYTES
}

/// The calling thread's side: pull units from the planner into the pool and
/// consume their output in unit order.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn drive<C: UnitConsumer>(
    rx: std::sync::mpsc::Receiver<PlanMsg>,
    phases: &[Phase],
    workers: usize,
    skip_equal_blobs: bool,
    make_sink: &(dyn Fn() -> C::Sink + Sync),
    consumer: &mut C,
    paths: (&Arc<std::path::Path>, &Arc<std::path::Path>),
    invocation: &crate::host::Recorder,
) -> Result<BlockPairMergeStats> {
    // The kind and blob count of every issued unit, in issue order, for the
    // consumer side to read as it reaches each unit (both on this thread).
    let issued: Rc<RefCell<VecDeque<(ElementKind, usize)>>> = Rc::default();
    let mut source = {
        let issued = Rc::clone(&issued);
        // A kind's start marker fires as its first unit is issued to the
        // pool, when its merge work can begin; its end marker fires once its
        // last unit is consumed. Kinds overlap between the two.
        let mut last_issued: Option<ElementKind> = None;
        move || -> Result<Option<Unit>> {
            match rx.recv() {
                Ok(PlanMsg::Unit(unit)) => {
                    if last_issued != Some(unit.kind) {
                        last_issued = Some(unit.kind);
                        if let Some(phase) = phases.iter().find(|p| p.kind == unit.kind) {
                            invocation.marker(phase.start_marker);
                        }
                    }
                    issued
                        .borrow_mut()
                        .push_back((unit.kind, unit.old.len() + unit.new.len()));
                    Ok(Some(unit))
                }
                Ok(PlanMsg::Done) => Ok(None),
                Ok(PlanMsg::Failed(e)) => Err(e),
                // The planner always ends with a terminal unless it
                // panicked; the join reports the panic itself.
                Err(_) => Err(Error::internal("diff unit planner ended without finishing")),
            }
        }
    };

    let mut adapter = Adapter {
        consumer,
        phases,
        issued,
        current: None,
        merge: BlockPairMergeStats::default(),
        invocation,
    };
    let (old_path, new_path) = paths;
    let config = StreamConfig {
        window: std::num::NonZeroUsize::new(workers + DIFF_UNIT_WINDOW_SLACK)
            .unwrap_or(std::num::NonZeroUsize::MIN),
        depth: DIFF_OUTPUT_CHUNK_DEPTH,
        cancel: invocation,
    };
    let work = |(): &mut (),
                task: usize,
                unit: Unit,
                emitter: &Emitter<
        '_,
        <C::Sink as UnitSink>::Item,
        UnitEnd<<C::Sink as UnitSink>::Stats>,
    >|
     -> Result<UnitEnd<<C::Sink as UnitSink>::Stats>> {
        // A panic in a unit becomes that unit's failure, in unit order. A
        // unit waits on nothing outside the pool, so no other task can be
        // left parked behind it.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Test-only: the fault plan's unit-panic hook simulates a unit
            // worker crash mid-run. Release builds compile this out.
            #[cfg(feature = "test-hooks")]
            if C::panic_hook(invocation.faults()).load(std::sync::atomic::Ordering::Relaxed) == task
            {
                panic!("{}", C::panic_message(task));
            }
            #[cfg(not(feature = "test-hooks"))]
            let _ = task;
            merge_unit(
                &unit,
                (old_path, new_path),
                skip_equal_blobs,
                make_sink(),
                emitter,
                invocation,
            )
        }))
        .unwrap_or_else(|payload| Err(crate::error::panic_error(C::WORKER, payload.as_ref())))
    };
    run_ordered_streams_from(&mut source, workers, config, |_| (), work, &mut adapter)?;
    adapter.close_kind();
    Ok(adapter.merge)
}

/// Merge one unit on a worker, streaming its chunks through `emitter`.
fn merge_unit<S: UnitSink>(
    unit: &Unit,
    (old_path, new_path): (&Arc<std::path::Path>, &Arc<std::path::Path>),
    skip_equal_blobs: bool,
    sink: S,
    emitter: &Emitter<'_, S::Item, UnitEnd<S::Stats>>,
    cancel: &crate::host::Recorder,
) -> Result<UnitEnd<S::Stats>> {
    let mut old_side = BlockPairSide::new(&unit.old, old_path, unit.window);
    let mut new_side = BlockPairSide::new(&unit.new, new_path, unit.window);
    let mut chunking = Chunking { sink, emitter };
    let mut merge = BlockPairMergeStats::default();
    block_pair_merge_phase(
        &mut old_side,
        &mut new_side,
        unit.kind,
        skip_equal_blobs,
        &mut chunking,
        &mut merge,
        cancel,
    )?;
    let (rest, stats) = chunking.sink.finish();
    for item in rest {
        emitter.send(item)?;
    }
    Ok(UnitEnd { stats, merge })
}

/// A unit sink whose due chunks go straight to the consumer.
struct Chunking<'e, 'a, S: UnitSink> {
    sink: S,
    emitter: &'e Emitter<'a, S::Item, UnitEnd<S::Stats>>,
}

impl<S: UnitSink> MergeSink for Chunking<'_, '_, S> {
    fn record(
        &mut self,
        kind: ElementKind,
        action: &crate::osc::merge_join::BlockMergeAction<'_>,
    ) -> Result<()> {
        self.sink.record(kind, action)?;
        while let Some(item) = self.sink.take_chunk() {
            self.emitter.send(item)?;
        }
        Ok(())
    }
}

/// The calling thread's consumer: phase markers and per-kind counters
/// around the command's own consumer.
struct Adapter<'a, C: UnitConsumer> {
    consumer: &'a mut C,
    phases: &'a [Phase],
    issued: Rc<RefCell<VecDeque<(ElementKind, usize)>>>,
    /// The kind being consumed, its unit count and blob extremes.
    current: Option<KindTally>,
    merge: BlockPairMergeStats,
    invocation: &'a crate::host::Recorder,
}

struct KindTally {
    kind: ElementKind,
    units: usize,
    max_blobs: usize,
    min_blobs: usize,
}

impl<C: UnitConsumer> Adapter<'_, C> {
    fn phase(&self, kind: ElementKind) -> Option<&Phase> {
        self.phases.iter().find(|p| p.kind == kind)
    }

    /// End the running kind: its end marker and counters.
    fn close_kind(&mut self) {
        if let Some(tally) = self.current.take() {
            self.consumer.on_kind_end(
                tally.kind,
                tally.units,
                tally.max_blobs,
                tally.min_blobs,
                self.invocation,
            );
            if let Some(phase) = self.phase(tally.kind) {
                self.invocation.marker(phase.end_marker);
            }
        }
    }
}

impl<C: UnitConsumer>
    StreamConsumer<<C::Sink as UnitSink>::Item, UnitEnd<<C::Sink as UnitSink>::Stats>>
    for Adapter<'_, C>
{
    fn before_task(&mut self, _task: usize) -> Result<()> {
        let Some((kind, blobs)) = self.issued.borrow_mut().pop_front() else {
            return Err(Error::internal(
                "diff: a unit was consumed that was never issued",
            ));
        };
        if self.current.as_ref().is_none_or(|t| t.kind != kind) {
            // The start marker fired when this kind's first unit was issued.
            self.close_kind();
            self.current = Some(KindTally {
                kind,
                units: 0,
                max_blobs: 0,
                min_blobs: usize::MAX,
            });
        }
        if let Some(tally) = self.current.as_mut() {
            tally.units += 1;
            tally.max_blobs = tally.max_blobs.max(blobs);
            tally.min_blobs = tally.min_blobs.min(blobs);
        }
        Ok(())
    }

    fn item(&mut self, _task: usize, item: <C::Sink as UnitSink>::Item) -> Result<()> {
        self.consumer.item(item)
    }

    fn finish_task(
        &mut self,
        _task: usize,
        end: UnitEnd<<C::Sink as UnitSink>::Stats>,
    ) -> Result<()> {
        self.merge.add(&end.merge);
        self.consumer.finish_unit(end.stats)
    }
}

// ---------------------------------------------------------------------------
// The planner
// ---------------------------------------------------------------------------

/// Cut every phase's kind into units and send them, then walk the rest of
/// both inputs so a defect past the last selected kind is still refused.
fn plan(
    cursors: &mut [BlobCursor; 2],
    kinds: &[ElementKind],
    target: usize,
    tx: &std::sync::mpsc::SyncSender<PlanMsg>,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    for &kind in kinds {
        // Both, not short-circuited: each cursor passes the earlier kinds.
        let [old, new] = cursors;
        let old_has = old.reaches(kind)?;
        let new_has = new.reaches(kind)?;
        if !old_has && !new_has {
            continue;
        }
        if !plan_kind(cursors, kind, target, tx, cancel)? {
            return Ok(());
        }
    }
    let [old, new] = cursors;
    old.finish()?;
    new.finish()
}

/// Cut one kind into units. `Ok(false)` when the consumer is gone.
fn plan_kind(
    cursors: &mut [BlobCursor; 2],
    kind: ElementKind,
    target: usize,
    tx: &std::sync::mpsc::SyncSender<PlanMsg>,
    cancel: &crate::host::Recorder,
) -> Result<bool> {
    let mut start: Option<OsmIdKey> = None;
    let mut carry: [Vec<Frame>; 2] = [Vec::new(), Vec::new()];
    loop {
        let mut sides = std::mem::take(&mut carry);
        let mut bytes: usize = sides.iter().flatten().map(|f| f.body.len()).sum();
        // The largest first key of a frame this unit took (carried frames
        // start before `start`, so they never bound the cut).
        let mut max_first: Option<OsmIdKey> = None;
        let end = loop {
            cancel.check()?;
            let peeks = [cursors[0].peek(kind)?, cursors[1].peek(kind)?];
            let next = peeks
                .iter()
                .enumerate()
                .filter_map(|(side, p)| p.map(|d| (d.first_key(), side)))
                .min();
            let Some((next_first, side)) = next else {
                // The kind is over on both inputs.
                break None;
            };
            if bytes >= target && max_first.is_some_and(|m| next_first > m) {
                break Some(next_first);
            }
            if bytes >= DIFF_UNIT_CAP_BYTES {
                return Err(unsplittable(kind, next_first, bytes));
            }
            let Some(frame) = cursors[side].next_of(kind)? else {
                return Err(Error::internal(
                    "diff unit planner: a peeked blob could not be read",
                ));
            };
            bytes += frame.body.len();
            max_first =
                Some(max_first.map_or(frame.desc.first_key(), |m| m.max(frame.desc.first_key())));
            sides[side].push(frame);
        };
        let window = Window { start, end };
        if let Some(end) = end {
            for (side, frames) in sides.iter().enumerate() {
                carry[side] = frames
                    .iter()
                    .filter(|f| f.desc.last_key() >= end)
                    .cloned()
                    .collect();
            }
        }
        let [old, new] = sides;
        let unit = Unit {
            kind,
            window,
            old,
            new,
        };
        if tx.send(PlanMsg::Unit(unit)).is_err() {
            return Ok(false);
        }
        match end {
            Some(end) => start = Some(end),
            None => return Ok(true),
        }
    }
}

/// The refusal of a unit no cut can close within the cap.
#[cold]
fn unsplittable(kind: ElementKind, at: OsmIdKey, bytes: usize) -> Error {
    Error::invalid_data(format!(
        "block-pair merge: cannot cut the {} stream into work units: {bytes} bytes of \
         blobs start at or before {} {} without a point to cut between them (one ID \
         repeated across that many blobs). This is a resource limit of the \
         comparison, not a verdict on the file.",
        kind.as_str(),
        kind.as_str(),
        crate::osm_id::osm_id_from_key(at),
    ))
}
