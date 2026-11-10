//! Blob-level permutation merge: the one engine behind `sort` (one input,
//! every element kept) and `cat --dedupe` (any number of inputs, exact
//! duplicates dropped).
//!
//! Pass 1 ([`index_inputs`]) walks every input's blob headers with
//! [`HeaderWalker`] (`O_DIRECT` under `--direct-io`) and records one [`Frame`] per OSMData blob it keeps, with
//! one [`BlobEntry`] per element kind the frame holds: one for an ordinary
//! blob, one per kind present for a block no single index describes. [`plan`]
//! orders the entries by (kind, first key), flags those whose ID ranges
//! overlap a same-kind neighbour plus those that must be rewritten whatever
//! their range (internally out of order, or a block no single index
//! describes), and groups the flagged ones into kind-bounded overlap runs.
//! Pass 2 ([`write_plan`]) writes the entries in order: a frame outside every
//! run is copied through (coalesced into one `copy_file_range` per contiguous
//! stretch of one input where the output can splice, or reframed with a
//! scanned index where its own is missing or false), and each overlap run is
//! decoded, merged by a k-way merge over borrowed cursors (`sweep.rs`) and
//! re-encoded.
//!
//! Overlap runs are swept on worker threads, which frame their own blocks
//! and stream them to the write loop in run order, through a bounded channel
//! per run and an admission window over the runs (see [`write_plan`] for the
//! memory bound and the shutdown protocol). A run is one serial sweep however
//! large, so a single giant overlap component streams rather than being held
//! whole.
//!
//! The commands differ only in their [`IndexPolicy`] and in whether the sweep
//! drops exact duplicates; their markers, counters and diagnostic keys stay
//! in the command modules.

use crate::Element;
use crate::ElementKind;
use crate::blob::{
    BlobKind, decode_blob_to_headerblock, decode_blob_to_primitiveblock, decompress_blob_data_into,
};
use crate::blob_meta::{
    BboxClaim, BlobIndex, TagIndex, try_scan_block_ids_and_tags, try_scan_block_ids_checked,
    try_scan_block_tags,
};
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::read::header_walker::{BlobHeaderMeta, HeaderWalker, Positional};
use crate::read::input::{InputFile, PinnedInput};
use crate::tunables::{
    DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED, FRAME_POOL_WINDOW_SLACK,
    PASS1_BELIEVED_BATCH,
};
use crate::writer::PbfWriter;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Blob index
// ---------------------------------------------------------------------------

/// One OSMData blob of one input that pass 1 kept: where its frame is, and
/// what a passthrough of it writes.
pub(crate) struct Frame {
    /// Which input (an index into the slice handed to [`index_inputs`], and
    /// into the sources pass 2 opens from it).
    pub(crate) source: usize,
    /// Byte offset of the complete frame in its input.
    pub(crate) file_offset: u64,
    /// Length of the complete frame (4 + header_len + data_size).
    pub(crate) frame_len: u64,
    /// The blob header a passthrough gives this frame instead of its own:
    /// set for a frame without indexdata, and for one whose indexdata pass 1
    /// scanned and found false. `None` copies the frame whole. [`plan`]
    /// clears it on a rewritten frame, so after planning it is exactly "this
    /// passthrough is reframed".
    pub(crate) reindex: Option<Reindex>,
}

/// The index and tag key set a reframed passthrough carries, both scanned
/// from its payload, so it is indexed as plain `cat` indexes it.
///
/// Every reframed passthrough's serialized key set is held from pass 1
/// until the plan is dropped after pass 2 (`plan` frees only a rewritten
/// frame's), so its memory is the sum of the tagdata the output gains, with
/// no fixed bound. Measured on an unindexed Europe input, 522k blobs all
/// passed through: about 895 MB of peak anon more than indexdata alone, the
/// 877 MB of tagdata the output grew by (reference/performance.md, "sort
/// --force on unindexed Europe"). Accepted: only `--force` on an unindexed
/// input and `--distrust-metadata` reframe blobs at that scale, and `cat`
/// is the streaming reindex for such a file. If bounded memory here becomes
/// a requirement, the remedy is spilling these headers past a budget to
/// scratch, read back by offset (pass 2 visits frames in output order, not
/// input order); re-scanning payloads in pass 2 would decompress every blob
/// twice.
pub(crate) struct Reindex {
    index: BlobIndex,
    tagdata: Option<Box<[u8]>>,
    /// The frame carried indexdata its payload contradicts (only `sort`,
    /// under a header that does not claim `Sort.Type_then_ID`, scans an
    /// indexed blob and can find this).
    replaces_false_claim: bool,
}

/// One element kind's share of a [`Frame`]: what the plan orders and the
/// sweep merges. An ordinary blob has one; a block no single index describes
/// has one per kind it holds, all of the same frame and all rewritten.
pub(crate) struct BlobEntry {
    /// The frame (an index into [`Plan::frames`]).
    pub(crate) frame: usize,
    /// Element kind + ID range of this kind's elements in the frame.
    pub(crate) index: BlobIndex,
    /// True when this blob's elements are internally out of canonical OSM ID
    /// order. Set by the payload scan, which runs for every blob except an
    /// indexed one under a header claiming `Sort.Type_then_ID`: intra-blob
    /// order there is a precondition of the claim itself (see
    /// CORRECTNESS.md). A flagged blob is routed into the rewrite path so the
    /// sweep reorders it, even when its range overlaps no neighbour. Not
    /// computed for a restructured entry, which is rewritten anyway.
    pub(crate) intra_unsorted: bool,
    /// The frame's block has no single index (kinds change across groups,
    /// or a group carries more than one element field), so it can never be
    /// copied: every one of its entries is rewritten through the sweep,
    /// whose extraction takes only the entry's kind from the frame.
    pub(crate) restructured: bool,
    /// True when `index` was derived by decoding the payload, false when it
    /// was read out of the BlobHeader and believed. Only the element counters
    /// need this: a passthrough blob's `index.count` on the header-only path
    /// is the file's own unverified claim, reported apart from the observed
    /// counts (`passthrough_claimed`).
    pub(crate) index_scanned: bool,
}

/// What pass 1 found: the frames it kept, their entries in input then file
/// order, and the blobs it dropped.
pub(crate) struct Indexed {
    pub(crate) frames: Vec<Frame>,
    pub(crate) entries: Vec<BlobEntry>,
    /// Blobs whose block decoded to no element at all. They are left out of
    /// the output, as plain `cat` leaves them out: no index can describe
    /// them, and they hold no OSM content (only whatever unknown fields they
    /// carried, which go with them).
    pub(crate) omitted: u64,
}

/// What pass 1 accepts and refuses: the part of a command's input contract
/// the shared walk enforces.
pub(crate) struct IndexPolicy {
    /// Refuse an input whose header does not claim `Sort.Type_then_ID`, with
    /// this reason (`cat --dedupe`, which promises a merge of sorted inputs).
    pub(crate) require_sorted: Option<&'static str>,
    /// Exempt the walk from the command-input refusal of a partially indexed
    /// input (`HeaderWalker::accepting_partial_index`). `sort` refuses one,
    /// as every command does; `cat --dedupe` accepts it, because `cat` is
    /// the remedy, scanning each unindexed blob (reference/indexdata-trust.md,
    /// "Missing and partial indexdata").
    pub(crate) accept_partial_index: bool,
    /// Believe no claim (`sort --distrust-metadata`): every blob is scanned
    /// from its payload as under a header without the sorted claim, its tag
    /// keys included, and a blob whose indexdata or tagdata the scan
    /// contradicts is reframed under the scanned ones. The remedy for a file
    /// refused as unsorted, which a plain sort under the file's sorted claim
    /// would copy unchanged.
    pub(crate) distrust_metadata: bool,
}

