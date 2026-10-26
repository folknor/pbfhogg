//! Blob-level permutation merge: the one engine behind `sort` (one input,
//! every element kept) and `cat --dedupe` (any number of inputs, exact
//! duplicates dropped).
//!
//! Pass 1 ([`index_inputs`]) walks every input's blob headers with
//! [`HeaderWalker`] and builds one [`BlobEntry`] per OSMData blob. [`plan`]
//! orders the entries by (kind, first key), flags the blobs whose ID ranges
//! overlap a same-kind neighbour plus those found internally out of order,
//! and groups the flagged ones into kind-bounded overlap runs. Pass 2
//! ([`write_plan`]) writes the entries in order: a blob outside every run is
//! copied through raw (coalesced into one `copy_file_range` per contiguous
//! stretch of one input where the output can splice), and each overlap run is
//! decoded and merged through a binary heap and re-encoded.
//!
//! Overlap runs are swept on worker threads and stream their blocks to the
//! write loop in run order, through a bounded channel per run and an
//! admission window over the runs (see [`write_plan`] for the memory bound
//! and the shutdown protocol). A run is one serial sweep however large, so a
//! single giant overlap component streams rather than being held whole.
//!
//! The commands differ only in their [`IndexPolicy`] and in whether the sweep
//! drops exact duplicates; their markers, counters and diagnostic keys stay
//! in the command modules.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::os::unix::fs::FileExt as _;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use crate::Element;
use crate::ElementKind;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{BlobIndex, scan_block_ids_checked};
use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::ordered_pool::{CloseOnDrop, OrderedGate};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, read_dense_node, read_node, read_relation, read_way,
    write_single_node_local, write_single_relation_local, write_single_way_local,
};
use crate::read::header_walker::{BlobHeaderMeta, HeaderWalker};
use crate::read::input::{InputFile, PinnedInput};
use crate::scan::classify::recv_checked;
use crate::writer::{PbfWriter, reframe_raw_with_index};

// ---------------------------------------------------------------------------
// Blob index
// ---------------------------------------------------------------------------

/// One OSMData blob of one input.
pub(crate) struct BlobEntry {
    /// Which input (an index into the slice handed to [`index_inputs`], and
    /// into the sources pass 2 opens from it).
    pub(crate) source: usize,
    /// Byte offset of the complete frame in its input.
    pub(crate) file_offset: u64,
    /// Length of the complete frame (4 + header_len + data_size).
    pub(crate) frame_len: u64,
    /// Element kind + ID range.
    pub(crate) index: BlobIndex,
    /// Whether the BlobHeader already carries indexdata.
    pub(crate) has_indexdata: bool,
    /// Per-blob tag key data from BlobHeader field 4, kept for a reframe.
    pub(crate) tagdata: Option<Box<[u8]>>,
    /// True when this blob's elements are internally out of canonical OSM ID
    /// order. Set by the payload scan, which runs for every blob except an
    /// indexed one under a header claiming `Sort.Type_then_ID`: intra-blob
    /// order there is a precondition of the claim itself (see
    /// CORRECTNESS.md). A flagged blob is routed into the rewrite path so the
    /// sweep reorders it, even when its range overlaps no neighbour.
    pub(crate) intra_unsorted: bool,
    /// True when `index` was derived by decoding the payload, false when it
    /// was read out of the BlobHeader and believed. Only the element counters
    /// need this: a passthrough blob's `index.count` on the header-only path
    /// is the file's own unverified claim, reported apart from the observed
    /// counts (`passthrough_claimed`).
    pub(crate) index_scanned: bool,
}

/// What pass 1 accepts and refuses: the part of a command's input contract
/// the shared walk enforces.
pub(crate) struct IndexPolicy {
    /// Refuse an input whose header does not claim `Sort.Type_then_ID`, with
    /// this reason (`cat --dedupe`, which promises a merge of sorted inputs).
    pub(crate) require_sorted: Option<&'static str>,
    /// Refuse a partially indexed input (`HeaderWalker::refusing_partial_index`).
    /// `sort` does; `cat --dedupe` accepts one, scanning each unindexed blob
    /// (reference/indexdata-trust.md, "Missing and partial indexdata").
    pub(crate) refuse_partial_index: bool,
}

/// Pass 1: walk every input, in order, and index its OSMData blobs.
///
/// Returns one header per input, in input order, and the entries in input
/// then file order. Each input must start with its one OSMHeader: a first
/// frame of any other type is the missing-header error at once, and a later
/// OSMHeader is refused by the walk's shared framing policy
/// (`read::frame::check_header_position`) rather than ignored, because what
/// it declares would otherwise be silently left out of the output header.
///
/// The walk preads headers only (`fadvise(RANDOM)`), never pulling skipped
/// payloads into the page cache: `sort`'s planet pass 1 measured 32 GB read
/// through a buffered reader against 674 MB walked (reference/
/// performance-history.md, "Sort"). An indexed blob under a header claiming
/// `Sort.Type_then_ID` is indexed from its indexdata alone. Every other blob
/// is preaded, decompressed and scanned, which also checks its intra-blob
/// order: indexdata alone does not prove it (`cat` attaches indexdata to
/// payloads it never reorders), and the trusted ordering signal is the header
/// claim, the contract `ElementReader` keys its guarantees on.
#[hotpath::measure]
pub(crate) fn index_inputs(
    inputs: &[PinnedInput],
    policy: &IndexPolicy,
    host: &crate::host::Host,
) -> Result<(Vec<crate::HeaderBlock>, Vec<BlobEntry>)> {
    let mut walk = Walk {
        policy,
        host,
        entries: Vec::new(),
        data_buf: Vec::new(),
        decompress_buf: Vec::new(),
        noted_unclaimed: false,
    };
    let mut headers = Vec::with_capacity(inputs.len());
    for (source, input) in inputs.iter().enumerate() {
        headers.push(walk.input(source, input)?);
    }
    Ok((headers, walk.entries))
}

/// Pass-1 state shared across every input of a run.
struct Walk<'a> {
    policy: &'a IndexPolicy,
    host: &'a crate::host::Host,
    entries: Vec<BlobEntry>,
    data_buf: Vec<u8>,
    decompress_buf: Vec<u8>,
    noted_unclaimed: bool,
}

impl Walk<'_> {
    fn input(&mut self, source: usize, input: &PinnedInput) -> Result<crate::HeaderBlock> {
        let mut walker = HeaderWalker::open(input, self.host)?;
        if self.policy.refuse_partial_index {
            walker = walker.refusing_partial_index();
        }
        let mut header: Option<crate::HeaderBlock> = None;
        while let Some(meta) = walker.next_header()? {
            let claims_sorted = header.as_ref().map(crate::HeaderBlock::is_sorted);
            match meta.blob_type {
                // The walk refuses an OSMHeader anywhere but first.
                BlobKind::OsmHeader => header = Some(self.header(&walker, &meta)?),
                _ if claims_sorted.is_none() => {
                    return Err(crate::error::new_missing_header(input.path()));
                }
                BlobKind::OsmData => {
                    let entry =
                        self.data_blob(source, &walker, meta, claims_sorted == Some(true))?;
                    self.entries.push(entry);
                }
                // Unknown extension blob: the walk has already stepped past it.
                _ => {}
            }
        }
        header.ok_or_else(|| crate::error::new_missing_header(input.path()))
    }

    fn header(
        &mut self,
        walker: &HeaderWalker,
        meta: &BlobHeaderMeta,
    ) -> Result<crate::HeaderBlock> {
        let located = |e| walker.locate_meta(meta, e);
        walker
            .pread_data(meta.data_offset, meta.data_size, &mut self.data_buf)
            .map_err(located)?;
        let hdr = decode_blob_to_headerblock(&self.data_buf).map_err(located)?;
        if let Some(reason) = self.policy.require_sorted {
            let path = walker.path();
            crate::commands::require_sorted(
                hdr.is_sorted(),
                path,
                &format!("Input file {}", path.display()),
                reason,
            )
            .map_err(located)?;
        }
        Ok(hdr)
    }

    fn data_blob(
        &mut self,
        source: usize,
        walker: &HeaderWalker,
        mut meta: BlobHeaderMeta,
        claims_sorted: bool,
    ) -> Result<BlobEntry> {
        let has_indexdata = meta.index.is_some();
        let (index, intra_unsorted, index_scanned) =
            match meta.index.take().filter(|_| claims_sorted) {
                Some(index) => (index, false, false),
                None => {
                    if has_indexdata && !self.noted_unclaimed {
                        self.noted_unclaimed = true;
                        // Only `sort` reaches this: `cat --dedupe` refuses an
                        // input whose header does not claim sorted.
                        crate::host::diag!(
                            self.host.diagnostics,
                            Note,
                            "sort.verify_unclaimed_order",
                            "input does not declare Sort.Type_then_ID; verifying \
                         intra-blob order (pass 1 decodes every blob payload)"
                        );
                    }
                    let (index, sorted) = self
                        .scan_payload(walker, &meta)
                        .map_err(|e| walker.locate_meta(&meta, e))?;
                    (index, !sorted, true)
                }
            };
        Ok(BlobEntry {
            source,
            file_offset: meta.frame_start,
            frame_len: meta.frame_size as u64,
            index,
            has_indexdata,
            tagdata: meta.tagdata,
            intra_unsorted,
            index_scanned,
        })
    }

    /// Pread, decompress and scan one payload's IDs, checking intra-blob
    /// monotonicity on the way (one compare per element, on a scan that
    /// visits every ID anyway). A blob no single index describes (empty, or
    /// kinds mixed) cannot be placed and is refused.
    fn scan_payload(
        &mut self,
        walker: &HeaderWalker,
        meta: &BlobHeaderMeta,
    ) -> Result<(BlobIndex, bool)> {
        walker.pread_data(meta.data_offset, meta.data_size, &mut self.data_buf)?;
        decompress_blob_data_into(&self.data_buf, &mut self.decompress_buf)?;
        // The collapsing scan is safe here although a reframed passthrough
        // carries this index: `None` - malformed bytes or a blob no single
        // index describes - refuses the run either way.
        scan_block_ids_checked(&self.decompress_buf)
            .ok_or_else(|| Error::invalid_data("failed to scan block IDs"))
    }
}

// ---------------------------------------------------------------------------
// Plan: order, overlap detection, runs
// ---------------------------------------------------------------------------

/// One rewrite run, `entries[start..end]`, all of `kind`: an overlap
/// component, or one internally unsorted blob that overlaps nothing.
pub(crate) struct Run {
    start: usize,
    end: usize,
    kind: ElementKind,
}

/// The entries in output order and what pass 2 does with each.
pub(crate) struct Plan {
    pub(crate) entries: Vec<BlobEntry>,
    /// Per entry: rewritten in an overlap run, rather than copied through.
    pub(crate) rewrite: Vec<bool>,
    /// Blobs in genuine range-overlap runs.
    pub(crate) overlapping: usize,
    /// Blobs routed to rewrite only for intra-blob disorder (disjoint from
    /// `overlapping`).
    pub(crate) intra_unsorted: usize,
    pub(crate) runs: Vec<Run>,
}