/// Pass 1: walk every input, in order, and index its OSMData blobs.
///
/// Returns one header per input, in input order, and what the walk indexed.
/// Each input must start with its one OSMHeader: a first
/// frame of any other type is the missing-header error at once, and a later
/// OSMHeader is refused by the walk's shared framing policy
/// (`read::frame::check_header_position`) rather than ignored, because what
/// it declares would otherwise be silently left out of the output header.
///
/// The walk preads headers (`fadvise(RANDOM)`) rather than streaming the
/// file: `sort`'s planet pass 1 measured 32 GB read through a buffered
/// reader against 674 MB walked (reference/performance-history.md, "Sort").
/// Only on a cold region of small frames does it read windows and prefetch
/// ahead of them (`header_walker::PreadSource`), which also leaves those
/// payloads cached for pass 2's copy. An indexed blob under a header claiming
/// `Sort.Type_then_ID` is indexed from its indexdata alone. Every other blob
/// is preaded, decompressed and scanned, which also checks its intra-blob
/// order: indexdata alone does not prove it (`cat` attaches indexdata to
/// payloads it never reorders), and the trusted ordering signal is the header
/// claim, the contract `ElementReader` keys its guarantees on.
///
/// A scanned payload ends in one of four ways. Malformed bytes fail the run
/// with the scan's own error. A block one index describes becomes one entry;
/// without indexdata it is also scanned for its tag keys, so a passthrough of
/// it is reframed with both, as plain `cat` indexes it. A block no single
/// index describes is decoded: with no element it is omitted, otherwise it
/// gets one restructured entry per kind it holds. An indexed blob scanned for
/// order (`sort` under a header without the sorted claim) whose indexdata
/// its payload contradicts is reframed with the scanned index rather than
/// copied with the false one. Its claim holds exactly when `check --index`
/// would report nothing against it: kind, count and ID range equal, and a
/// bbox it carries containing the scanned one (an absent bbox is no claim;
/// a node coordinate outside the decimicrodegree range, where the scan has
/// no box, makes any carried box false). [`BlobIndex::bbox_claim`] is that
/// rule's bbox half, shared with the check.
///
/// The header walk, the OSMHeader decode and the fold into [`Indexed`] run on
/// the calling thread; each scanned blob's pread, decompression, scan and
/// reframing decision run on a worker pool (`ordered_pool`'s streamed
/// tasks), at most `workers + FRAME_POOL_WINDOW_SLACK` tasks ahead of the
/// fold. A task is up to [`PASS1_BELIEVED_BATCH`] believed blobs followed by
/// at most one scanned one, so a believed walk stays header-only and pays
/// no pool round trip per blob. Buffered workers pread from the walker's
/// descriptor; an `O_DIRECT` walk reads each scanned payload on its own
/// thread, since its descriptor cannot leave it. The fold pushes in file
/// order, so frames, entries and the omitted count are the serial walk's,
/// and failures are reported as the serial walk reports them: a scanned
/// blob's failure after every earlier blob is folded (and after the
/// unclaimed-order note, if that blob is the first it covers), a walk
/// failure (framing, the partial index refusal, a missing or misplaced
/// header) after every earlier blob. Only a cancellation racing a failure
/// can differ: a failure the workers or the walk reached ahead of the fold
/// is speculative until the fold reaches it, and may lose to `Cancelled`
/// (ADR-0019).
///
/// `direct_io` (the `--direct-io` flag) walks every input `O_DIRECT`
/// ([`HeaderWalker::open_direct`]): the same probes and payload reads, as
/// aligned reads that bypass the page cache (where the filesystem honours
/// `O_DIRECT`; `crate::direct_io` names the ones that may not). Without the
/// `linux-direct-io` feature it is refused before any input is walked.
#[hotpath::measure]
pub(crate) fn index_inputs(
    inputs: &[PinnedInput],
    policy: &IndexPolicy,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<crate::HeaderBlock>, Indexed)> {
    #[cfg(not(feature = "linux-direct-io"))]
    if direct_io {
        return Err(crate::file_reader::direct_reads_unsupported());
    }
    #[cfg(feature = "linux-direct-io")]
    if direct_io {
        return index_over(inputs, policy, host, |input| {
            HeaderWalker::open_direct(input, host)
        });
    }
    index_over(inputs, policy, host, |input| {
        HeaderWalker::open(input, host)
    })
}

/// How a walk's scanned payloads reach the workers.
trait PayloadRead: Positional + Sized {
    /// The descriptor workers pread a scanned payload from, or `None` when
    /// the walk must read it on its own thread: an `O_DIRECT` walker's reads
    /// share one aligned scratch buffer, so its descriptor stays with it.
    fn worker_file(walker: &HeaderWalker<Self>) -> Option<Arc<std::fs::File>>;
}

impl PayloadRead for std::fs::File {
    fn worker_file(walker: &HeaderWalker<Self>) -> Option<Arc<std::fs::File>> {
        Some(Arc::clone(walker.shared_file()))
    }
}

#[cfg(feature = "linux-direct-io")]
impl PayloadRead for crate::read::direct_reader::DirectPositional {
    fn worker_file(_walker: &HeaderWalker<Self>) -> Option<Arc<std::fs::File>> {
        None
    }
}

/// Pass 1 over walkers `open` makes, one per input in order: the walk and
/// fold on the calling thread, the payload scans on a worker pool.
fn index_over<P: PayloadRead>(
    inputs: &[PinnedInput],
    policy: &IndexPolicy,
    host: &crate::host::Entered<'_>,
    open: impl Fn(&PinnedInput) -> Result<HeaderWalker<P>>,
) -> Result<(Vec<crate::HeaderBlock>, Indexed)> {
    let mut walk = Walk {
        inputs,
        policy,
        open,
        input: 0,
        current: None,
        believed: Vec::new(),
        deferred: None,
        headers: Vec::with_capacity(inputs.len()),
        data_buf: crate::read::read_buf::ReadBuf::default(),
        invocation: &host.invocation,
    };
    let mut fold = Fold {
        host,
        indexed: Indexed {
            frames: Vec::new(),
            entries: Vec::new(),
            omitted: 0,
        },
        noted_unclaimed: false,
    };
    let workers = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    );
    let config = crate::ordered_pool::StreamConfig {
        window: std::num::NonZeroUsize::new(workers.saturating_add(FRAME_POOL_WINDOW_SLACK))
            .unwrap_or(std::num::NonZeroUsize::MIN),
        // Terminal-only tasks: one slot lets a worker deposit its result and
        // take the next task without waiting for the fold.
        depth: 1,
        cancel: &host.invocation,
    };
    let base: &crate::host::Host = host;
    let cancel = &base.invocation;
    let recorder: &crate::host::Recorder = &host.invocation;
    let mut source = || walk.next_task();
    crate::ordered_pool::run_ordered_streams_from(
        &mut source,
        workers,
        config,
        |_| Scratch::default(),
        |scratch: &mut Scratch,
         _task: usize,
         task: Task,
         _emitter: &crate::ordered_pool::Emitter<'_, Infallible, Done>| {
            Ok(work(
                scratch,
                task,
                policy.distrust_metadata,
                cancel,
                recorder,
            ))
        },
        &mut fold,
    )?;
    Ok((walk.headers, fold.indexed))
}

/// The calling thread's walk: every input's headers in order, cut into
/// [`Task`]s. A failure is the source failure of the task it did not
/// produce, so every earlier blob is folded first; believed blobs it holds
/// when one strikes go out as a task of their own before it is returned.
struct Walk<'a, P, O> {
    inputs: &'a [PinnedInput],
    policy: &'a IndexPolicy,
    open: O,
    /// The next input to open.
    input: usize,
    /// The input being walked: its index, walker and header once read.
    current: Option<(usize, HeaderWalker<P>, Option<crate::HeaderBlock>)>,
    /// Believed blobs of the current input not yet sent.
    believed: Vec<Believed>,
    /// A failure held back while `believed` went out first.
    deferred: Option<Error>,
    headers: Vec<crate::HeaderBlock>,
    /// The OSMHeader payloads, and under `O_DIRECT` every scanned payload,
    /// are read here, on the walk's thread.
    data_buf: crate::read::read_buf::ReadBuf,
    invocation: &'a crate::host::Recorder,
}