/// Order the entries by (kind, first key), find the overlap runs and fold
/// in intra-blob disorder.
///
/// The sort is stable, so entries with equal keys keep input then file
/// order, which the sweep's heap pushes (and so which of two exact
/// duplicates survives) depend on.
pub(crate) fn plan(mut entries: Vec<BlobEntry>) -> Plan {
    entries.sort_by_key(|e| {
        (
            type_order(e.index.kind),
            crate::osm_id::blob_osm_first_key(e.index.min_id, e.index.max_id),
        )
    });
    let mut rewrite = vec![false; entries.len()];
    let mut runs = Vec::new();
    let (mut overlapping, mut intra_unsorted) = (0, 0);
    let mut components = overlap_components(&entries).into_iter().peekable();
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        if let Some(&(start, end)) = components.peek()
            && start == i
        {
            components.next();
            rewrite[start..end].fill(true);
            overlapping += end - start;
            runs.push(Run { start, end, kind });
            i = end;
            continue;
        }
        // Disorder inside a blob that overlaps nothing: rewritten alone, so
        // the sweep reorders it. A blob both unsorted and overlapping is in
        // its component already and counted there, keeping the two
        // attributions disjoint.
        if entries[i].intra_unsorted {
            rewrite[i] = true;
            intra_unsorted += 1;
            runs.push(Run {
                start: i,
                end: i + 1,
                kind,
            });
        }
        i += 1;
    }
    Plan {
        entries,
        rewrite,
        overlapping,
        intra_unsorted,
        runs,
    }
}

impl Plan {
    /// What the output's way blobs are, for the output header: a way blob in
    /// a rewrite run is re-encoded without enrichments, an unindexed
    /// passthrough way blob is reframed (field 5 dropped), every other way
    /// blob is copied whole.
    pub(crate) fn ways(&self) -> crate::commands::output_header::Ways {
        let way_blobs = || {
            self.entries
                .iter()
                .zip(&self.rewrite)
                .filter(|(e, _)| e.index.kind == ElementKind::Way)
        };
        crate::commands::output_header::Ways::from_copy(
            way_blobs().any(|(_, &r)| r),
            way_blobs().any(|(e, &r)| !r && !e.has_indexdata),
        )
    }
}

fn type_order(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Node => 0,
        ElementKind::Way => 1,
        ElementKind::Relation => 2,
    }
}

/// The range-overlap components, each `(start, end)` with at least two
/// entries.
///
/// Entries arrive sorted by first key within a kind. The test is a
/// running-maximum sweep, not a pairwise-adjacent comparison, because overlap
/// is **not transitive**: with A=[1..1000], B=[2..3], C=[500..600], A/B overlap
/// but B/C do not (`B.max=3 < C.min=500`), so a pairwise scan leaves C
/// unflagged. The sweep-merge then emits A's ids 500..1000 and C follows as a
/// raw passthrough blob carrying ids 500..600 - output that is not sorted while
/// the header still stamps `Sort.Type_then_ID`, and (in `cat --dedupe`) A/C
/// duplicates that the "exact duplicates removed" contract promised to drop.
///
/// Extending the run while `next.first_key <= run_max` closes that: once a blob
/// joins a run, every later blob whose first key falls inside the run's widest
/// reach so far joins too. Components stay kind-bounded - the downstream
/// sweep is kind-gated and silently drops off-kind elements handed to it
/// (the `cat --dedupe` regression where `merge_pbf([A, A])` lost its ways
/// and relations).
///
/// Each component is its own run, swept on its own: the next component of a
/// kind starts above this one's widest reach, so no element and no equal
/// (id, version) pair crosses the boundary, and sweeping them apart emits the
/// same element sequence as one sweep over both (that sweep empties its heap
/// at the boundary anyway). What the boundary does change is blocks: each run
/// flushes its builder, so adjacent components no longer share an output
/// block - and no longer share `BlockBuilder`'s backfill of default metadata
/// when one carries node metadata and the other does not. That is what lets
/// a file merged with itself, all two-blob components, sweep in parallel.
fn overlap_components(entries: &[BlobEntry]) -> Vec<(usize, usize)> {
    let mut components = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        let mut run_max =
            crate::osm_id::blob_osm_last_key(entries[i].index.min_id, entries[i].index.max_id);
        let mut j = i + 1;
        while j < entries.len()
            && entries[j].index.kind == kind
            && crate::osm_id::blob_osm_first_key(entries[j].index.min_id, entries[j].index.max_id)
                <= run_max
        {
            run_max = run_max.max(crate::osm_id::blob_osm_last_key(
                entries[j].index.min_id,
                entries[j].index.max_id,
            ));
            j += 1;
        }
        if j > i + 1 {
            components.push((i, j));
            i = j;
        } else {
            i += 1;
        }
    }
    components
}

// ---------------------------------------------------------------------------
// Pass 2
// ---------------------------------------------------------------------------

/// Open pass 2's inputs: one positional-read source per argument, from its
/// pin, consuming the pins and dropping each as soon as its source is open
/// (a repeated argument's shared pin goes with its last argument). An open,
/// verified source reads its file whatever its path names later, so the run
/// holds about one input descriptor per argument from here on, which is what
/// `cat --dedupe` asks the host for. The walk's `FADV_RANDOM` descriptions
/// are closed by now; these are fresh ones.
pub(crate) fn open_sources(inputs: Vec<PinnedInput>) -> Result<Vec<InputFile>> {
    let mut sources = Vec::with_capacity(inputs.len());
    for pin in inputs {
        sources.push(InputFile::open(&pin)?);
    }
    Ok(sources)
}

/// What pass 2 did.
#[derive(Default)]
pub(crate) struct WriteCounts {
    /// Observed counts: elements a sweep emitted, plus passthrough blobs
    /// whose index pass 1 scanned from the payload.
    pub(crate) nodes: u64,
    pub(crate) ways: u64,
    pub(crate) relations: u64,
    /// Elements in passthrough blobs counted from their own unverified
    /// indexdata.
    pub(crate) passthrough_claimed: u64,
    pub(crate) blobs_passthrough: u64,
    pub(crate) blobs_rewritten: u64,
    pub(crate) duplicates_removed: u64,
    pub(crate) copy_range_calls: u64,
    pub(crate) copy_range_coalesced: u64,
}

/// What one overlap run emitted.
#[derive(Debug, Default)]
struct RunCounts {
    elements: u64,
    duplicates: u64,
}

/// A message on one overlap run's channel. A run ends with exactly one of
/// `Finished` or `Failed`; a channel that closes without either is a worker
/// that died mid-run, never a short run.
enum RunMsg {
    Block(OwnedBlock),
    Finished(RunCounts),
    Failed(Error),
}

/// Pass 2: write the plan in order to `writer`.
///
/// `dedupe` drops, inside overlap runs, an element whose (id, version)
/// equals the one just emitted. `use_copy_range` is whether the output can
/// splice (`OutputBackend::supports_copy_range`).
///
/// With no overlap runs (an already-sorted input: production `sort`) this is
/// one serial loop and no thread starts. Otherwise the runs are swept on
/// worker threads (sized as the decode pools are) while this thread writes:
///
/// - A dispatcher walks the runs in order and admits run `k` only while
///   `k < drained + window`. For each it makes a channel of
///   [`OVERLAP_RUN_CHANNEL_DEPTH`] messages, queues the job (FIFO) and hands
///   the receiver to this thread over an ordered channel.
/// - A worker sweeps its run, sending each block as the builder fills it and
///   then the run's terminal message; a full channel parks it.
/// - This thread writes entries in order; at a run it takes the run's
///   receiver and writes its blocks as they arrive until the terminal message,
///   then credits the run.
///
/// Progress: when this thread waits on run `k`, every earlier run is drained
/// and run `k` was dequeued before any later one, so a worker is on it, and
/// that worker never waits on a later run. Memory: at most `window x depth`
/// blocks buffered plus one blocked send per worker, beside each active
/// sweep's heap (bounded by its run's overlap depth, as before; up to
/// `min(window, workers)` heaps at once). Shutdown: every receiver and the
/// consumer's gate guard live in the scope closure, so any return from it
/// (an error, a cancellation, a write failure) drops them before the join:
/// workers parked in a send fail it and stop, the dispatcher parked on the
/// gate wakes to it closed and drops the job queue, idle workers see it
/// close. The first error met in run order is returned, unless a worker
/// panicked, which the scope re-raises at the join.
///
/// [`OVERLAP_RUN_CHANNEL_DEPTH`]: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH
#[hotpath::measure]
pub(crate) fn write_plan(
    plan: &Plan,
    sources: &[InputFile],
    writer: &mut PbfWriter<FileWriter>,
    dedupe: bool,
    use_copy_range: bool,
    host: &crate::host::Host,
) -> Result<WriteCounts> {
    if plan.runs.is_empty() {
        return write_loop(plan, sources, writer, use_copy_range, |_, _| {
            Err(Error::internal("overlap run written with no runs planned"))
        });
    }

    let workers = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    )
    .clamp(1, plan.runs.len());
    let window = std::num::NonZeroUsize::new(
        workers.saturating_mul(crate::tunables::OVERLAP_RUN_WINDOW_PER_WORKER),
    )
    .unwrap_or(std::num::NonZeroUsize::MIN);
    // The ordered pool's admission gate, shared rather than the pool itself:
    // a run streams its blocks before it finishes, so it is not one result
    // per task. Only `close` is used (never `stop_claims`): a closed gate
    // admits nothing, which is this protocol's contract.
    let gate = OrderedGate::new();

    std::thread::scope(|scope| -> Result<WriteCounts> {
        let gate_ref = &gate;
        // Before any thread starts, so even a failed spawn unwinding out of
        // this closure closes the gate a parked dispatcher waits on. Every
        // receiver is declared after it and so drops first (locals drop in
        // reverse order): run receivers are gone before the gate closes,
        // and both before the scope joins.
        let _close = CloseOnDrop(gate_ref);
        let (job_tx, job_rx) = sync_channel::<(usize, SyncSender<RunMsg>)>(window.get());
        let job_rx = std::sync::Arc::new(std::sync::Mutex::new(job_rx));
        let (order_tx, order_rx) = sync_channel::<Receiver<RunMsg>>(window.get());

        scope.spawn(move || {
            for k in 0..plan.runs.len() {
                if !gate_ref.admit(k, window) {
                    break;
                }
                let (tx, rx) = sync_channel(crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH);
                if job_tx.send((k, tx)).is_err() || order_tx.send(rx).is_err() {
                    break;
                }
            }
        });

        for _ in 0..workers {
            let jobs = std::sync::Arc::clone(&job_rx);
            scope.spawn(move || {
                loop {
                    let job = jobs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv();
                    let Ok((k, tx)) = job else { break };
                    if !sweep_job(plan, &plan.runs[k], sources, dedupe, &tx, &host.cancel) {
                        // The write loop is gone: nothing will read this run
                        // or any later one.
                        break;
                    }
                }
            });
        }
        // Workers hold the only job receivers now, so their exit closes the
        // queue for a dispatcher still sending.
        drop(job_rx);

        let mut drained = 0_usize;
        write_loop(plan, sources, writer, use_copy_range, |k, writer| {
            let rx = recv_checked(&order_rx, &host.cancel)?
                .ok_or_else(|| Error::internal("overlap run dispatcher ended early"))?;
            debug_assert_eq!(k, drained, "overlap runs drained out of order");
            let counts = drain_run(&rx, &host.cancel, |block| {
                writer.write_primitive_block_owned(block)
            })?;
            drained += 1;
            gate_ref.advance(drained);
            Ok(counts)
        })
    })
}