/// A believed blob: indexed from its header alone.
struct Believed {
    file_offset: u64,
    frame_len: u64,
    index: BlobIndex,
}

/// One pool task: believed blobs of one input in file order, then at most
/// one scanned blob after them.
struct Task {
    source: usize,
    believed: Vec<Believed>,
    scan: Option<ScanJob>,
}

/// A blob to pread, decompress and scan.
struct ScanJob {
    file_offset: u64,
    frame_len: u64,
    /// Its indexdata, if it carried any.
    claimed: Option<BlobIndex>,
    /// Its carried tagdata, compared under `--distrust-metadata`.
    carried_tags: Option<Box<[u8]>>,
    /// Whether this blob is one the once-per-run order note announces (an
    /// indexed blob scanned under an unclaimed header, not under distrust).
    note: bool,
    path: Arc<Path>,
    blob_index: u64,
    payload: Payload,
}

/// Where a scanned blob's compressed bytes are.
enum Payload {
    /// To be preaded by the worker from this buffered descriptor.
    At {
        file: Arc<std::fs::File>,
        offset: u64,
        size: usize,
    },
    /// Read by the walk (an `O_DIRECT` walk): the bytes, or the read's
    /// failure, which the worker reports as the scan's.
    Read(Result<Vec<u8>>),
}

/// A task's result: always delivered, so the fold sees the believed prefix
/// and the note before a scan failure, as the serial walk did.
struct Done {
    source: usize,
    believed: Vec<Believed>,
    scan: Option<ScanDone>,
}

struct ScanDone {
    file_offset: u64,
    frame_len: u64,
    note: bool,
    outcome: Result<Outcome>,
}

/// What the worker decided for one scanned blob.
enum Outcome {
    /// One index describes it; `reindex` is the header a passthrough gives
    /// it instead of its own, `None` to copy it whole.
    Block {
        index: BlobIndex,
        sorted: bool,
        reindex: Option<Reindex>,
    },
    /// No single index does: the index of each kind it holds, in kind order,
    /// empty when it holds no element.
    Kinds(Vec<BlobIndex>),
}

impl<P: PayloadRead, O: Fn(&PinnedInput) -> Result<HeaderWalker<P>>> Walk<'_, P, O> {
    /// The next task, `None` after the last input, or the walk's failure.
    fn next_task(&mut self) -> Result<Option<Task>> {
        if let Some(e) = self.deferred.take() {
            return Err(e);
        }
        loop {
            let Some((source, walker, header)) = self.current.as_mut() else {
                let Some(input) = self.inputs.get(self.input) else {
                    return Ok(None);
                };
                let source = self.input;
                self.input += 1;
                let mut walker = (self.open)(input)?;
                if self.policy.accept_partial_index || self.policy.distrust_metadata {
                    walker = walker.accepting_partial_index();
                }
                self.current = Some((source, walker, None));
                continue;
            };
            let source = *source;
            let meta = match walker.next_header() {
                Ok(Some(meta)) => meta,
                Ok(None) => {
                    let path = Arc::clone(walker.path());
                    let header = header.take();
                    self.current = None;
                    let Some(header) = header else {
                        return self.fail(source, crate::error::new_missing_header(&path));
                    };
                    self.headers.push(header);
                    if let Some(task) = self.flush(source) {
                        return Ok(Some(task));
                    }
                    continue;
                }
                Err(e) => return self.fail(source, e),
            };
            let claims_sorted = header.as_ref().map(crate::HeaderBlock::is_sorted);
            match meta.blob_type {
                // The walk refuses an OSMHeader anywhere but first.
                BlobKind::OsmHeader => {
                    match read_header(
                        walker,
                        &meta,
                        self.policy,
                        &mut self.data_buf,
                        self.invocation,
                    ) {
                        Ok(hdr) => *header = Some(hdr),
                        Err(e) => return self.fail(source, e),
                    }
                }
                _ if claims_sorted.is_none() => {
                    let e = crate::error::new_missing_header(walker.path());
                    return self.fail(source, e);
                }
                BlobKind::OsmData => {
                    let distrust = self.policy.distrust_metadata;
                    let believed = claims_sorted == Some(true) && !distrust;
                    match meta.index {
                        Some(index) if believed => {
                            self.believed.push(Believed {
                                file_offset: meta.frame_start,
                                frame_len: meta.frame_size as u64,
                                index,
                            });
                            if self.believed.len() >= PASS1_BELIEVED_BATCH {
                                return Ok(self.flush(source));
                            }
                        }
                        claimed => {
                            let job = scan_job(
                                walker,
                                meta,
                                claimed,
                                distrust,
                                &mut self.data_buf,
                                self.invocation,
                            );
                            return Ok(Some(Task {
                                source,
                                believed: std::mem::take(&mut self.believed),
                                scan: Some(job),
                            }));
                        }
                    }
                }
                // Unknown extension blob: the walk has already stepped past it.
                _ => {}
            }
        }
    }

    /// The believed blobs held for `source`, as a task, if there are any.
    fn flush(&mut self, source: usize) -> Option<Task> {
        (!self.believed.is_empty()).then(|| Task {
            source,
            believed: std::mem::take(&mut self.believed),
            scan: None,
        })
    }

    /// Fail the walk with `e`, after the believed blobs it holds.
    fn fail(&mut self, source: usize, e: Error) -> Result<Option<Task>> {
        self.current = None;
        self.input = self.inputs.len();
        match self.flush(source) {
            Some(task) => {
                self.deferred = Some(e);
                Ok(Some(task))
            }
            None => Err(e),
        }
    }
}

/// The scan job for a blob the walk cannot believe. An `O_DIRECT` walk
/// reads its payload here; a buffered one leaves the read to the worker.
fn scan_job<P: PayloadRead>(
    walker: &HeaderWalker<P>,
    meta: BlobHeaderMeta,
    claimed: Option<BlobIndex>,
    distrust: bool,
    data_buf: &mut crate::read::read_buf::ReadBuf,
    invocation: &crate::host::Recorder,
) -> ScanJob {
    let payload = match P::worker_file(walker) {
        Some(file) => Payload::At {
            file,
            offset: meta.data_offset,
            size: meta.data_size,
        },
        None => {
            let read = walker
                .pread_data(meta.data_offset, meta.data_size, data_buf)
                .map(<[u8]>::to_vec);
            data_buf.release_outlier(invocation);
            Payload::Read(read)
        }
    };
    ScanJob {
        file_offset: meta.frame_start,
        frame_len: meta.frame_size as u64,
        claimed,
        carried_tags: meta.tagdata,
        note: claimed.is_some() && !distrust,
        path: Arc::clone(walker.path()),
        blob_index: meta.blob_index,
        payload,
    }
}