/// Sweep run `run` and send its blocks and terminal message on `tx`.
/// Returns false when the receiver is gone (the write loop stopped).
fn sweep_job(
    plan: &Plan,
    run: &Run,
    sources: &[InputFile],
    dedupe: bool,
    tx: &SyncSender<RunMsg>,
    cancel: &crate::host::Cancel,
) -> bool {
    let mut receiver_gone = false;
    let result = sweep_run(
        &plan.entries[run.start..run.end],
        run.kind,
        sources,
        dedupe,
        |block| {
            if tx.send(RunMsg::Block(block)).is_err() {
                receiver_gone = true;
                return Err(Error::internal("overlap run receiver dropped"));
            }
            Ok(())
        },
        cancel,
    );
    if receiver_gone {
        return false;
    }
    let terminal = match result {
        Ok(counts) => RunMsg::Finished(counts),
        Err(e) => RunMsg::Failed(e),
    };
    tx.send(terminal).is_ok()
}

/// Hand one run's blocks to `write` as they arrive, until its terminal
/// message.
fn drain_run(
    rx: &Receiver<RunMsg>,
    cancel: &crate::host::Cancel,
    mut write: impl FnMut(OwnedBlock) -> Result<()>,
) -> Result<RunCounts> {
    loop {
        match recv_checked(rx, cancel)? {
            Some(RunMsg::Block(block)) => write(block)?,
            Some(RunMsg::Finished(counts)) => return Ok(counts),
            Some(RunMsg::Failed(e)) => return Err(e),
            None => {
                return Err(Error::internal(
                    "overlap worker ended a run without finishing it",
                ));
            }
        }
    }
}

/// The ordered write loop. `run_output(k, writer)` writes run `k`'s blocks
/// and returns its counts; it is called once per run, in order.
fn write_loop(
    plan: &Plan,
    sources: &[InputFile],
    writer: &mut PbfWriter<FileWriter>,
    use_copy_range: bool,
    mut run_output: impl FnMut(usize, &mut PbfWriter<FileWriter>) -> Result<RunCounts>,
) -> Result<WriteCounts> {
    let mut counts = WriteCounts::default();
    // Passthrough coalescer: adjacent blobs contiguous in one input collapse
    // into one `copy_file_range`. On already-sorted input this is the entire
    // file, cutting the syscall count from O(blobs) to O(1). Mirrors the
    // contiguous-range coalescer in apply_changes.
    let mut copy = CopyRun::default();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut next_run = 0_usize;
    let mut i = 0;
    while i < plan.entries.len() {
        if plan.rewrite[i] {
            copy.flush(writer, sources, &mut counts)?;
            let run = plan
                .runs
                .get(next_run)
                .filter(|run| run.start == i)
                .ok_or_else(|| Error::internal("rewrite entry outside its planned run"))?;
            let emitted = run_output(next_run, writer)?;
            match run.kind {
                ElementKind::Node => counts.nodes += emitted.elements,
                ElementKind::Way => counts.ways += emitted.elements,
                ElementKind::Relation => counts.relations += emitted.elements,
            }
            counts.duplicates_removed += emitted.duplicates;
            counts.blobs_rewritten += (run.end - run.start) as u64;
            next_run += 1;
            i = run.end;
        } else {
            let entry = &plan.entries[i];
            if entry.has_indexdata && use_copy_range {
                copy.offer(entry, writer, sources, &mut counts)?;
            } else {
                copy.flush(writer, sources, &mut counts)?;
                write_passthrough(entry, &sources[entry.source], writer, &mut frame_buf)?;
            }
            count_passthrough(entry, &mut counts);
            counts.blobs_passthrough += 1;
            i += 1;
        }
    }
    copy.flush(writer, sources, &mut counts)?;
    Ok(counts)
}

/// The in-flight `copy_file_range` stretch: (source, start, end).
#[derive(Default)]
struct CopyRun(Option<(usize, u64, u64)>);

impl CopyRun {
    /// Extend the stretch with `entry` when it continues it in the same
    /// input, otherwise flush the stretch and start a new one.
    fn offer(
        &mut self,
        entry: &BlobEntry,
        writer: &mut PbfWriter<FileWriter>,
        sources: &[InputFile],
        counts: &mut WriteCounts,
    ) -> Result<()> {
        match &mut self.0 {
            Some((source, _, end)) if *source == entry.source && *end == entry.file_offset => {
                *end += entry.frame_len;
                counts.copy_range_coalesced += 1;
            }
            _ => {
                self.flush(writer, sources, counts)?;
                self.0 = Some((
                    entry.source,
                    entry.file_offset,
                    entry.file_offset + entry.frame_len,
                ));
            }
        }
        Ok(())
    }

    /// Hand the stretch to the writer as one copy. No-op when none is open.
    fn flush(
        &mut self,
        writer: &mut PbfWriter<FileWriter>,
        sources: &[InputFile],
        counts: &mut WriteCounts,
    ) -> Result<()> {
        if let Some((source, start, end)) = self.0.take() {
            writer.write_raw_copy(&sources[source], start, end - start)?;
            counts.copy_range_calls += 1;
        }
        Ok(())
    }
}

/// Passthrough without the copy path: the frame raw when it carries
/// indexdata (an output that cannot splice, such as `--direct-io`), else
/// reframed with the index pass 1 scanned.
fn write_passthrough(
    entry: &BlobEntry,
    source: &InputFile,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut Vec<u8>,
) -> Result<()> {
    read_frame(source, entry, frame_buf)?;
    if entry.has_indexdata {
        writer.write_raw(frame_buf)?;
    } else {
        let reframed = blob_bytes(frame_buf)
            .and_then(|blob| {
                reframe_raw_with_index(blob, &entry.index.serialize(), entry.tagdata.as_deref())
            })
            .map_err(|e| source.locate_at(entry.file_offset, e))?;
        writer.write_raw(&reframed)?;
    }
    Ok(())
}

/// Count a passthrough blob: observed when pass 1 scanned its payload, a
/// claim when its count is the blob's own indexdata.
fn count_passthrough(entry: &BlobEntry, counts: &mut WriteCounts) {
    if !entry.index_scanned {
        counts.passthrough_claimed += entry.index.count;
        return;
    }
    match entry.index.kind {
        ElementKind::Node => counts.nodes += entry.index.count,
        ElementKind::Way => counts.ways += entry.index.count,
        ElementKind::Relation => counts.relations += entry.index.count,
    }
}

/// Re-read `entry`'s complete frame from its input. A frame that no longer
/// reads (the input changed between the passes) fails located at the input
/// and the frame's offset.
pub(crate) fn read_frame(source: &InputFile, entry: &BlobEntry, buf: &mut Vec<u8>) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let len = entry.frame_len as usize;
    buf.clear();
    buf.resize(len, 0);
    source
        .file()
        .read_exact_at(buf, entry.file_offset)
        .map_err(|e| source.locate_at(entry.file_offset, Error::from(e)))
}

/// Re-read and decode an overlap-run entry's frame, a read or decode
/// failure located at its input and frame offset.
pub(crate) fn read_block(
    source: &InputFile,
    entry: &BlobEntry,
    buf: &mut Vec<u8>,
) -> Result<crate::PrimitiveBlock> {
    read_frame(source, entry, buf)?;
    blob_bytes(buf)
        .and_then(decode_blob_to_primitiveblock)
        .map_err(|e| source.locate_at(entry.file_offset, e))
}