/// Read and decode an input's OSMHeader, refusing it where the policy
/// requires the sorted claim.
fn read_header<P: Positional>(
    walker: &HeaderWalker<P>,
    meta: &BlobHeaderMeta,
    policy: &IndexPolicy,
    data_buf: &mut crate::read::read_buf::ReadBuf,
    invocation: &crate::host::Recorder,
) -> Result<crate::HeaderBlock> {
    let located = |e| walker.locate_meta(meta, e);
    let data = walker
        .pread_data(meta.data_offset, meta.data_size, data_buf)
        .map_err(located)?;
    let hdr = decode_blob_to_headerblock(data).map_err(located);
    data_buf.release_outlier(invocation);
    let hdr = hdr?;
    if let Some(reason) = policy.require_sorted {
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

/// A worker's buffers, kept across its blobs; both follow their retention
/// limits once per blob.
#[derive(Default)]
struct Scratch {
    data_buf: crate::read::read_buf::ReadBuf,
    decompress_buf: Vec<u8>,
}

/// Run one task on a worker: scan its blob, if it has one.
fn work(
    scratch: &mut Scratch,
    task: Task,
    distrust: bool,
    cancel: &crate::host::Invocation,
    recorder: &crate::host::Recorder,
) -> Done {
    let Task {
        source,
        believed,
        scan,
    } = task;
    let scan = scan.map(|job| {
        let location = crate::error::Location::new(
            Some(&job.path),
            Some(job.blob_index),
            Some(crate::ByteOffset(job.file_offset)),
        );
        let ScanJob {
            claimed,
            carried_tags,
            payload,
            ..
        } = job;
        let outcome = scan_blob(
            scratch,
            payload,
            claimed,
            carried_tags.as_deref(),
            distrust,
            cancel,
            recorder,
        )
        .map_err(|e| e.locate(location));
        ScanDone {
            file_offset: job.file_offset,
            frame_len: job.frame_len,
            note: job.note,
            outcome,
        }
    });
    Done {
        source,
        believed,
        scan,
    }
}

/// Pread (or take), decompress and scan one payload's IDs, checking
/// intra-blob monotonicity on the way (one compare per element, on a scan
/// that visits every ID anyway), and its tag keys where a passthrough would
/// be reframed: a blob without indexdata, and under distrust every blob,
/// whose carried key set is checked against them. Otherwise an indexed blob
/// is copied whole unless its claim turns out false, which rescans the
/// buffer. A block no single index describes is decoded for the kinds it
/// holds.
fn scan_blob(
    scratch: &mut Scratch,
    payload: Payload,
    claimed: Option<BlobIndex>,
    carried_tags: Option<&[u8]>,
    distrust: bool,
    cancel: &crate::host::Invocation,
    recorder: &crate::host::Recorder,
) -> Result<Outcome> {
    let decompressed = match payload {
        Payload::At { file, offset, size } => {
            let read = scratch
                .data_buf
                .read_exact_at(&*file, offset, size)
                .map_err(Error::from)
                .and_then(|data| decompress_blob_data_into(data, &mut scratch.decompress_buf));
            scratch.data_buf.release_outlier(recorder);
            read
        }
        Payload::Read(read) => {
            read.and_then(|data| decompress_blob_data_into(&data, &mut scratch.decompress_buf))
        }
    };
    let outcome =
        decompressed.and_then(|()| decide(scratch, claimed, carried_tags, distrust, cancel));
    crate::blob::release_decode_outlier(&mut scratch.decompress_buf, recorder);
    outcome
}

/// What the decompressed block in `scratch` makes of `job`'s blob: its
/// entry and, for a passthrough, whether and how it is reframed.
fn decide(
    scratch: &mut Scratch,
    claimed: Option<BlobIndex>,
    carried_tags: Option<&[u8]>,
    distrust: bool,
    cancel: &crate::host::Invocation,
) -> Result<Outcome> {
    let with_tags = claimed.is_none() || distrust;
    let (index, tags) = if with_tags {
        try_scan_block_ids_and_tags(&scratch.decompress_buf)?
    } else {
        (try_scan_block_ids_checked(&scratch.decompress_buf)?, None)
    };
    let Some((index, sorted)) = index else {
        return kind_indexes(std::mem::take(&mut scratch.decompress_buf), cancel)
            .map(Outcome::Kinds);
    };
    let tagdata = tags.as_ref().and_then(serialize_tags);
    let tags_hold = !distrust || carried_tags_hold(carried_tags, tagdata.as_deref());
    let reindex = match claimed {
        None => Some(Reindex {
            index,
            tagdata,
            replaces_false_claim: false,
        }),
        Some(claim) if claim_holds(&claim, &index) && tags_hold => None,
        Some(_) if distrust => Some(Reindex {
            index,
            tagdata,
            replaces_false_claim: true,
        }),
        Some(_) => Some(Reindex {
            index,
            tagdata: scanned_tagdata(&scratch.decompress_buf)?,
            replaces_false_claim: true,
        }),
    };
    Ok(Outcome::Block {
        index,
        sorted,
        reindex,
    })
}

/// The calling thread's fold: every task's blobs pushed in file order, as
/// the serial walk pushed them.
struct Fold<'a, 'h> {
    host: &'a crate::host::Entered<'h>,
    indexed: Indexed,
    noted_unclaimed: bool,
}

impl Fold<'_, '_> {
    fn push(&mut self, frame: Frame, entries: impl IntoIterator<Item = BlobEntry>) {
        self.indexed.frames.push(frame);
        self.indexed.entries.extend(entries);
    }
}

impl crate::ordered_pool::StreamConsumer<Infallible, Done> for Fold<'_, '_> {
    fn item(&mut self, _task: usize, item: Infallible) -> Result<()> {
        match item {}
    }

    fn finish_task(&mut self, _task: usize, done: Done) -> Result<()> {
        let Done {
            source,
            believed,
            scan,
        } = done;
        for b in believed {
            let frame = self.indexed.frames.len();
            // Believed: the frame's own header (its tagdata included) is
            // what a passthrough copies.
            self.push(
                Frame {
                    source,
                    file_offset: b.file_offset,
                    frame_len: b.frame_len,
                    reindex: None,
                },
                [BlobEntry {
                    frame,
                    index: b.index,
                    intra_unsorted: false,
                    restructured: false,
                    index_scanned: false,
                }],
            );
        }
        let Some(scan) = scan else {
            return Ok(());
        };
        if scan.note && !self.noted_unclaimed {
            self.noted_unclaimed = true;
            // Only `sort` reaches this: `cat --dedupe` refuses an input
            // whose header does not claim sorted.
            crate::host::diag!(
                self.host.diagnostics,
                Note,
                "sort.verify_unclaimed_order",
                "input does not declare Sort.Type_then_ID; verifying \
                 intra-blob order (pass 1 decodes every blob payload)"
            );
        }
        let frame = self.indexed.frames.len();
        let mut new_frame = Frame {
            source,
            file_offset: scan.file_offset,
            frame_len: scan.frame_len,
            reindex: None,
        };
        match scan.outcome? {
            Outcome::Block {
                index,
                sorted,
                reindex,
            } => {
                new_frame.reindex = reindex;
                self.push(
                    new_frame,
                    [BlobEntry {
                        frame,
                        index,
                        intra_unsorted: !sorted,
                        restructured: false,
                        index_scanned: true,
                    }],
                );
            }
            Outcome::Kinds(kinds) if kinds.is_empty() => self.indexed.omitted += 1,
            Outcome::Kinds(kinds) => self.push(
                new_frame,
                kinds.into_iter().map(|index| BlobEntry {
                    frame,
                    index,
                    intra_unsorted: false,
                    restructured: true,
                    index_scanned: true,
                }),
            ),
        }
        Ok(())
    }
}

/// Whether a blob's indexdata claim is true of the index scanned from its
/// payload, by `check --index`'s rule: kind, count and ID range exact, and
/// the bbox judged by [`BlobIndex::bbox_claim`] (an absent one no claim, a
/// present one containing the scanned box; a node coordinate outside the
/// decimicrodegree range makes any box false).
fn claim_holds(claim: &BlobIndex, scanned: &BlobIndex) -> bool {
    claim.kind == scanned.kind
        && claim.count == scanned.count
        && claim.min_id == scanned.min_id
        && claim.max_id == scanned.max_id
        && matches!(
            claim.bbox_claim(scanned),
            BboxClaim::Absent | BboxClaim::Holds
        )
}

/// Whether a blob's carried tagdata, compared as a key set, is the set the
/// scan found (`scanned`, serialized). Absent carried tagdata claims nothing
/// and holds; carried bytes that do not parse, or a scanned set too large to
/// encode, cannot match. Parsing accepts only the canonical encoding (keys
/// strictly ascending), so a carried index naming the right keys out of
/// order or repeated does not hold and is replaced: every reader would treat
/// it as no tagdata.
fn carried_tags_hold(carried: Option<&[u8]>, scanned: Option<&[u8]>) -> bool {
    let Some(carried) = carried else {
        return true;
    };
    let (Some(carried), Some(scanned)) = (
        TagIndex::deserialize(carried),
        scanned.and_then(TagIndex::deserialize),
    ) else {
        return false;
    };
    carried.keys() == scanned.keys()
}

/// The serialized tag key set of a block one index describes.
fn scanned_tagdata(block: &[u8]) -> Result<Option<Box<[u8]>>> {
    Ok(try_scan_block_tags(block)?
        .as_ref()
        .and_then(serialize_tags))
}

fn serialize_tags(tags: &TagIndex) -> Option<Box<[u8]>> {
    tags.serialize().map(Vec::into_boxed_slice)
}

/// The index of each element kind a block holds, in kind order, for a block
/// the scan found no single index for. Decoding is the cold path's cost: such
/// blocks come only from third-party producers, and a decode failure here is
/// the block's real error (the scan validates less than the decoder does).
/// The bbox is left out, since these entries are rewritten, never copied.
/// One cancellation check before the decode: the walk checks between blobs,
/// and one block is bounded by the blob size limit. The caller hands over
/// its decompression buffer, which becomes the block rather than being
/// copied into one; its next blob decompresses into a fresh buffer.
fn kind_indexes(block: Vec<u8>, cancel: &crate::host::Invocation) -> Result<Vec<BlobIndex>> {
    cancel.check()?;
    let block = crate::PrimitiveBlock::from_vec(block)?;
    let mut ranges: [Option<(i64, i64, u64)>; 3] = [None; 3];
    for element in block.elements() {
        let (kind, id) = match &element {
            Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
            Element::Node(n) => (ElementKind::Node, n.id()),
            Element::Way(w) => (ElementKind::Way, w.id()),
            Element::Relation(r) => (ElementKind::Relation, r.id()),
        };
        let range = &mut ranges[usize::from(type_order(kind))];
        *range = Some(match *range {
            None => (id, id, 1),
            Some((min, max, count)) => (min.min(id), max.max(id), count + 1),
        });
    }
    Ok([ElementKind::Node, ElementKind::Way, ElementKind::Relation]
        .into_iter()
        .zip(ranges)
        .filter_map(|(kind, range)| {
            range.map(|(min_id, max_id, count)| BlobIndex {
                kind,
                min_id,
                max_id,
                count,
                bbox: None,
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Plan: order, overlap detection, runs
// ---------------------------------------------------------------------------

/// One rewrite run, `entries[start..end]`, all of `kind`: an overlap
/// component, or one entry that overlaps nothing but must be rewritten.
pub(crate) struct Run {
    start: usize,
    end: usize,
    kind: ElementKind,
}

/// The entries in output order and what pass 2 does with each.
///
/// The counts are of input blobs (frames), never of entries, so a blob that
/// holds several kinds counts once in each total: `blobs_total` is
/// `omitted + passthrough + rewritten`, the passthroughs being exactly the
/// frames no entry of which is rewritten. The attributions (`overlapping`,
/// `intra_unsorted`, `restructured`) are not additive: a restructured blob
/// whose way entry joins an overlap component counts in both.
pub(crate) struct Plan {
    pub(crate) frames: Vec<Frame>,
    pub(crate) entries: Vec<BlobEntry>,
    /// Per entry: rewritten in an overlap run, rather than copied through.
    pub(crate) rewrite: Vec<bool>,
    /// Blobs with an entry in a genuine range-overlap run.
    pub(crate) overlapping: usize,
    /// Blobs routed to rewrite only for intra-blob disorder (disjoint from
    /// `overlapping` and `restructured`).
    pub(crate) intra_unsorted: usize,
    /// Blobs whose block no single index describes, rewritten whatever their
    /// ranges.
    pub(crate) restructured: usize,
    /// Blobs left out because they hold no element ([`Indexed::omitted`]).
    pub(crate) omitted: u64,
    /// Blobs with at least one rewritten entry.
    pub(crate) blobs_rewritten: u64,
    /// Passthrough blobs reframed because their indexdata was false.
    pub(crate) false_claims_replaced: u64,
    pub(crate) runs: Vec<Run>,
}

impl Plan {
    /// Every input data blob pass 1 saw, omitted ones included.
    pub(crate) fn blobs_total(&self) -> u64 {
        self.frames.len() as u64 + self.omitted
    }
}

/// Order the entries by (kind, first key), find the overlap runs and fold
/// in the entries that must be rewritten whatever their range.
///
/// The sort is stable, so entries with equal keys keep input then file
/// order, which the sweep's heap pushes (and so which of two exact
/// duplicates survives) depend on.
pub(crate) fn plan(indexed: Indexed) -> Plan {
    let Indexed {
        mut frames,
        mut entries,
        omitted,
    } = indexed;
    entries.sort_by_key(|e| {
        (
            type_order(e.index.kind),
            crate::osm_id::blob_osm_first_key(e.index.min_id, e.index.max_id),
        )
    });
    let mut rewrite = vec![false; entries.len()];
    let mut runs = Vec::new();
    // Per frame: an entry in an overlap component, rewritten for disorder
    // alone, restructured.
    let mut in_overlap = vec![false; frames.len()];
    let mut for_disorder = vec![false; frames.len()];
    let mut components = overlap_components(&entries).into_iter().peekable();
    let mut i = 0;
    while i < entries.len() {
        let kind = entries[i].index.kind;
        if let Some(&(start, end)) = components.peek()
            && start == i
        {
            components.next();
            rewrite[start..end].fill(true);
            for entry in &entries[start..end] {
                in_overlap[entry.frame] = true;
            }
            runs.push(Run { start, end, kind });
            i = end;
            continue;
        }
        // An entry that overlaps nothing but cannot be copied: rewritten
        // alone, so the sweep reorders it (disorder) or re-encodes its kind's
        // share of the block (restructured). One both unsorted and
        // overlapping is in its component already and counted there,
        // keeping the attributions disjoint.
        let entry = &entries[i];
        if entry.restructured || entry.intra_unsorted {
            rewrite[i] = true;
            if !entry.restructured {
                for_disorder[entry.frame] = true;
            }
            runs.push(Run {
                start: i,
                end: i + 1,
                kind,
            });
        }
        i += 1;
    }

    let mut rewritten = vec![false; frames.len()];
    let mut restructured = vec![false; frames.len()];
    for (entry, &r) in entries.iter().zip(&rewrite) {
        rewritten[entry.frame] |= r;
        restructured[entry.frame] |= entry.restructured;
    }
    // A restructured entry is never copied: every entry the loop above visits
    // either starts an overlap component, whose entries are all rewritten, or
    // is taken alone and rewritten when restructured.
    // A rewritten frame is never reframed: what its passthrough would have
    // carried goes, so `reindex` now means "this passthrough is reframed".
    for (frame, &r) in frames.iter_mut().zip(&rewritten) {
        if r {
            frame.reindex = None;
        }
    }
    let count = |flags: &[bool]| flags.iter().filter(|&&f| f).count();
    let false_claims_replaced = frames
        .iter()
        .filter(|f| f.reindex.as_ref().is_some_and(|r| r.replaces_false_claim))
        .count() as u64;
    Plan {
        overlapping: count(&in_overlap),
        intra_unsorted: count(&for_disorder),
        restructured: count(&restructured),
        omitted,
        blobs_rewritten: count(&rewritten) as u64,
        false_claims_replaced,
        frames,
        entries,
        rewrite,
        runs,
    }
}

impl Plan {
    /// What the output's way blobs are, for the output header: a way blob in
    /// a rewrite run is re-encoded without enrichments; every other way blob
    /// keeps its payload and every header field but its index and tag keys,
    /// which a reframed passthrough replaces (`write_passthrough`), so its
    /// field 5 still describes its ways.
    pub(crate) fn ways(&self) -> crate::commands::output_header::Ways {
        let any_way_rewritten = self
            .entries
            .iter()
            .zip(&self.rewrite)
            .any(|(e, &r)| r && e.index.kind == ElementKind::Way);
        crate::commands::output_header::Ways::from_copy(any_way_rewritten, false)
    }
}

/// An element's ID, whatever its kind.
fn element_id(element: &Element<'_>) -> i64 {
    match element {
        Element::DenseNode(dn) => dn.id(),
        Element::Node(n) => n.id(),
        Element::Way(w) => w.id(),
        Element::Relation(r) => r.id(),
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
/// sweep is kind-gated and drops off-kind elements handed to it (the
/// `cat --dedupe` regression where `merge_pbf([A, A])` lost its ways and
/// relations); it refuses a believed claim they contradict, and a
/// restructured frame relies on the drop to give each entry its own kind.
///
/// The ranges compared here are claims for an entry pass 1 did not scan.
/// The sweep refuses an entry whose decoded IDs its claim does not describe
/// exactly, so every element of a run lies within the run's claimed reach
/// and every passthrough after it starts above that reach.
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
/// `cat --dedupe` asks the host for. The walk's descriptions are closed by
/// now; these are fresh ones.
///
/// `direct_reads` (`--direct-io`) opens them `O_DIRECT`, whatever the
/// output: the transport is the input's ([`InputFile::direct_align`]), and
/// every reader of a pass-2 source honours it. The sweeps and the owned
/// passthrough read through [`FrameBuf`]; a passthrough the output copies
/// (`splice`, the backend's `supports_copy_range`) goes through
/// `crate::write::copy_range`, which reads a direct input in aligned spans
/// and never hands it to `copy_file_range`. So a splicing output (`--io-uring`,
/// whose copy is a positioned read into its registered buffers) costs no
/// second descriptor and no buffered read, and the run holds one descriptor
/// per argument either way.
pub(crate) fn open_sources(
    inputs: Vec<PinnedInput>,
    direct_reads: bool,
    splice: bool,
    invocation: &crate::host::Recorder,
) -> Result<Sources> {
    let direct = direct_reads && cfg!(feature = "linux-direct-io");
    let mut files = Vec::with_capacity(inputs.len());
    for pin in inputs {
        files.push(Source::open(&pin, direct, invocation)?);
    }
    Ok(Sources {
        files,
        use_copy_range: splice,
    })
}

/// Pass 2's inputs, one per argument, and whether passthrough may splice
/// from them.
pub(crate) struct Sources {
    files: Vec<Source>,
    use_copy_range: bool,
}

impl Sources {
    /// Whether pass 2 reads its inputs `O_DIRECT`.
    #[cfg(test)]
    pub(crate) fn direct(&self) -> bool {
        self.files.iter().any(|f| f.input.direct_align().is_some())
    }
}

impl std::ops::Index<usize> for Sources {
    type Output = Source;
    fn index(&self, i: usize) -> &Source {
        &self.files[i]
    }
}

/// One pass-2 input, opened buffered or `O_DIRECT` (the transport rides on
/// the [`InputFile`]).
pub(crate) struct Source {
    input: InputFile,
}

impl Source {
    fn open(pin: &PinnedInput, direct: bool, _invocation: &crate::host::Recorder) -> Result<Self> {
        #[cfg(feature = "linux-direct-io")]
        if direct {
            return Ok(Self {
                input: InputFile::direct(pin, _invocation)?,
            });
        }
        let _ = direct;
        Ok(Self {
            input: InputFile::open(pin)?,
        })
    }

    /// Locate a failure at byte `offset` of this input.
    #[cold]
    pub(crate) fn locate_at(&self, offset: u64, err: Error) -> Error {
        self.input.locate_at(offset, err)
    }
}

/// A frame read buffer, one per thread that reads frames: a plain buffer
/// for buffered sources and an aligned scratch for direct ones (remade when
/// a source's alignment differs from the one it was made for). Both follow
/// the read buffers' retention limit through [`FrameBuf::release_outlier`],
/// called once a frame's bytes are done with.
#[derive(Default)]
pub(crate) struct FrameBuf {
    buf: crate::read::read_buf::ReadBuf,
    #[cfg(feature = "linux-direct-io")]
    scratch: Option<crate::read::direct_reader::AlignedScratch>,
}

impl FrameBuf {
    /// Re-read a complete frame from its input. A frame that no longer reads
    /// (the input changed between the passes) fails located at the input and
    /// the frame's offset.
    pub(crate) fn read(&mut self, source: &Source, frame: &Frame) -> Result<&[u8]> {
        #[allow(clippy::cast_possible_truncation)]
        let len = frame.frame_len as usize;
        #[cfg(feature = "linux-direct-io")]
        if let Some(align) = source.input.direct_align() {
            use crate::read::direct_reader::{AlignedScratch, read_aligned_in};
            use std::os::unix::fs::FileExt as _;
            let scratch = match &mut self.scratch {
                Some(s) if s.align() == align => s,
                slot => slot.insert(AlignedScratch::new(align)),
            };
            let file = source.input.file();
            return read_aligned_in(len, frame.file_offset, scratch, |dst, at| {
                file.read_at(dst, at)
            })
            .map_err(|e| source.locate_at(frame.file_offset, Error::from(e)));
        }
        self.buf
            .read_exact_at(&**source.input.file(), frame.file_offset, len)
            .map_err(|e| source.locate_at(frame.file_offset, Error::from(e)))
    }

    /// Apply the read buffers' retention limit (`crate::read::read_buf`) to
    /// whichever buffer the last frame went through.
    pub(crate) fn release_outlier(&mut self, invocation: &crate::host::Recorder) {
        self.buf.release_outlier(invocation);
        #[cfg(feature = "linux-direct-io")]
        if let Some(scratch) = &mut self.scratch {
            scratch.release_outlier(invocation);
        }
    }
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
    /// Input blobs copied through, raw or reframed.
    pub(crate) blobs_passthrough: u64,
    /// Input blobs with at least one entry rewritten ([`Plan::blobs_rewritten`]).
    pub(crate) blobs_rewritten: u64,
    pub(crate) duplicates_removed: u64,
    pub(crate) copy_range_calls: u64,
    pub(crate) copy_range_coalesced: u64,
}

/// What one overlap run emitted, and what it held while it ran (the
/// resource figures of `sweep.rs`'s module docs).
#[derive(Debug, Default)]
struct RunCounts {
    elements: u64,
    duplicates: u64,
    pinned_high_water: usize,
    demoted: u64,
    chunks: u64,
    chunk_high_water: usize,
    staged_high_water: usize,
}

/// The resource figures of every run, folded as the write loop finishes
/// each: maxima of the high-water marks, sums of the counts.
#[derive(Default)]
struct RunResources {
    pinned_high_water: usize,
    demoted: u64,
    chunks: u64,
    chunk_high_water: usize,
    staged_high_water: usize,
}

impl RunResources {
    fn fold(&mut self, run: &RunCounts) {
        self.pinned_high_water = self.pinned_high_water.max(run.pinned_high_water);
        self.demoted += run.demoted;
        self.chunks += run.chunks;
        self.chunk_high_water = self.chunk_high_water.max(run.chunk_high_water);
        self.staged_high_water = self.staged_high_water.max(run.staged_high_water);
    }

    fn emit(&self, invocation: &crate::host::Recorder) {
        invocation.counter(
            "overlap_run_pinned_high_water_bytes",
            self.pinned_high_water,
        );
        invocation.counter("overlap_run_demoted_cursors", self.demoted);
        invocation.counter("overlap_run_chunks", self.chunks);
        invocation.counter("overlap_run_chunk_high_water_bytes", self.chunk_high_water);
        invocation.counter(
            "overlap_run_staged_high_water_bytes",
            self.staged_high_water,
        );
    }
}

/// Pass 2: write the plan in order to `writer`.
///
/// `dedupe` keeps, inside overlap runs, one element per (id, version), the
/// copy from the earliest input, then frame, then position. Passthrough splices when `sources` allows
/// it (see [`open_sources`]).
///
/// With no overlap runs (an already-sorted input: production `sort`) this is
/// one serial loop and no thread starts. Otherwise each run is one task of a
/// streamed ordered pool ([`crate::ordered_pool::run_ordered_streams`]):
/// runs are swept on worker threads (sized as the decode pools are), each
/// framing its own blocks and sending them, packed into chunks of at most
/// [`OVERLAP_RUN_CHUNK_BYTES`], through a channel of
/// [`OVERLAP_RUN_CHANNEL_DEPTH`] messages, while this thread writes the
/// entries in order, a run's chunks as they arrive. At most `window` runs
/// ([`OVERLAP_RUN_WINDOW_PER_WORKER`] per worker) are issued ahead of the
/// one being written. Memory: at most `window x depth` chunks buffered, each
/// within the chunk budget (a frame larger than a chunk is sent in pieces),
/// plus, per worker, the chunk being built and the unsent part of the
/// output it is shipping, beside each active sweep's open cursors (see
/// `sweep.rs`: decoded blocks of its live overlap, sparse tails demoted past
/// [`SWEEP_PINNED_BYTES`]; up to `min(window, workers)` sweeps at once).
///
/// Progress, shutdown and the failure order are the pool's; the first error
/// met in run order is returned, unless a worker panicked, which the join
/// re-raises. Its stall counters (`ordered_pool_*_wait_ns`) are this pass's:
/// the write loop waiting on a sweep is `ordered_pool_result_recv_wait_ns`,
/// a sweep blocked behind the write loop `ordered_pool_result_send_wait_ns`.
/// What the sweeps held (`overlap_run_*`, see `sweep.rs`) is emitted once
/// the pool has finished, and only when there were runs.
///
/// [`OVERLAP_RUN_CHUNK_BYTES`]: crate::tunables::OVERLAP_RUN_CHUNK_BYTES
/// [`OVERLAP_RUN_CHANNEL_DEPTH`]: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH
/// [`OVERLAP_RUN_WINDOW_PER_WORKER`]: crate::tunables::OVERLAP_RUN_WINDOW_PER_WORKER
/// [`SWEEP_PINNED_BYTES`]: crate::tunables::SWEEP_PINNED_BYTES
#[hotpath::measure]
pub(crate) fn write_plan(
    plan: &Plan,
    sources: &Sources,
    writer: &mut PbfWriter<FileWriter>,
    dedupe: bool,
    compression: crate::writer::Compression,
    host: &crate::host::Entered<'_>,
) -> Result<WriteCounts> {
    // The writer's invocation, whose framing counters the sweeps record into
    // now that they frame their own blocks.
    let writer_invocation = writer.cancel_token().permit();
    let mut out = PlanWriter {
        plan,
        sources,
        writer,
        use_copy_range: sources.use_copy_range,
        // Counted per frame by the plan: a blob holding several kinds is
        // rewritten in several runs but is one blob.
        counts: WriteCounts {
            blobs_rewritten: plan.blobs_rewritten,
            ..WriteCounts::default()
        },
        copy: CopyRun::default(),
        frame_buf: FrameBuf::default(),
        cursor: 0,
        resources: RunResources::default(),
    };
    if !plan.runs.is_empty() {
        let metrics = &writer_invocation.metrics().writer;
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
        crate::ordered_pool::run_ordered_streams(
            plan.runs.len(),
            workers,
            crate::ordered_pool::StreamConfig {
                window,
                depth: crate::tunables::OVERLAP_RUN_CHANNEL_DEPTH,
                cancel: &host.invocation,
            },
            |_| FrameBuf::default(),
            |frame_buf: &mut FrameBuf, k, emitter| {
                let run = &plan.runs[k];
                sweep::Sweep {
                    entries: &plan.entries[run.start..run.end],
                    frames: &plan.frames,
                    sources,
                    kind: run.kind,
                    dedupe,
                    cancel: &host.invocation,
                    emitter,
                    compression: &compression,
                    metrics,
                }
                .run(frame_buf)
            },
            &mut out,
        )?;
        out.resources.emit(&host.invocation);
    }
    out.passthrough_until(plan.entries.len())?;
    let PlanWriter {
        mut copy,
        writer,
        mut counts,
        ..
    } = out;
    copy.flush(writer, sources, &mut counts)?;
    Ok(counts)
}

/// The ordered write loop: passthrough entries written (or coalesced into a
/// copy) in plan order, each overlap run's blocks at its position.
struct PlanWriter<'a> {
    plan: &'a Plan,
    sources: &'a Sources,
    writer: &'a mut PbfWriter<FileWriter>,
    use_copy_range: bool,
    counts: WriteCounts,
    /// Passthrough coalescer: adjacent blobs contiguous in one input collapse
    /// into one `copy_file_range`. On already-sorted input this is the entire
    /// file, cutting the syscall count from O(blobs) to O(1). Mirrors the
    /// contiguous-range coalescer in apply_changes.
    copy: CopyRun,
    frame_buf: FrameBuf,
    /// The next plan entry to write.
    cursor: usize,
    /// The runs' resource figures, folded as each run finishes.
    resources: RunResources,
}

impl PlanWriter<'_> {
    /// Write the passthrough entries from the cursor up to `end`. Every one
    /// must be a copy: a rewrite entry the runs do not start at is a plan
    /// defect.
    fn passthrough_until(&mut self, end: usize) -> Result<()> {
        while self.cursor < end {
            let i = self.cursor;
            if self.plan.rewrite[i] {
                return Err(Error::internal("rewrite entry outside its planned run"));
            }
            // A copied entry is its frame's only one: a frame with several
            // is restructured, and every restructured entry is rewritten.
            let entry = &self.plan.entries[i];
            let frame = &self.plan.frames[entry.frame];
            match &frame.reindex {
                None if self.use_copy_range => {
                    self.copy
                        .offer(frame, self.writer, self.sources, &mut self.counts)?;
                }
                reindex => {
                    self.copy
                        .flush(self.writer, self.sources, &mut self.counts)?;
                    write_passthrough(
                        frame,
                        reindex.as_ref(),
                        &self.sources[frame.source],
                        self.writer,
                        &mut self.frame_buf,
                    )?;
                }
            }
            count_passthrough(entry, &mut self.counts);
            self.counts.blobs_passthrough += 1;
            self.cursor += 1;
        }
        Ok(())
    }
}

impl crate::ordered_pool::StreamConsumer<sweep::Chunk, RunCounts> for PlanWriter<'_> {
    fn before_task(&mut self, k: usize) -> Result<()> {
        let start = self.plan.runs[k].start;
        self.passthrough_until(start)?;
        if self.cursor != start {
            return Err(Error::internal("rewrite entry outside its planned run"));
        }
        self.copy.flush(self.writer, self.sources, &mut self.counts)
    }

    fn item(&mut self, _k: usize, chunk: sweep::Chunk) -> Result<()> {
        self.writer.write_raw_chunks(chunk)
    }

    fn finish_task(&mut self, k: usize, emitted: RunCounts) -> Result<()> {
        let run = &self.plan.runs[k];
        match run.kind {
            ElementKind::Node => self.counts.nodes += emitted.elements,
            ElementKind::Way => self.counts.ways += emitted.elements,
            ElementKind::Relation => self.counts.relations += emitted.elements,
        }
        self.counts.duplicates_removed += emitted.duplicates;
        self.resources.fold(&emitted);
        self.cursor = run.end;
        Ok(())
    }
}

/// The in-flight `copy_file_range` stretch: (source, start, end).
#[derive(Default)]
struct CopyRun(Option<(usize, u64, u64)>);

impl CopyRun {
    /// Extend the stretch with `entry` when it continues it in the same
    /// input, otherwise flush the stretch and start a new one.
    fn offer(
        &mut self,
        frame: &Frame,
        writer: &mut PbfWriter<FileWriter>,
        sources: &Sources,
        counts: &mut WriteCounts,
    ) -> Result<()> {
        match &mut self.0 {
            Some((source, _, end)) if *source == frame.source && *end == frame.file_offset => {
                *end += frame.frame_len;
                counts.copy_range_coalesced += 1;
            }
            _ => {
                self.flush(writer, sources, counts)?;
                self.0 = Some((
                    frame.source,
                    frame.file_offset,
                    frame.file_offset + frame.frame_len,
                ));
            }
        }
        Ok(())
    }

    /// Hand the stretch to the writer as one copy. No-op when none is open.
    fn flush(
        &mut self,
        writer: &mut PbfWriter<FileWriter>,
        sources: &Sources,
        counts: &mut WriteCounts,
    ) -> Result<()> {
        if let Some((source, start, end)) = self.0.take() {
            writer.write_raw_copy(&sources[source].input, start, end - start)?;
            counts.copy_range_calls += 1;
        }
        Ok(())
    }
}

/// Passthrough without the copy path: the frame raw when it keeps its own
/// header (an output that cannot splice, such as `--direct-io`), else
/// reframed with the index and tag keys pass 1 scanned, every other header
/// field (way members included: the payload, and so each way's position,
/// is unchanged) kept as `cat --reindex` keeps it.
fn write_passthrough(
    frame: &Frame,
    reindex: Option<&Reindex>,
    source: &Source,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut FrameBuf,
) -> Result<()> {
    let written = write_frame(frame, reindex, source, writer, frame_buf);
    frame_buf.release_outlier(writer.cancel_token());
    written
}

/// [`write_passthrough`]'s read and write, before the release.
fn write_frame(
    frame: &Frame,
    reindex: Option<&Reindex>,
    source: &Source,
    writer: &mut PbfWriter<FileWriter>,
    frame_buf: &mut FrameBuf,
) -> Result<()> {
    let frame_bytes = frame_buf.read(source, frame)?;
    match reindex {
        None => writer.write_raw(frame_bytes)?,
        Some(reindex) => {
            let mut header_buf = Vec::new();
            let reframed = frame_parts(frame_bytes)
                .and_then(|(header, blob)| {
                    crate::commands::cat::reindex::reframe_preserving(
                        header,
                        blob,
                        &reindex.index.serialize(),
                        reindex.tagdata.as_deref(),
                        &mut header_buf,
                    )
                    .map(|(bytes, _)| bytes)
                })
                .map_err(|e| source.locate_at(frame.file_offset, e))?;
            writer.write_raw(&reframed)?;
        }
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

/// Re-read and decode an overlap-run entry's frame, a read or decode
/// failure located at its input and frame offset.
///
/// The block owns its decoded bytes, so `buf` follows its retention limit
/// before this returns.
pub(crate) fn read_block(
    source: &Source,
    frame: &Frame,
    buf: &mut FrameBuf,
    invocation: &crate::host::Recorder,
) -> Result<crate::PrimitiveBlock> {
    let block = buf.read(source, frame).and_then(|bytes| {
        frame_parts(bytes)
            .and_then(|(_, blob)| decode_blob_to_primitiveblock(blob))
            .map_err(|e| source.locate_at(frame.file_offset, e))
    });
    buf.release_outlier(invocation);
    block
}

/// The BlobHeader and Blob bytes of a complete frame,
/// `[4-byte header_len][BlobHeader][Blob]`.
fn frame_parts(frame: &[u8]) -> Result<(&[u8], &[u8])> {
    if frame.len() < 4 {
        return Err(Error::invalid_data("frame too short"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let blob_start = 4 + header_len;
    if blob_start > frame.len() {
        return Err(Error::invalid_data("invalid header length"));
    }
    Ok((&frame[4..blob_start], &frame[blob_start..]))
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

// The sweep itself lives in `sweep.rs`: a borrowed k-way cursor merge.
mod sweep;
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ElementKind, min_id: i64, max_id: i64) -> BlobEntry {
        BlobEntry {
            frame: 0,
            index: BlobIndex {
                kind,
                min_id,
                max_id,
                count: 1,
                bbox: None,
            },
            intra_unsorted: false,
            restructured: false,
            index_scanned: true,
        }
    }

    fn frame() -> Frame {
        Frame {
            source: 0,
            file_offset: 0,
            frame_len: 0,
            reindex: None,
        }
    }

    /// Each entry on a frame of its own, numbered in order.
    fn indexed(mut entries: Vec<BlobEntry>) -> Indexed {
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.frame = i;
        }
        Indexed {
            frames: entries.iter().map(|_| frame()).collect(),
            entries,
            omitted: 0,
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
        let plan = plan(indexed(entries));
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

    /// A restructured frame's entries are each rewritten, wherever their
    /// kinds sort, and the frame counts once: as restructured, as rewritten,
    /// and as overlapping when one of its entries joins a component.
    #[test]
    fn a_restructured_frame_is_rewritten_per_kind_and_counted_once() {
        let mut entries = vec![
            entry(ElementKind::Node, 1, 5),
            entry(ElementKind::Way, 1, 5),
            entry(ElementKind::Way, 3, 9),
            entry(ElementKind::Relation, 50, 60),
        ];
        for e in &mut entries[..2] {
            e.restructured = true;
        }
        let mut indexed = indexed(entries);
        // Frame 0 holds the node and the first way entry.
        indexed.entries[1].frame = 0;
        indexed.frames.pop();
        indexed.entries[2].frame = 1;
        indexed.entries[3].frame = 2;
        indexed.frames[1].reindex = Some(Reindex {
            index: indexed.entries[2].index,
            tagdata: None,
            replaces_false_claim: true,
        });
        indexed.omitted = 4;
        let plan = plan(indexed);

        let runs: Vec<_> = plan.runs.iter().map(|r| (r.start, r.end, r.kind)).collect();
        assert_eq!(
            runs,
            vec![(0, 1, ElementKind::Node), (1, 3, ElementKind::Way)],
            "the node share is rewritten alone, the way share in its component"
        );
        assert_eq!(plan.rewrite, vec![true, true, true, false]);
        assert_eq!(
            (plan.overlapping, plan.intra_unsorted, plan.restructured),
            (2, 0, 1)
        );
        assert_eq!(plan.blobs_rewritten, 2, "frame 0 once, frame 1 once");
        assert_eq!(plan.blobs_total(), 7, "three frames plus four omitted");
        assert!(
            plan.frames[1].reindex.is_none(),
            "a rewritten frame is never reframed"
        );
        assert_eq!(plan.false_claims_replaced, 0);
    }

    /// The claim rule is `check --index`'s: kind, count and range exact, a
    /// claimed bbox containing the scanned one, an absent one no claim, and
    /// any claimed one false over a coordinate out of range.
    #[test]
    fn a_claim_holds_on_exact_ranges_and_a_containing_bbox() {
        let scanned = BlobIndex {
            bbox: Some(crate::blob_meta::BlobBbox::new(10, 20, 10, 20)),
            ..entry(ElementKind::Node, 1, 10).index
        };
        let claim = |f: fn(&mut BlobIndex)| {
            let mut c = scanned;
            f(&mut c);
            claim_holds(&c, &scanned)
        };
        assert!(claim(|_| {}));
        assert!(claim(|c| c.bbox = None), "no bbox is no spatial claim");
        assert!(claim(|c| {
            c.bbox = Some(crate::blob_meta::BlobBbox::new(0, 30, 0, 30));
        }));
        assert!(!claim(|c| {
            c.bbox = Some(crate::blob_meta::BlobBbox::new(15, 20, 10, 20));
        }));
        assert!(!claim(|c| c.count = 2));
        assert!(!claim(|c| c.max_id = 11));
        assert!(!claim(|c| c.min_id = 0));
        assert!(!claim(|c| c.kind = ElementKind::Way));
        let boxless = BlobIndex {
            bbox: None,
            ..scanned
        };
        assert!(
            !claim_holds(&scanned, &boxless),
            "a node blob the scan gives no box has a coordinate out of range, \
             which no claimed box contains"
        );
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
        scanned.index.count = 7;
        count_passthrough(&scanned, &mut counts);
        assert_eq!(counts.ways, 7, "a pass-1 scan count is observed");
        assert_eq!(counts.passthrough_claimed, 1_000_000);
    }
}