/// The Blob bytes of a complete frame, `[4-byte header_len][BlobHeader][Blob]`.
fn blob_bytes(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 4 {
        return Err(Error::invalid_data("frame too short"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(Error::invalid_data("invalid header length"));
    }
    Ok(&frame[blob_start..])
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Identity of an owned element in the sweep: OSM ID order, and for
/// `cat --dedupe` an exact duplicate is equal (type, id, version).
///
/// Absent metadata is version 0, the same mapping the `Owned*` `Ord` impls
/// use and libosmium's default version. That is element identity only; it is
/// unrelated to the OSC writers' rule that an absent version is not printed
/// as 0. An "exact duplicate" is equal in (type, id, version) and nothing
/// else: coordinates, tags, members and the rest of the metadata are not
/// compared, and the heap gives no input-order guarantee for which of two
/// such elements survives.
trait SweepElement: Ord {
    fn id(&self) -> i64;
    fn version(&self) -> i32;
}

impl SweepElement for OwnedNode {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl SweepElement for OwnedWay {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

impl SweepElement for OwnedRelation {
    fn id(&self) -> i64 {
        self.id
    }
    fn version(&self) -> i32 {
        self.metadata.as_ref().map_or(0, |m| m.version)
    }
}

/// Sweep one run of `kind`, handing each block to `emit` as it fills.
fn sweep_run(
    entries: &[BlobEntry],
    kind: ElementKind,
    sources: &[InputFile],
    dedupe: bool,
    emit: impl FnMut(OwnedBlock) -> Result<()>,
    cancel: &crate::host::Cancel,
) -> Result<RunCounts> {
    let sweep = Sweep {
        entries,
        sources,
        dedupe,
        cancel,
    };
    match kind {
        ElementKind::Node => sweep.run(
            |e, heap| {
                match e {
                    Element::DenseNode(dn) => heap.push(Reverse(read_dense_node(dn)?)),
                    Element::Node(n) => heap.push(Reverse(read_node(n)?)),
                    _ => {}
                }
                Ok(())
            },
            write_single_node_local,
            emit,
        ),
        ElementKind::Way => sweep.run(
            |e, heap| {
                if let Element::Way(w) = e {
                    heap.push(Reverse(read_way(w)?));
                }
                Ok(())
            },
            write_single_way_local,
            emit,
        ),
        ElementKind::Relation => sweep.run(
            |e, heap| {
                if let Element::Relation(r) = e {
                    heap.push(Reverse(read_relation(r)?));
                }
                Ok(())
            },
            write_single_relation_local,
            emit,
        ),
    }
}

struct Sweep<'a> {
    entries: &'a [BlobEntry],
    sources: &'a [InputFile],
    dedupe: bool,
    cancel: &'a crate::host::Cancel,
}

impl Sweep<'_> {
    /// Before each blob, emit every element below the blob's first ID; push
    /// the blob's elements; at the end, drain the heap. One `BlockBuilder`
    /// for the run, flushed at its end.
    ///
    /// The heap holds every element of the run not yet below the next blob's
    /// first ID, so it is bounded by the run's overlap width, not by the
    /// input: one blob whose range spans a kind's whole ID space makes it the
    /// whole kind. That is inherent to an in-memory sweep merge - it must
    /// hold whatever overlaps the current position - and bounding it would
    /// take an external merge. Streaming bounds only what the sweep has
    /// already re-encoded.
    fn run<T: SweepElement>(
        &self,
        mut extract: impl FnMut(&Element<'_>, &mut BinaryHeap<Reverse<T>>) -> Result<()>,
        mut write_elem: impl FnMut(&T, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> Result<()>,
        mut emit: impl FnMut(OwnedBlock) -> Result<()>,
    ) -> Result<RunCounts> {
        let mut heap: BinaryHeap<Reverse<T>> = BinaryHeap::new();
        let mut bb = BlockBuilder::new();
        let mut filled: Vec<OwnedBlock> = Vec::new();
        let mut frame_buf: Vec<u8> = Vec::new();
        let mut counts = RunCounts::default();
        let mut write = |elem: &T, counts: &mut RunCounts| -> Result<()> {
            write_elem(elem, &mut bb, &mut filled)?;
            counts.elements += 1;
            filled.drain(..).try_for_each(&mut emit)
        };

        for entry in self.entries {
            // Once per blob, before it is read: a pathological input can put
            // most of a file into one run, swept on one worker, and a run
            // whose heap only grows writes nothing until it drains.
            self.cancel.check()?;
            let below = crate::osm_id::blob_osm_first_id(entry.index.min_id, entry.index.max_id);
            while heap
                .peek()
                .is_some_and(|Reverse(e)| crate::osm_id::osm_id_cmp(e.id(), below).is_lt())
            {
                self.pop_one(&mut heap, &mut counts, &mut write)?;
            }
            let source = &self.sources[entry.source];
            let block = read_block(source, entry, &mut frame_buf)?;
            for element in block.elements() {
                extract(&element, &mut heap).map_err(|e| source.locate_at(entry.file_offset, e))?;
            }
        }
        while !heap.is_empty() {
            self.pop_one(&mut heap, &mut counts, &mut write)?;
        }
        drop(write);
        crate::commands::flush_local(&mut bb, &mut filled)?;
        filled.into_iter().try_for_each(emit)?;
        Ok(counts)
    }

    /// Pop and write the smallest element; under dedupe, then drop every
    /// element equal to it in (id, version).
    fn pop_one<T: SweepElement>(
        &self,
        heap: &mut BinaryHeap<Reverse<T>>,
        counts: &mut RunCounts,
        write: &mut impl FnMut(&T, &mut RunCounts) -> Result<()>,
    ) -> Result<()> {
        let Some(Reverse(elem)) = heap.pop() else {
            return Ok(());
        };
        write(&elem, counts)?;
        if self.dedupe {
            let (id, version) = (elem.id(), elem.version());
            while heap
                .peek()
                .is_some_and(|Reverse(e)| e.id() == id && e.version() == version)
            {
                heap.pop();
                counts.duplicates += 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ElementKind, min_id: i64, max_id: i64) -> BlobEntry {
        BlobEntry {
            source: 0,
            file_offset: 0,
            frame_len: 0,
            index: BlobIndex {
                kind,
                min_id,
                max_id,
                count: 1,
                bbox: None,
            },
            has_indexdata: true,
            tagdata: None,
            intra_unsorted: false,
            index_scanned: true,
        }
    }

    /// Per entry: inside an overlap component.
    fn detect_overlaps(entries: &[BlobEntry]) -> Vec<bool> {
        let mut flags = vec![false; entries.len()];
        for (start, end) in overlap_components(entries) {
            flags[start..end].fill(true);
        }
        flags
    }

    /// The transitivity failure. Entries are in first-key order:
    /// A=[1..1000], B=[2..3], C=[500..600]. A/B overlap; B/C do NOT, so a
    /// pairwise-adjacent scan leaves C unflagged although its range sits
    /// inside A's: the run [A,B] then flushes A's ids 500..1000 and C is
    /// copied after it with ids 500..600 - unsorted output under the sorted
    /// stamp, and in `cat --dedupe` A/C duplicates that survive. Realistic
    /// for merging extracts, whose per-blob ID ranges are wide and sparse.
    #[test]
    fn overlap_runs_are_transitive_not_pairwise() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Node, 500, 600),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, true],
            "C=[500..600] lies inside A=[1..1000] and must join the run even \
             though it does not overlap its immediate predecessor B=[2..3]"
        );
    }

    /// Passing counterpart: disjoint blobs stay on the passthrough path, so
    /// a "flag everything" regression cannot satisfy the test above.
    #[test]
    fn disjoint_blobs_are_not_flagged_as_overlapping() {
        let entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 11, 20),
            entry(ElementKind::Node, 21, 30),
        ];
        assert_eq!(detect_overlaps(&entries), vec![false, false, false]);
    }

    /// Runs stay kind-bounded: the sweep is kind-gated and silently drops
    /// off-kind elements handed to it.
    #[test]
    fn a_run_never_crosses_a_kind_boundary() {
        let entries = vec![
            entry(ElementKind::Node, 1, 1000),
            entry(ElementKind::Node, 2, 3),
            entry(ElementKind::Way, 5, 6),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the way blob shares an id range with the node run but is a \
             different kind, so it must not be pulled into it"
        );
    }

    /// A run that ends must not tint the blob that follows it.
    #[test]
    fn a_new_run_starts_after_the_previous_one_ends() {
        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
            entry(ElementKind::Node, 250, 260),
        ];
        assert_eq!(detect_overlaps(&entries), vec![true, true, true, true]);

        let entries = vec![
            entry(ElementKind::Node, 1, 100),
            entry(ElementKind::Node, 50, 60),
            entry(ElementKind::Node, 200, 300),
        ];
        assert_eq!(
            detect_overlaps(&entries),
            vec![true, true, false],
            "the blob after a run must be judged on its own, not inherit the \
             run's flag"
        );
    }

    /// Each overlap component is its own run, adjacent ones included, so
    /// they sweep in parallel; an internally unsorted blob that overlaps
    /// nothing is a run alone; one inside a component is counted there.
    #[test]
    fn runs_are_overlap_components_and_lone_unsorted_blobs() {
        let mut entries = vec![
            entry(ElementKind::Node, 1, 10),
            entry(ElementKind::Node, 5, 6),
            entry(ElementKind::Node, 20, 30),
            entry(ElementKind::Node, 25, 26),
            entry(ElementKind::Node, 40, 50),
            entry(ElementKind::Way, 1, 10),
            entry(ElementKind::Way, 5, 6),
            entry(ElementKind::Way, 40, 50),
        ];
        entries[1].intra_unsorted = true;
        entries[4].intra_unsorted = true;
        let plan = plan(entries);
        let runs: Vec<_> = plan.runs.iter().map(|r| (r.start, r.end, r.kind)).collect();
        assert_eq!(
            runs,
            vec![
                (0, 2, ElementKind::Node),
                (2, 4, ElementKind::Node),
                (4, 5, ElementKind::Node),
                (5, 7, ElementKind::Way),
            ]
        );
        assert_eq!((plan.overlapping, plan.intra_unsorted), (6, 1));
        assert_eq!(
            plan.rewrite,
            vec![true, true, true, true, true, true, true, false]
        );
    }

    fn one_block() -> OwnedBlock {
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), None);
        bb.take_owned().expect("encode").expect("non-empty block")
    }

    /// A run ends only on its terminal message: blocks before it are written,
    /// `Finished` carries the counts, `Failed` is the run's error.
    #[test]
    fn a_run_is_drained_up_to_its_terminal_message() {
        let cancel = crate::host::Cancel::new();
        let (tx, rx) = sync_channel(4);
        tx.send(RunMsg::Block(one_block())).expect("send");
        tx.send(RunMsg::Finished(RunCounts {
            elements: 1,
            duplicates: 0,
        }))
        .expect("send");
        let mut written = 0;
        let counts = drain_run(&rx, &cancel, |_| {
            written += 1;
            Ok(())
        })
        .expect("finished run");
        assert_eq!((written, counts.elements), (1, 1));

        let (tx, rx) = sync_channel(4);
        tx.send(RunMsg::Failed(Error::invalid_data("bad blob")))
            .expect("send");
        let err = drain_run(&rx, &cancel, |_| Ok(())).expect_err("failed run");
        assert!(err.to_string().contains("bad blob"), "{err}");
    }

    /// A channel that closes without a terminal message is a worker that
    /// died mid-run (a panic), never a shorter run: the blocks it did send
    /// must not pass for the whole run.
    #[test]
    fn a_run_that_closes_without_finishing_is_an_error() {
        let cancel = crate::host::Cancel::new();
        let (tx, rx) = sync_channel(4);
        tx.send(RunMsg::Block(one_block())).expect("send");
        drop(tx);
        let err = drain_run(&rx, &cancel, |_| Ok(())).expect_err("unfinished run");
        assert!(
            matches!(err.kind(), crate::ErrorKind::Internal { .. }),
            "{err:?}"
        );
    }

    /// A cancelled run stops at the consumer's checkpoint even when its
    /// worker would still finish it.
    #[test]
    fn a_cancelled_drain_stops_at_its_checkpoint() {
        let cancel = crate::host::Cancel::new();
        let (tx, rx) = sync_channel(4);
        tx.send(RunMsg::Block(one_block())).expect("send");
        tx.send(RunMsg::Finished(RunCounts::default()))
            .expect("send");
        cancel.cancel();
        let err = drain_run(&rx, &cancel, |_| Ok(())).expect_err("cancelled");
        assert!(err.is_cancelled(), "{err:?}");
    }

    /// A passthrough blob's count is a claim when it came from indexdata and
    /// an observation when pass 1 scanned the payload. Only the latter may
    /// reach the observed per-kind totals.
    #[test]
    fn passthrough_counts_split_claimed_from_scanned() {
        let mut counts = WriteCounts::default();
        let mut claimed = entry(ElementKind::Node, 1, 10);
        claimed.index_scanned = false;
        claimed.index.count = 1_000_000;
        count_passthrough(&claimed, &mut counts);
        assert_eq!(counts.nodes, 0, "an indexdata claim is not an observation");
        assert_eq!(counts.passthrough_claimed, 1_000_000);

        let mut scanned = entry(ElementKind::Way, 1, 10);
        scanned.has_indexdata = false;
        scanned.index.count = 7;
        count_passthrough(&scanned, &mut counts);
        assert_eq!(counts.ways, 7, "a pass-1 scan count is observed");
        assert_eq!(counts.passthrough_claimed, 1_000_000);
    }
}
